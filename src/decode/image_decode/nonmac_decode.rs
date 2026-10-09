// SPDX-License-Identifier: MIT OR Apache-2.0

//! Decode and metadata for Linux, Windows, and wasm32, mounted into
//! `image_decode` as a child module with a glob re-export. RAW files go to
//! `decode::rawler`; everything else goes through the `image` and
//! `kamadak-exif` crates.

use std::path::Path;

#[cfg(not(target_os = "macos"))]
use crate::decode::image_decode::{
    fit_within, DecodedImage, DecodedImageFields, Flash, Gps, ImageMetadata, PixelFormat,
    WhiteBalance,
};

#[cfg(not(target_os = "macos"))]
use crate::decode::image_decode::is_raw_extension;

#[cfg(not(target_os = "macos"))]
pub use crate::develop::apply_raw_preview_boost;

/// Decodes `path`, downscaling so neither side exceeds `max_dim`, and applies
/// EXIF orientation like the mac version. RAW goes to
/// [`decode_raw_nonmac`](crate::decode::rawler::decode_raw_nonmac);
/// everything else goes through the `image` crate.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    if is_raw_extension(path) {
        return crate::decode::rawler::decode_raw_nonmac(path, max_dim);
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_nonraw_from_bytes(&bytes, max_dim)
}

/// The non-RAW half of [`decode`], from bytes. The wasm32 worker shares it
/// so browser decodes get the same orientation handling and Lanczos3 resize.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_nonraw_from_bytes(bytes: &[u8], max_dim: u32) -> Result<DecodedImage, String> {
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
    Ok(DecodedImage::new_tracked(DecodedImageFields {
        width: w,
        height: h,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    }))
}

/// Capture time for `path`: EXIF `DateTimeOriginal` with its sub-seconds,
/// else the file mtime, the same fallback the mac version uses. A RAW goes
/// straight to the mtime.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn capture_time(path: &Path) -> Option<std::time::SystemTime> {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(t) = exif_capture_time(path) {
        return Some(t);
    }
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn exif_capture_time(path: &Path) -> Option<std::time::SystemTime> {
    if is_raw_extension(path) {
        return None;
    }
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .ok()?;
    let ascii = |tag| {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    };
    let t = crate::decode::image_decode::parse_exif_datetime(&ascii(exif::Tag::DateTimeOriginal)?)?;
    Some(crate::decode::image_decode::with_subsec(
        t,
        ascii(exif::Tag::SubSecTimeOriginal).as_deref(),
    ))
}

/// EXIF `DateTimeOriginal` with `OffsetTimeOriginal`, else `DateTime` with
/// `OffsetTime`. Reads JPEG and TIFF-based RAW containers.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
pub fn capture_stamp(path: &Path) -> Option<crate::decode::image_decode::CaptureStamp> {
    let file = std::fs::File::open(path).ok()?;
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::BufReader::new(file))
        .ok()?;
    stamp_from_exif(&exif)
}

/// [`capture_stamp`] for a source the caller already holds in memory, which is
/// how the browser's export worker gets its photos.
pub fn capture_stamp_from_bytes(bytes: &[u8]) -> Option<crate::decode::image_decode::CaptureStamp> {
    let exif = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(bytes))
        .ok()?;
    stamp_from_exif(&exif)
}

fn stamp_from_exif(exif: &exif::Exif) -> Option<crate::decode::image_decode::CaptureStamp> {
    let ascii = |tag| {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(v) => v.first().map(|b| String::from_utf8_lossy(b).into_owned()),
            _ => None,
        }
    };
    let stamp = |time, offset| {
        crate::decode::image_decode::CaptureStamp::new(&ascii(time)?, ascii(offset).as_deref())
    };
    stamp(exif::Tag::DateTimeOriginal, exif::Tag::OffsetTimeOriginal)
        .or_else(|| stamp(exif::Tag::DateTime, exif::Tag::OffsetTime))
}

/// Metadata for `path`. `source_size` is in display orientation (width and
/// height swapped for EXIF 5..=8), to match [`decode`].
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn read_metadata(path: &Path) -> ImageMetadata {
    let mut meta = ImageMetadata::default();
    crate::decode::image_decode::fill_file_facts(&mut meta, path);
    meta.source_size = pixel_size(path).map(|(w, h)| {
        if matches!(orientation_of(path), 5..=8) {
            (h, w)
        } else {
            (w, h)
        }
    });
    if is_raw_extension(path) {
        crate::decode::rawler::fill_raw_metadata_from_path(&mut meta, path);
    } else if let Ok(file) = std::fs::File::open(path) {
        if let Ok(exif) =
            exif::Reader::new().read_from_container(&mut std::io::BufReader::new(file))
        {
            fill_from_exif(&mut meta, &exif);
        }
    }
    meta
}

