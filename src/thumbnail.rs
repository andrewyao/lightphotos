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
//! `ThumbCache`, `EmbeddedPreview`, and the on-disk cache helpers below are
//! platform-independent (no objc2 dependency) and unconditional. The cache
//! stores one JPEG per photo in that photo's own `<dir>/.lightphotos/`, the
//! directory `catalog.rs` already keeps ratings and edits in.
//!
//! ## Pipeline position
//! - `loader.rs`'s `Job::Speed`/`Job::Preview` (Pipeline 1, opening a photo)
//!   call `decode_at_size` directly — `UseIfPresent` for the cheap first
//!   pass, `Never` for the forced screen-fit decode once that pass comes
//!   back short.
//! - `loader.rs`'s `Job::Thumb` (Pipeline 2, Grid/filmstrip) calls
//!   `ThumbCache::get_or_make`, which checks the on-disk cache before falling
//!   back to `decode_at_size(.., UseIfPresent)`.
//! - wasm32 doesn't reach `ThumbCache` itself — it has no `std::fs` — but it
//!   caches to the same directory, under the same filenames, through
//!   `web/web_thumb_cache.rs` and the naming helpers below, which it shares.
//!   A folder cached by either build is readable by the other.
//!   See `ARCHITECTURE.md`.

// TODO: remove once wired into loader (T3)
#![allow(dead_code)]

