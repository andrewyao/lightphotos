// SPDX-License-Identifier: GPL-3.0-or-later

//! Thumbnail generation plus a small on-disk thumbnail cache.
//!
//! macOS: via Apple's ImageIO (decode-at-size, uses embedded previews, applies
//! EXIF orientation). Mirrors `image_decode.rs`: open a `CGImageSource` from
//! the file, ask ImageIO for a thumbnail `CGImage`, then reuse
//! `image_decode::cgimage_to_rgba` to read it back as tightly-packed RGBA8.
//!
//! Non-mac: tries to extract a file's embedded EXIF/TIFF preview
//! (`try_extract_embedded_preview`, via `kamadak-exif`) first, then rawler's
//! own per-format `Decoder::full_image()` for containers the former can't
//! even open (CR3, RAF), falling back to a full decode-at-size through
//! `image_decode::decode` — see each function's non-mac doc comment for
//! exactly what is and isn't handled.
//!
//! `ThumbCache`, `EmbeddedPreview`, and the on-disk `.tw` cache format below
//! are platform-independent (no objc2 dependency) and unconditional.
//!
//! ## Pipeline position
//! - `loader.rs`'s `Job::Speed`/`Job::Preview` (Pipeline 1, opening a photo)
//!   call `decode_at_size` directly — `UseIfPresent` for the cheap first
//!   pass, `Never` for the forced screen-fit decode once that pass comes
//!   back short.
//! - `loader.rs`'s `Job::Thumb` (Pipeline 2, Grid/filmstrip) calls
//!   `ThumbCache::get_or_make`, which checks the on-disk `.tw` cache before
//!   falling back to `decode_at_size(.., UseIfPresent)`.
//! - wasm32 doesn't reach `ThumbCache` at all — `wasm_worker.rs` calls the
//!   bytes-based `embedded_preview_from_bytes`/`rawler_full_image_from_bytes`
//!   helpers directly, in-memory only. See `ARCHITECTURE.md`.

// TODO: remove once wired into loader (T3)
#![allow(dead_code)]

#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary,
    CFNumber, CFNumberType, CFRetained, CFString,
};
#[cfg(target_os = "macos")]
use objc2_image_io::{
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailFromImageIfAbsent,
    kCGImageSourceCreateThumbnailWithTransform, kCGImageSourceThumbnailMaxPixelSize,
};

#[cfg(target_os = "macos")]
use crate::image_decode::cgimage_to_rgba;
use crate::image_decode::{DecodedImage, PixelFormat};

/// Whether ImageIO may substitute the file's embedded preview for a real
/// decode-at-size.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum EmbeddedPreview {
    /// Take the embedded preview when the file has one. Right for grid
    /// thumbnails: they're small, so a camera's embedded JPEG is already more
    /// than enough detail, and skipping the decode is most of the speed.
    UseIfPresent,
    /// Always decode from the full image. Right for the loupe's screen-fit
    /// preview: embedded previews are typically ~1600px, which would show as
    /// visible softness at the size the loupe displays.
    Never,
}

/// Decode `path` at a reduced size, longest side at most `max_px`.
///
/// This is the fast path that makes the loupe's preview tier worth having:
/// ImageIO scales *during* decode, unlike `image_decode::decode`, which decodes
/// the full image and only then draws it down — strictly more work than not
/// downscaling at all.
#[cfg(target_os = "macos")]
pub fn decode_at_size(
    path: &Path,
    max_px: u32,
    embedded: EmbeddedPreview,
) -> Result<DecodedImage, String> {
    let source = crate::image_decode::open_image_source(path)?;

    let options = build_thumbnail_options(max_px, embedded)?;

    // SAFETY: `source` is valid; `options` is a CFDictionary whose keys are the
    // documented thumbnail option keys and whose values are the correct CF types
    // (CFBoolean / CFNumber). The returned CGImage is +1 retained and wrapped in
    // CFRetained, which releases it on drop.
    let image: CFRetained<_> = unsafe { source.thumbnail_at_index(0, Some(&options)) }
        .ok_or("ImageIO could not create thumbnail")?;

    let w = objc2_core_graphics::CGImage::width(Some(&image)) as u32;
    let h = objc2_core_graphics::CGImage::height(Some(&image)) as u32;
    if w == 0 || h == 0 {
        return Err("thumbnail has zero dimension".into());
    }

    // Convert the thumbnail CGImage to RGBA at its own (already-scaled) size.
    cgimage_to_rgba(&image, w, h)
}