/// Camera, exposure, date, and GPS fields from a whole file in memory. The
/// browser has bytes, not a path. File facts and `source_size` are left to
/// the caller.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub fn fill_metadata_from_bytes(meta: &mut ImageMetadata, bytes: &[u8], raw: bool) {
    if raw {
        crate::decode::rawler::fill_raw_metadata_from_bytes(meta, bytes);
    } else if let Ok(exif) =
        exif::Reader::new().read_from_container(&mut std::io::Cursor::new(bytes))
    {
        fill_from_exif(meta, &exif);
    }
}

#[cfg(not(target_os = "macos"))]
fn fill_from_exif(meta: &mut ImageMetadata, exif: &exif::Exif) {
    use exif::{In, Tag, Value};
    let field = |tag| exif.get_field(tag, In::PRIMARY).map(|f| &f.value);
    let text = |tag| match field(tag)? {
        Value::Ascii(parts) => {
            let s = String::from_utf8_lossy(parts.first()?);
            non_empty(s.trim_end_matches('\0'))
        }
        _ => None,
    };
    let number = |tag| match field(tag)? {
        Value::Rational(r) => finite(r.first()?.to_f64()),
        Value::SRational(r) => finite(r.first()?.to_f64()),
        v => v.get_uint(0).map(f64::from),
    };
    let degrees = |tag| match field(tag)? {
        Value::Rational(r) if r.len() >= 3 => {
            finite(dms_to_degrees(r[0].to_f64(), r[1].to_f64(), r[2].to_f64()))
        }
        _ => None,
    };
    let uint = |tag| field(tag)?.get_uint(0);

    meta.camera_make = text(Tag::Make);
    meta.camera_model = text(Tag::Model);
    meta.lens_model = text(Tag::LensModel);
    meta.f_number = number(Tag::FNumber);
    meta.exposure_time = number(Tag::ExposureTime);
    meta.iso = uint(Tag::PhotographicSensitivity).filter(|&iso| iso > 0);
    meta.focal_length = number(Tag::FocalLength);
    meta.exposure_bias = number(Tag::ExposureBiasValue);
    meta.flash = uint(Tag::Flash).and_then(Flash::from_exif);
    meta.white_balance = uint(Tag::WhiteBalance).and_then(WhiteBalance::from_exif);
    meta.capture_date = text(Tag::DateTimeOriginal)
        .or_else(|| text(Tag::DateTime))
        .and_then(|s| crate::decode::image_decode::parse_exif_datetime_display(&s));
    if let (Some(lat), Some(lon)) = (degrees(Tag::GPSLatitude), degrees(Tag::GPSLongitude)) {
        meta.gps = Gps::from_exif(
            lat,
            text(Tag::GPSLatitudeRef).as_deref(),
            lon,
            text(Tag::GPSLongitudeRef).as_deref(),
            number(Tag::GPSAltitude),
            uint(Tag::GPSAltitudeRef) == Some(1),
        );
    }
}

/// EXIF stores a coordinate as degrees, minutes, and seconds.
#[cfg(not(target_os = "macos"))]
pub(crate) fn dms_to_degrees(d: f64, m: f64, s: f64) -> f64 {
    d + m / 60.0 + s / 3600.0
}

/// A zero denominator gives infinity or NaN, which means "unknown" here.
#[cfg(not(target_os = "macos"))]
pub(crate) fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Stored pixel dimensions, before EXIF orientation. Reads only the header.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn pixel_size(path: &Path) -> Option<(u32, u32)> {
    image::image_dimensions(path).ok()
}

