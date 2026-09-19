// SPDX-License-Identifier: GPL-3.0-or-later

//! Reduced-size decodes and the on-disk thumbnail cache. macOS asks ImageIO
//! for a thumbnail. Other targets try the file's embedded preview, then fall
//! back to a full decode and resize. The cache stores one JPEG per photo in
//! `<dir>/.lightphotos/`, beside the catalog sidecars. The wasm32 build reads
//! and writes the same files through `web/web_thumb_cache.rs`.

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
    /// Use the embedded preview when there is one. Fast, and detailed enough
    /// for small grid thumbnails.
    UseIfPresent,
    /// Always decode the full image. For the Loupe's screen-fit preview,
    /// where a ~1600px embedded preview looks soft.
    Never,
}

/// Decode `path` with its longest side at most `max_px`. ImageIO scales
/// during decode, which is much cheaper than `image_decode::decode`'s full
/// decode followed by a downscale.
#[cfg(target_os = "macos")]
pub fn decode_at_size(
    path: &Path,
    max_px: u32,
    embedded: EmbeddedPreview,
) -> Result<DecodedImage, String> {
    let source = crate::image_decode::open_image_source(path)?;

    let options = build_thumbnail_options(max_px, embedded)?;

    // SAFETY: `options` holds documented thumbnail keys with CFBoolean and
    // CFNumber values of the right types.
    let image: CFRetained<_> = unsafe { source.thumbnail_at_index(0, Some(&options)) }
        .ok_or("ImageIO could not create thumbnail")?;

    let w = objc2_core_graphics::CGImage::width(Some(&image)) as u32;
    let h = objc2_core_graphics::CGImage::height(Some(&image)) as u32;
    if w == 0 || h == 0 {
        return Err("thumbnail has zero dimension".into());
    }

    cgimage_to_rgba(&image, w, h)
}

/// Decode `path` with its longest side at most `max_px`. Without ImageIO
/// there is no scaled decode, so this is a full decode plus resize, unless
/// `UseIfPresent` finds an embedded preview first.
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

/// The file's embedded preview, fit within `max_px` and never upscaled.
/// `None` on any failure, so the caller falls back to a full decode.
///
/// Reads the EXIF IFD1 thumbnail, which is often only 160x120. Larger
/// maker-specific previews are not parsed, except CR3 and RAF through rawler.
/// There is no minimum size here: the Loupe's `Job::Speed` tier wants any
/// preview fast and escalates later. The cache applies its own minimum.
#[cfg(not(target_os = "macos"))]
fn try_extract_embedded_preview(path: &Path, max_px: u32) -> Option<DecodedImage> {
    let bytes = fs::read(path).ok()?;
    embedded_preview_from_bytes(&bytes, max_px)
        .or_else(|| rawler_full_image_from_bytes(&bytes, max_px))
}

/// The EXIF IFD1 thumbnail from file bytes. wasm32 calls this directly
/// because it has no file path.
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

    // Orientation belongs to the main image, so read it from the primary IFD.
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

/// The embedded preview of a CR3 or RAF file, via rawler's
/// `Decoder::full_image()`. `kamadak-exif` cannot open those containers.
///
/// Only CR3 and RAF. rawler also returns a preview for TIFF-based RAWs, but
/// that camera JPEG has the camera's own tone and color and looks very
/// different from our RAW develop, so it must not stand in for it.
/// `catch_unwind` guards against panics inside rawler.
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

        // Take orientation from the RAW metadata, as `decode_raw_nonmac`
        // does. The embedded image's own EXIF may lack it.
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

/// Options for `CGImageSourceCreateThumbnailAtIndex`. `WithTransform` makes
/// ImageIO apply EXIF orientation.
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

    // SAFETY: keys and values hold 3 valid CFType pointers each. The CFType
    // callbacks retain entries, so `max_px_num` may drop afterwards.
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

/// Longest side of every cached thumbnail. One fixed size, so the cache
/// holds one entry per photo. 512 covers the largest grid cell on a HiDPI
/// display.
pub const THUMB_PX: u32 = 512;

/// Whether a preview reaches at least half of `max_px` on its longest side.
pub(crate) fn preview_is_large_enough(width: u32, height: u32, max_px: u32) -> bool {
    width > 0 && height > 0 && width.max(height) >= max_px.div_ceil(2)
}