/// Decode `path` at a reduced size, longest side at most `max_px`.
///
/// `EmbeddedPreview::Never` (the loupe's screen-fit preview) skips the
/// preview-extraction branch entirely and always does a real full decode —
/// there's no cross-platform equivalent of ImageIO's decode-at-size, so this
/// is a full decode followed by a resize, same shape as
/// `image_decode::decode`'s non-mac arm (which this calls directly).
/// `EmbeddedPreview::UseIfPresent` (grid/filmstrip thumbnails) tries the
/// file's embedded preview ([`try_extract_embedded_preview`] — cheap when it
/// works, no full decode) and falls back to a real full decode-at-size via
/// `image_decode::decode` on `None` (missing preview, decode failure,
/// unsupported format). That fallback is what keeps the extractor safe to
/// treat as best-effort: nothing it can get wrong actually fails the request.
#[cfg(not(target_os = "macos"))]
pub fn decode_at_size(
    path: &Path,
    max_px: u32,
    embedded: EmbeddedPreview,
) -> Result<DecodedImage, String> {
    match embedded {
        EmbeddedPreview::Never => crate::image_decode::decode(path, max_px),
        EmbeddedPreview::UseIfPresent => try_extract_embedded_preview(path, max_px)
            .map(Ok)
            .unwrap_or_else(|| crate::image_decode::decode(path, max_px)),
    }
}

/// Best-effort extraction of a RAW/TIFF-based file's embedded EXIF thumbnail:
/// the standard baseline JPEG thumbnail stored in IFD1 via the
/// `JPEGInterchangeFormat`/`JPEGInterchangeFormatLength` tags (TIFF 6.0 / EXIF
/// 2.3 §4.6.4), decoded and resized to fit `max_px`.
///
/// **What this does and doesn't handle**: camera RAW containers (CR2, NEF,
/// ARW, DNG, ...) are TIFF-based, so `kamadak-exif`'s generic TIFF/EXIF reader
/// (`Reader::read_from_container`, which detects the TIFF magic and reads the
/// whole file) can open them directly, and this reads the same baseline
/// thumbnail tag every EXIF-aware JPEG/TIFF viewer already relies on. That
/// baseline thumbnail is typically small — cameras commonly store around
/// 160x120 — not a full-size preview. Several formats additionally carry a
/// much larger preview via a manufacturer-specific mechanism (CR2's second
/// IFD, a DNG sub-image with `NewSubfileType=1`, MakerNote `PreviewImageStart`
/// tags, ...); none of that is parsed here — a genuinely complete marker
/// parser was explicitly out of scope for this first pass. Never upscales: if
/// the extracted preview is already smaller than `max_px`, it's returned as-is
/// (still satisfies "longest side at most `max_px`").
///
/// Returns `None` on any failure — unreadable file, no TIFF/EXIF structure, no
/// IFD1 thumbnail tags, an out-of-bounds offset/length, or a blob that
/// doesn't actually decode as JPEG — so `decode_at_size`'s full-decode
/// fallback is always safe to take; this must never be what makes a thumbnail
/// request fail outright.
#[cfg(not(target_os = "macos"))]
fn try_extract_embedded_preview(path: &Path, max_px: u32) -> Option<DecodedImage> {
    let bytes = fs::read(path).ok()?;
    embedded_preview_from_bytes(&bytes, max_px)
        .or_else(|| rawler_full_image_from_bytes(&bytes, max_px))
}

