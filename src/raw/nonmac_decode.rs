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
    apply_exif_orientation, fit_within, DecodedImage, DecodedImageFields, PixelFormat,
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
pub(crate) fn decode_raw_nonmac_from_shared_vec(
    bytes: std::sync::Arc<Vec<u8>>,
    max_dim: u32,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_shared_vec(bytes);
    decode_raw_nonmac_from_source(source, max_dim)
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
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
pub fn capture_time(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Metadata for `path`. Non-mac fills only `source_size`, in display
/// orientation (width and height swapped for EXIF 5..=8), to match [`decode`].
#[cfg(not(target_os = "macos"))]
pub fn read_metadata(path: &Path) -> crate::image_decode::ImageMetadata {
    let source_size = pixel_size(path).map(|(w, h)| {
        if matches!(orientation_of(path), 5..=8) {
            (h, w)
        } else {
            (w, h)
        }
    });
    crate::image_decode::ImageMetadata {
        source_size,
        ..crate::image_decode::ImageMetadata::default()
    }
}

/// Stored pixel dimensions, before EXIF orientation. Reads only the header.
#[cfg(not(target_os = "macos"))]
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
}
