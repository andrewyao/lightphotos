// SPDX-License-Identifier: GPL-3.0-or-later

//! Decode images to RGBA8 and read their metadata. macOS uses ImageIO for
//! every format, RAW included. Other targets use the `image` crate,
//! `rawler`, and `kamadak-exif`, in `raw/nonmac_decode.rs`.

#[cfg(target_os = "macos")]
use std::ffi::c_void;
use std::path::Path;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use std::time::SystemTime;

#[cfg(target_os = "macos")]
use objc2_core_foundation::{
    CFArray, CFDictionary, CFNumber, CFNumberType, CFRetained, CFString, CGPoint, CGRect, CGSize,
};
#[cfg(target_os = "macos")]
use objc2_core_graphics::{CGContext, CGImage};
#[cfg(target_os = "macos")]
use objc2_image_io::{
    kCGImagePropertyExifDateTimeOriginal, kCGImagePropertyExifDictionary,
    kCGImagePropertyExifExposureBiasValue, kCGImagePropertyExifExposureTime,
    kCGImagePropertyExifFNumber, kCGImagePropertyExifFlash, kCGImagePropertyExifFocalLength,
    kCGImagePropertyExifISOSpeedRatings, kCGImagePropertyExifLensModel,
    kCGImagePropertyExifOffsetTime, kCGImagePropertyExifOffsetTimeOriginal,
    kCGImagePropertyExifWhiteBalance, kCGImagePropertyGPSAltitude, kCGImagePropertyGPSAltitudeRef,
    kCGImagePropertyGPSDictionary, kCGImagePropertyGPSLatitude, kCGImagePropertyGPSLatitudeRef,
    kCGImagePropertyGPSLongitude, kCGImagePropertyGPSLongitudeRef, kCGImagePropertyOrientation,
    kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth, kCGImagePropertyTIFFDateTime,
    kCGImagePropertyTIFFDictionary, kCGImagePropertyTIFFMake, kCGImagePropertyTIFFModel,
    CGImageSource,
};

#[cfg(target_os = "macos")]
use crate::coregraphics;

/// The non-mac decode and metadata functions, re-exported so callers use
/// `image_decode::decode` on every platform.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[path = "raw/nonmac_decode.rs"]
mod nonmac_decode;
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) use nonmac_decode::*;

/// How `DecodedImage::rgba` is laid out. Every decoder produces `Srgb8`
/// except `raw_preview::decode_raw_quality_from_bytes` (the wasm32 Loupe RAW
/// decode). It returns linear light, and `raw_shader.wgsl` applies gamma and
/// the display boost on the GPU.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PixelFormat {
    /// sRGB-encoded RGBA8, 4 bytes per pixel.
    #[default]
    Srgb8,
    /// Linear-light RGBA as `half::f16`, 8 bytes per pixel. Alpha is always 1.
    LinearF16,
}

#[lightwatch::track(manual_measured)]
pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed, row-major pixels in `pixel_format`. For `Srgb8`, alpha
    /// is premultiplied on macOS (CoreGraphics output) and straight elsewhere
    /// (`image` crate output). Do not rely on alpha values matching across
    /// platforms.
    pub rgba: Vec<u8>,
    pub pixel_format: PixelFormat,
}

// The generated `Measured` would report `size_of::<Self>()`, about 40 bytes for
// an image whose pixels are megabytes. The census would then be a histogram of
// 40s and would answer nothing.
impl lightwatch::Measured for DecodedImage {
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.rgba.capacity()
    }
}

// CoreFoundation type IDs, used to check a value's type before casting it.
#[cfg(target_os = "macos")]
#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFGetTypeID(cf: *const c_void) -> core::ffi::c_ulong;
    fn CFNumberGetTypeID() -> core::ffi::c_ulong;
    fn CFStringGetTypeID() -> core::ffi::c_ulong;
    fn CFDictionaryGetTypeID() -> core::ffi::c_ulong;
    fn CFArrayGetTypeID() -> core::ffi::c_ulong;
}

