// SPDX-License-Identifier: GPL-3.0-or-later

//! Decode and metadata for Linux, Windows, and wasm32, mounted into
//! `image_decode` with `#[path]` and a glob re-export. Also owns the RAW
//! display boost and auto-denoise strength shared with `raw/preview.rs`.
//!
//! Items gated on `feature = "raw-probe"` also build on macOS so the
//! `decode_probe` binary (`raw/probe.rs`) and its parity tests can call them.

use std::path::Path;

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
use crate::image_decode::{
    apply_exif_orientation, fit_within, DecodedImage, DecodedImageFields, Flash, Gps,
    ImageMetadata, PixelFormat, WhiteBalance,
};

/// True for camera RAW extensions, which the `image` crate cannot decode.
/// Mirrors the RAW subset of `navigation.rs`'s `IMAGE_EXTS`.
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

/// Decodes a RAW/DNG file to rawler's undeveloped `RawImage`: sensor samples
/// with no white balance, color matrix, or gamma. Still mosaiced (cpp=1) for
/// Bayer/X-Trans files, already RGB (cpp=3/4) for Linear DNG.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// Unused in the main binary on a mac+raw-probe build.
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn decode_raw_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    rawler::decode_file(path).map_err(|e| e.to_string())
}

/// Maps rawler's `Orientation` to the EXIF code `1..=8` that
/// [`apply_exif_orientation`] expects. The variants are the same eight EXIF
/// cases in the same order.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) fn exif_code_from_rawler_orientation(o: rawler::Orientation) -> u8 {
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

/// Brightness and contrast boost for every RAW display rendering. A plain
/// linear-to-sRGB RAW conversion looks flatter and darker than a camera JPEG;
/// this is the extra look transform on top. The input is already
/// sRGB-encoded, not linear.
///
/// `raw_shader.wgsl` has a WGSL twin with the same formula and constants.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
const RAW_PREVIEW_BRIGHTNESS_GAMMA: f32 = 1.1;
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
const RAW_PREVIEW_CONTRAST_MIX: f32 = 0.75;

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) fn apply_raw_preview_boost(srgb: f32) -> f32 {
    let brightened = srgb.powf(1.0 / RAW_PREVIEW_BRIGHTNESS_GAMMA);
    let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
    (brightened + (contrast_curve - brightened) * RAW_PREVIEW_CONTRAST_MIX).clamp(0.0, 1.0)
}

/// Strength (0..=100, the Denoise slider's scale) of the always-on denoise
/// applied when decoding RAW. It stands in for the noise reduction ImageIO
/// does on macOS. Not ISO-scaled, because non-mac has no EXIF ISO read.
/// The `Fast` tier skips it: its 2x2 binning already averages out noise.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
pub(crate) const AUTO_RAW_DENOISE_STRENGTH: f32 = 25.0;

/// Decodes a camera RAW file with rawler's `RawDevelop` (demosaic, white
/// balance, color matrix, crop, sRGB gamma), then applies EXIF orientation.
/// There is no per-camera profile; color comes from `raw.color_matrix`.
///
/// rawler's demosaic dispatch panics via `todo!()` on a few unusual CFA
/// layouts. Ordinary Bayer and X-Trans files never reach those arms.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// On mac, only the raw-probe parity test calls this.
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn decode_raw_nonmac(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let raw = decode_raw_via_rawler(path)?;
    // Memory-map instead of `std::fs::read` + `new_from_slice`, which would
    // hold two full heap copies of the file just to read the orientation.
    let orientation = rawler::rawsource::RawSource::new(path)
        .ok()
        .and_then(|source| {
            let params = rawler::decoders::RawDecodeParams::default();
            rawler::get_decoder(&source)
                .ok()
                .and_then(|decoder| decoder.raw_metadata(&source, &params).ok())
                .and_then(|meta| meta.exif.orientation)
                .map(|orientation| {
                    exif_code_from_rawler_orientation(rawler::Orientation::from_u16(orientation))
                })
        })
        .unwrap_or_else(|| exif_code_from_rawler_orientation(raw.orientation));

    develop_raw_image_to_srgb8(&raw, orientation, max_dim)
}

/// [`decode_raw_nonmac`] for an in-memory file. Export uses it, and a browser
/// has no path to open. The parity test in `raw/probe.rs` checks both
/// produce identical bytes.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn decode_raw_nonmac_from_bytes(
    bytes: &[u8],
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    decode_raw_nonmac_from_source(source, max_dim)
}

