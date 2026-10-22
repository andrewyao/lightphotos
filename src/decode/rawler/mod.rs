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

/// rawler allocates its raw buffer at whatever size the file claims. 0.7.2
/// panicked past 50,000 samples on a side or 500 million in all; 0.8.0 drops
/// that check and tries the allocation, which wraps or aborts on wasm32 and
/// strands everything the decode thread held. rawler counts in samples,
/// padded to whole tiles, so the cap here is 500 million of them (1 GB of
/// u16), checked before rawler runs. A long side alone is fine: a 40,000 px
/// panorama decodes.
pub fn check_rawler_size_limit(source: &rawler::rawsource::RawSource) -> Result<(), String> {
    let Some(RawIfdSize {
        width,
        height,
        cpp,
        row,
        rows,
    }) = raw_ifd_size(source)
    else {
        return Ok(());
    };
    let row = row.saturating_mul(cpp);
    if width == 0 || height == 0 || row.saturating_mul(rows) > 500_000_000 {
        return Err(format!(
            "{width}x{height} with {cpp} samples per pixel is too large to decode"
        ));
    }
    Ok(())
}

/// The raw image's size as its TIFF tags give it, before rawler decodes.
struct RawIfdSize {
    width: usize,
    height: usize,
    cpp: usize,
    /// `width` and `height` padded to whole tiles.
    row: usize,
    rows: usize,
}

/// The raw IFD's size tags, or `None` for a format without one (CR3) or
/// tags that are missing.
fn raw_ifd_size(source: &rawler::rawsource::RawSource) -> Option<RawIfdSize> {
    use rawler::decoders::WellKnownIFD;
    use rawler::tags::TiffCommonTag;

    let ifd = rawler::get_decoder(source)
        .ok()?
        .ifd(WellKnownIFD::Raw)
        .ok()
        .flatten()?;
    let tag = |t| ifd.get_entry(t).map(|e| e.value.force_usize(0));
    let (width, height) = (
        tag(TiffCommonTag::ImageWidth)?,
        tag(TiffCommonTag::ImageLength)?,
    );
    Some(RawIfdSize {
        width,
        height,
        cpp: tag(TiffCommonTag::SamplesPerPixel).unwrap_or(1).max(1),
        row: padded_to_tiles(width, tag(TiffCommonTag::TileWidth)),
        rows: padded_to_tiles(height, tag(TiffCommonTag::TileLength)),
    })
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

/// rawler's `apply_scaling` panics on a Linear DNG whose black level
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
/// balance, color matrix, crop) to `LinearF16`, then applies EXIF orientation.
/// There is no per-camera profile; color comes from `raw.color_matrix`.
///
/// The vendored rawler's develop returns an error, not a panic, for the
/// layouts it cannot take (`patches/rawler-develop-errors.patch`).
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac(
    path: &Path,
    max_dim: u32,
    intent: RawIntent,
) -> Result<DecodedImage, String> {
    // Memory-mapped, so the file is never copied onto the heap.
    let source = rawler::rawsource::RawSource::new(path).map_err(|e| e.to_string())?;
    decode_raw_nonmac_from_source(source, max_dim, intent)
}

/// What a RAW develop is for. Only a display decode may bin 2x2 when it is
/// shrunk at least 2x: the superpixel's channels sit half a pixel apart, so
/// an export at a reduced size keeps PPG and a downscale.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawIntent {
    /// Thumbnails, previews, the Loupe and scoring.
    Display,
    Export,
}

/// [`decode_raw_nonmac`] for an in-memory file, for scoring and the
/// `decode_probe` harness, since a browser has no path to open. The parity
/// test in `decode_probe` checks both produce identical bytes.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac_from_bytes(
    bytes: &[u8],
    max_dim: u32,
    intent: RawIntent,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    decode_raw_nonmac_from_source(source, max_dim, intent)
}

/// Like [`decode_raw_nonmac_from_bytes`], but takes ownership of the buffer
/// so rawler can use it without a copy. Export uses this.
#[allow(dead_code)]
#[hotpath::measure]
pub fn decode_raw_nonmac_from_shared_vec(
    bytes: std::sync::Arc<Vec<u8>>,
    max_dim: u32,
    intent: RawIntent,
) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_shared_vec(bytes);
    decode_raw_nonmac_from_source(source, max_dim, intent)
}

#[hotpath::measure]
fn decode_raw_nonmac_from_source(
    source: rawler::rawsource::RawSource,
    max_dim: u32,
    intent: RawIntent,
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

    develop_raw_image_to_linear_f16(&mut raw, orientation, max_dim, intent)
}