/// File facts, camera, lens, exposure, capture date, and location for
/// display. Every platform's reader fills this one struct. Read from the file
/// each time and never stored in the catalog. Missing fields are `None`.
#[derive(Default)]
pub struct ImageMetadata {
    pub file_size: Option<u64>,
    /// Local wall-clock time of the file's last modification.
    pub modified: Option<CaptureDate>,
    /// Short format name from the extension, such as `JPEG` or `CR3`.
    pub format: Option<String>,
    pub camera_make: Option<String>,
    pub camera_model: Option<String>,
    pub lens_model: Option<String>,
    pub f_number: Option<f64>,
    pub exposure_time: Option<f64>,
    pub iso: Option<u32>,
    pub focal_length: Option<f64>,
    /// Exposure compensation in EV.
    pub exposure_bias: Option<f64>,
    pub flash: Option<Flash>,
    pub white_balance: Option<WhiteBalance>,
    pub gps: Option<Gps>,
    pub capture_date: Option<CaptureDate>,
    /// Full-resolution size in display orientation, as [`decode`] would
    /// return it. Read from the header without decoding, so the Loupe knows
    /// the true resolution while it still shows a preview.
    pub source_size: Option<(u32, u32)>,
}

/// A wall-clock time for display. For a capture date this is the camera's
/// clock as recorded, since EXIF has no time zone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureDate {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
}

/// `t` in the local time zone.
pub(crate) fn local_date(t: SystemTime) -> CaptureDate {
    use chrono::{Datelike, Timelike};
    let d: chrono::DateTime<chrono::Local> = t.into();
    CaptureDate {
        year: d.year(),
        month: d.month(),
        day: d.day(),
        hour: d.hour(),
        minute: d.minute(),
    }
}

/// Whether the flash fired. `None` when the camera has no flash.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Flash {
    Fired,
    DidNotFire,
}

impl Flash {
    /// Bit 0 of the EXIF `Flash` bitfield is "fired" and bit 5 is "no flash
    /// function". The other bits describe the mode and return light.
    pub(crate) fn from_exif(bits: u32) -> Option<Flash> {
        if bits & 0x01 != 0 {
            Some(Flash::Fired)
        } else if bits & 0x20 != 0 {
            None
        } else {
            Some(Flash::DidNotFire)
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WhiteBalance {
    Auto,
    Manual,
}

impl WhiteBalance {
    /// EXIF `WhiteBalance`: 0 is auto and 1 is manual.
    pub(crate) fn from_exif(value: u32) -> Option<WhiteBalance> {
        match value {
            0 => Some(WhiteBalance::Auto),
            1 => Some(WhiteBalance::Manual),
            _ => None,
        }
    }
}

/// Signed decimal degrees: north and east are positive. Altitude in meters,
/// negative below sea level.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gps {
    pub lat: f64,
    pub lon: f64,
    pub alt: Option<f64>,
}

impl Gps {
    /// EXIF stores unsigned magnitudes and puts the sign in the Ref tags:
    /// `S` and `W` are negative, and an altitude ref of 1 is below sea level.
    /// `None` for coordinates off the globe.
    pub(crate) fn from_exif(
        lat: f64,
        lat_ref: Option<&str>,
        lon: f64,
        lon_ref: Option<&str>,
        alt: Option<f64>,
        alt_below_sea_level: bool,
    ) -> Option<Gps> {
        let negative = |r: Option<&str>, neg: char| {
            r.and_then(|r| r.trim().chars().next())
                .is_some_and(|c| c.eq_ignore_ascii_case(&neg))
        };
        let lat = if negative(lat_ref, 'S') {
            -lat.abs()
        } else {
            lat.abs()
        };
        let lon = if negative(lon_ref, 'W') {
            -lon.abs()
        } else {
            lon.abs()
        };
        if !(lat.is_finite() && lon.is_finite() && lat.abs() <= 90.0 && lon.abs() <= 180.0) {
            return None;
        }
        let alt = alt.filter(|a| a.is_finite()).map(|a| {
            if alt_below_sea_level {
                -a.abs()
            } else {
                a.abs()
            }
        });
        Some(Gps { lat, lon, alt })
    }
}

/// Short format name from the file extension, such as `JPEG` or `CR3`.
pub(crate) fn format_name(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_uppercase();
    Some(match ext.as_str() {
        "JPG" | "JPE" => "JPEG".to_string(),
        "TIF" => "TIFF".to_string(),
        _ => ext,
    })
}

/// Size, modified time, and format: the facts that come from the file
/// system rather than the image.
pub(crate) fn fill_file_facts(meta: &mut ImageMetadata, path: &Path) {
    meta.format = format_name(path);
    if let Ok(fs_meta) = std::fs::metadata(path) {
        meta.file_size = Some(fs_meta.len());
        meta.modified = fs_meta.modified().ok().map(local_date);
    }
}

/// Open `path` as an ImageIO `CGImageSource`.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn open_image_source(path: &Path) -> Result<CFRetained<CGImageSource>, String> {
    let url = coregraphics::file_url(path)?;

    // SAFETY: url is a valid CFURL and no options are passed.
    unsafe { CGImageSource::with_url(&url, None) }
        .ok_or_else(|| "ImageIO could not open file".into())
}

/// Decode `path` upright, downscaled so neither side exceeds `max_dim`
/// (the GPU's max texture size, or `u32::MAX` for full resolution).
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let source = open_image_source(path)?;

