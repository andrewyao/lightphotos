//! Thumbnail generation via Apple's ImageIO (decode-at-size, uses embedded
//! previews, applies EXIF orientation) plus a small on-disk thumbnail cache.
//!
//! Mirrors `image_decode.rs`: open a `CGImageSource` from the file, ask ImageIO
//! for a thumbnail `CGImage`, then reuse `image_decode::cgimage_to_rgba` to read
//! it back as tightly-packed RGBA8.

// TODO: remove once wired into loader (T3)
#![allow(dead_code)]

use std::ffi::c_void;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use objc2_core_foundation::{
    kCFBooleanTrue, kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary,
    CFNumber, CFNumberType, CFRetained, CFString, CFURL, CFURLPathStyle,
};
use objc2_image_io::{
    kCGImageSourceCreateThumbnailFromImageIfAbsent, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize, CGImageSource,
};

use crate::image_decode::{cgimage_to_rgba, DecodedImage};

/// Decode a thumbnail of `path` whose longest side is at most `max_px` pixels.
///
/// Uses `CGImageSourceCreateThumbnailAtIndex`, which prefers an embedded preview
/// when present, falls back to decoding-at-size from the full image, and applies
/// the file's EXIF orientation.
pub fn thumbnail(path: &Path, max_px: u32) -> Result<DecodedImage, String> {
    let path_str = path.to_string_lossy();
    let cf_path = CFString::from_str(&path_str);
    let url = CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or("could not build CFURL")?;

    // SAFETY: url is a valid CFURL; no decode options for opening the source.
    let source = unsafe { CGImageSource::with_url(&url, None) }
        .ok_or("ImageIO could not open file")?;

    let options = build_thumbnail_options(max_px)?;

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

/// Build the options `CFDictionary` for `CGImageSourceCreateThumbnailAtIndex`.
fn build_thumbnail_options(max_px: u32) -> Result<CFRetained<CFDictionary>, String> {
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
            kCGImageSourceCreateThumbnailFromImageIfAbsent as *const CFString as *const c_void,
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
/// `~/Library/Caches/com.imageviewer/thumbnails/`.
///
/// Cache files are named `<hex fnv1a-64 key>.tw`. The key hashes the
/// canonicalized source path, its mtime (ns), its length, and `max_px`, so any
/// change to the source file or requested size yields a fresh entry.
pub struct ThumbCache {
    root: PathBuf,
}

impl ThumbCache {
    /// Create the cache, ensuring the root directory exists. If `$HOME` is
    /// unavailable, falls back to a relative `./.imageviewer-thumbnails`.
    pub fn new() -> ThumbCache {
        let root = match std::env::var("HOME") {
            Ok(home) => PathBuf::from(home)
                .join("Library/Caches/com.imageviewer/thumbnails"),
            Err(_) => PathBuf::from(".imageviewer-thumbnails"),
        };
        // Best-effort: errors here surface later on read/write.
        let _ = fs::create_dir_all(&root);
        ThumbCache { root }
    }

    /// Return a cached thumbnail for `(path, max_px)` if present on disk;
    /// otherwise generate it via `thumbnail()`, persist it, and return it.
    pub fn get_or_make(&self, path: &Path, max_px: u32) -> Result<Arc<DecodedImage>, String> {
        let key = self.cache_key(path, max_px)?;
        let file = self.root.join(format!("{:016x}.tw", key));

        if let Ok(img) = read_tw(&file) {
            return Ok(Arc::new(img));
        }

        let img = thumbnail(path, max_px)?;
        // Best-effort write; a failed cache write must not fail the request.
        let _ = write_tw(&file, &img);
        Ok(Arc::new(img))
    }

    /// FNV-1a 64-bit hash of (canonical path bytes, mtime_ns, len, max_px).
    /// Hand-rolled so the key is stable across process runs (unlike
    /// `std::collections::hash_map::DefaultHasher`).
    fn cache_key(&self, path: &Path, max_px: u32) -> Result<u64, String> {
        let canon = fs::canonicalize(path).map_err(|e| format!("canonicalize: {e}"))?;
        let meta = fs::metadata(&canon).map_err(|e| format!("metadata: {e}"))?;
        let mtime_ns = meta
            .modified()
            .map_err(|e| format!("mtime: {e}"))?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        let len = meta.len();

        let mut h = Fnv1a::new();
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

/// Minimal FNV-1a 64-bit hasher. Stable, deterministic, dependency-free.
struct Fnv1a {
    state: u64,
}

impl Fnv1a {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;

    fn new() -> Self {
        Fnv1a { state: Self::OFFSET_BASIS }
    }

    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state ^= b as u64;
            self.state = self.state.wrapping_mul(Self::PRIME);
        }
    }

    fn finish(&self) -> u64 {
        self.state
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
    Ok(DecodedImage { width, height, rgba })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_key_is_stable_and_deterministic() {
        let mut a = Fnv1a::new();
        a.write(b"/abs/IMG_001.jpg");
        a.write(&123u64.to_le_bytes());
        a.write(&456u64.to_le_bytes());
        a.write(&256u32.to_le_bytes());

        let mut b = Fnv1a::new();
        b.write(b"/abs/IMG_001.jpg");
        b.write(&123u64.to_le_bytes());
        b.write(&456u64.to_le_bytes());
        b.write(&256u32.to_le_bytes());

        assert_eq!(a.finish(), b.finish(), "identical inputs -> identical key");

        // Known FNV-1a-64 anchor: empty input hashes to the offset basis.
        assert_eq!(Fnv1a::new().finish(), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a_key_differs_when_max_px_differs() {
        let mk = |max_px: u32| {
            let mut h = Fnv1a::new();
            h.write(b"/abs/IMG_001.jpg");
            h.write(&123u64.to_le_bytes());
            h.write(&456u64.to_le_bytes());
            h.write(&max_px.to_le_bytes());
            h.finish()
        };
        assert_ne!(mk(256), mk(512), "different max_px -> different key");
    }

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
}
