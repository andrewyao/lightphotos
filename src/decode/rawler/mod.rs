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

/// rawler allocates its raw buffer at whatever size the file claims. 0.7.2
/// panicked past 50,000 samples on a side or 500 million in all; 0.8.0 drops
/// that check and tries the allocation. Either is fatal to a wasm32 decode
/// thread and strands everything the thread held, so a file past that limit
/// is refused here first. rawler counts a row in samples, padded to whole
/// tiles, so a 3-channel DNG wider than 16,666 px (a Lightroom panorama) is
/// past it.
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
    let cpp = tag(TiffCommonTag::SamplesPerPixel).unwrap_or(1).max(1);
    let row = padded_to_tiles(width, tag(TiffCommonTag::TileWidth)).saturating_mul(cpp);
    let rows = padded_to_tiles(height, tag(TiffCommonTag::TileLength));
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

/// `n` rounded up to whole tiles of `tile`. Saturating, because both come
/// from tags and a wrapped product on wasm32's 32-bit `usize` would slip a
/// huge image past [`check_rawler_size_limit`].
fn padded_to_tiles(n: usize, tile: Option<usize>) -> usize {
    match tile {
        Some(t) if t > 0 => n.div_ceil(t).saturating_mul(t),
        _ => n,
    }
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
    if cpp == 0 || !black.len().is_multiple_of(cpp) || !(white.len() == 1 || white.len() == cpp) {
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

/// `RawExposureBias` at DR100, the headroom every Fujifilm RAF carries. The
/// RAW look curve already covers it, as it covers other makers' headroom.
const FUJI_DR100_BIAS_EV: f32 = 0.72;

/// The linear gain that undoes a Fujifilm DR200/DR400 setting, which
/// underexposes the raw by 1 or 2 stops for highlight room. Apple's RAW
/// engine applies about the same gain. `None` when the file is not a RAF,
/// was shot at DR100, or has no readable bias.
///
/// rawler skips the RAF header's `RawExposureBias` tag (0x9650), so this reads
/// it directly: offset 92 holds a big-endian pointer to a tag directory, a
/// `u32` count, then `u16` tag, `u16` length and data per entry. The bias is
/// two big-endian `i16`s, numerator and denominator, in EV.
pub fn fuji_dynamic_range_gain(file: &[u8]) -> Option<f32> {
    const MAX_ENTRIES: u32 = 256;
    const MAX_EXTRA_EV: f32 = 3.0;
    let be_u16 = |at: usize| {
        Some(u16::from_be_bytes(
            file.get(at..at.checked_add(2)?)?.try_into().ok()?,
        ))
    };
    let be_u32 = |at: usize| {
        Some(u32::from_be_bytes(
            file.get(at..at.checked_add(4)?)?.try_into().ok()?,
        ))
    };

    if file.get(..16)? != b"FUJIFILMCCD-RAW " {
        return None;
    }
    let dir = usize::try_from(be_u32(92)?).ok()?;
    let count = be_u32(dir)?;
    if count > MAX_ENTRIES {
        return None;
    }
    let mut at = dir.checked_add(4)?;
    for _ in 0..count {
        let tag = be_u16(at)?;
        let len = usize::from(be_u16(at.checked_add(2)?)?);
        let data = at.checked_add(4)?;
        if tag == 0x9650 && len >= 4 {
            let num = f32::from(be_u16(data)? as i16);
            let den = f32::from(be_u16(data.checked_add(2)?)? as i16);
            if den <= 0.0 {
                return None;
            }
            let extra_ev = (-num / den - FUJI_DR100_BIAS_EV).clamp(0.0, MAX_EXTRA_EV);
            return (extra_ev > 0.05).then(|| extra_ev.exp2());
        }
        at = data.checked_add(len)?;
    }
    None
}

/// Folds [`fuji_dynamic_range_gain`] into `raw.wb_coeffs`. Both RAW develop
/// paths multiply by them in linear light, before the color matrix and the
/// highlight clip, so the gain reaches every pixel exactly once.
pub fn apply_fuji_dynamic_range_gain(raw: &mut rawler::RawImage, file: &[u8]) {
    let Some(gain) = fuji_dynamic_range_gain(file) else {
        return;
    };
    let wb = &mut raw.wb_coeffs;
    if wb[..3].iter().all(|c| c.is_finite()) {
        wb[..3].iter_mut().for_each(|c| *c *= gain);
    } else {
        // Both paths read non-finite coefficients as neutral.
        *wb = [gain, gain, gain, wb[3]];
    }
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
    // Memory-mapped, so the file is never copied onto the heap.
    let source = rawler::rawsource::RawSource::new(path).map_err(|e| e.to_string())?;
    decode_raw_nonmac_from_source(source, max_dim)
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
    apply_fuji_dynamic_range_gain(&mut raw, source.buf());
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

/// `RawDevelop`'s default steps without the last, `SRgb`. rawler runs its
/// steps on its own one-thread pool in the browser, and that gamma step's
/// `powf` per channel was most of a 24 MP develop. [`display_rgba8`] does it
/// in parallel instead.
const LINEAR_DEVELOP_STEPS: [rawler::imgop::develop::ProcessingStep; 7] = {
    use rawler::imgop::develop::ProcessingStep::*;
    [
        Rescale,
        Demosaic,
        FujiRotate,
        CropActiveArea,
        WhiteBalance,
        Calibrate,
        CropDefault,
    ]
};

/// One linear channel as rawler's `SRgb` step, `to_dynamic_image` and
/// `into_rgba8` together made it: gamma, scaled to 16 bits, then to 8.
fn rawler_srgb8(v: f32) -> u8 {
    let u = (rawler::imgop::srgb::srgb_apply_gamma(v) * f32::from(u16::MAX)) as u16;
    let one = image::ImageBuffer::from_pixel(1, 1, image::Rgb([u, u, u]));
    image::DynamicImage::ImageRgb16(one).into_rgba8().into_raw()[0]
}

/// [`rawler_srgb8`] by a search of 255 steps instead of a `powf`. Its
/// output only rises with its input, so each step is the smallest input
/// reaching that output, found by bisection over the float's bits. Bit for
/// bit the same for every input, NaN and the infinities included.
fn linear_to_rawler_srgb8(v: f32) -> u8 {
    static STEPS: std::sync::OnceLock<[f32; 255]> = std::sync::OnceLock::new();
    let steps = STEPS.get_or_init(|| {
        std::array::from_fn(|i| {
            let k = i as u8 + 1;
            // rawler_srgb8(2.0) is 255, so every step lies below it.
            let (mut lo, mut hi) = (0u32, 2.0f32.to_bits());
            while lo < hi {
                let mid = lo + (hi - lo) / 2;
                if rawler_srgb8(f32::from_bits(mid)) >= k {
                    hi = mid;
                } else {
                    lo = mid + 1;
                }
            }
            f32::from_bits(lo)
        })
    });
    steps.partition_point(|&step| step <= v) as u8
}

/// A linear `RawDevelop` result as display sRGB8 RGBA: what rawler's `SRgb`
/// step and `to_dynamic_image().into_rgba8()` made of it, then the display
/// boost, a band of rows per thread. `None` for a buffer short of its size,
/// which `to_dynamic_image` also refuses.
fn display_rgba8(developed: &rawler::imgop::develop::Intermediate) -> Option<image::RgbaImage> {
    use rawler::imgop::develop::Intermediate;
    let boost: [u8; 256] =
        std::array::from_fn(|i| (apply_raw_preview_boost(i as f32 / 255.0) * 255.0).round() as u8);
    let shown = |v: f32| boost[linear_to_rawler_srgb8(v) as usize];
    let (w, h, len) = match developed {
        Intermediate::Monochrome(p) => (p.width, p.height, p.data.len()),
        Intermediate::ThreeColor(p) => (p.width, p.height, p.data.len()),
        Intermediate::FourColor(p) => (p.width, p.height, p.data.len()),
    };
    if w.checked_mul(h)? != len {
        return None;
    }
    let mut rgba = vec![0u8; len.checked_mul(4)?];
    let fill = |rgba: &mut [u8], px: &(dyn Fn(usize) -> Option<[u8; 4]> + Sync)| {
        crate::develop::parallel::for_each_row(rgba, w * 4, |y, row| {
            for (x, out) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                if let Some(p) = px(y * w + x) {
                    *out = p;
                }
            }
        });
    };
    match developed {
        Intermediate::Monochrome(p) => fill(&mut rgba, &|i| {
            let v = shown(*p.data.get(i)?);
            Some([v, v, v, 255])
        }),
        Intermediate::ThreeColor(p) => fill(&mut rgba, &|i| {
            let [r, g, b] = *p.data.get(i)?;
            Some([shown(r), shown(g), shown(b), 255])
        }),
        // The fourth channel becomes alpha, as `to_dynamic_image` made it.
        Intermediate::FourColor(p) => fill(&mut rgba, &|i| {
            let [r, g, b, a] = *p.data.get(i)?;
            Some([shown(r), shown(g), shown(b), linear_to_rawler_srgb8(a)])
        }),
    }
    image::RgbaImage::from_raw(u32::try_from(w).ok()?, u32::try_from(h).ok()?, rgba)
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
    let developed = rawler::imgop::develop::RawDevelop::new_with(&LINEAR_DEVELOP_STEPS)
        .develop_intermediate(raw)
        .map_err(|e| e.to_string())?;
    let mut img = display_rgba8(&developed).ok_or("rawler produced an empty developed image")?;

    let (src_w, src_h) = (img.width(), img.height());
    if src_w == 0 || src_h == 0 {
        return Err("decoded RAW image has zero dimension".into());
    }
    let (w, h) = fit_within(src_w, src_h, max_dim);

    // Shrink before denoising, so the f32 buffer below is preview-sized
    // rather than sensor-sized (about 290 MB less on a 24 MP file).
    if (w, h) != (src_w, src_h) {
        img = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
    }

    // `RawDevelop` has no mid-pipeline hook, so denoise runs on the finished
    // sRGB8 image, decoded to the linear light the denoise expects and
    // encoded back in place, a band of rows per thread.
    let (dw, dh) = (img.width() as usize, img.height() as usize);
    let lut = crate::develop::image_ops::srgb8_to_linear_lut();
    let linear: Vec<[f32; 3]> = img
        .pixels()
        .map(|p| [p[0], p[1], p[2]].map(|c| lut[c as usize]))
        .collect();
    let encode = crate::develop::image_ops::linear_to_srgb8;
    crate::develop::parallel::for_each_row(&mut img, dw * 4, |y, row| {
        for (x, px) in row.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let lin = crate::develop::denoise_linear_rgb_pixel(
                AUTO_RAW_DENOISE_STRENGTH,
                dw,
                dh,
                &linear,
                x,
                y,
            );
            px[0] = encode(lin[0]);
            px[1] = encode(lin[1]);
            px[2] = encode(lin[2]);
        }
    });

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

#[cfg(test)]
mod tests {
    use super::{
        apply_raw_preview_boost, display_rgba8, fuji_dynamic_range_gain, linear_to_rawler_srgb8,
        padded_to_tiles, rawler_srgb8,
    };
    use rawler::imgop::develop::Intermediate;
    use rawler::pixarray::{Color2D, PixF32};

    #[test]
    fn the_srgb8_search_matches_rawlers_curve_for_every_input() {
        let specials = [
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
            -0.0,
            0.0,
            1.0,
            1e30,
        ];
        let sweep = (0..=u32::MAX).step_by(65_537).map(f32::from_bits);
        for v in specials.into_iter().chain(sweep) {
            assert_eq!(linear_to_rawler_srgb8(v), rawler_srgb8(v), "{v:e}");
        }
    }

    /// The parallel pass gives the bytes rawler's own `SRgb` step and
    /// `to_dynamic_image().into_rgba8()` gave before it, boost and all.
    #[test]
    fn display_rgba8_matches_rawlers_srgb_step_and_conversion() {
        let (w, h) = (37, 41);
        let values: Vec<f32> = (0..w * h * 3)
            .map(|i| (i as f32 * 0.618_034).fract() * 1.2 - 0.1)
            .collect();
        let rgb: Vec<[f32; 3]> = values.as_chunks::<3>().0.to_vec();
        let mut gamma = Color2D::new_with(rgb.clone(), w, h);
        gamma.for_each(rawler::imgop::srgb::srgb_apply_gamma_n);
        let mut gray = PixF32::new_with(values[..w * h].to_vec(), w, h);
        gray.for_each(rawler::imgop::srgb::srgb_apply_gamma);

        let boost = |c: u8| (apply_raw_preview_boost(f32::from(c) / 255.0) * 255.0).round() as u8;
        for (linear, encoded) in [
            (
                Intermediate::ThreeColor(Color2D::new_with(rgb, w, h)),
                Intermediate::ThreeColor(gamma),
            ),
            (
                Intermediate::Monochrome(PixF32::new_with(values[..w * h].to_vec(), w, h)),
                Intermediate::Monochrome(gray),
            ),
        ] {
            let mut old = encoded.to_dynamic_image().unwrap().into_rgba8();
            for px in old.pixels_mut() {
                for c in 0..3 {
                    px[c] = boost(px[c]);
                }
            }
            assert_eq!(display_rgba8(&linear).unwrap(), old);
        }
    }

    #[test]
    fn display_rgba8_refuses_a_buffer_short_of_its_size() {
        let short = Intermediate::ThreeColor(Color2D {
            width: 4,
            height: 4,
            data: vec![[0.5; 3]; 15],
        });
        assert!(display_rgba8(&short).is_none());
    }

    #[test]
    fn tile_padding_of_a_huge_tag_saturates_instead_of_wrapping() {
        assert_eq!(padded_to_tiles(100, Some(64)), 128);
        assert_eq!(padded_to_tiles(100, Some(0)), 100);
        assert_eq!(padded_to_tiles(usize::MAX, Some(2)), usize::MAX);
    }

    /// A RAF header whose tag directory sits at 100 and holds a 0x0100 entry
    /// ahead of `RawExposureBias` = `num`/`den` EV.
    fn raf_with_bias(num: i16, den: i16) -> Vec<u8> {
        let mut f = b"FUJIFILMCCD-RAW ".to_vec();
        f.resize(92, 0);
        f.extend_from_slice(&100u32.to_be_bytes());
        f.resize(100, 0);
        f.extend_from_slice(&2u32.to_be_bytes());
        f.extend_from_slice(&[0x01, 0x00, 0x00, 0x04, 1, 2, 3, 4]);
        f.extend_from_slice(&[0x96, 0x50, 0x00, 0x04]);
        f.extend_from_slice(&num.to_be_bytes());
        f.extend_from_slice(&den.to_be_bytes());
        f
    }

    #[test]
    fn dr400_gains_two_stops_and_dr100_none() {
        let dr400 = fuji_dynamic_range_gain(&raf_with_bias(-272, 100)).unwrap();
        assert!((dr400 - 4.0).abs() < 1e-4, "{dr400}");
        let dr200 = fuji_dynamic_range_gain(&raf_with_bias(-172, 100)).unwrap();
        assert!((dr200 - 2.0).abs() < 1e-4, "{dr200}");
        assert_eq!(fuji_dynamic_range_gain(&raf_with_bias(-72, 100)), None);
    }

    #[test]
    fn a_malformed_raf_header_gives_no_gain_instead_of_panicking() {
        let full = raf_with_bias(-272, 100);
        for end in 0..full.len() {
            assert_eq!(
                fuji_dynamic_range_gain(&full[..end]),
                None,
                "truncated at {end}"
            );
        }
        assert_eq!(fuji_dynamic_range_gain(&raf_with_bias(-272, 0)), None);
        assert_eq!(fuji_dynamic_range_gain(&raf_with_bias(-272, -100)), None);
        let huge = fuji_dynamic_range_gain(&raf_with_bias(i16::MIN, 1)).unwrap();
        assert!((huge - 8.0).abs() < 1e-4, "capped at 3 stops: {huge}");

        let mut far = full.clone();
        far[92..96].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(fuji_dynamic_range_gain(&far), None);
        let mut many = full.clone();
        many[100..104].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(fuji_dynamic_range_gain(&many), None);
        let mut not_raf = full;
        not_raf[0] = b'X';
        assert_eq!(fuji_dynamic_range_gain(&not_raf), None);
    }
}