    let image: CFRetained<CGImage> =
        unsafe { source.image_at_index(0, None) }.ok_or("ImageIO could not decode image")?;

    let src_w = CGImage::width(Some(&image)) as u32;
    let src_h = CGImage::height(Some(&image)) as u32;
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }

    let (w, h) = fit_within(src_w, src_h, max_dim);

    let decoded = cgimage_to_rgba(&image, w, h)?;
    // `image_at_index` ignores EXIF orientation, so rotate here to match the
    // thumbnails, which ImageIO orients for us.
    Ok(apply_exif_orientation(decoded, read_orientation(&source)))
}

/// Validated fields of an EXIF datetime.
struct DateTimeParts {
    y: i64,
    mo: u32,
    da: u32,
    h: u64,
    mi: u64,
    /// Read only by the macOS burst clock; display stops at minutes.
    #[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
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

/// Parse an EXIF datetime as UTC. EXIF has no time zone, and burst grouping
/// only needs times to be consistent with each other.
#[cfg(not(target_arch = "wasm32"))]
fn parse_exif_datetime(s: &str) -> Option<SystemTime> {
    let p = parse_exif_datetime_parts(s)?;
    let secs = days_from_civil(p.y, p.mo, p.da) * 86_400 + (p.h * 3600 + p.mi * 60 + p.se) as i64;
    (secs >= 0).then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(secs as u64))
}

pub(crate) fn parse_exif_datetime_display(s: &str) -> Option<CaptureDate> {
    let p = parse_exif_datetime_parts(s)?;
    Some(CaptureDate {
        year: p.y as i32,
        month: p.mo,
        day: p.da,
        hour: p.h as u32,
        minute: p.mi as u32,
    })
}

/// Days since the Unix epoch for a Gregorian date (Howard Hinnant's
/// `days_from_civil`). Expects `m` in 1..=12 and `d` in 1..=31.
#[cfg(not(target_arch = "wasm32"))]
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400; // [0, 399]
    let (m, d) = (m as i64, d as i64);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// When a photo was taken, as its EXIF recorded it, so an export can carry the
/// date forward and an upload can send the right instant.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Debug, PartialEq)]
pub struct CaptureStamp {
    /// `YYYY:MM:DD HH:MM:SS` on the camera's clock.
    pub local: String,
    /// `+HH:MM` or `-HH:MM` from `OffsetTimeOriginal`, when the camera
    /// recorded one.
    pub offset: Option<String>,
}

#[cfg(not(target_arch = "wasm32"))]
impl CaptureStamp {
    /// Validates both fields, so a malformed tag is dropped rather than
    /// written into an export.
    pub fn new(local: &str, offset: Option<&str>) -> Option<Self> {
        let local = local.trim();
        parse_exif_datetime_parts(local)?;
        Some(Self {
            local: local.to_string(),
            offset: offset
                .map(str::trim)
                .filter(|o| offset_seconds(o).is_some())
                .map(str::to_string),
        })
    }

    /// The moment this names. Needs the offset: a camera clock reading
    /// without one could be in any time zone.
    pub fn instant(&self) -> Option<SystemTime> {
        let as_if_utc = parse_exif_datetime(&self.local)?;
        let offset = offset_seconds(self.offset.as_deref()?)?;
        if offset >= 0 {
            as_if_utc.checked_sub(Duration::from_secs(offset as u64))
        } else {
            as_if_utc.checked_add(Duration::from_secs(offset.unsigned_abs()))
        }
    }
}

