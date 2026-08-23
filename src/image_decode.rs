// SPDX-License-Identifier: GPL-3.0-or-later

//! Decode an image to RGBA8 bytes.
//!
//! macOS: any format macOS understands (JPEG/PNG/GIF/TIFF/BMP/HEIC/RAW) via
//! Apple's ImageIO + CoreGraphics, no third-party codecs. Pipeline: CFURL ->
//! CGImageSource -> CGImage -> draw into a CGBitmapContext backed by our own
//! buffer (sRGB, premultiplied RGBA, big-endian byte order), then read the
//! buffer back.
//!
//! Non-mac: JPEG/PNG/TIFF via the `image` crate; camera RAW via `rawler`
//! (`decode_raw_nonmac`) — decode the sensor samples, then run rawler's own
//! demosaic/white-balance/color-calibration/gamma pipeline to get a viewable
//! image. Metadata reading (EXIF camera/lens fields, capture time beyond
//! mtime) isn't wired up yet on this platform — see each function's non-mac
//! doc comment for its exact fallback behavior.

#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::path::Path;
use std::time::{Duration, SystemTime};

#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
#[cfg(target_os = "macos")]
use objc2_core_graphics::{CGContext, CGImage};
#[cfg(target_os = "macos")]
use objc2_image_io::{
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
    kCGImagePropertyExifExposureTime, kCGImagePropertyExifFNumber, kCGImagePropertyExifFocalLength,
    kCGImagePropertyExifISOSpeedRatings, kCGImagePropertyExifLensModel,
    kCGImagePropertyOrientation, kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth,
    kCGImagePropertyTIFFDateTime, kCGImagePropertyTIFFMake,
    kCGImagePropertyTIFFModel, CGImageSource,
};

#[cfg(target_os = "macos")]
use crate::coregraphics;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, row-major. Premultiplied alpha on the mac arm
    /// (drawn through a CGBitmapContext); straight (non-premultiplied) alpha
    /// on the non-mac arm (produced by the `image` crate). The renderer's
    /// blend mode is straight-alpha, so this divergence is currently
    /// harmless, but it is a real difference between platforms worth knowing
    /// about before relying on alpha values off mac.
    pub rgba: Vec<u8>,
}

// CoreFoundation runtime type introspection, used to verify a value's concrete
// type before reinterpreting it. CoreFoundation is already linked transitively.
#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(cf: *const c_void) -> core::ffi::c_ulong;
    fn CFNumberGetTypeID() -> core::ffi::c_ulong;
    fn CFStringGetTypeID() -> core::ffi::c_ulong;
    fn CFDictionaryGetTypeID() -> core::ffi::c_ulong;
    fn CFArrayGetTypeID() -> core::ffi::c_ulong;
}

/// Camera/lens/exposure metadata plus capture date, read live from a file's
/// EXIF/TIFF properties for display (never persisted — see `catalog.rs`).
/// Any field absent from the source (screenshots, re-exports, stripped EXIF)
/// is simply `None`.
#[derive(Default)]
pub struct ImageMetadata {
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub f_number: Option<f64>,
    pub exposure_time: Option<f64>,
    pub iso: Option<u32>,
    pub focal_length: Option<f64>,
    pub capture_date: Option<CaptureDate>,
    /// The original's pixel dimensions in *display* orientation (i.e. with the
    /// EXIF rotation already applied, matching what [`decode`] produces). Read
    /// from the image properties, so it costs no decode — which is the point:
    /// it lets the loupe know the true source resolution while it is still
    /// showing a thumbnail or a downscaled preview.
    pub source_size: Option<(u32, u32)>,
}