/// EXIF orientation code `1..=8` for `path`, via the `image` crate. Returns
/// `1` for RAW files, PNGs, and anything unreadable.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A 16x8 JPEG carrying an APP1 EXIF segment with the given fields,
    /// built the way a camera lays it out: SOI, APP1, then the image.
    #[cfg(not(target_os = "macos"))]
    fn jpeg_with_exif(fields: &[exif::Field]) -> Vec<u8> {
        let mut writer = exif::experimental::Writer::new();
        for f in fields {
            writer.push_field(f);
        }
        let mut tiff = std::io::Cursor::new(Vec::new());
        writer.write(&mut tiff, false).unwrap();
        let tiff = tiff.into_inner();

        let jpeg = plain_jpeg();
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((tiff.len() + 8) as u16).to_be_bytes());
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[cfg(not(target_os = "macos"))]
    fn plain_jpeg() -> Vec<u8> {
        let mut jpeg = Vec::new();
        image::RgbImage::new(16, 8)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .unwrap();
        jpeg
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_jpegs_exif_fills_camera_exposure_date_and_signed_gps() {
        use exif::{Field, In, Rational, SRational, Tag, Value};
        let f = |tag, value| Field {
            tag,
            ifd_num: In::PRIMARY,
            value,
        };
        let r = |num, denom| Rational { num, denom };
        let ascii = |s: &str| Value::Ascii(vec![s.as_bytes().to_vec()]);
        let fields = [
            f(Tag::Make, ascii("Canon")),
            f(Tag::Model, ascii("Canon EOS R5")),
            f(Tag::LensModel, ascii("RF24-70mm F2.8 L IS USM")),
            f(Tag::FNumber, Value::Rational(vec![r(28, 10)])),
            f(Tag::ExposureTime, Value::Rational(vec![r(1, 250)])),
            f(Tag::PhotographicSensitivity, Value::Short(vec![400])),
            f(Tag::FocalLength, Value::Rational(vec![r(50, 1)])),
            f(
                Tag::ExposureBiasValue,
                Value::SRational(vec![SRational { num: -2, denom: 3 }]),
            ),
            f(Tag::Flash, Value::Short(vec![0x19])),
            f(Tag::WhiteBalance, Value::Short(vec![1])),
            f(Tag::DateTimeOriginal, ascii("2026:07:14 15:42:09")),
            f(Tag::GPSLatitudeRef, ascii("S")),
            f(
                Tag::GPSLatitude,
                Value::Rational(vec![r(33, 1), r(51, 1), r(36, 1)]),
            ),
            f(Tag::GPSLongitudeRef, ascii("E")),
            f(
                Tag::GPSLongitude,
                Value::Rational(vec![r(151, 1), r(12, 1), r(0, 1)]),
            ),
            f(Tag::GPSAltitudeRef, Value::Byte(vec![1])),
            f(Tag::GPSAltitude, Value::Rational(vec![r(25, 2)])),
        ];
        let bytes = jpeg_with_exif(&fields);

        let mut meta = ImageMetadata::default();
        fill_metadata_from_bytes(&mut meta, &bytes, false);

        assert_eq!(meta.camera_make.as_deref(), Some("Canon"));
        assert_eq!(meta.camera_model.as_deref(), Some("Canon EOS R5"));
        assert_eq!(meta.lens_model.as_deref(), Some("RF24-70mm F2.8 L IS USM"));
        assert_eq!(meta.f_number, Some(2.8));
        assert_eq!(meta.exposure_time, Some(1.0 / 250.0));
        assert_eq!(meta.iso, Some(400));
        assert_eq!(meta.focal_length, Some(50.0));
        assert_eq!(meta.exposure_bias, Some(-2.0 / 3.0));
        assert_eq!(meta.flash, Some(Flash::Fired));
        assert_eq!(meta.white_balance, Some(WhiteBalance::Manual));
        assert_eq!(
            meta.capture_date,
            Some(crate::decode::image_decode::CaptureDate {
                year: 2026,
                month: 7,
                day: 14,
                hour: 15,
                minute: 42,
            })
        );
        let gps = meta.gps.expect("GPS read");
        assert!(
            (gps.lat - -33.86).abs() < 1e-9,
            "south is negative: {}",
            gps.lat
        );
        assert!((gps.lon - 151.2).abs() < 1e-9, "{}", gps.lon);
        assert_eq!(gps.alt, Some(-12.5), "ref 1 is below sea level");

        // The JPEG is still a JPEG the decoder reads.
        let decoded = decode_nonraw_from_bytes(&bytes, 64).unwrap();
        assert_eq!((decoded.width, decoded.height), (16, 8));
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn a_jpeg_without_exif_leaves_every_field_empty() {
        let mut meta = ImageMetadata::default();
        fill_metadata_from_bytes(&mut meta, &plain_jpeg(), false);
        assert!(meta.camera_make.is_none() && meta.gps.is_none() && meta.flash.is_none());
        assert!(meta.capture_date.is_none() && meta.f_number.is_none());
    }
}