/// Seconds east of UTC for an EXIF offset such as `+09:00` or `-05:30`.
#[cfg(not(target_arch = "wasm32"))]
fn offset_seconds(s: &str) -> Option<i64> {
    let (sign, rest) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    let (h, m) = rest.split_once(':')?;
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 14 && m < 60 && rest.len() == 5).then_some(sign * (h * 3600 + m * 60))
}

/// Capture time from EXIF or TIFF, else the file's mtime so grouping always
/// has something to sort by. `None` only when the mtime is unreadable too.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn capture_time(path: &Path) -> Option<SystemTime> {
    let source = open_image_source(path).ok()?;
    read_capture_time(&source)
        .or_else(|| std::fs::metadata(path).ok().and_then(|m| m.modified().ok()))
}

/// EXIF `DateTimeOriginal`, else TIFF `DateTime`.
#[cfg(target_os = "macos")]
fn read_capture_time(source: &CGImageSource) -> Option<SystemTime> {
    // SAFETY: index 0 exists and no options are passed.
    let props = unsafe { source.properties_at_index(0, None) }?;

    if let Some(exif) = dict_dictionary(&props, unsafe { kCGImagePropertyExifDictionary }) {
        if let Some(t) = dict_string(exif, unsafe { kCGImagePropertyExifDateTimeOriginal })
            .and_then(|s| parse_exif_datetime(&s))
        {
            return Some(t);
        }
    }

    dict_dictionary(&props, unsafe { kCGImagePropertyTIFFDictionary })
        .and_then(|tiff| dict_string(tiff, unsafe { kCGImagePropertyTIFFDateTime }))
        .and_then(|s| parse_exif_datetime(&s))
}

/// EXIF `DateTimeOriginal` with `OffsetTimeOriginal`, else TIFF `DateTime`
/// with `OffsetTime`. No mtime fallback: an export's EXIF should only claim a
/// date the camera recorded.
#[cfg(target_os = "macos")]
pub fn capture_stamp(path: &Path) -> Option<CaptureStamp> {
    let source = open_image_source(path).ok()?;
    // SAFETY: index 0 exists and no options are passed.
    let props = unsafe { source.properties_at_index(0, None) }?;
    let exif = dict_dictionary(&props, unsafe { kCGImagePropertyExifDictionary });
    let exif_str = |key| exif.and_then(|e| dict_string(e, key));
    if let Some(stamp) = exif_str(unsafe { kCGImagePropertyExifDateTimeOriginal }).and_then(|t| {
        CaptureStamp::new(
            &t,
            exif_str(unsafe { kCGImagePropertyExifOffsetTimeOriginal }).as_deref(),
        )
    }) {
        return Some(stamp);
    }
    let t = dict_string(&props, unsafe { kCGImagePropertyTIFFDateTime })?;
    CaptureStamp::new(
        &t,
        exif_str(unsafe { kCGImagePropertyExifOffsetTime }).as_deref(),
    )
}

/// Like `read_capture_time`, but for display. No mtime fallback, because an
/// mtime is not a capture date and should not be shown as one.
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

    dict_dictionary(&props, unsafe { kCGImagePropertyTIFFDictionary })
        .and_then(|tiff| dict_string(tiff, unsafe { kCGImagePropertyTIFFDateTime }))
        .and_then(|s| parse_exif_datetime_display(&s))
}