/// Decode `path` for the cache. Entries last as long as the photo, so a tiny
/// embedded preview (under [`preview_is_large_enough`]) is skipped for a
/// source decode. Small originals are cached at their native size.
#[cfg(all(not(target_arch = "wasm32"), not(target_os = "macos")))]
fn decode_for_cache(path: &Path) -> Result<DecodedImage, String> {
    // Try the preview directly instead of `decode_at_size(UseIfPresent)`, so
    // a small original is never decoded twice.
    if let Some(img) = try_extract_embedded_preview(path, THUMB_PX)
        .filter(|img| preview_is_large_enough(img.width, img.height, THUMB_PX))
    {
        return Ok(img);
    }
    crate::image_decode::decode(path, THUMB_PX)
}

/// macOS version. ImageIO does not report whether it used the embedded
/// preview, so a short result is retried with `Never`. The retry is cheap
/// when the source itself is small.
#[cfg(all(not(target_arch = "wasm32"), target_os = "macos"))]
fn decode_for_cache(path: &Path) -> Result<DecodedImage, String> {
    let img = decode_at_size(path, THUMB_PX, EmbeddedPreview::UseIfPresent)?;
    if preview_is_large_enough(img.width, img.height, THUMB_PX) {
        return Ok(img);
    }
    decode_at_size(path, THUMB_PX, EmbeddedPreview::Never)
}

/// Suffix of every cache entry name. Not `.xmp`, so the catalog's sidecar
/// scans skip these files.
const CACHE_SUFFIX: &str = ".thumb.jpg";

/// Cache key from the source's mtime and length. FNV-1a instead of
/// `DefaultHasher`, so keys are stable across runs. The path is not hashed,
/// so moving a folder keeps its cache valid.
///
/// The mtime is in milliseconds because the browser's `File.lastModified` is.
/// Native and web builds must compute the same key to share entries.
pub(crate) fn cache_key(mtime_ms: u64, len: u64) -> u64 {
    let mut h = crate::hash::Fnv1a::new();
    // Bump the version to invalidate every existing entry on native and web.
    h.write(b"lightphotos-thumb-v3");
    h.write(&mtime_ms.to_le_bytes());
    h.write(&len.to_le_bytes());
    h.finish()
}

/// Cache entry name `<photo filename>.<key:016x>.thumb.jpg`. Built with
/// `OsString::push` so non-UTF-8 names round-trip exactly.
pub(crate) fn cache_name(photo: &OsStr, key: u64) -> OsString {
    let mut name = photo.to_os_string();
    name.push(format!(".{key:016x}{CACHE_SUFFIX}"));
    name
}

/// Split a cache entry name into photo name and key. `None` for anything
/// else, so the sweep leaves sidecars and user files alone. Needs UTF-8, so
/// the sweep never removes entries for non-UTF-8 photo names.
pub(crate) fn parse_cache_name(name: &OsStr) -> Option<(OsString, u64)> {
    let rest = name.to_str()?.strip_suffix(CACHE_SUFFIX)?;
    // The key is the last field. The photo name may contain dots.
    let (photo, hex) = rest.rsplit_once('.')?;
    if photo.is_empty() || hex.len() != 16 {
        return None;
    }
    let key = u64::from_str_radix(hex, 16).ok()?;
    Some((OsString::from(photo), key))
}

/// Current cache entry path for `photo`, or `None` if it cannot be stat'd.
#[cfg(not(target_arch = "wasm32"))]
fn entry_path(photo: &Path) -> Option<PathBuf> {
    let dir = photo.parent()?;
    let name = photo.file_name()?;
    Some(
        dir.join(crate::catalog::SIDECAR_DIR)
            .join(cache_name(name, current_key(photo)?)),
    )
}

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

/// On-disk thumbnail cache in `<photo dir>/.lightphotos/`. It lives beside
/// the photos because the browser can write only inside the picked folder.
/// Writes are best-effort, so a read-only folder decodes every session.
#[cfg(not(target_arch = "wasm32"))]
pub struct ThumbCache;

#[cfg(not(target_arch = "wasm32"))]
impl ThumbCache {
    /// Also deletes the old central cache in the background.
    pub fn new() -> ThumbCache {
        remove_legacy_cache();
        ThumbCache
    }

