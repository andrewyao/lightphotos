// SPDX-License-Identifier: MIT OR Apache-2.0

//! Decode any image macOS understands (JPEG/PNG/GIF/TIFF/BMP/HEIC/RAW) to
//! RGBA8 bytes using Apple's ImageIO + CoreGraphics. No third-party codecs.
//!
//! Pipeline: CFURL -> CGImageSource -> CGImage -> draw into a CGBitmapContext
//! backed by our own buffer (sRGB, premultiplied RGBA, big-endian byte order),
//! then read the buffer back.

use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, SystemTime};

use objc2_core_foundation::{
    CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{CGContext, CGImage};
use objc2_image_io::{
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
    kCGImagePropertyOrientation, kCGImagePropertyTIFFDateTime, CGImageSource,
};

use crate::coregraphics;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, row-major, premultiplied alpha.
    pub rgba: Vec<u8>,
}

// CoreFoundation runtime type introspection, used to verify a value's concrete
// type before reinterpreting it. CoreFoundation is already linked transitively.
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(cf: *const c_void) -> core::ffi::c_ulong;
    fn CFNumberGetTypeID() -> core::ffi::c_ulong;
    fn CFStringGetTypeID() -> core::ffi::c_ulong;
    fn CFDictionaryGetTypeID() -> core::ffi::c_ulong;
}

/// Decode `path`, optionally downscaling so neither side exceeds `max_dim`
/// (so images larger than the GPU's max texture size still display).
/// Open `path` as a `CGImageSource` (the shared CFURL + ImageIO open path used
/// by both full-resolution decode and thumbnail generation).
pub fn open_image_source(path: &Path) -> Result<CFRetained<CGImageSource>, String> {
    let url = coregraphics::file_url(path)?;

    // SAFETY: url is a valid CFURL; passing no decode options. The returned
    // CGImageSource is +1 retained and wrapped in CFRetained, released on drop.
    unsafe { CGImageSource::with_url(&url, None) }.ok_or_else(|| "ImageIO could not open file".into())
}

pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let source = open_image_source(path)?;

    let image: CFRetained<CGImage> = unsafe { source.image_at_index(0, None) }
        .ok_or("ImageIO could not decode image")?;

    let src_w = CGImage::width(Some(&image)) as u32;
    let src_h = CGImage::height(Some(&image)) as u32;
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }

    // Downscale to fit max_dim while preserving aspect ratio.
    let (w, h) = fit_within(src_w, src_h, max_dim);

    let decoded = cgimage_to_rgba(&image, w, h)?;
    // `image_at_index` returns raw pixels; apply the file's EXIF orientation so
    // the full decode matches the thumbnails (which orient via ImageIO's
    // WithTransform). Loupe, crop, and export all consume `decode()`, so this
    // keeps every downstream view upright and consistent.
    Ok(apply_exif_orientation(decoded, read_orientation(&source)))
}

/// Parse an EXIF datetime string (`"YYYY:MM:DD HH:MM:SS"`) into a `SystemTime`,
/// interpreting it as UTC (EXIF carries no timezone; only *consistency* matters
/// for burst grouping, not absolute correctness). Returns `None` for empty,
/// zeroed, or malformed values.
fn parse_exif_datetime(s: &str) -> Option<SystemTime> {
    let (date, time) = s.trim().split_once(' ')?;
    let mut d = date.split(':');
    let y: i64 = d.next()?.trim().parse().ok()?;
    let mo: u32 = d.next()?.parse().ok()?;
    let da: u32 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let h: u64 = t.next()?.parse().ok()?;
    let mi: u64 = t.next()?.parse().ok()?;
    let se: u64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&mo) || !(1..=31).contains(&da) || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let secs = days_from_civil(y, mo, da) * 86_400 + (h * 3600 + mi * 60 + se) as i64;
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's
/// `days_from_civil`). Valid for any in-range `m` (1..=12), `d` (1..=31).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Capture time for `path` from its EXIF/TIFF metadata, falling back to the
/// file's modification time so grouping always has *something* to order by.
/// Never panics; returns `None` only when even the mtime is unavailable.
pub fn capture_time(path: &Path) -> Option<SystemTime> {
    let source = open_image_source(path).ok()?;
    read_capture_time(&source)
        .or_else(|| std::fs::metadata(path).ok().and_then(|m| m.modified().ok()))
}