#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
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
/// IFD1 thumbnail tags, an out-of-bounds offset/length, a blob that
/// does not decode as JPEG, or no preview meeting the minimum resolution
/// (including rawler's larger camera preview) — so `decode_at_size`'s full-decode
/// fallback is always safe to take; this must never be what makes a thumbnail
/// request fail outright.
#[cfg(not(target_os = "macos"))]
fn try_extract_embedded_preview(path: &Path, max_px: u32) -> Option<DecodedImage> {
    let bytes = fs::read(path).ok()?;
    embedded_preview_from_bytes(&bytes, max_px)
        .filter(|img| preview_is_large_enough(img.width, img.height, max_px))
        .or_else(|| {
            rawler_full_image_from_bytes(&bytes, max_px)
                .filter(|img| preview_is_large_enough(img.width, img.height, max_px))
        })
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

/// Longest-side pixel target every cached thumbnail is generated at.
///
/// One fixed size, not a per-request one: the cache lives in the user's photo
/// folder, so a slider-driven target would write a separate entry for every
/// size the user ever dragged through. 512 is the largest the grid ever draws
/// and stays sharp on a HiDPI display, where the cell is 192 points.
pub const THUMB_PX: u32 = 512;

/// Embedded previews must reach at least half the requested longest side.
/// Smaller previews fall back to source decoding; genuinely small originals
/// can still be cached at their native resolution.
pub(crate) fn preview_is_large_enough(width: u32, height: u32, max_px: u32) -> bool {
    width > 0 && height > 0 && width.max(height) >= max_px.div_ceil(2)
}

/// Filename suffix shared by every cache entry, after the
/// `<photo filename>.<16 hex key>` prefix. Deliberately not `.xmp`, so
/// `catalog.rs`'s sidecar scans (which filter on that extension) skip these.
const CACHE_SUFFIX: &str = ".thumb.jpg";

/// FNV-1a-64 of the cache version and source's mtime and byte length — the half of a cache
/// entry's identity that isn't already carried by its filename. Hand-rolled
/// (not `DefaultHasher`) so the value is stable across process runs.
///
/// The path is deliberately *not* hashed: an entry lives in its photo's own
/// `.lightphotos/` directory and is named after it, so moving or renaming the
/// folder keeps the cache valid instead of orphaning all of it.
///
/// **Milliseconds, not nanoseconds**, even though every filesystem this runs
/// on stores finer than that (APFS and ext4 both keep nanoseconds). The
/// browser only ever exposes `File.lastModified`, which is milliseconds, so
/// hashing native's full precision would make the two builds compute
/// different keys for the same untouched file — each would miss the other's
/// entries and rewrite them, quietly costing exactly the interop this cache
/// exists in the photo folder to get. Resolution lost here doesn't weaken
/// invalidation in practice: the byte length is hashed alongside, and a file
/// rewritten within the same millisecond at an identical size is not a case
/// worth chasing.
pub(crate) fn cache_key(mtime_ms: u64, len: u64) -> u64 {
    let mut h = crate::hash::Fnv1a::new();
    // Invalidate older entries that discarded transparency or accepted undersized
    // embedded previews. Native and web must miss the same obsolete entries.
    h.write(b"lightphotos-thumb-v3");
    h.write(&mtime_ms.to_le_bytes());
    h.write(&len.to_le_bytes());
    h.finish()
}

/// `<photo filename>.<key:016x>.thumb.jpg` — the cache entry name for `photo`.
///
/// Built by `OsString::push` rather than formatting through `to_string_lossy`
/// so a non-UTF-8 filename round-trips exactly, same as `catalog.rs`'s
/// `sidecar_path`.
pub(crate) fn cache_name(photo: &OsStr, key: u64) -> OsString {
    let mut name = photo.to_os_string();
    name.push(format!(".{key:016x}{CACHE_SUFFIX}"));
    name
}

/// The inverse of [`cache_name`]: split a cache entry's filename back into the
/// photo it belongs to and the key it was written under. `None` for anything
/// that isn't a cache entry, which is how the sweep below leaves `.xmp`
/// sidecars (and anything else a user dropped in there) alone.
///
/// Requires a UTF-8 name, unlike `cache_name`. A non-UTF-8 photo filename
/// still gets a working cache entry — lookup joins the exact `OsString` — it
/// just isn't reachable by the orphan sweep.
pub(crate) fn parse_cache_name(name: &OsStr) -> Option<(OsString, u64)> {
    let rest = name.to_str()?.strip_suffix(CACHE_SUFFIX)?;
    // The key is the final dot-separated field; `rsplit_once` keeps the rest
    // intact, so a photo named `PHOTO1.ARW` (a dot of its own) survives.
    let (photo, hex) = rest.rsplit_once('.')?;
    if photo.is_empty() || hex.len() != 16 {
        return None;
    }
    let key = u64::from_str_radix(hex, 16).ok()?;
    Some((OsString::from(photo), key))
}

/// The cache entry path for `photo`, or `None` when the photo has no parent
/// directory, no filename, or can't be stat'd (deleted between listing and
/// decode). Reads the source's metadata, so it is the one place that decides
/// whether an on-disk entry is current.
#[cfg(not(target_arch = "wasm32"))]
fn entry_path(photo: &Path) -> Option<PathBuf> {
    let dir = photo.parent()?;
    let name = photo.file_name()?;
    Some(
        dir.join(crate::catalog::SIDECAR_DIR)
            .join(cache_name(name, current_key(photo)?)),
    )
}

/// The key `photo`'s current bytes hash to, or `None` if it can't be stat'd.
#[cfg(not(target_arch = "wasm32"))]
fn current_key(photo: &Path) -> Option<u64> {
    let meta = fs::metadata(photo).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(cache_key(mtime_ms, meta.len()))
}

/// On-disk thumbnail cache, stored as JPEGs in each photo's own
/// `<photo dir>/.lightphotos/` — the same directory `catalog.rs` already keeps
/// ratings and develop edits in.
///
/// Holds no state: an entry's location is derived from the photo's own path,
/// so the cache follows the photos when a folder is moved, copied, or opened
/// from a different machine. That is also what lets the browser build share
/// it, since File System Access has no path outside the picked folder to
/// write to (`web/web_thumb_cache.rs` is the wasm32 half).
///
/// A folder that can't be written (read-only volume, locked card) simply
/// decodes every session: writes are best-effort and their failure is never
/// surfaced.
#[cfg(not(target_arch = "wasm32"))]
pub struct ThumbCache;

#[cfg(not(target_arch = "wasm32"))]
impl ThumbCache {
    /// Reclaim the pre-`.lightphotos` central cache, then hand back the
    /// (stateless) cache handle.
    pub fn new() -> ThumbCache {
        remove_legacy_cache();
        ThumbCache
    }

    /// Return `path`'s cached thumbnail if one is on disk and current;
    /// otherwise decode it, persist it, and return it.
    pub fn get_or_make(&self, path: &Path) -> Result<Arc<DecodedImage>, String> {
        let entry = entry_path(path);

        // A hit decodes a ~45 KB JPEG instead of a multi-megabyte RAW. A
        // corrupt or half-written entry just fails here and falls through to
        // the real decode below, which overwrites it.
        if let Some(file) = &entry {
            if let Ok(img) = decode_at_size(file, THUMB_PX, EmbeddedPreview::UseIfPresent) {
                return Ok(Arc::new(img));
            }
        }

        let mut img = decode_at_size(path, THUMB_PX, EmbeddedPreview::UseIfPresent)?;
        if !preview_is_large_enough(img.width, img.height, THUMB_PX) {
            img = decode_at_size(path, THUMB_PX, EmbeddedPreview::Never)?;
        }
        // Best-effort write; a failed cache write must not fail the request.
        if let Some(file) = &entry {
            let _ = write_entry(file, &img);
        }
        Ok(Arc::new(img))
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for ThumbCache {
    fn default() -> Self {
        Self::new()
    }
}

/// wasm32 stand-in. `loader.rs` builds a `ThumbCache` for its decode workers
/// on every target, but those workers never run in a browser — the Web Worker
/// pool decodes there instead (`web/web_worker_pool.rs`), and caches through
/// `web/web_thumb_cache.rs`, which reaches `.lightphotos/` over File System
/// Access rather than `std::fs`. Keeping the type present here is what lets
/// `loader.rs` stay platform-agnostic.
#[cfg(target_arch = "wasm32")]
pub struct ThumbCache;

#[cfg(target_arch = "wasm32")]
impl ThumbCache {
    pub fn new() -> ThumbCache {
        ThumbCache
    }

    pub fn get_or_make(&self, path: &Path) -> Result<std::sync::Arc<DecodedImage>, String> {
        Err(format!(
            "no std::fs on wasm32; {} is decoded by the Worker pool",
            path.display()
        ))
    }
}

#[cfg(target_arch = "wasm32")]
impl Default for ThumbCache {
    fn default() -> Self {
        Self::new()
    }
}

/// JPEG cannot preserve transparency or linear floating-point pixels.
pub(crate) fn jpeg_cacheable(img: &DecodedImage) -> bool {
    img.pixel_format == PixelFormat::Srgb8 && img.rgba.chunks_exact(4).all(|pixel| pixel[3] == 255)
}

/// Encode `img` as a JPEG at `file`, creating `.lightphotos/` if this is the
/// directory's first entry. Atomic: writes a `.tmp` sibling and renames, the
/// same shape `catalog.rs`'s `write_sidecar_file` uses.
///
/// Only opaque sRGB8 is written. The RAW tiers can hand back `LinearF16`, which a
/// JPEG can't represent — caching that would silently store wrong pixels, so
/// those photos decode every session instead.
#[cfg(not(target_arch = "wasm32"))]
fn write_entry(file: &Path, img: &DecodedImage) -> Result<(), String> {
    if !jpeg_cacheable(img) {
        return Err("only opaque sRGB8 images are cacheable as JPEG".into());
    }
    let dir = file.parent().ok_or("cache entry has no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // Push rather than `with_extension`, so the temp name keeps the full
    // entry name and the sweep below can recognise an interrupted write.
    let mut tmp = file.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    crate::image_encode::encode_jpeg(&tmp, img.width, img.height, &img.rgba)?;
    if let Err(e) = fs::rename(&tmp, file) {
        // Don't leave the orphaned temp file behind on failure.
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

/// Delete every cache entry in `dir/.lightphotos/` whose photo is gone or has
/// changed since the entry was written. Called when a folder opens.
/// Temporary files are left alone: another worker or app may be writing them.
///
/// This is the whole eviction story — there is no byte budget. One entry per
/// photo means a folder's cache is bounded by its own photo count, and an
/// entry that stops matching its photo is deleted rather than aged out.
/// Best-effort throughout: a failed delete just leaves the file.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sweep_orphans(dir: &Path) {
    let cache_dir = dir.join(crate::catalog::SIDECAR_DIR);
    let Ok(entries) = fs::read_dir(&cache_dir) else {
        return; // no .lightphotos yet — nothing was ever cached here
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some((photo, key)) = parse_cache_name(OsStr::new(name)) else {
            continue;
        };
        if current_key(&dir.join(photo)) != Some(key) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// One-time reclaim of the central caches this cache replaced
/// (`~/Library/Caches/com.lightphotos/thumbnails` and its pre-rename
/// `com.imageviewer` predecessor). Runs off the main path — deleting a cache
/// that grew to its old 512 MiB budget is thousands of unlinks, and startup
/// must not block on it.
///
/// A no-op after the first launch, and on wasm32, where `$HOME` is unset.
#[cfg(not(target_arch = "wasm32"))]
fn remove_legacy_cache() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let base = PathBuf::from(home).join("Library/Caches");
    // Builder::spawn (Result-returning), not the bare free `thread::spawn`
    // (which panics on failure): not every target has real threads, and
    // startup must degrade rather than crash.
    let _ = std::thread::Builder::new()
        .name("thumb-cache-reclaim".into())
        .spawn(move || {
            for legacy in ["com.lightphotos/thumbnails", "com.imageviewer/thumbnails"] {
                let _ = fs::remove_dir_all(base.join(legacy));
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_key_invalidates_legacy_opaque_thumbnails() {
        let mut legacy = crate::hash::Fnv1a::new();
        legacy.write(&1_000u64.to_le_bytes());
        legacy.write(&4_096u64.to_le_bytes());
        assert_ne!(cache_key(1_000, 4_096), legacy.finish());
    }

    #[test]
    fn cache_key_invalidates_undersized_previews() {
        let mut legacy = crate::hash::Fnv1a::new();
        legacy.write(b"lightphotos-thumb-v2");
        legacy.write(&1_000u64.to_le_bytes());
        legacy.write(&4_096u64.to_le_bytes());
        assert_ne!(cache_key(1_000, 4_096), legacy.finish());
    }

    #[test]
    fn preview_resolution_policy() {
        assert!(!preview_is_large_enough(160, 120, THUMB_PX));
        assert!(!preview_is_large_enough(255, 192, THUMB_PX));
        assert!(preview_is_large_enough(256, 192, THUMB_PX));
        assert!(preview_is_large_enough(192, 256, THUMB_PX));
        assert!(!preview_is_large_enough(256, 192, 513));
        assert!(!preview_is_large_enough(512, 0, THUMB_PX));
    }

    #[test]
    fn cache_name_round_trips_through_parse() {
        let photo = OsStr::new("IMG_0001.ARW");
        let name = cache_name(photo, 0xa3f1_c07b_91e4_d2f8);
        assert_eq!(
            name.to_str().unwrap(),
            "IMG_0001.ARW.a3f1c07b91e4d2f8.thumb.jpg"
        );

        let (back, key) = parse_cache_name(&name).expect("should parse");
        assert_eq!(back, photo);
        assert_eq!(key, 0xa3f1_c07b_91e4_d2f8);
    }

    #[test]
    fn parse_rejects_non_cache_names() {
        // A sidecar, which shares the directory and must be left alone.
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.xmp")).is_none());
        // Right suffix, but no key field at all.
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.thumb.jpg")).is_none());
        // Right suffix, key isn't 16 hex digits.
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.abc.thumb.jpg")).is_none());
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.zzzzzzzzzzzzzzzz.thumb.jpg")).is_none());
        // No photo name left once the key is stripped.
        assert!(parse_cache_name(OsStr::new(".a3f1c07b91e4d2f8.thumb.jpg")).is_none());
    }

    #[test]
    fn key_changes_with_mtime_or_length() {
        let base = cache_key(1_000, 4_096);
        assert_eq!(base, cache_key(1_000, 4_096), "same inputs, same key");
        assert_ne!(base, cache_key(1_001, 4_096), "a touched file must miss");
        // The browser hands us whole milliseconds; native must agree with it
        // exactly, or neither build ever reads the other's entries.
        assert_eq!(
            base,
            cache_key(
                std::time::Duration::from_nanos(1_000_999_999).as_millis() as u64,
                4_096
            ),
            "sub-millisecond mtime precision must not reach the key"
        );
        assert_ne!(base, cache_key(1_000, 4_097), "a resized file must miss");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn transparency_survives_reopening_and_opaque_images_hit_cache() {
        let dir = std::env::temp_dir().join(format!("lp-alpha-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        const TRANSPARENT: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 168, 8,
            208, 96, 0, 0, 3, 37, 0, 241, 104, 150, 229, 28, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
            96, 130,
        ];
        const OPAQUE: &[u8] = &[
            137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1,
            8, 6, 0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 13, 73, 68, 65, 84, 120, 156, 99, 168, 8,
            208, 248, 15, 0, 4, 36, 1, 240, 183, 238, 60, 203, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66,
            96, 130,
        ];
        let cache = ThumbCache;
        for (name, png, opaque) in [
            ("transparent.png", TRANSPARENT, false),
            ("opaque.png", OPAQUE, true),
        ] {
            let photo = dir.join(name);
            fs::write(&photo, png).unwrap();
            let entry = entry_path(&photo).unwrap();
            let first = cache.get_or_make(&photo).unwrap();
            assert_eq!(jpeg_cacheable(&first), opaque);
            assert_eq!(entry.exists(), opaque);
            // A second request hits JPEG for opaque images and re-decodes
            // transparent sources without losing alpha.
            let second = cache.get_or_make(&photo).unwrap();
            assert_eq!(jpeg_cacheable(&second), opaque);
            assert_eq!(first.rgba[3], second.rgba[3]);
            assert_eq!(entry.exists(), opaque);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// A photo, its current cache entry, a stale entry from before it was
    /// edited, an entry for a photo that's been deleted, an interrupted
    /// write's `.tmp`, and a sidecar that must survive all of it.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn sweep_keeps_current_entries_and_sidecars() {
        let dir = std::env::temp_dir().join(format!("lp-sweep-test-{}", std::process::id()));
        let cache = dir.join(crate::catalog::SIDECAR_DIR);
        fs::create_dir_all(&cache).unwrap();

        let photo = dir.join("IMG_0001.ARW");
        fs::write(&photo, b"pretend raw bytes").unwrap();
        let key = current_key(&photo).unwrap();

        let current = cache.join(cache_name(OsStr::new("IMG_0001.ARW"), key));
        let stale = cache.join(cache_name(OsStr::new("IMG_0001.ARW"), key ^ 1));
        let orphan = cache.join(cache_name(OsStr::new("GONE.JPG"), key));
        let interrupted = cache.join("IMG_0001.ARW.0123456789abcdef.thumb.jpg.tmp");
        let sidecar = cache.join("IMG_0001.ARW.xmp");
        for f in [&current, &stale, &orphan, &interrupted, &sidecar] {
            fs::write(f, b"x").unwrap();
        }

        let malformed = [
            "reference.thumb.jpg",
            "reference.thumb.jpg.tmp",
            "IMG_0001.ARW.abc.thumb.jpg",
            "IMG_0001.ARW.zzzzzzzzzzzzzzzz.thumb.jpg.tmp",
            ".0123456789abcdef.thumb.jpg",
        ];
        for name in malformed {
            fs::write(cache.join(name), b"unrelated user file").unwrap();
        }

        sweep_orphans(&dir);
        for name in malformed {
            assert!(cache.join(name).exists(), "{name} must survive");
        }

        assert!(current.exists(), "an entry matching its photo must survive");
        assert!(sidecar.exists(), "sidecars are not ours to delete");
        assert!(!stale.exists(), "an entry from before an edit must go");
        assert!(!orphan.exists(), "an entry whose photo is gone must go");
        assert!(
            interrupted.exists(),
            "a potentially active temp file must survive"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