/// A capture timestamp broken into calendar fields as the camera recorded them
/// (EXIF carries no timezone, so these are displayed as-is — the camera's own
/// wall-clock reading — rather than converted through `SystemTime`).
#[derive(Clone, Copy)]
pub struct CaptureDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// Decode `path`, optionally downscaling so neither side exceeds `max_dim`
/// (so images larger than the GPU's max texture size still display).
/// Open `path` as a `CGImageSource` (the shared CFURL + ImageIO open path used
/// by both full-resolution decode and thumbnail generation).
#[cfg(target_os = "macos")]
pub fn open_image_source(path: &Path) -> Result<CFRetained<CGImageSource>, String> {
    let url = coregraphics::file_url(path)?;

    // SAFETY: url is a valid CFURL; passing no decode options. The returned
    // CGImageSource is +1 retained and wrapped in CFRetained, released on drop.
    unsafe { CGImageSource::with_url(&url, None) }
        .ok_or_else(|| "ImageIO could not open file".into())
}

#[cfg(target_os = "macos")]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let source = open_image_source(path)?;

    let image: CFRetained<CGImage> =
        unsafe { source.image_at_index(0, None) }.ok_or("ImageIO could not decode image")?;

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

/// Extensions we treat as camera RAW on the non-mac decode path — these route
/// to `decode_raw_nonmac` instead of the `image` crate (which doesn't parse
/// RAW containers). Mirrors the RAW subset of `navigation.rs`'s `IMAGE_EXTS`.
/// `pub(crate)`, not just private: `app/web.rs`'s wasm32 thumbnail decode
/// reuses this same list to skip RAW files for now (M1's scope is JPEG
/// only, per the wasm port plan — RAW gets its own decode path in M3,
/// `image::load_from_memory` can't read RAW sensor data at all, it's not a
/// baseline-TIFF image despite the TIFF-based container).
#[cfg(not(target_os = "macos"))]
pub(crate) fn is_raw_extension(path: &Path) -> bool {
    const RAW_EXTS: &[&str] = &[
        "cr2", "cr3", "nef", "arw", "dng", "raf", "rw2", "orf", "pef", "srw",
    ];
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| RAW_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// Decode a RAW/DNG file into rawler's native `RawImage`: undeveloped sensor
/// samples (still mosaiced for a typical Bayer/X-Trans camera file, cpp=1;
/// already-demosaiced for Linear DNG, cpp=3/4) — no white balance, color
/// matrix, or gamma applied yet.
///
/// Gated on `feature = "raw-probe"` as well as `not(target_os = "macos")` so
/// `decode_probe.rs`'s ground-truth harness (Task 8) can call this same
/// function — including from a mac dev build via `cargo run --bin
/// decode_probe --features raw-probe`, where `rawler` is available as the
/// optional top-level dependency — instead of duplicating the
/// `rawler::decode_file` call site. That harness compares these raw,
/// undeveloped samples directly against an analytic fixture, so it must NOT
/// be routed through [`decode_raw_nonmac`]'s develop/demosaic pipeline below.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// On mac+raw-probe this compiles into the *main* `lightphotos` binary too
// (the feature has no way to scope itself to just `decode_probe.rs`'s
// build), where nothing calls it — only `decode_probe.rs`'s own copy of this
// module does. Genuinely used on non-mac (by `decode_raw_nonmac` below) and
// via `cargo run/test --bin decode_probe --features raw-probe`.
#[allow(dead_code)]
pub(crate) fn decode_raw_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    rawler::decode_file(path).map_err(|e| e.to_string())
}

/// Map rawler's own `Orientation` enum (read from the file's EXIF/TIFF
/// orientation tag during decode) to the raw EXIF orientation code (`1..=8`)
/// that [`apply_exif_orientation`] expects. This is a direct rename, not a
/// reinterpretation — rawler's variants are the same 8 EXIF cases in the same
/// order (see `rawler::Orientation::from_u16`).
#[cfg(not(target_os = "macos"))]
fn exif_code_from_rawler_orientation(o: rawler::Orientation) -> u8 {
    use rawler::Orientation::*;
    match o {
        Normal => 1,
        HorizontalFlip => 2,
        Rotate180 => 3,
        VerticalFlip => 4,
        Transpose => 5,
        Rotate90 => 6,
        Transverse => 7,
        Rotate270 => 8,
        Unknown => 1,
    }
}