/// Read the capture timestamp from an open source: EXIF `DateTimeOriginal`
/// first, then TIFF `DateTime`. `None` when neither is present/parseable.
fn read_capture_time(source: &CGImageSource) -> Option<SystemTime> {
    // SAFETY: index 0 exists; no options. Dictionary is +1 retained, freed on drop.
    let props = unsafe { source.properties_at_index(0, None) }?;

    // EXIF sub-dictionary → DateTimeOriginal (preferred).
    // SAFETY: reading extern static keys; `value` returns a borrowed pointer.
    let exif_ptr =
        unsafe { props.value(kCGImagePropertyExifDictionary as *const CFString as *const c_void) };
    if !exif_ptr.is_null() && unsafe { CFGetTypeID(exif_ptr) } == unsafe { CFDictionaryGetTypeID() }
    {
        // SAFETY: confirmed the value is a CFDictionary above.
        let exif = unsafe { &*(exif_ptr as *const CFDictionary) };
        if let Some(t) = dict_string(exif, unsafe { kCGImagePropertyExifDateTimeOriginal })
            .and_then(|s| parse_exif_datetime(&s))
        {
            return Some(t);
        }
    }

    // TIFF DateTime (top-level) fallback.
    dict_string(&props, unsafe { kCGImagePropertyTIFFDateTime }).and_then(|s| parse_exif_datetime(&s))
}

/// Read a CFString value from a CFDictionary for `key`, verifying the concrete
/// type before reinterpreting (a crafted file could store another CFType).
fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    // SAFETY: `key` is a valid CFString option key; `value` returns a borrowed
    // pointer to the stored value, or null when absent.
    let ptr = unsafe { dict.value(key as *const CFString as *const c_void) };
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFString.
    Some(unsafe { &*(ptr as *const CFString) }.to_string())
}

/// The image's EXIF orientation tag (`1..=8`), or `1` when absent/unreadable.
/// Never panics — any missing property yields the identity orientation.
fn read_orientation(source: &CGImageSource) -> u8 {
    // SAFETY: index 0 exists (we already decoded it); no options passed. The
    // returned dictionary is +1 retained and released on drop.
    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return 1;
    };
    // SAFETY: the orientation key is a valid CFString option key; `value` returns
    // a borrowed (non-owned) pointer to the CFNumber, or null if absent.
    let ptr = unsafe { props.value(kCGImagePropertyOrientation as *const CFString as *const c_void) };
    if ptr.is_null() {
        return 1;
    }
    // A well-formed file stores a CFNumber here, but a crafted/broken file could
    // store some other CFType. Verify the concrete type before reinterpreting —
    // casting an arbitrary CF object to CFNumber and calling `value` on it is UB.
    if unsafe { CFGetTypeID(ptr) } != unsafe { CFNumberGetTypeID() } {
        return 1;
    }
    // SAFETY: confirmed above that the value is a CFNumber; read it as SInt32.
    let number = unsafe { &*(ptr as *const CFNumber) };
    let mut out: i32 = 0;
    let ok = unsafe {
        number.value(CFNumberType::SInt32Type, &mut out as *mut i32 as *mut c_void)
    };
    if ok && (1..=8).contains(&out) {
        out as u8
    } else {
        1
    }
}

/// Reorient tightly-packed RGBA8 pixels per an EXIF orientation (`1..=8`),
/// returning the corrected image. Orientation `1` is returned untouched (fast
/// path). Cases `5..=8` swap width/height.
///
/// Mapping is `out(xo, yo) = in(xs, ys)`; see the EXIF orientation table. Shares
/// its shape with `app::rotate_rgba`, extended to cover the mirrored cases.
fn apply_exif_orientation(img: DecodedImage, orientation: u8) -> DecodedImage {
    if orientation <= 1 {
        return img;
    }
    let (w, h) = (img.width, img.height);
    let swaps = matches!(orientation, 5 | 6 | 7 | 8);
    let (nw, nh) = if swaps { (h, w) } else { (w, h) };
    let mut dst = vec![0u8; (nw * nh * 4) as usize];
    let src_idx = |x: u32, y: u32| ((y * w + x) * 4) as usize;
    for yo in 0..nh {
        for xo in 0..nw {
            let (xs, ys) = match orientation {
                2 => (w - 1 - xo, yo),         // mirror horizontal
                3 => (w - 1 - xo, h - 1 - yo), // rotate 180
                4 => (xo, h - 1 - yo),         // mirror vertical
                5 => (yo, xo),                 // transpose
                6 => (yo, h - 1 - xo),         // rotate 90° CW
                7 => (w - 1 - yo, h - 1 - xo), // transverse
                _ => (w - 1 - yo, xo),         // 8: rotate 270° CW
            };
            let s = src_idx(xs, ys);
            let d = ((yo * nw + xo) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&img.rgba[s..s + 4]);
        }
    }
    DecodedImage { width: nw, height: nh, rgba: dst }
}

