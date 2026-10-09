// SPDX-License-Identifier: MIT OR Apache-2.0

//! Every call into rawler, the camera RAW decoder used off macOS (Linux,
//! Windows and wasm32). macOS decodes RAW through ImageIO and must never link
//! rawler (LGPL-2.1), so `decode/mod.rs` builds this module only off macOS.
//! The `decode_probe` harness in `probe/` is the one other rawler user.
//!
//! - This file: full RAW decode with `RawDevelop`, RAW metadata, and the
//!   embedded CR3/RAF preview.
//! - `raw_preview`: the two-tier RAW decode for the wasm32 worker.

pub mod raw_preview;

use std::path::Path;

use crate::decode::image_decode::{
    apply_exif_orientation, check_decode_size, dms_to_degrees, finite, fit_within, non_empty,
    DecodedImage, DecodedImageFields, Flash, Gps, ImageMetadata, PixelFormat, WhiteBalance,
};
use crate::develop::apply_raw_preview_boost;

/// rawler 0.7.2 panics instead of returning an error when the raw buffer it
/// is about to allocate is over 50,000 samples on a side or 500 million in
/// all. A panic is fatal to a wasm32 decode thread and strands everything the
/// thread held, so a file past the limit is refused here first. rawler counts
/// a row in samples, padded to whole tiles, so a 3-channel DNG wider than
/// 16,666 px (a Lightroom panorama) is past it.
pub fn check_rawler_size_limit(source: &rawler::rawsource::RawSource) -> Result<(), String> {
    use rawler::decoders::WellKnownIFD;
    use rawler::tags::TiffCommonTag;

    let Some(ifd) = rawler::get_decoder(source)
        .ok()
        .and_then(|decoder| decoder.ifd(WellKnownIFD::Raw).ok().flatten())
    else {
        return Ok(());
    };
    let tag = |t| ifd.get_entry(t).map(|e| e.value.force_usize(0));
    let (Some(width), Some(height)) = (
        tag(TiffCommonTag::ImageWidth),
        tag(TiffCommonTag::ImageLength),
    ) else {
        return Ok(());
    };
    let padded = |n: usize, tile: Option<usize>| match tile {
        Some(t) if t > 0 => n.div_ceil(t) * t,
        _ => n,
    };
    let cpp = tag(TiffCommonTag::SamplesPerPixel).unwrap_or(1).max(1);
    let row = padded(width, tag(TiffCommonTag::TileWidth)).saturating_mul(cpp);
    let rows = padded(height, tag(TiffCommonTag::TileLength));
    if width == 0
        || height == 0
        || row > 50_000
        || rows > 50_000
        || row.saturating_mul(rows) > 500_000_000
    {
        return Err(format!(
            "{width}x{height} with {cpp} samples per pixel is too large to decode"
        ));
    }
    Ok(())
}

/// rawler 0.7.2's `apply_scaling` panics on a Linear DNG whose black level
/// count differs from its white level count. The Samsung Galaxy S23 Ultra
/// writes a 2x2 `BlackLevelRepeatDim` (12 levels for 3 channels) against 3
/// white levels. This averages the repeat down to one black level per channel
/// and widens a single white level to every channel, or refuses the file when
/// the levels still don't line up. Must run before `apply_scaling` or
/// `RawDevelop`, since a panic is fatal to a wasm32 decode thread.
pub fn normalize_linear_levels(raw: &mut rawler::RawImage) -> Result<(), String> {
    use rawler::rawimage::{BlackLevel, RawPhotometricInterpretation, WhiteLevel};

    if !matches!(raw.photometric, RawPhotometricInterpretation::LinearRaw) {
        return Ok(());
    }
    let cpp = raw.cpp;
    let black = raw.blacklevel.as_vec();
    let white = &raw.whitelevel.0;
    if black.len() == white.len() {
        return Ok(());
    }
    if cpp == 0 || black.len() % cpp != 0 || !(white.len() == 1 || white.len() == cpp) {
        return Err(format!(
            "unsupported RAW levels ({} black, {} white, cpp={cpp})",
            black.len(),
            white.len()
        ));
    }
    // Levels are stored position-major: `levels[pos * cpp + channel]`.
    let positions = black.len() / cpp;
    let per_channel: Vec<f32> = (0..cpp)
        .map(|c| (0..positions).map(|p| black[p * cpp + c]).sum::<f32>() / positions as f32)
        .collect();
    raw.blacklevel = BlackLevel::new(&per_channel, 1, 1, cpp);
    if white.len() == 1 {
        raw.whitelevel = WhiteLevel(vec![white[0]; cpp]);
    }
    Ok(())
}