    /// The cached thumbnail if current, else decode, cache, and return it.
    pub fn get_or_make(&self, path: &Path) -> Result<Arc<DecodedImage>, String> {
        let entry = entry_path(path);

        // A corrupt entry fails to decode and is overwritten below.
        if let Some(file) = &entry {
            if let Ok(img) = decode_at_size(file, THUMB_PX, EmbeddedPreview::UseIfPresent) {
                return Ok(Arc::new(img));
            }
        }

        let img = decode_for_cache(path)?;
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

/// wasm32 stub so `loader.rs` compiles unchanged. In the browser the Web
/// Worker pool decodes, and `web/web_thumb_cache.rs` does the caching.
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

/// Write `img` to `file` as a JPEG via a `.tmp` sibling and rename. Refuses
/// anything [`jpeg_cacheable`] rejects, so those photos decode every session.
#[cfg(not(target_arch = "wasm32"))]
fn write_entry(file: &Path, img: &DecodedImage) -> Result<(), String> {
    if !jpeg_cacheable(img) {
        return Err("only opaque sRGB8 images are cacheable as JPEG".into());
    }
    let dir = file.parent().ok_or("cache entry has no parent")?;
    fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;

    // Append `.tmp` to the full name so `sweep_orphans` can parse it.
    let mut tmp = file.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);

    crate::image_encode::encode_jpeg(&tmp, img.width, img.height, &img.rgba)?;
    if let Err(e) = fs::rename(&tmp, file) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

/// Age after which a `.tmp` entry is abandoned. A live write takes seconds.
#[cfg(not(target_arch = "wasm32"))]
const TMP_REAP_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// Delete cache entries whose photo is gone or changed, and abandoned `.tmp`
/// files. Runs when a folder opens. This is the only eviction; there is no
/// size budget, since there is one entry per photo.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn sweep_orphans(dir: &Path) {
    let cache_dir = dir.join(crate::catalog::SIDECAR_DIR);
    let Ok(entries) = fs::read_dir(&cache_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if let Some(stem) = name.strip_suffix(".tmp") {
            // The age check avoids deleting a temp file a worker is writing.
            if parse_cache_name(OsStr::new(stem)).is_some() && is_older_than(&entry, TMP_REAP_AFTER)
            {
                let _ = fs::remove_file(entry.path());
            }
            continue;
        }
        let Some((photo, key)) = parse_cache_name(OsStr::new(name)) else {
            continue;
        };
        if current_key(&dir.join(photo)) != Some(key) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

/// `false` when the mtime is unreadable or in the future, so unknown files
/// are kept.
#[cfg(not(target_arch = "wasm32"))]
fn is_older_than(entry: &fs::DirEntry, age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .is_some_and(|elapsed| elapsed > age)
}

/// Delete the old central caches under `~/Library/Caches` on a background
/// thread. They can hold thousands of files, and startup must not wait.
#[cfg(not(target_arch = "wasm32"))]
fn remove_legacy_cache() {
    let Ok(home) = std::env::var("HOME") else {
        return;
    };
    let base = PathBuf::from(home).join("Library/Caches");
    // `Builder::spawn` returns an error instead of panicking.
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
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.xmp")).is_none());
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.thumb.jpg")).is_none());
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.abc.thumb.jpg")).is_none());
        assert!(parse_cache_name(OsStr::new("IMG_0001.ARW.zzzzzzzzzzzzzzzz.thumb.jpg")).is_none());
        assert!(parse_cache_name(OsStr::new(".a3f1c07b91e4d2f8.thumb.jpg")).is_none());
    }

    #[test]
    fn key_changes_with_mtime_or_length() {
        let base = cache_key(1_000, 4_096);
        assert_eq!(base, cache_key(1_000, 4_096), "same inputs, same key");
        assert_ne!(base, cache_key(1_001, 4_096), "a touched file must miss");
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
            // Opaque images hit the cache. Transparent ones re-decode and
            // keep alpha.
            let second = cache.get_or_make(&photo).unwrap();
            assert_eq!(jpeg_cacheable(&second), opaque);
            assert_eq!(first.rgba[3], second.rgba[3]);
            assert_eq!(entry.exists(), opaque);
        }
        fs::remove_dir_all(dir).unwrap();
    }

    /// Covers current, stale, and orphaned entries, a fresh and an abandoned
    /// `.tmp`, a sidecar, and unrelated files.
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
        let abandoned = cache.join("IMG_0002.ARW.fedcba9876543210.thumb.jpg.tmp");
        let sidecar = cache.join("IMG_0001.ARW.xmp");
        for f in [
            &current,
            &stale,
            &orphan,
            &interrupted,
            &abandoned,
            &sidecar,
        ] {
            fs::write(f, b"x").unwrap();
        }
        fs::File::options()
            .write(true)
            .open(&abandoned)
            .unwrap()
            .set_modified(std::time::SystemTime::now() - TMP_REAP_AFTER * 2)
            .unwrap();

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
            "a temp file a live writer may still hold must survive"
        );
        assert!(
            !abandoned.exists(),
            "a temp file older than any live write must go"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