/// Draw a `CGImage` into a freshly-allocated sRGB bitmap context sized
/// `(target_w, target_h)` and read back the result as tightly-packed,
/// premultiplied RGBA8 (byte order R,G,B,A — matches `Rgba8UnormSrgb`).
///
/// Scales the image into the target rect, so callers can use this both for a
/// full-size decode and for a thumbnail (passing the thumbnail's own size).
pub fn cgimage_to_rgba(
    image: &CGImage,
    target_w: u32,
    target_h: u32,
) -> Result<DecodedImage, String> {
    if target_w == 0 || target_h == 0 {
        return Err("target dimensions must be non-zero".into());
    }

    let bytes_per_row = (target_w as usize) * 4;
    let mut buffer = vec![0u8; bytes_per_row * (target_h as usize)];

    // SAFETY: buffer is large enough (target_w*target_h*4) and outlives `ctx`.
    let ctx = unsafe {
        coregraphics::srgb_bitmap_context(
            buffer.as_mut_ptr() as *mut c_void,
            target_w,
            target_h,
            bytes_per_row,
        )?
    };

    // Draw the image scaled into our (possibly smaller) context rect.
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize { width: target_w as f64, height: target_h as f64 },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    Ok(DecodedImage { width: target_w, height: target_h, rgba: buffer })
}

fn fit_within(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if w <= max_dim && h <= max_dim {
        return (w, h);
    }
    let scale = (max_dim as f64 / w as f64).min(max_dim as f64 / h as f64);
    let nw = ((w as f64 * scale).floor() as u32).max(1).min(max_dim);
    let nh = ((h as f64 * scale).floor() as u32).max(1).min(max_dim);
    (nw, nh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_exif_datetime_unix_epoch_anchor() {
        assert_eq!(
            parse_exif_datetime("1970:01:01 00:00:00"),
            Some(SystemTime::UNIX_EPOCH)
        );
        assert_eq!(
            parse_exif_datetime("1970:01:02 00:00:00"),
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400))
        );
    }

    #[test]
    fn parse_exif_datetime_relative_diffs() {
        let a = parse_exif_datetime("2026:07:15 08:30:00").unwrap();
        let b = parse_exif_datetime("2026:07:15 08:30:05").unwrap();
        assert_eq!(b.duration_since(a).unwrap(), Duration::from_secs(5));

        let d0 = parse_exif_datetime("2026:07:15 08:30:00").unwrap();
        let d1 = parse_exif_datetime("2026:07:16 08:30:00").unwrap();
        assert_eq!(d1.duration_since(d0).unwrap(), Duration::from_secs(86_400));
    }

    #[test]
    fn parse_exif_datetime_handles_leap_year() {
        // 2024 is a leap year, so Feb 28 -> Mar 1 is two days (Feb 29 exists).
        let feb28 = parse_exif_datetime("2024:02:28 00:00:00").unwrap();
        let mar01 = parse_exif_datetime("2024:03:01 00:00:00").unwrap();
        assert_eq!(mar01.duration_since(feb28).unwrap(), Duration::from_secs(2 * 86_400));
    }

    #[test]
    fn parse_exif_datetime_rejects_malformed() {
        assert_eq!(parse_exif_datetime(""), None);
        assert_eq!(parse_exif_datetime("0000:00:00 00:00:00"), None); // zeroed / unset
        assert_eq!(parse_exif_datetime("2026:13:01 00:00:00"), None); // bad month
        assert_eq!(parse_exif_datetime("garbage"), None);
        assert_eq!(parse_exif_datetime("2026:07:15"), None); // no time part
    }

    /// End-to-end decode through ImageIO + our CGBitmapContext path. Requires a
    /// test image at /tmp/iv-test/a.png (created by the dev workflow). Skipped
    /// if absent so the suite still passes on CI without it.
    #[test]
    fn decodes_known_image_to_nonblank_rgba() {
        let path = Path::new("/tmp/iv-test/a.png");
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let img = decode(path, 16384).expect("decode should succeed");
        assert!(img.width > 0 && img.height > 0, "non-zero dimensions");
        assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
        // The fixture is a solid color, so the alpha channel must be opaque and
        // the RGB must not be all-zero (which would mean nothing was drawn).
        let any_color = img.rgba.chunks_exact(4).any(|p| p[0] | p[1] | p[2] != 0);
        let opaque = img.rgba.chunks_exact(4).all(|p| p[3] == 255);
        assert!(any_color, "decoded pixels are all black -> draw failed");
        assert!(opaque, "expected opaque alpha for a solid-color image");
    }

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn orientation_1_is_identity() {
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 1);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(10));
        assert_eq!(&out.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_6_rotates_90cw_and_swaps_dims() {
        // A,B side by side (w=2,h=1). Rotate 90° CW → 1×2 column A over B.
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 6);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba[0..4], &px(10)); // top
        assert_eq!(&out.rgba[4..8], &px(20)); // bottom
    }

    #[test]
    fn orientation_8_rotates_270cw() {
        // 90° CW then 90° CCW must return to the original layout.
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let cw = apply_exif_orientation(img, 6); // 1×2 [10; 20]
        let back = apply_exif_orientation(cw, 8); // rot270 CW → back to 2×1 [10,20]
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(&back.rgba[0..4], &px(10));
        assert_eq!(&back.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_2_mirrors_horizontally_keeping_dims() {
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(20)); // columns swapped
        assert_eq!(&out.rgba[4..8], &px(10));
    }
}