/// The bytes-based core of [`try_extract_embedded_preview`] above — same
/// logic, minus the file read, so wasm32's own thumbnail decode
/// (`app/web.rs`, reading via `FileSystemFileHandle` instead of
/// `std::fs::read`) can share it exactly rather than re-implementing EXIF
/// thumbnail extraction a second time. `kamadak-exif`'s
/// `read_from_container` only needs `Read + Seek`, which `io::Cursor` gives
/// a byte slice for free — no real file involved at all.
#[cfg(not(target_os = "macos"))]
pub(crate) fn embedded_preview_from_bytes(bytes: &[u8], max_px: u32) -> Option<DecodedImage> {
    let mut reader = std::io::Cursor::new(bytes);
    let source = exif::Reader::new().read_from_container(&mut reader).ok()?;

    let offset = source
        .get_field(exif::Tag::JPEGInterchangeFormat, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    let length = source
        .get_field(exif::Tag::JPEGInterchangeFormatLength, exif::In::THUMBNAIL)?
        .value
        .get_uint(0)? as usize;
    if length == 0 {
        return None;
    }

    let buf = source.buf();
    let end = offset.checked_add(length)?;
    if end > buf.len() {
        return None;
    }
    let jpeg_bytes = &buf[offset..end];

    let img = image::load_from_memory_with_format(jpeg_bytes, image::ImageFormat::Jpeg)
        .ok()?
        .into_rgba8();
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return None;
    }

    // The orientation tag describes the *main* image, not the embedded
    // thumbnail — read it from the primary IFD, matching
    // `image_decode.rs`'s non-mac `orientation_of`. Defaults to identity (1)
    // when absent, same as every other orientation read path in this crate.
    let orientation = source
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
        .and_then(|f| f.value.get_uint(0))
        .unwrap_or(1) as u8;

    let (nw, nh) = crate::image_decode::fit_within(w, h, max_px);
    let rgba = if (nw, nh) == (w, h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
    };
    // Resize before orienting (same order `decode_raw_nonmac` uses) so the
    // fit-within math runs against the pre-rotation aspect ratio consistently
    // with the rest of this crate; orientation swaps width/height for the
    // 5..=8 cases, which would otherwise fit the wrong ratio.
    Some(crate::image_decode::apply_exif_orientation(
        DecodedImage {
            width: nw,
            height: nh,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        },
        orientation,
    ))
}

/// Fallback for when [`embedded_preview_from_bytes`] above can't even open
/// the container — CR3's ISO-BMFF (`ftyp`/`crx`) wrapper and RAF's
/// proprietary `"FUJIFILM..."` header both fail `kamadak-exif`'s TIFF/JPEG
/// magic sniff outright, so that function always returns `None` for them,
/// regardless of file content.
///
/// Asks `rawler`'s own `Decoder::full_image()` instead — a per-format trait
/// method (default `Ok(None)`) that formats overriding it use to hand back
/// whatever embedded JPEG/preview their container carries, without touching
/// the CFA/sensor block or running any demosaic.
///
/// **Gated to `FormatHint::RAF`/`CR3` on purpose** — those are the only two
/// formats [`embedded_preview_from_bytes`] can't open at all, which is this
/// function's actual job. `full_image()` is *also* overridden by several
/// TIFF-based decoders this crate treats as RAW (CR2, NEF, ARW, DNG, RW2,
/// PEF — confirmed against `vendor/rawler-0.7.2/src/decoders/*.rs`), whose
/// containers `embedded_preview_from_bytes` opens fine already; an earlier,
/// ungated version of this function asked `full_image()` unconditionally for
/// any format, and for those it would win over the caller's real RAW-quality
/// decode whenever the baseline IFD1 thumbnail was "too small" — which is
/// nearly always. That silently substituted the camera's own embedded JPEG
/// (its own in-camera tone/color rendering, often a very different image)
/// for the Loupe's actual linear-RAW develop, on every ARW/CR2/NEF/DNG/RW2/
/// PEF file, confirmed via a real Sony ARW: embedded JPEG mean sRGB ~0.27 vs
/// the real demosaic's ~0.18 — a completely different picture, not a subtle
/// tonemap bug. Scoped back to its original purpose.
///
/// Wrapped in `catch_unwind` for defense-in-depth, matching
/// `raw/preview.rs`'s own convention around `rawler` calls — largely a
/// no-op on `wasm32-unknown-unknown` (`panic = "abort"`, no real unwinding),
/// but `full_image()`'s implementations read their embedded-image
/// offset/length fields through `RawSource::subview`, which is
/// bounds-checked and `Result`-returning rather than raw slice indexing, so
/// this call path isn't the panic-prone kind to begin with.
#[cfg(not(target_os = "macos"))]
pub(crate) fn rawler_full_image_from_bytes(bytes: &[u8], max_px: u32) -> Option<DecodedImage> {
    let run = std::panic::AssertUnwindSafe(|| -> Option<DecodedImage> {
        let source = rawler::rawsource::RawSource::new_from_slice(bytes);
        let params = rawler::decoders::RawDecodeParams::default();
        let decoder = rawler::get_decoder(&source).ok()?;
        if !matches!(
            decoder.format_hint(),
            rawler::decoders::FormatHint::RAF | rawler::decoders::FormatHint::CR3
        ) {
            return None;
        }

        let dynamic = decoder.full_image(&source, &params).ok().flatten()?;
        let img = dynamic.into_rgba8();
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }

        // Orientation from `raw_metadata()`, not the embedded image's own
        // EXIF (may be absent, or describe only the sub-image rather than
        // the shot) — same source and the same `Option<u16>` ->
        // `rawler::Orientation` -> EXIF-code conversion `decode_raw_nonmac`
        // already uses for the full-decode path.
        let orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|meta| meta.exif.orientation)
            .map(|code| {
                crate::image_decode::exif_code_from_rawler_orientation(
                    rawler::Orientation::from_u16(code),
                )
            })
            .unwrap_or(1);

        let (nw, nh) = crate::image_decode::fit_within(w, h, max_px);
        let rgba = if (nw, nh) == (w, h) {
            img.into_raw()
        } else {
            image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
        };
        // Resize before orienting — same order `embedded_preview_from_bytes`
        // uses, for the same reason (orientation swaps w/h for cases 5..=8,
        // which would otherwise fit the wrong aspect ratio).
        Some(crate::image_decode::apply_exif_orientation(
            DecodedImage {
                width: nw,
                height: nh,
                rgba,
                pixel_format: PixelFormat::Srgb8,
            },
            orientation,
        ))
    });
    std::panic::catch_unwind(run).ok().flatten()
}