/// Like [`decode_raw_nonmac_from_bytes`], but takes ownership of the buffer
/// so rawler can use it without a copy. The wasm export worker uses this.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn decode_raw_nonmac_from_shared_vec(
    bytes: std::sync::Arc<Vec<u8>>,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_shared_vec(bytes);
    decode_raw_nonmac_from_source(source, max_dim)
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[hotpath::measure]
fn decode_raw_nonmac_from_source(
    source: rawler::rawsource::RawSource,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let params = rawler::decoders::RawDecodeParams::default();
    let raw = rawler::decode(&source, &params).map_err(|e| e.to_string())?;
    let orientation = rawler::get_decoder(&source)
        .ok()
        .and_then(|decoder| decoder.raw_metadata(&source, &params).ok())
        .and_then(|meta| meta.exif.orientation)
        .map(|orientation| {
            exif_code_from_rawler_orientation(rawler::Orientation::from_u16(orientation))
        })
        .unwrap_or_else(|| exif_code_from_rawler_orientation(raw.orientation));

    develop_raw_image_to_srgb8(&raw, orientation, max_dim)
}

/// Develops a decoded RAW to premultiplied sRGB8: `RawDevelop`, display
/// boost, auto-denoise, `max_dim` downscale, then EXIF orientation.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[hotpath::measure]
fn develop_raw_image_to_srgb8(
    raw: &rawler::RawImage,
    orientation: u8,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let developed = rawler::imgop::develop::RawDevelop::default()
        .develop_intermediate(raw)
        .map_err(|e| e.to_string())?;
    let dynamic = developed
        .to_dynamic_image()
        .ok_or("rawler produced an empty developed image")?;
    let mut img = dynamic.into_rgba8();

    // `RawDevelop` already gamma-encoded these bytes, so the boost applies
    // directly through a u8 lookup table.
    let boost_lut: [u8; 256] =
        std::array::from_fn(|i| (apply_raw_preview_boost(i as f32 / 255.0) * 255.0).round() as u8);
    for px in img.pixels_mut() {
        px[0] = boost_lut[px[0] as usize];
        px[1] = boost_lut[px[1] as usize];
        px[2] = boost_lut[px[2] as usize];
    }

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded RAW image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);

    // Shrink before denoising, so the two f32 buffers below are preview-sized
    // rather than sensor-sized (about 570 MB less on a 24 MP file).
    if (w, h) != (src_w, src_h) {
        img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
    }

    // `RawDevelop` has no mid-pipeline hook, so denoise runs on the finished
    // sRGB8 image. It decodes with the 2.2 gamma approximation because that
    // is the space `denoise_linear_rgb_buffer` expects.
    let (dw, dh) = (img.width() as usize, img.height() as usize);
    let linear: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| {
            [
                (p[0] as f32 / 255.0).powf(2.2),
                (p[1] as f32 / 255.0).powf(2.2),
                (p[2] as f32 / 255.0).powf(2.2),
            ]
        })
        .collect();
    let denoised =
        crate::develop::denoise_linear_rgb_buffer(AUTO_RAW_DENOISE_STRENGTH, dw, dh, &linear);
    let encode = |v: f32| {
        (v.max(0.0).powf(1.0 / 2.2) * 255.0)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    for (px, lin) in img.pixels_mut().zip(denoised) {
        px[0] = encode(lin[0]);
        px[1] = encode(lin[1]);
        px[2] = encode(lin[2]);
    }

    Ok(apply_exif_orientation(
        DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba: img.into_raw(),
            pixel_format: PixelFormat::Srgb8,
        }),
        orientation,
    ))
}

/// Decodes `path`, downscaling so neither side exceeds `max_dim`, and applies
/// EXIF orientation like the mac version. RAW goes to [`decode_raw_nonmac`];
/// everything else goes through the `image` crate.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    if is_raw_extension(path) {
        return decode_raw_nonmac(path, max_dim);
    }
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    decode_nonraw_from_bytes(&bytes, max_dim)
}

/// The non-RAW half of [`decode`], from bytes. The wasm32 worker shares it
/// so browser decodes get the same orientation handling and Lanczos3 resize.
/// Built under raw-probe so export's round-trip test runs on mac.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub(crate) fn decode_nonraw_from_bytes(bytes: &[u8], max_dim: u32) -> Result<DecodedImage, String> {
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