/// Metadata for `path`. An unreadable file gives all `None`.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn read_metadata(path: &Path) -> ImageMetadata {
    let mut meta = ImageMetadata::default();
    fill_file_facts(&mut meta, path);
    let Ok(source) = open_image_source(path) else {
        return meta;
    };
    meta.capture_date = read_capture_date(&source);

    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return meta;
    };

    if let Some(tiff) = dict_dictionary(&props, unsafe { kCGImagePropertyTIFFDictionary }) {
        meta.camera_make = dict_string(tiff, unsafe { kCGImagePropertyTIFFMake });
        meta.camera_model = dict_string(tiff, unsafe { kCGImagePropertyTIFFModel });
    }

    // Swap to display orientation for EXIF 5..=8, matching `decode`.
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
        meta.exposure_bias = dict_f64(exif, unsafe { kCGImagePropertyExifExposureBiasValue });
        meta.flash = dict_f64(exif, unsafe { kCGImagePropertyExifFlash })
            .and_then(|v| Flash::from_exif(v as u32));
        meta.white_balance = dict_f64(exif, unsafe { kCGImagePropertyExifWhiteBalance })
            .and_then(|v| WhiteBalance::from_exif(v as u32));
    }

    if let Some(gps) = dict_dictionary(&props, unsafe { kCGImagePropertyGPSDictionary }) {
        if let (Some(lat), Some(lon)) = (
            dict_f64(gps, unsafe { kCGImagePropertyGPSLatitude }),
            dict_f64(gps, unsafe { kCGImagePropertyGPSLongitude }),
        ) {
            meta.gps = Gps::from_exif(
                lat,
                dict_string(gps, unsafe { kCGImagePropertyGPSLatitudeRef }).as_deref(),
                lon,
                dict_string(gps, unsafe { kCGImagePropertyGPSLongitudeRef }).as_deref(),
                dict_f64(gps, unsafe { kCGImagePropertyGPSAltitude }),
                dict_f64(gps, unsafe { kCGImagePropertyGPSAltitudeRef }) == Some(1.0),
            );
        }
    }

    meta
}

/// Borrowed pointer to the value for `key`, unchecked. Null if absent.
#[cfg(target_os = "macos")]
fn dict_raw(dict: &CFDictionary, key: &CFString) -> *const c_void {
    // SAFETY: `key` is a valid CFString.
    unsafe { dict.value(key as *const CFString as *const c_void) }
}

/// String value for `key`. This and the helpers below check the CF type
/// before casting, because a crafted file can store any type under any key.
#[cfg(target_os = "macos")]
fn dict_string(dict: &CFDictionary, key: &CFString) -> Option<String> {
    let ptr = dict_raw(dict, key);
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFStringGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFString.
    Some(unsafe { &*(ptr as *const CFString) }.to_string())
}

#[cfg(target_os = "macos")]
fn dict_dictionary<'a>(dict: &'a CFDictionary, key: &CFString) -> Option<&'a CFDictionary> {
    let ptr = dict_raw(dict, key);
    if ptr.is_null() || unsafe { CFGetTypeID(ptr) } != unsafe { CFDictionaryGetTypeID() } {
        return None;
    }
    // SAFETY: confirmed the value is a CFDictionary.
    Some(unsafe { &*(ptr as *const CFDictionary) })
}

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

#[cfg(target_os = "macos")]
fn dict_f64(dict: &CFDictionary, key: &CFString) -> Option<f64> {
    number_f64(dict_raw(dict, key))
}

/// First number under `key`. EXIF stores some values, like ISOSpeedRatings,
/// as an array of numbers, but some files store a bare number.
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
        // SAFETY: count is non-zero, so index 0 is in bounds.
        let first = unsafe { array.value_at_index(0) };
        return number_f64(first).map(|v| v as u32);
    }
    None
}

/// Stored pixel size, before EXIF orientation, read without decoding. Swap
/// the axes for orientations `5..=8` to get the display size.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn pixel_size(path: &Path) -> Option<(u32, u32)> {
    let source = open_image_source(path).ok()?;
    // SAFETY: index 0 exists for any image the source opened.
    let props = unsafe { source.properties_at_index(0, None) }?;
    let w = dict_f64(&props, unsafe { kCGImagePropertyPixelWidth })?;
    let h = dict_f64(&props, unsafe { kCGImagePropertyPixelHeight })?;
    if w <= 0.0 || h <= 0.0 {
        return None;
    }
    Some((w as u32, h as u32))
}

/// EXIF orientation `1..=8` of `path`, `1` when absent. [`decode`] already
/// applies it. Use it to align output computed in stored orientation, such as
/// Vision's segmentation masks.
#[cfg(target_os = "macos")]
pub fn orientation_of(path: &Path) -> u8 {
    open_image_source(path)
        .map(|source| read_orientation(&source))
        .unwrap_or(1)
}

/// EXIF orientation `1..=8` of `path` via the `image` crate. `1` for RAW
/// files, PNGs, and anything unreadable.
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