/// Decodes a RAW/DNG file to rawler's undeveloped `RawImage`: sensor samples
/// with no white balance, color matrix, or gamma. Still mosaiced (cpp=1) for
/// Bayer/X-Trans files, already RGB (cpp=3/4) for Linear DNG.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_via_rawler(path: &Path) -> Result<rawler::RawImage, String> {
    rawler::decode_file(path).map_err(|e| e.to_string())
}

/// Maps rawler's `Orientation` to the EXIF code `1..=8` that
/// [`apply_exif_orientation`] expects. The variants are the same eight EXIF
/// cases in the same order.
pub fn exif_code_from_rawler_orientation(o: rawler::Orientation) -> u8 {
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

/// Strength (0..=100, the Denoise slider's scale) of the always-on denoise
/// applied when decoding RAW. It stands in for the noise reduction ImageIO
/// does on macOS. Not ISO-scaled, because non-mac has no EXIF ISO read.
/// The `Fast` tier skips it: its 2x2 binning already averages out noise.
pub const AUTO_RAW_DENOISE_STRENGTH: f32 = 25.0;

/// Decodes a camera RAW file with rawler's `RawDevelop` (demosaic, white
/// balance, color matrix, crop, sRGB gamma), then applies EXIF orientation.
/// There is no per-camera profile; color comes from `raw.color_matrix`.
///
/// rawler's demosaic dispatch panics via `todo!()` on a few unusual CFA
/// layouts. Ordinary Bayer and X-Trans files never reach those arms.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
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
/// has no path to open. The parity test in `decode_probe` checks both
/// produce identical bytes.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac_from_bytes(bytes: &[u8], max_dim: u32) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    decode_raw_nonmac_from_source(source, max_dim)
}

/// Like [`decode_raw_nonmac_from_bytes`], but takes ownership of the buffer
/// so rawler can use it without a copy. The wasm export worker uses this.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac_from_shared_vec(
    bytes: std::sync::Arc<Vec<u8>>,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_shared_vec(bytes);
    decode_raw_nonmac_from_source(source, max_dim)
}

#[hotpath::measure]
fn decode_raw_nonmac_from_source(
    source: rawler::rawsource::RawSource,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    check_rawler_size_limit(&source)?;
    let params = rawler::decoders::RawDecodeParams::default();
    let mut raw = rawler::decode(&source, &params).map_err(|e| e.to_string())?;
    normalize_linear_levels(&mut raw)?;
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
#[hotpath::measure]
fn develop_raw_image_to_srgb8(
    raw: &rawler::RawImage,
    orientation: u8,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let side = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    check_decode_size(side(raw.width), side(raw.height))?;
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

/// Camera, exposure, date and GPS fields of the RAW at `path`, read through
/// a memory map.
pub fn fill_raw_metadata_from_path(meta: &mut ImageMetadata, path: &Path) {
    if let Ok(source) = rawler::rawsource::RawSource::new(path) {
        fill_from_raw_source(meta, &source);
    }
}

/// [`fill_raw_metadata_from_path`] for a whole RAW file in memory.
pub fn fill_raw_metadata_from_bytes(meta: &mut ImageMetadata, bytes: &[u8]) {
    fill_from_raw_source(meta, &rawler::rawsource::RawSource::new_from_slice(bytes));
}

/// `catch_unwind` because rawler panics on some malformed files, and a
/// metadata read must not take its worker thread down.
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
        .and_then(crate::decode::image_decode::parse_exif_datetime_display);
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

/// The embedded preview of a CR3 or RAF file, via rawler's
/// `Decoder::full_image()`. `kamadak-exif` cannot open those containers.
///
/// Only CR3 and RAF. rawler also returns a preview for TIFF-based RAWs, but
/// that camera JPEG has the camera's own tone and color and looks very
/// different from our RAW develop, so it must not stand in for it.
/// `catch_unwind` guards against panics inside rawler.
#[hotpath::measure]
pub fn rawler_full_image_from_bytes(bytes: &[u8], max_px: u32) -> Option<DecodedImage> {
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
            .map(|code| exif_code_from_rawler_orientation(rawler::Orientation::from_u16(code)))
            .unwrap_or(1);

        let (nw, nh) = fit_within(w, h, max_px);
        let rgba = if (nw, nh) == (w, h) {
            img.into_raw()
        } else {
            image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
        };
        Some(apply_exif_orientation(
            DecodedImage::new_tracked(DecodedImageFields {
                width: nw,
                height: nh,
                rgba,
                pixel_format: PixelFormat::Srgb8,
            }),
            orientation,
        ))
    });
    std::panic::catch_unwind(run).ok().flatten()
}