/// Build the options `CFDictionary` for `CGImageSourceCreateThumbnailAtIndex`.
#[cfg(target_os = "macos")]
fn build_thumbnail_options(
    max_px: u32,
    embedded: EmbeddedPreview,
) -> Result<CFRetained<CFDictionary>, String> {
    // kCGImageSourceThumbnailMaxPixelSize wants an integer CFNumber.
    let max_px_i: i32 = max_px as i32;
    // SAFETY: value_ptr points at a valid i32 matching SInt32Type.
    let max_px_num = unsafe {
        CFNumber::new(
            None,
            CFNumberType::SInt32Type,
            &max_px_i as *const i32 as *const c_void,
        )
    }
    .ok_or("could not create CFNumber for max pixel size")?;

    // SAFETY: kCFBooleanTrue is a valid static; present at runtime on macOS.
    let bool_true = unsafe { kCFBooleanTrue }.ok_or("kCFBooleanTrue unavailable")?;

    // Keys and values as raw CFType pointers, in matching order.
    // SAFETY: these statics are valid CFString option keys at runtime.
    let mut keys: [*const c_void; 3] = unsafe {
        [
            match embedded {
                EmbeddedPreview::UseIfPresent => {
                    kCGImageSourceCreateThumbnailFromImageIfAbsent as *const CFString
                }
                EmbeddedPreview::Never => {
                    kCGImageSourceCreateThumbnailFromImageAlways as *const CFString
                }
            } as *const c_void,
            kCGImageSourceThumbnailMaxPixelSize as *const CFString as *const c_void,
            kCGImageSourceCreateThumbnailWithTransform as *const CFString as *const c_void,
        ]
    };
    let mut values: [*const c_void; 3] = [
        bool_true as *const _ as *const c_void,
        &*max_px_num as *const CFNumber as *const c_void,
        bool_true as *const _ as *const c_void,
    ];

    // SAFETY: keys/values are valid arrays of 3 CFType pointers; the standard
    // CFType callbacks retain/release entries, so the dictionary keeps its own
    // references and the temporaries (max_px_num) may drop after this returns.
    let dict = unsafe {
        CFDictionary::new(
            None,
            keys.as_mut_ptr(),
            values.as_mut_ptr(),
            keys.len() as isize,
            &kCFTypeDictionaryKeyCallBacks,
            &kCFTypeDictionaryValueCallBacks,
        )
    }
    .ok_or("could not create thumbnail options dictionary")?;

    Ok(dict)
}