/// EXIF orientation `1..=8`, or `1` when absent or invalid.
#[cfg(target_os = "macos")]
fn read_orientation(source: &CGImageSource) -> u8 {
    // SAFETY: index 0 exists and no options are passed.
    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return 1;
    };
    // SAFETY: the key is a valid CFString. The result is borrowed or null.
    let ptr =
        unsafe { props.value(kCGImagePropertyOrientation as *const CFString as *const c_void) };
    if ptr.is_null() {
        return 1;
    }
    // A crafted file can store any CF type here, and reading a non-CFNumber
    // as one is UB.
    if unsafe { CFGetTypeID(ptr) } != unsafe { CFNumberGetTypeID() } {
        return 1;
    }
    // SAFETY: the type check above confirmed a CFNumber.
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

/// Rotate or mirror RGBA8 pixels upright for EXIF orientation `1..=8`.
/// Orientations `5..=8` swap width and height.
#[hotpath::measure]
pub(crate) fn apply_exif_orientation(img: DecodedImage, orientation: u8) -> DecodedImage {
    if orientation <= 1 {
        return img;
    }
    // The loop assumes 4 bytes per pixel. The only `LinearF16` producer
    // orients its own output and never calls this.
    debug_assert_eq!(img.pixel_format, PixelFormat::Srgb8);
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
    DecodedImage::new_tracked(DecodedImageFields {
        width: nw,
        height: nh,
        rgba: dst,
        pixel_format: img.pixel_format,
    })
}

/// Draw `image` scaled to `target_w` x `target_h` and return premultiplied
/// sRGB RGBA8 pixels.
#[cfg(target_os = "macos")]
#[hotpath::measure]
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

    // SAFETY: buffer holds target_w * target_h * 4 bytes and outlives `ctx`.
    let ctx = unsafe {
        coregraphics::srgb_bitmap_context(
            buffer.as_mut_ptr() as *mut c_void,
            target_w,
            target_h,
            bytes_per_row,
        )?
    };

    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: target_w as f64,
            height: target_h as f64,
        },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    Ok(DecodedImage::new_tracked(DecodedImageFields {
        width: target_w,
        height: target_h,
        rgba: buffer,
        pixel_format: PixelFormat::Srgb8,
    }))
}