/// Capture time for `path`. Non-mac reads no EXIF here, so this is the file
/// mtime, the same fallback the mac version uses.
#[cfg(not(target_os = "macos"))]
#[hotpath::measure]
pub fn capture_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// EXIF `DateTimeOriginal` with `OffsetTimeOriginal`, else `DateTime` with
/// `OffsetTime`. Reads JPEG and TIFF-based RAW containers.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
pub fn capture_stamp(path: &Path) -> Option<crate::image_decode::CaptureStamp> {
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
    let stamp = |time, offset| {
        crate::image_decode::CaptureStamp::new(&ascii(time)?, ascii(offset).as_deref())
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
    crate::image_decode::fill_file_facts(&mut meta, path);
    meta.source_size = pixel_size(path).map(|(w, h)| {
        if matches!(orientation_of(path), 5..=8) {
            (h, w)
        } else {
            (w, h)
        }
    });
    if is_raw_extension(path) {
        if let Ok(source) = rawler::rawsource::RawSource::new(path) {
            fill_from_raw_source(&mut meta, &source);
        }
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
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn fill_metadata_from_bytes(meta: &mut ImageMetadata, bytes: &[u8], raw: bool) {
    if raw {
        fill_from_raw_source(meta, &rawler::rawsource::RawSource::new_from_slice(bytes));
    } else if let Ok(exif) =
        exif::Reader::new().read_from_container(&mut std::io::Cursor::new(bytes))
    {
        fill_from_exif(meta, &exif);
    }
}

/// `catch_unwind` because rawler panics on some malformed files, and a
/// metadata read must not take its worker thread down.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn fill_from_raw_source(meta: &mut ImageMetadata, source: &rawler::rawsource::RawSource) {
    let read = std::panic::AssertUnwindSafe(|| {
        let params = rawler::decoders::RawDecodeParams::default();
        rawler::get_decoder(source)
            .ok()?
            .raw_metadata(source, &params)
            .ok()
    });
    if let Some(raw_meta) = std::panic::catch_unwind(read).ok().flatten() {
        fill_from_rawler(meta, &raw_meta);
    }
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn fill_from_rawler(meta: &mut ImageMetadata, raw: &rawler::decoders::RawMetadata) {
    use rawler::formats::tiff::{Rational, SRational};
    let ratio = |r: &Rational| finite(r.n as f64 / r.d as f64);
    let sratio = |r: &SRational| finite(r.n as f64 / r.d as f64);
    let exif = &raw.exif;
    meta.camera_make = non_empty(&raw.make);
    meta.camera_model = non_empty(&raw.model);
    meta.lens_model = exif
        .lens_model
        .as_deref()
        .and_then(non_empty)
        .or_else(|| raw.lens.as_ref().and_then(|l| non_empty(&l.lens_model)));
    meta.f_number = exif.fnumber.as_ref().and_then(ratio);
    meta.exposure_time = exif.exposure_time.as_ref().and_then(ratio);
    meta.iso = exif
        .iso_speed_ratings
        .map(u32::from)
        .or(exif.iso_speed)
        .filter(|&iso| iso > 0);
    meta.focal_length = exif.focal_length.as_ref().and_then(ratio);
    meta.exposure_bias = exif.exposure_bias.as_ref().and_then(sratio);
    meta.flash = exif.flash.and_then(|f| Flash::from_exif(f.into()));
    meta.white_balance = exif
        .white_balance
        .and_then(|w| WhiteBalance::from_exif(w.into()));
    meta.capture_date = exif
        .date_time_original
        .as_deref()
        .or(exif.create_date.as_deref())
        .and_then(crate::image_decode::parse_exif_datetime_display);
    meta.gps = exif.gps.as_ref().and_then(|gps| {
        let dms = |v: &[Rational; 3]| {
            finite(dms_to_degrees(
                v[0].n as f64 / v[0].d as f64,
                v[1].n as f64 / v[1].d as f64,
                v[2].n as f64 / v[2].d as f64,
            ))
        };
        Gps::from_exif(
            gps.gps_latitude.as_ref().and_then(dms)?,
            gps.gps_latitude_ref.as_deref(),
            gps.gps_longitude.as_ref().and_then(dms)?,
            gps.gps_longitude_ref.as_deref(),
            gps.gps_altitude.as_ref().and_then(ratio),
            gps.gps_altitude_ref == Some(1),
        )
    });
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
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
        .and_then(|s| crate::image_decode::parse_exif_datetime_display(&s));
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
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn dms_to_degrees(d: f64, m: f64, s: f64) -> f64 {
    d + m / 60.0 + s / 3600.0
}

/// A zero denominator gives infinity or NaN, which means "unknown" here.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn finite(v: f64) -> Option<f64> {
    v.is_finite().then_some(v)
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn non_empty(s: &str) -> Option<String> {
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

    /// 0 and 1 stay fixed, so the boost never clips or crushes, and mid-gray
    /// comes out brighter.
    #[test]
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    fn raw_preview_boost_is_identity_at_endpoints_and_brightens_midtones() {
        assert_eq!(apply_raw_preview_boost(0.0), 0.0);
        assert!((apply_raw_preview_boost(1.0) - 1.0).abs() < 1e-6);

        let mid = apply_raw_preview_boost(0.5);
        assert!(mid > 0.5, "expected midtone brightening, got {mid}");
    }

    /// A 16x8 JPEG carrying an APP1 EXIF segment with the given fields,
    /// built the way a camera lays it out: SOI, APP1, then the image.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
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

    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
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
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
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
            Some(crate::image_decode::CaptureDate {
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
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    fn a_jpeg_without_exif_leaves_every_field_empty() {
        let mut meta = ImageMetadata::default();
        fill_metadata_from_bytes(&mut meta, &plain_jpeg(), false);
        assert!(meta.camera_make.is_none() && meta.gps.is_none() && meta.flash.is_none());
        assert!(meta.capture_date.is_none() && meta.f_number.is_none());
    }
}