/// Decode a camera RAW file on non-mac platforms via `rawler`: decode the raw
/// sensor samples ([`decode_raw_via_rawler`]), then run rawler's own
/// `RawDevelop` pipeline (rescale -> demosaic -> active-area crop -> white
/// balance -> color-matrix calibration -> default crop -> sRGB gamma) to turn
/// them into a viewable image — a raw sensor mosaic isn't displayable pixel
/// data on its own. Finally applies the file's EXIF/TIFF orientation the same
/// way the mac arm and the non-mac JPEG/PNG/TIFF arm do, so callers never see
/// a sideways/mirrored RAW regardless of platform.
///
/// Known gap (tracked in the plan, Task 12/14): this pipeline has only been
/// exercised end-to-end against the synthetic Linear DNG fixture (cpp=3, so
/// the demosaic branch below is never taken) via `decode_probe.rs` — no
/// real-camera Bayer-CFA RAW (CR2/NEF/ARW) has been run through it. Note also
/// that rawler's own demosaic dispatch (`RawDevelop::develop_intermediate`)
/// panics via `todo!()` for a couple of CFA layouts it doesn't recognize;
/// ordinary Bayer/X-Trans cameras don't hit those arms, but it's a real,
/// narrow panic surface inherited from the dependency, not something this
/// function can guard against from the outside.
#[cfg(not(target_os = "macos"))]
fn decode_raw_nonmac(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let raw = decode_raw_via_rawler(path)?;
    let orientation = exif_code_from_rawler_orientation(raw.orientation);

    let developed = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(&raw)
        .map_err(|e| e.to_string())?;
    let dynamic = developed
        .to_dynamic_image()
        .ok_or("rawler produced an empty developed image")?;
    let img = dynamic.into_rgba8();

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded RAW image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);
    let rgba = if (w, h) == (src_w, src_h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Ok(apply_exif_orientation(
        DecodedImage {
            width: w,
            height: h,
            rgba,
        },
        orientation,
    ))
}

/// Decode `path`, optionally downscaling so neither side exceeds `max_dim`.
/// JPEG/PNG/TIFF go through the `image` crate; RAW extensions are routed to
/// `decode_raw_nonmac` (Task 10). Applies EXIF orientation via the decoder's
/// own `orientation()` (JPEG/TIFF support it; PNG has none and defaults to
/// identity), matching the mac arm's behavior so callers never see a
/// sideways/mirrored image regardless of platform.
#[cfg(not(target_os = "macos"))]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    if is_raw_extension(path) {
        return decode_raw_nonmac(path, max_dim);
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_jpeg_png_tiff_from_bytes(&bytes, max_dim)
}

/// The non-RAW half of [`decode`] above, minus the file read — bytes-based
/// so wasm32's own decode path (`app/web.rs`, reading via
/// `FileSystemFileHandle` instead of `std::fs::read`) can share this exact
/// logic (full decode + EXIF-orientation-correct + `Lanczos3` resize)
/// rather than a second, easy-to-drift-from-correct reimplementation —
/// which is exactly what happened once already: an earlier wasm32 version
/// used `DynamicImage::thumbnail()` (a fast/low-quality filter, not
/// `Lanczos3`) and applied no orientation at all.
#[cfg(not(target_os = "macos"))]
pub(crate) fn decode_jpeg_png_tiff_from_bytes(
    bytes: &[u8],
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut decoder = reader.into_decoder().map_err(|e| e.to_string())?;

    use image::ImageDecoder;
    let orientation = decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms);

    let mut img = image::DynamicImage::from_decoder(decoder).map_err(|e| e.to_string())?;
    img.apply_orientation(orientation);
    let img = img.into_rgba8();

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);
    let rgba = if (w, h) == (src_w, src_h) {
        img.into_raw()
    } else {
        image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Ok(DecodedImage {
        width: w,
        height: h,
        rgba,
    })
}