/// Scale `(w, h)` down, keeping aspect ratio, so neither side exceeds
/// `max_dim`.
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

    // 997 and 331 are deliberately unrelated. Every construction of this type
    // now routes its two adjacent `u32` dimensions through `DecodedImageFields`,
    // and a square or a round number would let a transposed pair pass.
    #[test]
    fn width_and_height_land_in_their_own_fields() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 997,
            height: 331,
            rgba: vec![9; 997 * 331 * 4],
            pixel_format: PixelFormat::Srgb8,
        });
        assert_eq!((img.width, img.height), (997, 331));

        // Orientation 6 swaps them, so this pins the constructor inside
        // `apply_exif_orientation` too.
        let rotated = apply_exif_orientation(img, 6);
        assert_eq!((rotated.width, rotated.height), (331, 997));
        assert_eq!(rotated.rgba.len(), 997 * 331 * 4);
    }

    #[test]
    fn a_capture_stamp_names_an_instant_only_with_its_offset() {
        let tokyo = CaptureStamp::new("2024:06:01 18:00:00", Some("+09:00")).unwrap();
        let utc = parse_exif_datetime("2024:06:01 09:00:00").unwrap();
        assert_eq!(tokyo.instant(), Some(utc));

        let pacific = CaptureStamp::new("2024:06:01 02:00:00", Some("-07:00")).unwrap();
        assert_eq!(pacific.instant(), Some(utc));

        let bare = CaptureStamp::new("2024:06:01 18:00:00", None).unwrap();
        assert_eq!(bare.instant(), None, "a bare clock time could be any zone");
    }

    #[test]
    fn malformed_capture_tags_are_dropped() {
        assert_eq!(CaptureStamp::new("0000:00:00 00:00:00", None), None);
        let stamp = CaptureStamp::new(" 2024:06:01 18:00:00 ", Some("+9")).unwrap();
        assert_eq!(stamp.local, "2024:06:01 18:00:00");
        assert_eq!(stamp.offset, None, "an offset that isn't ±HH:MM is dropped");
    }

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

    /// Needs a solid-color image at /tmp/iv-test/a.png. Skips when it is
    /// missing.
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
        // All-zero RGB would mean nothing was drawn.
        let any_color = img.rgba.chunks_exact(4).any(|p| p[0] | p[1] | p[2] != 0);
        let opaque = img.rgba.chunks_exact(4).all(|p| p[3] == 255);
        assert!(any_color, "decoded pixels are all black -> draw failed");
        assert!(opaque, "expected opaque alpha for a solid-color image");
    }

    #[test]
    fn flash_reads_the_fired_bit_and_hides_cameras_without_one() {
        assert_eq!(Flash::from_exif(0x00), Some(Flash::DidNotFire));
        assert_eq!(Flash::from_exif(0x01), Some(Flash::Fired));
        // Auto mode, fired, return light detected.
        assert_eq!(Flash::from_exif(0x1F), Some(Flash::Fired));
        // Compulsory off and auto did-not-fire both set mode bits only.
        assert_eq!(Flash::from_exif(0x10), Some(Flash::DidNotFire));
        assert_eq!(Flash::from_exif(0x18), Some(Flash::DidNotFire));
        assert_eq!(Flash::from_exif(0x20), None, "no flash function");
    }

    #[test]
    fn white_balance_maps_auto_and_manual_only() {
        assert_eq!(WhiteBalance::from_exif(0), Some(WhiteBalance::Auto));
        assert_eq!(WhiteBalance::from_exif(1), Some(WhiteBalance::Manual));
        assert_eq!(WhiteBalance::from_exif(2), None);
    }

    #[test]
    fn gps_applies_the_hemisphere_and_sea_level_refs() {
        let north_east = Gps::from_exif(37.5, Some("N"), 122.25, Some("E"), Some(12.0), false);
        assert_eq!(
            north_east,
            Some(Gps {
                lat: 37.5,
                lon: 122.25,
                alt: Some(12.0)
            })
        );
        let south_west = Gps::from_exif(33.9, Some("S"), 151.2, Some("w"), Some(30.0), true);
        assert_eq!(
            south_west,
            Some(Gps {
                lat: -33.9,
                lon: -151.2,
                alt: Some(-30.0)
            })
        );
        let no_refs = Gps::from_exif(1.0, None, 2.0, None, None, false).unwrap();
        assert_eq!((no_refs.lat, no_refs.lon, no_refs.alt), (1.0, 2.0, None));
    }

    #[test]
    fn gps_rejects_coordinates_off_the_globe() {
        assert_eq!(
            Gps::from_exif(91.0, Some("N"), 0.0, Some("E"), None, false),
            None
        );
        assert_eq!(
            Gps::from_exif(0.0, Some("N"), 181.0, Some("E"), None, false),
            None
        );
        assert_eq!(Gps::from_exif(f64::NAN, None, 0.0, None, None, false), None);
    }

    #[test]
    fn format_name_normalizes_common_extensions() {
        assert_eq!(
            format_name(Path::new("a/IMG_1.jpg")).as_deref(),
            Some("JPEG")
        );
        assert_eq!(format_name(Path::new("b.tif")).as_deref(), Some("TIFF"));
        assert_eq!(format_name(Path::new("c.cr3")).as_deref(), Some("CR3"));
        assert_eq!(format_name(Path::new("noext")), None);
    }

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn orientation_1_is_identity() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
            pixel_format: PixelFormat::Srgb8,
        });
        let out = apply_exif_orientation(img, 1);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(10));
        assert_eq!(&out.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_6_rotates_90cw_and_swaps_dims() {
        // 2x1 [A, B] rotated 90° CW is a 1x2 column, A over B.
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
            pixel_format: PixelFormat::Srgb8,
        });
        let out = apply_exif_orientation(img, 6);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba[0..4], &px(10)); // top
        assert_eq!(&out.rgba[4..8], &px(20)); // bottom
    }

    #[test]
    fn orientation_8_rotates_270cw() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
            pixel_format: PixelFormat::Srgb8,
        });
        let cw = apply_exif_orientation(img, 6);
        let back = apply_exif_orientation(cw, 8);
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(&back.rgba[0..4], &px(10));
        assert_eq!(&back.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_2_mirrors_horizontally_keeping_dims() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 2,
            height: 1,
            rgba: [px(10), px(20)].concat(),
            pixel_format: PixelFormat::Srgb8,
        });
        let out = apply_exif_orientation(img, 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(20)); // columns swapped
        assert_eq!(&out.rgba[4..8], &px(10));
    }
}