/// On-disk thumbnail cache rooted at
/// `~/Library/Caches/com.lightphotos/thumbnails/`.
///
/// Cache files are named `<hex fnv1a-64 key>.tw`. The key hashes the
/// canonicalized source path, its mtime (ns), its length, and `max_px`, so any
/// change to the source file or requested size yields a fresh entry.
pub struct ThumbCache {
    root: PathBuf,
}

impl ThumbCache {
    /// Soft cap on the total size of the on-disk `.tw` cache. The cache keys on
    /// (path, mtime, len, max_px), so edits/resizes/new folders accumulate stale
    /// entries indefinitely; a startup sweep evicts the least-recently-modified
    /// files back under this budget.
    const BUDGET_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB

    /// Create the cache, ensuring the root directory exists. If `$HOME` is
    /// unavailable, falls back to a relative `./.lightphotos-thumbnails`.
    pub fn new() -> ThumbCache {
        let root = match std::env::var("HOME") {
            Ok(home) => {
                let base = PathBuf::from(home).join("Library/Caches");
                let root = base.join("com.lightphotos/thumbnails");
                // Preserve the cache built under the pre-rename name.
                crate::paths::migrate_legacy_dir(&root, &base.join("com.imageviewer/thumbnails"));
                root
            }
            Err(_) => PathBuf::from(".lightphotos-thumbnails"),
        };
        // Best-effort: errors here surface later on read/write.
        let _ = fs::create_dir_all(&root);

        // Prune stale entries off the main path so startup never blocks on a
        // large cache directory. Best-effort — any failure just leaves the cache.
        let prune_root = root.clone();
        // Builder::spawn (Result-returning), not the bare free `thread::spawn`
        // (which panics on failure): not every target has real threads yet
        // (e.g. wasm32 pre-Web-Worker-pool — see the wasm port plan's M4),
        // and startup must degrade (no pruning happens) rather than crash.
        if let Err(e) = std::thread::Builder::new()
            .name("thumb-cache-prune".into())
            .spawn(move || prune_dir(&prune_root, ThumbCache::BUDGET_BYTES))
        {
            eprintln!("[thumbnail] could not spawn cache-prune thread: {e}");
        }

        ThumbCache { root }
    }

    /// Return a cached thumbnail for `(path, max_px)` if present on disk;
    /// otherwise generate it via `decode_at_size(.., UseIfPresent)`, persist
    /// it, and return it.
    pub fn get_or_make(&self, path: &Path, max_px: u32) -> Result<Arc<DecodedImage>, String> {
        let key = self.cache_key(path, max_px)?;
        let file = self.root.join(format!("{:016x}.tw", key));

        if let Ok(img) = read_tw(&file) {
            return Ok(Arc::new(img));
        }

        let img = decode_at_size(path, max_px, EmbeddedPreview::UseIfPresent)?;
        // Best-effort write; a failed cache write must not fail the request.
        let _ = write_tw(&file, &img);
        Ok(Arc::new(img))
    }

    /// FNV-1a 64-bit hash of (canonical path bytes, mtime_ns, len, max_px).
    /// Hand-rolled so the key is stable across process runs (unlike
    /// `std::collections::hash_map::DefaultHasher`).
    fn cache_key(&self, path: &Path, max_px: u32) -> Result<u64, String> {
        // normalize() yields the canonical path when the file exists; if it
        // doesn't, the metadata() call below fails and we return Err anyway.
        let canon = crate::paths::normalize(path);
        let meta = fs::metadata(&canon).map_err(|e| format!("metadata: {e}"))?;
        let mtime_ns = meta
            .modified()
            .map_err(|e| format!("mtime: {e}"))?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let len = meta.len();

        let mut h = crate::hash::Fnv1a::new();
        h.write(canon.as_os_str().as_encoded_bytes());
        h.write(&mtime_ns.to_le_bytes());
        h.write(&len.to_le_bytes());
        h.write(&max_px.to_le_bytes());
        Ok(h.finish())
    }
}