/// `RawDevelop`'s default steps without the last, `SRgb`. rawler runs its
/// steps on its own one-thread pool in the browser, and that gamma step's
/// `powf` per channel was most of a 24 MP develop. `raw_shader.wgsl` applies
/// the gamma on the GPU instead.
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

/// What `RawDevelop` runs on a [`half_res_image`], which is already scaled,
/// demosaiced and cropped to its active area.
const HALF_RES_DEVELOP_STEPS: [rawler::imgop::develop::ProcessingStep; 3] = {
    use rawler::imgop::develop::ProcessingStep::*;
    [WhiteBalance, Calibrate, CropDefault]
};

/// `raw` binned to half size over its area by [`raw_preview::bin_bayer`],
/// as a linear three-color image `RawDevelop` develops without its
/// demosaic, when the shown size (the default crop) is at least twice
/// `max_dim` (`half_res_is_enough`). PPG's full-size result would be
/// box-shrunk at least 2x straight away, so the detail it adds is lost, and
/// it costs 4x the demosaic work and memory. `None`, with `raw` untouched,
/// for a larger output, anything [`raw_preview::bayer_mosaic`] does not
/// take, or a Fuji rotated sensor, whose rotation step only runs inside
/// `RawDevelop`'s own demosaic.
///
/// `RawDevelop::develop_intermediate` clones the image it is given and then
/// copies it into its own layout, so a quarter-size buffer is held three
/// times while it runs; its API leaves no way to hand it ours.
fn half_res_image(
    raw: &mut rawler::RawImage,
    max_dim: u32,
) -> Result<Option<rawler::RawImage>, String> {
    if raw.fuji_rotation_width.is_some() {
        return Ok(None);
    }
    let Some(mosaic) = raw_preview::bayer_mosaic(raw) else {
        return Ok(None);
    };
    if !raw_preview::half_res_is_enough(mosaic.shown_size(), max_dim) {
        return Ok(None);
    }
    raw.apply_scaling().map_err(|e| e.to_string())?;
    let rawler::RawImageData::Float(data) = &mut raw.data else {
        return Err("RAW scaling left integer samples".to_string());
    };
    // `bayer_mosaic` checked there are at least this many; a longer buffer
    // is cut, as the vendored `RawDevelop` cuts it, since `Pix2D` holds
    // exactly its size.
    let mut data = std::mem::take(data);
    data.truncate(raw.width * raw.height);
    let pixels = rawler::pixarray::Pix2D::new_with(data, raw.width, raw.height);
    let binned = raw_preview::bin_bayer(&pixels, &mosaic);
    drop(pixels);
    let (width, height) = (binned.width, binned.height);
    // The binned image starts at the area and is half its size.
    let crop = mosaic
        .crop
        .and_then(|crop| raw_preview::crop_in_demosaiced(crop, mosaic.area, true, width, height));

    // `raw.data` is empty now, so this copies only the metadata.
    let mut half = raw.clone();
    half.data = rawler::RawImageData::Float(binned.into_inner().into_flattened());
    half.width = width;
    half.height = height;
    half.cpp = 3;
    half.photometric = rawler::rawimage::RawPhotometricInterpretation::LinearRaw;
    half.active_area = None;
    half.crop_area = crop;
    Ok(Some(half))
}

/// A linear `RawDevelop` result as RGB with every channel finite, at least
/// 0, and at most the largest finite f16. Values above 1.0 stay: they are the
/// highlight headroom the GPU tone pass recovers. A four-color result drops
/// its fourth channel. `None` for a buffer short of its size.
///
/// Consumes `developed` so a full-size image is never held twice: a
/// three-color buffer is clamped in place, and the other two are converted
/// in one pass that frees the source when it ends.
fn linear_rgb(
    developed: rawler::imgop::develop::Intermediate,
) -> Option<(usize, usize, Vec<[f32; 3]>)> {
    use rawler::imgop::develop::Intermediate;
    let sized = |w: usize, h: usize, len: usize| w.checked_mul(h) == Some(len);
    match developed {
        Intermediate::Monochrome(p) if sized(p.width, p.height, p.data.len()) => {
            let rgb = p.data.iter().map(|&v| [f16_range(v); 3]).collect();
            Some((p.width, p.height, rgb))
        }
        Intermediate::ThreeColor(mut p) if sized(p.width, p.height, p.data.len()) => {
            p.data.iter_mut().for_each(|px| *px = px.map(f16_range));
            Some((p.width, p.height, p.data))
        }
        Intermediate::FourColor(p) if sized(p.width, p.height, p.data.len()) => {
            let rgb = p
                .data
                .iter()
                .map(|&[r, g, b, _]| [r, g, b].map(f16_range))
                .collect();
            Some((p.width, p.height, rgb))
        }
        _ => None,
    }
}