/// Validated calendar/time components parsed from an EXIF datetime string.
struct DateTimeParts {
    y: i64,
    mo: u32,
    da: u32,
    h: u64,
    mi: u64,
    se: u64,
}

/// Parse an EXIF datetime string (`"YYYY:MM:DD HH:MM:SS"`) into validated
/// components. Returns `None` for empty, zeroed, or malformed values.
fn parse_exif_datetime_parts(s: &str) -> Option<DateTimeParts> {
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
    Some(DateTimeParts {
        y,
        mo,
        da,
        h,
        mi,
        se,
    })
}

/// Parse an EXIF datetime string into a `SystemTime`, interpreting it as UTC
/// (EXIF carries no timezone; only *consistency* matters for burst grouping,
/// not absolute correctness).
fn parse_exif_datetime(s: &str) -> Option<SystemTime> {
    let p = parse_exif_datetime_parts(s)?;
    let secs = days_from_civil(p.y, p.mo, p.da) * 86_400 + (p.h * 3600 + p.mi * 60 + p.se) as i64;
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

/// Parse an EXIF datetime string into calendar fields for display, as the
/// camera recorded them — no timezone conversion (see `CaptureDate`).
fn parse_exif_datetime_display(s: &str) -> Option<CaptureDate> {
    let p = parse_exif_datetime_parts(s)?;
    Some(CaptureDate {
        year: p.y as i32,
        month: p.mo,
        day: p.da,
        hour: p.h as u32,
        minute: p.mi as u32,
    })
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
#[cfg(target_os = "macos")]
pub fn capture_time(path: &Path) -> Option<SystemTime> {
    let source = open_image_source(path).ok()?;
    read_capture_time(&source)
        .or_else(|| std::fs::metadata(path).ok().and_then(|m| m.modified().ok()))
}

/// Capture time for `path`. Non-mac has no EXIF reader wired up yet, so this
/// always takes the mtime fallback described above (the same path the mac
/// arm takes for any file whose EXIF is absent/unparseable).
#[cfg(not(target_os = "macos"))]
pub fn capture_time(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Read the capture timestamp from an open source: EXIF `DateTimeOriginal`
/// first, then TIFF `DateTime`. `None` when neither is present/parseable.
#[cfg(target_os = "macos")]
fn read_capture_time(source: &CGImageSource) -> Option<SystemTime> {
    // SAFETY: index 0 exists; no options. Dictionary is +1 retained, freed on drop.
    let props = unsafe { source.properties_at_index(0, None) }?;

    // EXIF sub-dictionary → DateTimeOriginal (preferred).
    if let Some(exif) = dict_dictionary(&props, unsafe { kCGImagePropertyExifDictionary }) {
        if let Some(t) = dict_string(exif, unsafe { kCGImagePropertyExifDateTimeOriginal })
            .and_then(|s| parse_exif_datetime(&s))
        {
            return Some(t);
        }
    }

    // TIFF DateTime (top-level) fallback.
    dict_string(&props, unsafe { kCGImagePropertyTIFFDateTime })
        .and_then(|s| parse_exif_datetime(&s))
}

/// Read the capture date for display: EXIF `DateTimeOriginal` first, then
/// TIFF `DateTime`. `None` when neither is present/parseable — unlike
/// `capture_time`, this has no filesystem-mtime fallback, since a
/// modification time isn't a capture date and shouldn't be shown as one.
#[cfg(target_os = "macos")]
fn read_capture_date(source: &CGImageSource) -> Option<CaptureDate> {
    let props = unsafe { source.properties_at_index(0, None) }?;

    if let Some(exif) = dict_dictionary(&props, unsafe { kCGImagePropertyExifDictionary }) {
        if let Some(d) = dict_string(exif, unsafe { kCGImagePropertyExifDateTimeOriginal })
            .and_then(|s| parse_exif_datetime_display(&s))
        {
            return Some(d);
        }
    }

    dict_string(&props, unsafe { kCGImagePropertyTIFFDateTime })
        .and_then(|s| parse_exif_datetime_display(&s))
}

/// Read camera/lens/exposure metadata plus capture date for `path`. Never
/// panics; an unreadable file yields an all-`None` `ImageMetadata`.
#[cfg(target_os = "macos")]
pub fn read_metadata(path: &Path) -> ImageMetadata {
    let mut meta = ImageMetadata::default();
    let Ok(source) = open_image_source(path) else {
        return meta;
    };
    meta.capture_date = read_capture_date(&source);

    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return meta;
    };

    meta.camera_make = dict_string(&props, unsafe { kCGImagePropertyTIFFMake });
    meta.camera_model = dict_string(&props, unsafe { kCGImagePropertyTIFFModel });

    // Stored pixel dimensions, swapped into display orientation for the
    // quarter-turn EXIF orientations so they line up with `decode`'s output.
    if let (Some(w), Some(h)) = (
        dict_f64(&props, unsafe { kCGImagePropertyPixelWidth }),
        dict_f64(&props, unsafe { kCGImagePropertyPixelHeight }),
    ) {
        if w > 0.0 && h > 0.0 {
            let (w, h) = (w as u32, h as u32);
            meta.source_size = Some(if matches!(read_orientation(&source), 5..=8) {
                (h, w)
            } else {
                (w, h)
            });
        }
    }

    if let Some(exif) = dict_dictionary(&props, unsafe { kCGImagePropertyExifDictionary }) {
        meta.lens_model = dict_string(exif, unsafe { kCGImagePropertyExifLensModel });
        meta.f_number = dict_f64(exif, unsafe { kCGImagePropertyExifFNumber });
        meta.exposure_time = dict_f64(exif, unsafe { kCGImagePropertyExifExposureTime });
        meta.focal_length = dict_f64(exif, unsafe { kCGImagePropertyExifFocalLength });
        meta.iso = dict_first_u32(exif, unsafe { kCGImagePropertyExifISOSpeedRatings });
    }

    meta
}

/// Read camera/lens/exposure metadata plus capture date for `path`. Non-mac
/// has no EXIF reader wired up yet, so every field is `None` except
/// `source_size`, which comes from [`pixel_size`] (the stored, pre-orientation
/// dimensions) swapped into display orientation for the quarter-turn EXIF
/// orientations via [`orientation_of`] — mirroring the mac arm's logic exactly
/// so the result lines up with what [`decode`] actually produces.
#[cfg(not(target_os = "macos"))]
pub fn read_metadata(path: &Path) -> ImageMetadata {
    let source_size = pixel_size(path).map(|(w, h)| {
        if matches!(orientation_of(path), 5..=8) {
            (h, w)
        } else {
            (w, h)
        }
    });
    ImageMetadata {
        source_size,
        ..ImageMetadata::default()
    }
}

/// Fetch a dictionary value by key with no type checking; null if absent.
#[cfg(target_os = "macos")]
fn dict_raw(dict: &CFDictionary, key: &CFString) -> *const c_void {
    // SAFETY: `key` is a valid CFString option key; `value` returns a borrowed
    // pointer to the stored value, or null when absent.
    unsafe { dict.value(key as *const CFString as *const c_void) }
}

/// Read a CFString value from a CFDictionary for `key`, verifying the concrete
/// type before reinterpreting (a crafted file could store another CFType).
#[cfg(target_os = "macos")]
fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    let ptr = dict_raw(dict, key);
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFString.
    Some(unsafe { &*(ptr as *const CFString) }.to_string())
}

/// Read a CFDictionary sub-value from a CFDictionary for `key`.
#[cfg(target_os = "macos")]
fn dict_dictionary<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFDictionary> {
    let ptr = dict_raw(dict, key);
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFDictionaryGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFDictionary.
    Some(unsafe { &*(ptr as *const CFDictionary) })
}

/// Read a `CFNumber` at a raw (already-fetched) pointer as `f64`, verifying
/// the concrete type before reinterpreting.
#[cfg(target_os = "macos")]
fn number_f64(ptr: *const c_void) -> Option<f64> {
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFNumberGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFNumber.
    let number = unsafe { &*(ptr as *const CFNumber) };
    let mut out: f64 = 0.0;
    let ok = unsafe {
        number.value(
            CFNumberType::Float64Type,
            &mut out as *mut f64 as *mut c_void,
        )
    };
    ok.then_some(out)
}

/// Read a CFNumber value from a CFDictionary for `key` as `f64`.
#[cfg(target_os = "macos")]
fn dict_f64(dict: &CFDictionary, key: &CFString) -> Option<f64> {
    number_f64(dict_raw(dict, key))
}

/// Read the first numeric value from a CFDictionary entry for `key`, which per
/// the EXIF spec may be stored as a CFArray of CFNumbers (ISOSpeedRatings) —
/// falls back to reading it as a bare CFNumber for lenient sources.
#[cfg(target_os = "macos")]
fn dict_first_u32(dict: &CFDictionary, key: &CFString) -> Option<u32> {
    let ptr = dict_raw(dict, key);
    if ptr.is_null() {
        return None;
    }
    let type_id = unsafe { CFGetTypeID(ptr) };
    if type_id == unsafe { CFNumberGetTypeID() } {
        return number_f64(ptr).map(|v| v as u32);
    }
    if type_id == unsafe { CFArrayGetTypeID() } {
        // SAFETY: confirmed the value is a CFArray.
        let array = unsafe { &*(ptr as *const CFArray) };
        if array.count() == 0 {
            return None;
        }
        // SAFETY: index 0 is in bounds (count checked above); borrowed pointer.
        let first = unsafe { array.value_at_index(0) };
        return number_f64(first).map(|v| v as u32);
    }
    None
}

/// The image's stored pixel dimensions, read from ImageIO's properties without
/// decoding a single pixel.
///
/// These are the dimensions *as stored*, before EXIF orientation is applied —
/// which is exactly what a caller comparing against another framework's
/// upright-assuming coordinate space wants (see `facequality.rs`). Callers who
/// want display dimensions should swap the axes themselves for orientations
/// `5..=8`, the way [`apply_exif_orientation`] does.
#[cfg(target_os = "macos")]
pub fn pixel_size(path: &Path) -> Option<(u32, u32)> {
    let source = open_image_source(path).ok()?;
    // SAFETY: index 0 exists for any image the source opened; no options passed.
    let props = unsafe { source.properties_at_index(0, None) }?;
    let w = dict_f64(&props, unsafe { kCGImagePropertyPixelWidth })?;
    let h = dict_f64(&props, unsafe { kCGImagePropertyPixelHeight })?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// The image's stored pixel dimensions, before EXIF orientation is applied —
/// same semantics as the mac arm (see above). Reads just the header via the
/// `image` crate's decoder, no full decode.
#[cfg(not(target_os = "macos"))]
pub fn pixel_size(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// The EXIF orientation of the image at `path` (`1..=8`, `1` when absent).
///
/// [`decode`] already applies this, so callers only need it to line something
/// up with a decoded image that was produced *outside* this pipeline — Vision's
/// segmentation masks, which are computed in the file's stored orientation.
#[cfg(target_os = "macos")]
pub fn orientation_of(path: &Path) -> u8 {
    open_image_source(path)
        .map(|source| read_orientation(&source))
        .unwrap_or(1)
}

/// The EXIF orientation of the image at `path`. Non-mac: reads it via the
/// `image` crate's own decoder-level orientation support (the same mechanism
/// [`decode`]'s non-mac arm already uses), converted back to the raw EXIF
/// code (`1..=8`) via [`image::metadata::Orientation::to_exif`]. Returns the
/// identity orientation (`1`) for RAW files (no `image`-crate decoder), PNGs
/// (no orientation tag), or any unreadable/unrecognized file.
#[cfg(not(target_os = "macos"))]
pub fn orientation_of(path: &Path) -> u8 {
    let Ok(reader) = image::ImageReader::open(path) else {
        return 1;
    };
    let Ok(reader) = reader.with_guessed_format() else {
        return 1;
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return 1;
    };
    use image::ImageDecoder;
    decoder
        .orientation()
        .unwrap_or(image::metadata::Orientation::NoTransforms)
        .to_exif()
}

/// The image's EXIF orientation tag (`1..=8`), or `1` when absent/unreadable.
/// Never panics — any missing property yields the identity orientation.
#[cfg(target_os = "macos")]
fn read_orientation(source: &CGImageSource) -> u8 {
    // SAFETY: index 0 exists (we already decoded it); no options passed. The
    // returned dictionary is +1 retained and released on drop.
    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return 1;
    };
    // SAFETY: the orientation key is a valid CFString option key; `value` returns
    // a borrowed (non-owned) pointer to the CFNumber, or null if absent.
    let ptr =
        unsafe { props.value(kCGImagePropertyOrientation as *const CFString as *const c_void) };
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
        number.value(
            CFNumberType::SInt32Type,
            &mut out as *mut i32 as *mut c_void,
        )
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
pub(crate) fn apply_exif_orientation(img: DecodedImage, orientation: u8) -> DecodedImage {
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
    DecodedImage {
        width: nw,
        height: nh,
        rgba: dst,
    }
}

/// Draw a `CGImage` into a freshly-allocated sRGB bitmap context sized
/// `(target_w, target_h)` and read back the result as tightly-packed,
/// premultiplied RGBA8 (byte order R,G,B,A — matches `Rgba8UnormSrgb`).
///
/// Scales the image into the target rect, so callers can use this both for a
/// full-size decode and for a thumbnail (passing the thumbnail's own size).
#[cfg(target_os = "macos")]
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
        size: CGSize {
            width: target_w as f64,
            height: target_h as f64,
        },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    Ok(DecodedImage {
        width: target_w,
        height: target_h,
        rgba: buffer,
    })
}

pub(crate) fn fit_within(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
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
        assert_eq!(
            mar01.duration_since(feb28).unwrap(),
            Duration::from_secs(2 * 86_400)
        );
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
        let img = DecodedImage {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
        };
        let out = apply_exif_orientation(img, 1);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(10));
        assert_eq!(&out.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_6_rotates_90cw_and_swaps_dims() {
        // A,B side by side (w=2,h=1). Rotate 90° CW → 1×2 column A over B.
        let img = DecodedImage {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
        };
        let out = apply_exif_orientation(img, 6);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba[0..4], &px(10)); // top
        assert_eq!(&out.rgba[4..8], &px(20)); // bottom
    }

    #[test]
    fn orientation_8_rotates_270cw() {
        // 90° CW then 90° CCW must return to the original layout.
        let img = DecodedImage {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
        };
        let cw = apply_exif_orientation(img, 6); // 1×2 [10; 20]
        let back = apply_exif_orientation(cw, 8); // rot270 CW → back to 2×1 [10,20]
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(&back.rgba[0..4], &px(10));
        assert_eq!(&back.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_2_mirrors_horizontally_keeping_dims() {
        let img = DecodedImage {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
        };
        let out = apply_exif_orientation(img, 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(20)); // columns swapped
        assert_eq!(&out.rgba[4..8], &px(10));
    }
}