impl Default for ThumbCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Evict the least-recently-modified `.tw` files in `root` until the total size
/// of remaining cache files is at or below `budget`. Best-effort: metadata and
/// remove errors are ignored, and a cache already under budget does no work.
fn prune_dir(root: &Path, budget: u64) {
    let Ok(entries) = fs::read_dir(root) else {
        return;
    };
    // (path, size, mtime) for every cache file.
    let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = Vec::new();
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("tw") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let len = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total += len;
        files.push((path, len, mtime));
    }
    if total <= budget {
        return;
    }
    // Oldest first, delete until under budget.
    files.sort_by_key(|(_, _, mtime)| *mtime);
    for (path, len, _) in files {
        if total <= budget {
            break;
        }
        if fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(len);
        }
    }
}

/// `.tw` file format: little-endian `u32 width`, `u32 height`, then
/// `width * height * 4` RGBA bytes.
fn write_tw(path: &Path, img: &DecodedImage) -> Result<(), String> {
    let mut buf = Vec::with_capacity(8 + img.rgba.len());
    buf.extend_from_slice(&img.width.to_le_bytes());
    buf.extend_from_slice(&img.height.to_le_bytes());
    buf.extend_from_slice(&img.rgba);

    // Atomic-ish: write to a temp sibling then rename.
    let tmp = path.with_extension("tw.tmp");
    {
        let mut f = fs::File::create(&tmp).map_err(|e| format!("create temp: {e}"))?;
        f.write_all(&buf).map_err(|e| format!("write temp: {e}"))?;
    }
    if let Err(e) = fs::rename(&tmp, path) {
        // Don't leave the orphaned temp file behind on failure.
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

fn read_tw(path: &Path) -> Result<DecodedImage, String> {
    let mut f = fs::File::open(path).map_err(|e| format!("open: {e}"))?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).map_err(|e| format!("read: {e}"))?;
    if buf.len() < 8 {
        return Err("truncated .tw header".into());
    }
    let width = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let height = u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]);
    let expected = (width as usize) * (height as usize) * 4;
    let rgba = buf.split_off(8);
    if rgba.len() != expected {
        return Err(format!(
            "truncated .tw body: {} bytes, expected {}",
            rgba.len(),
            expected
        ));
    }
    Ok(DecodedImage {
        width,
        height,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tw_round_trip_is_identical() {
        // A known small (w, h, RGBA) blob; no real image fixture needed.
        let img = DecodedImage {
            width: 2,
            height: 2,
            rgba: vec![
                1, 2, 3, 255, 4, 5, 6, 255, // row 0
                7, 8, 9, 255, 10, 11, 12, 255, // row 1
            ],
            pixel_format: PixelFormat::Srgb8,
        };

        let dir = std::env::temp_dir().join(format!("iv-tw-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("blob.tw");

        write_tw(&file, &img).expect("write should succeed");
        let back = read_tw(&file).expect("read should succeed");

        assert_eq!(back.width, img.width);
        assert_eq!(back.height, img.height);
        assert_eq!(back.rgba, img.rgba);

        let _ = fs::remove_file(&file);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn prune_evicts_until_under_budget() {
        let dir = std::env::temp_dir().join(format!("iv-prune-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        // Ten 1 KiB .tw files (10 KiB total) plus a non-.tw file that must survive.
        for i in 0..10 {
            fs::write(dir.join(format!("f{i}.tw")), vec![0u8; 1024]).unwrap();
        }
        fs::write(dir.join("keep.txt"), vec![0u8; 4096]).unwrap();

        // Budget of 4 KiB → at most 4 of the .tw files may remain.
        prune_dir(&dir, 4096);

        let remaining_tw: u64 = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("tw"))
            .map(|e| e.metadata().unwrap().len())
            .sum();
        assert!(
            remaining_tw <= 4096,
            "cache should be pruned under budget, got {remaining_tw}"
        );
        assert!(
            dir.join("keep.txt").exists(),
            "non-cache files must be left alone"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