/// `v` clamped to what a `LinearF16` channel holds: non-finite and negative
/// become 0, and anything past f16's range its largest finite value rather
/// than infinity.
fn f16_range(v: f32) -> f32 {
    if v.is_finite() {
        v.clamp(0.0, half::f16::MAX.to_f32())
    } else {
        0.0
    }
}

/// Develops a decoded RAW to `LinearF16`: `RawDevelop` without its gamma
/// step, `max_dim` downscale, auto-denoise, then EXIF orientation. The sRGB
/// gamma and display boost are left to `raw_shader.wgsl`.
#[hotpath::measure]
fn develop_raw_image_to_linear_f16(
    raw: &mut rawler::RawImage,
    orientation: u8,
    max_dim: u32,
    intent: RawIntent,
) -> Result<DecodedImage, String> {
    use rawler::imgop::develop::RawDevelop;
    let side = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
    check_decode_size(side(raw.width), side(raw.height))?;
    let half = match intent {
        RawIntent::Display => half_res_image(raw, max_dim)?,
        RawIntent::Export => None,
    };
    let developed = match half {
        Some(half) => RawDevelop::new_with(&HALF_RES_DEVELOP_STEPS).develop_intermediate(&half),
        None => RawDevelop::new_with(&LINEAR_DEVELOP_STEPS).develop_intermediate(raw),
    }
    .map_err(|e| e.to_string())?;
    let (src_w, src_h, mut linear) =
        linear_rgb(developed).ok_or("rawler produced an empty developed image")?;
    if src_w == 0 || src_h == 0 {
        return Err("decoded RAW image has zero dimension".into());
    }
    let (w, h) = fit_within(side(src_w), side(src_h), max_dim);
    let (dw, dh) = (w as usize, h as usize);

    // Shrink before denoising, so the denoise reads a preview-sized buffer
    // rather than a sensor-sized one.
    if (dw, dh) != (src_w, src_h) {
        linear = raw_preview::box_downsample_rgb(linear, src_w, src_h, dw, dh);
    }

    let mut rgba = vec![
        0u8;
        dw.checked_mul(dh)
            .and_then(|n| n.checked_mul(8))
            .ok_or("RAW image too large")?
    ];
    let one = half::f16::ONE.to_le_bytes();
    crate::develop::parallel::for_each_row(&mut rgba, dw * 8, |y, row| {
        for (x, px) in row.as_chunks_mut::<8>().0.iter_mut().enumerate() {
            let lin = crate::develop::denoise_linear_rgb_pixel(
                AUTO_RAW_DENOISE_STRENGTH,
                dw,
                dh,
                &linear,
                x,
                y,
            );
            for (c, v) in lin.into_iter().enumerate() {
                px[c * 2..c * 2 + 2]
                    .copy_from_slice(&half::f16::from_f32(f16_range(v)).to_le_bytes());
            }
            px[6..8].copy_from_slice(&one);
        }
    });
    // Before orienting, which copies `rgba` once more.
    drop(linear);

    Ok(apply_exif_orientation(
        DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::LinearF16,
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

        // One grant for the camera JPEG's decode inside rawler and the fit
        // after it, taken first. The JPEG is about the sensor's size; a
        // format without size tags (CR3) is guessed at a pixel a file byte,
        // which a RAW's raw data alone exceeds.
        #[cfg(target_arch = "wasm32")]
        let _grant = {
            let side = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
            let size = raw_ifd_size(&source).map_or_else(
                || {
                    let side_px = side((bytes.len() as f64).sqrt() as usize);
                    (side_px, side_px)
                },
                |s| (side(s.width), side(s.height)),
            );
            crate::decode::decode_budget::acquire(crate::decode::image_decode::decoded_grant_bytes(
                size, max_px,
            ))
        };
        let dynamic = decoder.full_image(&source, &params).ok().flatten()?;

        // Take orientation from the RAW metadata, as `decode_raw_nonmac`
        // does. The embedded image's own EXIF may lack it.
        let orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|meta| meta.exif.orientation)
            .map(|code| exif_code_from_rawler_orientation(rawler::Orientation::from_u16(code)))
            .unwrap_or(1);
        let orientation = image::metadata::Orientation::from_exif(orientation)
            .unwrap_or(image::metadata::Orientation::NoTransforms);
        let img = crate::decode::image_decode::fit_decoded(dynamic, max_px, orientation).ok()?;
        Some(DecodedImage::new_tracked(DecodedImageFields {
            width: img.width(),
            height: img.height(),
            rgba: img.into_raw(),
            pixel_format: PixelFormat::Srgb8,
        }))
    });
    std::panic::catch_unwind(run).ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::{fuji_dynamic_range_gain, linear_rgb, padded_to_tiles};
    use rawler::imgop::develop::Intermediate;
    use rawler::pixarray::{Color2D, PixF32};

    use super::{develop_raw_image_to_linear_f16, RawIntent};

    /// A display develop, which may bin.
    fn develop(
        raw: &mut rawler::RawImage,
        max_dim: u32,
    ) -> Result<crate::decode::image_decode::DecodedImage, String> {
        develop_raw_image_to_linear_f16(raw, 1, max_dim, RawIntent::Display)
    }
    use rawler::imgop::{Dim2, Point, Rect};

    /// A `w` x `h` 12-bit Bayer image of `pattern`, every sample `v`.
    fn bayer(w: usize, h: usize, pattern: &str, v: u16) -> rawler::RawImage {
        let cfa = rawler::CFA::new(pattern);
        let colors = rawler::cfa::PlaneColor::new("RGB");
        rawler::RawImage::new(
            rawler::decoders::Camera::new(),
            rawler::pixarray::PixU16::new_with(vec![v; w * h], w, h),
            1,
            [1.0; 4],
            rawler::rawimage::RawPhotometricInterpretation::Cfa(rawler::rawimage::CFAConfig::new(
                &cfa, &colors,
            )),
            Some(rawler::rawimage::BlackLevel::new(&[0u16], 1, 1, 1)),
            Some(rawler::rawimage::WhiteLevel::new(vec![4095])),
            false,
        )
    }

    fn rect(x: usize, y: usize, w: usize, h: usize) -> Rect {
        Rect::new(Point::new(x, y), Dim2::new(w, h))
    }

    /// Smallest and largest red channel, as f32.
    fn red_range(img: &crate::decode::image_decode::DecodedImage) -> (f32, f32) {
        img.rgba
            .as_chunks::<8>()
            .0
            .iter()
            .map(|px| half::f16::from_le_bytes([px[0], px[1]]).to_f32())
            .fold((f32::MAX, f32::MIN), |(lo, hi), v| (lo.min(v), hi.max(v)))
    }

    #[test]
    fn a_raw_too_small_or_short_of_its_samples_is_refused() {
        for (w, h) in [(1, 8), (8, 1)] {
            let mut raw = bayer(w, h, "RGGB", 1000);
            assert!(develop(&mut raw, u32::MAX).is_err());
        }
        let mut raw = bayer(8, 8, "RGGB", 1000);
        raw.width = 0;
        assert!(develop(&mut raw, u32::MAX).is_err());
        let mut raw = bayer(8, 8, "RGGB", 1000);
        raw.height = 9;
        assert!(develop(&mut raw, u32::MAX).is_err());
    }

    /// The vendored `RawDevelop` returns an error, not a panic, for layouts
    /// it cannot take.
    #[test]
    fn an_unsupported_layout_is_refused_instead_of_reaching_rawler() {
        let mut raw = bayer(8, 8, "RRRR", 1000);
        assert!(develop(&mut raw, u32::MAX).is_err());
        let mut raw = bayer(8, 8, "RGGB", 1000);
        raw.active_area = Some(rect(4, 4, 8, 8));
        assert!(develop(&mut raw, u32::MAX).is_err());
        let mut raw = bayer(8, 8, "RGGB", 1000);
        raw.photometric = rawler::rawimage::RawPhotometricInterpretation::BlackIsZero;
        assert!(develop(&mut raw, u32::MAX).is_err());
    }

    /// An odd sensor with an odd active-area origin and a default crop: the
    /// full develop and the half-size one frame it alike, and neither shows
    /// its last row or column, which the vendored `apply_scaling` now
    /// scales, as a bright edge.
    #[test]
    fn the_full_and_half_develops_agree_on_an_odd_sensor_and_its_crop() {
        let odd = || {
            let mut raw = bayer(33, 25, "RGGB", 1000);
            raw.active_area = Some(rect(1, 1, 32, 24));
            raw.crop_area = Some(rect(3, 3, 24, 16));
            raw
        };
        let full = develop(&mut odd(), u32::MAX).unwrap();
        assert_eq!((full.width, full.height), (24, 16));
        let small = develop(&mut odd(), 8).unwrap();
        let half = develop(&mut odd(), 12).unwrap();
        assert_eq!((half.width, half.height), (12, 8), "binned, then cropped");
        assert_eq!((small.width, small.height), (8, 5));
        for img in [&full, &half, &small] {
            let (lo, hi) = red_range(img);
            assert!(
                lo > 0.0 && hi <= lo * 1.05,
                "flat field, no edge: {lo}..{hi}"
            );
        }
        let (full_lo, _) = red_range(&full);
        let (half_lo, _) = red_range(&half);
        assert!(
            (full_lo - half_lo).abs() <= 0.02 * full_lo,
            "{full_lo} vs {half_lo}"
        );
    }

    /// A default crop reaching past the active area is bad metadata. Both
    /// develops clamp it to the area alike, rather than one cropping, the
    /// other not, or `Rect::adapt` asserting.
    #[test]
    fn a_crop_past_the_area_is_clamped_by_both_develops() {
        let bad = || {
            let mut raw = bayer(40, 24, "RGGB", 1000);
            raw.crop_area = Some(rect(30, 0, 20, 24));
            raw
        };
        let full = develop(&mut bad(), u32::MAX).unwrap();
        assert_eq!((full.width, full.height), (10, 24));
        let shrunk = develop_raw_image_to_linear_f16(&mut bad(), 1, 12, RawIntent::Export).unwrap();
        let half = develop(&mut bad(), 12).unwrap();
        assert_eq!((half.width, half.height), (5, 12), "binned, then cropped");
        assert_eq!((shrunk.width, shrunk.height), (5, 12));
    }

    /// rawler clears the active area of a rotated Fuji sensor, and the
    /// half-size path leaves such a file, and its area, alone, so the full
    /// develop does not meet an area it cannot rotate.
    #[test]
    fn a_rotated_fuji_sensor_is_left_to_the_full_develop_untouched() {
        let mut raw = bayer(16, 16, "RGGB", 1000);
        raw.fuji_rotation_width = Some(8);
        assert!(super::half_res_image(&mut raw, 2).unwrap().is_none());
        assert!(raw.active_area.is_none());
        assert!(
            matches!(raw.data, rawler::RawImageData::Integer(_)),
            "not scaled"
        );
    }

    /// A buffer longer than its size bins rather than panicking in
    /// `Pix2D::new_with`, which wants exactly its size.
    #[test]
    fn surplus_samples_bin_without_panicking() {
        let mut raw = bayer(40, 24, "RGGB", 1000);
        if let rawler::RawImageData::Integer(data) = &mut raw.data {
            data.extend([1000; 7]);
        }
        let img = develop(&mut raw, 20).unwrap();
        assert_eq!((img.width, img.height), (20, 12));
    }

    /// An export at a reduced size keeps PPG: the superpixel's channels sit
    /// half a pixel apart.
    #[test]
    fn only_a_display_decode_bins() {
        let mut raw = bayer(40, 24, "RGGB", 1000);
        let mut copy = raw.clone();
        assert!(super::half_res_image(&mut raw, 20).unwrap().is_some());
        let export = develop_raw_image_to_linear_f16(&mut copy, 1, 20, RawIntent::Export).unwrap();
        assert_eq!((export.width, export.height), (20, 12));
        assert!(
            matches!(copy.data, rawler::RawImageData::Integer(_)),
            "never binned"
        );
    }

    #[test]
    fn linear_rgb_clamps_to_what_f16_holds_and_keeps_headroom() {
        let rgb = Intermediate::ThreeColor(Color2D::new_with(
            vec![[f32::NAN, -0.5, 2.5], [f32::INFINITY, 1e9, 0.25]],
            2,
            1,
        ));
        let (w, h, px) = linear_rgb(rgb).unwrap();
        assert_eq!((w, h), (2, 1));
        assert_eq!(px, vec![[0.0, 0.0, 2.5], [0.0, 65504.0, 0.25]]);
        let gray = Intermediate::Monochrome(PixF32::new_with(vec![0.5], 1, 1));
        assert_eq!(linear_rgb(gray).unwrap().2, vec![[0.5; 3]]);
    }

    #[test]
    fn linear_rgb_refuses_a_buffer_short_of_its_size() {
        let short = Intermediate::ThreeColor(Color2D {
            width: 4,
            height: 4,
            data: vec![[0.5; 3]; 15],
        });
        assert!(linear_rgb(short).is_none());
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
