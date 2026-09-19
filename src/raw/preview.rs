// SPDX-License-Identifier: GPL-3.0-or-later

//! RAW decode for the wasm32 worker, in two tiers picked by `DemosaicMode`.
//! Browsers have no ImageIO, so this runs after the embedded-preview
//! extractors fail. Native non-mac uses `decode_raw_nonmac` instead.
//!
//! - `Fast` (Grid thumbnails and the Loupe's first paint): quarter-res 2x2
//!   binning, baked to sRGB8 through a CPU lookup table. About 6x faster than
//!   full PPG demosaic in the original wasm benchmark.
//! - `Quality` (Loupe `Preview`/`Full`): full-res PPG demosaic, left in linear
//!   light as `PixelFormat::LinearF16`. `raw_shader.wgsl` applies gamma and
//!   the display boost on the GPU.
//!
//! The bytes entry points are also built under `raw-probe` so the
//! `decode_probe` binary (`raw/probe.rs`) can test them on macOS.

use crate::image_decode::{fit_within, DecodedImage, PixelFormat};

/// `Fast` is rawler's `Superpixel3Channel` (quarter-res 2x2 bin) with
/// `Srgb8` output. `Quality` is rawler's `PPGDemosaic` (full-res,
/// edge-directed) with `LinearF16` output.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DemosaicMode {
    Fast,
    Quality,
}

impl DemosaicMode {
    /// 4 for `Fast` (u8 sRGB RGBA), 8 for `Quality` (f16 linear RGBA).
    fn bytes_per_pixel(self) -> usize {
        match self {
            DemosaicMode::Fast => 4,
            DemosaicMode::Quality => 8,
        }
    }
}

/// Linear to display u8 lookup table: rawler's sRGB curve, then
/// `apply_raw_preview_boost`. Replacing three per-pixel curve evaluations
/// with a table lookup gave most of the `Fast` tier's measured speedup.
const GAMMA_LUT_SIZE: usize = 4097;
static GAMMA_LUT: std::sync::OnceLock<[u8; GAMMA_LUT_SIZE]> = std::sync::OnceLock::new();

fn to_srgb_u8(v: f32) -> u8 {
    let lut = GAMMA_LUT.get_or_init(|| {
        let mut table = [0u8; GAMMA_LUT_SIZE];
        for (i, entry) in table.iter_mut().enumerate() {
            let linear = i as f32 / (GAMMA_LUT_SIZE - 1) as f32;
            let srgb = rawler::imgop::srgb::srgb_apply_gamma(linear);
            *entry = (crate::image_decode::apply_raw_preview_boost(srgb) * 255.0).round() as u8;
        }
        table
    });
    let idx = (v.clamp(0.0, 1.0) * (GAMMA_LUT_SIZE - 1) as f32).round() as usize;
    lut[idx]
}

/// Decodes RAW bytes to the `Fast` sRGB8 preview, Lanczos3-resized to fit
/// `max_px`. The `decode_probe` golden-hash tests call it by name.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_fast_from_bytes(
    bytes: &[u8],
    max_px: u32,
) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Fast)
}

/// Decodes RAW bytes to the `Quality` preview: linear `LinearF16`, resized to
/// fit `max_px`. The bound matters: the Loupe keeps its `zoom` when a sharper
/// tier of the same photo lands (`app/thumbs.rs::upload_shown`), so a much
/// larger `Quality` image would suddenly appear zoomed in.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_quality_from_bytes(
    bytes: &[u8],
    max_px: u32,
) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Quality)
}

/// Shared body of [`decode_raw_fast_from_bytes`] and
/// [`decode_raw_quality_from_bytes`].
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn decode_raw_preview_from_bytes(
    bytes: &[u8],
    max_px: u32,
    mode: DemosaicMode,
) -> Result<DecodedImage, String> {
    use rawler::rawimage::RawPhotometricInterpretation;

    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    let params = rawler::decoders::RawDecodeParams::default();
    let orientation = real_orientation(&source, &params);

    let mut raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rawler::decode(&source, &params)
    }))
    .map_err(|_| "panicked during RAW decode".to_string())?
    .map_err(|e| e.to_string())?;

    // wasm32 builds with `panic=abort`, so `catch_unwind` only helps on native.
    // Every layout rawler would panic on must be rejected before the call.
    // `apply_scaling()` hits `todo!()` for `BlackIsZero` (rawler 0.7.2).
    if !matches!(
        raw.photometric,
        RawPhotometricInterpretation::Cfa(_) | RawPhotometricInterpretation::LinearRaw
    ) {
        return Err(format!(
            "unsupported RAW layout ({})",
            describe_photometric(&raw.photometric)
        ));
    }

    let (w, h, rgba) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        raw.apply_scaling().map_err(|e| e.to_string())?;
        demosaic_preview(&mut raw, orientation, mode, max_px).ok_or_else(|| {
            format!(
                "unsupported RAW layout (cpp={}, {})",
                raw.cpp,
                describe_photometric(&raw.photometric)
            )
        })
    }))
    .map_err(|_| "panicked during fast preview".to_string())??;

    if w == 0 || h == 0 {
        return Err("fast preview has zero dimension".to_string());
    }

    if mode == DemosaicMode::Quality && raw.cpp != 3 {
        let (nw, nh, rgba) = resize_linear_f16(&rgba, w, h, max_px);
        return Ok(DecodedImage {
            width: nw,
            height: nh,
            rgba,
            pixel_format: PixelFormat::LinearF16,
        });
    }
    if mode == DemosaicMode::Quality {
        return Ok(DecodedImage {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::LinearF16,
        });
    }

    let (nw, nh) = fit_within(w, h, max_px);
    let rgba = if (nw, nh) == (w, h) {
        rgba
    } else {
        let buf = image::RgbaImage::from_raw(w, h, rgba)
            .ok_or_else(|| "fast preview buffer size mismatch".to_string())?;
        image::imageops::resize(&buf, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
    };
    Ok(DecodedImage {
        width: nw,
        height: nh,
        rgba,
        pixel_format: PixelFormat::Srgb8,
    })
}

/// Short label for error messages. The `Debug` output of the `Cfa` case
/// dumps the whole 48x48 pattern matrix.
fn describe_photometric(photometric: &rawler::rawimage::RawPhotometricInterpretation) -> String {
    use rawler::rawimage::RawPhotometricInterpretation as P;
    match photometric {
        P::Cfa(config) => format!("cfa={}", config.cfa.name),
        P::LinearRaw => "linear".to_string(),
        P::BlackIsZero => "black-is-zero".to_string(),
    }
}

/// rawler 0.7.2 always sets `RawImage.orientation` to `Normal`, so read the
/// EXIF orientation from `raw_metadata()` instead.
fn real_orientation(
    source: &rawler::rawsource::RawSource,
    params: &rawler::decoders::RawDecodeParams,
) -> rawler::decoders::Orientation {
    rawler::get_decoder(source)
        .and_then(|decoder| decoder.raw_metadata(source, params))
        .ok()
        .and_then(|meta| meta.exif.orientation)
        .map(rawler::decoders::Orientation::from_u16)
        .unwrap_or(rawler::decoders::Orientation::Normal)
}

/// Demosaics a Bayer mosaic (cpp == 1) or decimates already-RGB data
/// (cpp == 3, some DNGs), then applies orientation. `None` for any other
/// layout, such as monochrome.
fn demosaic_preview(
    raw: &mut rawler::RawImage,
    orientation: rawler::decoders::Orientation,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => demosaic_cfa(raw, mode, max_px),
        3 => decimate_linear_rgb(raw, mode, max_px),
        _ => None,
    }?;
    Some(apply_orientation(
        orientation,
        w,
        h,
        rgba,
        mode.bytes_per_pixel(),
    ))
}

/// Applies `orientation` to a packed buffer with `bpp` bytes per pixel. The
/// sensor buffer is always in the camera's native orientation. Flips must run
/// before the transpose (see `Orientation::to_flips`).
fn apply_orientation(
    orientation: rawler::decoders::Orientation,
    w: u32,
    h: u32,
    mut rgba: Vec<u8>,
    bpp: usize,
) -> (u32, u32, Vec<u8>) {
    let (transpose, flip_h, flip_v) = orientation.to_flips();
    let (w, h) = (w as usize, h as usize);

    if flip_h {
        for row in 0..h {
            let start = row * w * bpp;
            for col in 0..w / 2 {
                let (a, b) = (start + col * bpp, start + (w - 1 - col) * bpp);
                for c in 0..bpp {
                    rgba.swap(a + c, b + c);
                }
            }
        }
    }
    if flip_v {
        for row in 0..h / 2 {
            let (a, b) = (row * w * bpp, (h - 1 - row) * w * bpp);
            for c in 0..w * bpp {
                rgba.swap(a + c, b + c);
            }
        }
    }
    if transpose {
        let mut out = vec![0u8; rgba.len()];
        for row in 0..h {
            for col in 0..w {
                let src = (row * w + col) * bpp;
                let dst = (col * h + row) * bpp;
                out[dst..dst + bpp].copy_from_slice(&rgba[src..src + bpp]);
            }
        }
        return (h as u32, w as u32, out);
    }

    (w as u32, h as u32, rgba)
}

/// Box-filter downsample of a `Quality` f16 RGBA buffer to fit `max_px`. The
/// `image` crate cannot resize f16 data. Averaging in linear light is also
/// more accurate than resizing gamma-encoded values, which darkens edges.
/// Returns the input unchanged when it already fits.
fn resize_linear_f16(rgba: &[u8], w: u32, h: u32, max_px: u32) -> (u32, u32, Vec<u8>) {
    let (out_w, out_h) = fit_within(w, h, max_px);
    if (out_w, out_h) == (w, h) {
        return (w, h, rgba.to_vec());
    }
    let (w, h) = (w as usize, h as usize);
    let (out_w, out_h) = (out_w as usize, out_h as usize);
    let mut out = vec![0u8; out_w * out_h * 8];
    for oy in 0..out_h {
        let y0 = oy * h / out_h;
        let y1 = ((oy + 1) * h / out_h).max(y0 + 1).min(h);
        for ox in 0..out_w {
            let x0 = ox * w / out_w;
            let x1 = ((ox + 1) * w / out_w).max(x0 + 1).min(w);
            let mut sum = [0f32; 3];
            let mut n = 0u32;
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = (y * w + x) * 8;
                    sum[0] += half::f16::from_le_bytes([rgba[i], rgba[i + 1]]).to_f32();
                    sum[1] += half::f16::from_le_bytes([rgba[i + 2], rgba[i + 3]]).to_f32();
                    sum[2] += half::f16::from_le_bytes([rgba[i + 4], rgba[i + 5]]).to_f32();
                    n += 1;
                }
            }
            let avg = sum.map(|s| s / n.max(1) as f32);
            let idx = (oy * out_w + ox) * 8;
            out[idx..idx + 2].copy_from_slice(&half::f16::from_f32(avg[0]).to_le_bytes());
            out[idx + 2..idx + 4].copy_from_slice(&half::f16::from_f32(avg[1]).to_le_bytes());
            out[idx + 4..idx + 6].copy_from_slice(&half::f16::from_f32(avg[2]).to_le_bytes());
            out[idx + 6..idx + 8].copy_from_slice(&half::f16::from_f32(1.0).to_le_bytes());
        }
    }
    (out_w as u32, out_h as u32, out)
}

/// Camera RGB to linear sRGB matrix. Reimplements rawler's
/// `map_3ch_to_rgb`, which is crate-private. `None` when the file has no
/// color matrix; callers then output white-balanced camera RGB, as
/// `RawDevelop` does.
fn build_cam2rgb(raw: &rawler::RawImage) -> Option<[[f32; 4]; 3]> {
    use rawler::imgop::matrix::{multiply, normalize, pseudo_inverse};
    use rawler::imgop::xyz::{Illuminant, SRGB_TO_XYZ_D65};

    let (_, flat) = raw
        .color_matrix
        .iter()
        .find(|(illuminant, _)| **illuminant == Illuminant::D65)
        .or_else(|| raw.color_matrix.iter().next())?;

    let mut xyz2cam = [[0f32; 3]; 4];
    let components = (flat.len() / 3).min(4);
    for i in 0..components {
        for j in 0..3 {
            xyz2cam[i][j] = flat[i * 3 + j];
        }
    }

    let rgb2cam = normalize(multiply(&xyz2cam, &SRGB_TO_XYZ_D65));
    Some(pseudo_inverse(rgb2cam))
}

/// Applies the color matrix to one white-balanced sample.
/// Samples can exceed 1.0 here; `clip_euclidean_norm_avg` rolls them off
/// softly, keeping hue near white, instead of clipping each channel.
fn apply_cam2rgb(cam2rgb: &[[f32; 4]; 3], rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let srgb = [
        cam2rgb[0][0] * r + cam2rgb[0][1] * g + cam2rgb[0][2] * b,
        cam2rgb[1][0] * r + cam2rgb[1][1] * g + cam2rgb[1][2] * b,
        cam2rgb[2][0] * r + cam2rgb[2][1] * g + cam2rgb[2][2] * b,
    ];
    rawler::imgop::raw::clip_euclidean_norm_avg(&srgb)
}

/// One white-balanced, normalized camera-RGB sample to opaque display RGBA8.
fn render_rgb_sample(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 4] {
    let srgb = match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    };
    [
        to_srgb_u8(srgb[0]),
        to_srgb_u8(srgb[1]),
        to_srgb_u8(srgb[2]),
        255,
    ]
}

/// Like [`render_rgb_sample`] but stops after the color matrix, leaving linear
/// light for `raw_shader.wgsl`.
fn render_rgb_sample_linear(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [f32; 3] {
    match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    }
}

/// [`render_rgb_sample_linear`] packed as little-endian f16 RGBA, alpha 1.
fn render_rgb_sample_linear_bytes(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 8] {
    let [r, g, b] = render_rgb_sample_linear(rgb, cam2rgb);
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&half::f16::from_f32(r).to_le_bytes());
    out[2..4].copy_from_slice(&half::f16::from_f32(g).to_le_bytes());
    out[4..6].copy_from_slice(&half::f16::from_f32(b).to_le_bytes());
    out[6..8].copy_from_slice(&half::f16::from_f32(1.0).to_le_bytes());
    out
}

/// Whether rawler's Bayer demosaic impls can take this layout without
/// panicking, which is fatal under wasm32's `panic=abort`:
///
/// - `Superpixel3Channel` panics unless there are 3 planes and the CFA is one
///   of the four 2x2 RGGB-family patterns. X-Trans hits its `unreachable!()`.
/// - `PPGDemosaic` checks only `is_rgb()` and then assumes a 2x2 tiling.
/// - Both index the buffer at the active area unchecked.
///
/// The name is checked after `cfa.shift` to the active-area origin, because an
/// odd origin turns RGGB into GRBG.
fn is_supported_bayer_layout(
    cfa: &rawler::CFA,
    colors: &rawler::cfa::PlaneColor,
    area: rawler::imgop::Rect,
    width: usize,
    height: usize,
) -> bool {
    colors.plane_count() == 3
        && matches!(
            cfa.shift(area.p.x, area.p.y).name.as_str(),
            "RGGB" | "BGGR" | "GBRG" | "GRBG"
        )
        && area_fits(area, width, height)
}

// TODO(x-trans): the X-Trans path has known bugs. (1) An active-area origin
// inside a skipped tile may crop too much of the first tile. (2)
// `apply_scaling()` uses the Bayer-only `correct_blacklevel_cfa`, which
// collapses X-Trans's 6x6 per-phase black levels to one value. (3)
// `downsample_xtrans_mosaic` clamps a partial trailing block to a row or
// column of a different CFA phase, corrupting color at the bottom and right
// edges. It only runs for RAF files with no usable embedded image, since
// `thumbnail.rs`'s `rawler_full_image_from_bytes` handles the common case.
fn is_supported_xtrans_layout(
    cfa: &rawler::CFA,
    colors: &rawler::cfa::PlaneColor,
    area: rawler::imgop::Rect,
    width: usize,
    height: usize,
) -> bool {
    cfa.is_rgb()
        && colors.plane_count() == 3
        && cfa.shift(area.p.x, area.p.y).name.len() == 36
        && area_fits(area, width, height)
}

fn neutral_if_non_finite(wb: [f32; 4]) -> [f32; 4] {
    if wb[..3].iter().all(|coefficient| coefficient.is_finite()) {
        wb
    } else {
        [1.0; 4]
    }
}

#[cfg(test)]
mod tests {
    use super::{downsample_xtrans_mosaic, map_xtrans_coord, neutral_if_non_finite};

    #[test]
    fn missing_or_partial_white_balance_is_neutral() {
        assert_eq!(neutral_if_non_finite([f32::NAN; 4]), [1.0; 4]);
        let rgb_with_unused_nan = neutral_if_non_finite([2.0, 1.0, 0.5, f32::NAN]);
        assert_eq!(rgb_with_unused_nan[..3], [2.0, 1.0, 0.5]);
        assert!(rgb_with_unused_nan[3].is_nan());
        assert_eq!(
            neutral_if_non_finite([2.0, 1.0, 0.5, 1.0]),
            [2.0, 1.0, 0.5, 1.0]
        );
    }

    #[test]
    fn xtrans_bounded_sampling_preserves_all_cfa_phases() {
        // Column-independent so each output column's average collapses to
        // the same expression as the row it belongs to.
        let (width, height) = (72, 72);
        let source: Vec<f32> = (0..width * height)
            .map(|index| (index / width) as f32)
            .collect();
        let (reduced, reduced_width, reduced_height) =
            downsample_xtrans_mosaic(&source, width, height, 12);

        assert_eq!((reduced_width, reduced_height), (12, 12));

        // reduction = 6, tile_step = 36: tile row 0's phase row `p` averages
        // source rows {p, p+6, .., p+30}; tile row 1's averages {36+p, ..,
        // 66+p}. Reading only row `p` would fail this.
        for phase_row in 0..6 {
            let block0_mean: f32 = (0..6).map(|i| (phase_row + i * 6) as f32).sum::<f32>() / 6.0;
            let block1_mean: f32 =
                (0..6).map(|i| (36 + phase_row + i * 6) as f32).sum::<f32>() / 6.0;
            assert_eq!(reduced[phase_row * reduced_width], block0_mean);
            assert_eq!(reduced[(6 + phase_row) * reduced_width], block1_mean);
        }
    }

    #[test]
    fn xtrans_active_area_bounds_skip_unretained_tile_rows() {
        assert_eq!(map_xtrans_coord(0, 12), 0);
        assert_eq!(map_xtrans_coord(5, 12), 5);
        assert_eq!(map_xtrans_coord(6, 12), 6);
        assert_eq!(map_xtrans_coord(11, 12), 6);
        assert_eq!(map_xtrans_coord(12, 12), 6);
        assert_eq!(map_xtrans_coord(18, 12), 12);
    }
}

/// `area` lies inside a `width * height` buffer. `checked_add` because
/// `usize` is 32 bits on wasm32, and a garbage active-area tag could wrap a
/// plain addition back into range.
fn area_fits(area: rawler::imgop::Rect, width: usize, height: usize) -> bool {
    area.p.x.checked_add(area.d.w).is_some_and(|x1| x1 <= width)
        && area
            .p
            .y
            .checked_add(area.d.h)
            .is_some_and(|y1| y1 <= height)
}

fn map_xtrans_coord(coord: usize, tile_step: usize) -> usize {
    let tile = coord / tile_step;
    let offset = coord % tile_step;
    tile * 6 + if offset < 6 { offset } else { 6 }
}

/// Demosaics a CFA mosaic with rawler's `Demosaic` impls: `Superpixel3Channel`
/// for `Fast`, `PPGDemosaic` for `Quality`. Returns `None` for layouts that
/// would panic (see `is_supported_bayer_layout`).
pub(crate) fn demosaic_cfa(
    raw: &mut rawler::RawImage,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::imgop::sensor::bayer::{
        ppg::PPGDemosaic, superpixel::Superpixel3Channel, Demosaic,
    };
    use rawler::pixarray::Pix2D;
    use rawler::rawimage::RawPhotometricInterpretation;
    use rawler::RawImageData;

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return None;
    };
    let cfa = config.cfa.clone();
    let colors = config.colors.clone();
    let wb = neutral_if_non_finite(raw.wb_coeffs);
    let (width, height) = (raw.width, raw.height);

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, height),
    ));
    // `area` is remapped below for X-Trans. The default-crop step needs the
    // real active area, which is also the demosaiced buffer's origin.
    let original_active_area = area;
    let is_xtrans = is_supported_xtrans_layout(&cfa, &colors, area, width, height);
    if !is_xtrans && !is_supported_bayer_layout(&cfa, &colors, area, width, height) {
        return None;
    }

    let RawImageData::Float(data) = &mut raw.data else {
        return None; // apply_scaling always leaves Float data
    };
    if data.len() != width * height {
        return None;
    }
    // `mem::take`, not `clone`: the sensor buffer is ~96MB at 24MP, and
    // nothing reads `raw.data` after this.
    let pixels = if is_xtrans {
        let max_dim = width.max(height);
        let requested = max_px.max(1) as usize;
        // Keep every position of each 6x6 CFA tile and average matching
        // phases across skipped tiles. A plain stride would keep one phase.
        let reduction = max_dim.div_ceil(requested).max(1);
        if reduction == 1 {
            Pix2D::new_with(std::mem::take(data), width, height)
        } else {
            let source = std::mem::take(data);
            let (reduced, reduced_width, reduced_height) =
                downsample_xtrans_mosaic(&source, width, height, max_px);
            Pix2D::new_with(reduced, reduced_width, reduced_height)
        }
    } else {
        Pix2D::new_with(std::mem::take(data), width, height)
    };

    let xtrans_stride = if is_xtrans {
        width.max(height).div_ceil(max_px.max(1) as usize).max(1)
    } else {
        1
    };
    let area = if is_xtrans && pixels.width != width {
        let tile_step = xtrans_stride * 6;
        // Map to the first retained coordinate at or after `coord`, so
        // coordinates in a skipped part of a tile never pull in border samples.
        let map_start = |coord: usize| map_xtrans_coord(coord, tile_step);
        let x0 = map_start(area.p.x).min(pixels.width);
        let y0 = map_start(area.p.y).min(pixels.height);
        let x1 = map_start(area.p.x + area.d.w).min(pixels.width);
        let y1 = map_start(area.p.y + area.d.h).min(pixels.height);
        rawler::imgop::Rect::new(
            rawler::imgop::Point::new(x0, y0),
            rawler::imgop::Dim2::new(x1 - x0, y1 - y0),
        )
    } else {
        area
    };

    let demosaiced = if is_xtrans {
        // rawler 0.7.2 has no X-Trans demosaic. `PPGDemosaic` does not panic
        // on it because it reads colors via `cfa.color_at()`, but it assumes
        // Bayer neighborhoods, so the result is soft.
        PPGDemosaic::new().demosaic(&pixels, &cfa, &colors, area)
    } else {
        match mode {
            DemosaicMode::Fast => Superpixel3Channel::new().demosaic(&pixels, &cfa, &colors, area),
            DemosaicMode::Quality => PPGDemosaic::new().demosaic(&pixels, &cfa, &colors, area),
        }
    };

    // Match `RawDevelop`'s `CropDefault` step: crop to `raw.crop_area`, the
    // DNG default display rectangle, which is often tighter than the active
    // area. Skipping it lets dark edge pixels into the mip chain and darkens
    // the fitted view. `crop_area` is in sensor coordinates, so adapt it to the
    // active area and halve it for `Fast`'s quarter-res bin. X-Trans is
    // skipped because its demosaic ran in reduced coordinates.
    let demosaiced = match (!is_xtrans)
        .then(|| raw.crop_area.or(Some(original_active_area)))
        .flatten()
    {
        Some(mut crop)
            if crop.d != rawler::imgop::Dim2::new(demosaiced.width, demosaiced.height) =>
        {
            crop = crop.adapt(&original_active_area);
            if mode == DemosaicMode::Fast {
                crop.scale(0.5);
            }
            // A crop that does not fit (bad metadata) means no crop.
            let fits =
                crop.p.x + crop.d.w <= demosaiced.width && crop.p.y + crop.d.h <= demosaiced.height;
            if fits && !crop.is_empty() {
                let cropped = rawler::imgop::crop(
                    demosaiced.pixels(),
                    rawler::imgop::Dim2::new(demosaiced.width, demosaiced.height),
                    crop,
                );
                rawler::pixarray::Color2D::new_with(cropped, crop.d.w, crop.d.h)
            } else {
                demosaiced
            }
        }
        _ => demosaiced,
    };

    let cam2rgb = build_cam2rgb(raw);
    let (w, h) = (demosaiced.width, demosaiced.height);

    // Auto-denoise (`AUTO_RAW_DENOISE_STRENGTH`), `Quality` only. Runs on
    // linear camera RGB before white balance and the color matrix.
    let denoised_quality;
    let demosaic_pixels: &[[f32; 3]] = if mode == DemosaicMode::Quality {
        denoised_quality = crate::develop::denoise_linear_rgb_buffer(
            crate::image_decode::AUTO_RAW_DENOISE_STRENGTH,
            w,
            h,
            demosaiced.pixels(),
        );
        &denoised_quality
    } else {
        demosaiced.pixels()
    };

    // White balance comes after demosaic, as in `RawDevelop`, because PPG's
    // edge decisions depend on channel values.
    let mut rgba = vec![0u8; w * h * mode.bytes_per_pixel()];
    match mode {
        DemosaicMode::Fast => {
            for (i, &rgb) in demosaic_pixels.iter().enumerate() {
                let balanced = [rgb[0] * wb[0], rgb[1] * wb[1], rgb[2] * wb[2]];
                let px = render_rgb_sample(balanced, &cam2rgb);
                rgba[i * 4..i * 4 + 4].copy_from_slice(&px);
            }
        }
        DemosaicMode::Quality => {
            for (i, &rgb) in demosaic_pixels.iter().enumerate() {
                let balanced = [rgb[0] * wb[0], rgb[1] * wb[1], rgb[2] * wb[2]];
                let px = render_rgb_sample_linear_bytes(balanced, &cam2rgb);
                rgba[i * 8..i * 8 + 8].copy_from_slice(&px);
            }
        }
    }
    Some((w as u32, h as u32, rgba))
}

/// Shrinks an X-Trans mosaic while keeping its 6x6 CFA pattern intact. Each
/// output sample averages all same-phase samples in the
/// `reduction`x`reduction` block of tiles it stands for. Averaging instead of
/// picking one tile avoids banding. See the TODO on
/// `is_supported_xtrans_layout` for the edge clamping bug.
fn downsample_xtrans_mosaic(
    source: &[f32],
    width: usize,
    height: usize,
    max_px: u32,
) -> (Vec<f32>, usize, usize) {
    let reduction = width.max(height).div_ceil(max_px.max(1) as usize).max(1);
    if reduction == 1 {
        return (source.to_vec(), width, height);
    }
    let tile_step = reduction * 6;
    let reduced_width = width.div_ceil(tile_step) * 6;
    let reduced_height = height.div_ceil(tile_step) * 6;
    let reduced: Vec<f32> = (0..reduced_height)
        .flat_map(|row| {
            let (tile_row, phase_row) = (row / 6, row % 6);
            (0..reduced_width).map(move |col| {
                let (tile_col, phase_col) = (col / 6, col % 6);
                let mut sum = 0f32;
                for i in 0..reduction {
                    let source_row = (tile_row * tile_step + phase_row + i * 6).min(height - 1);
                    for j in 0..reduction {
                        let source_col = (tile_col * tile_step + phase_col + j * 6).min(width - 1);
                        sum += source[source_row * width + source_col];
                    }
                }
                sum / (reduction * reduction) as f32
            })
        })
        .collect();
    (reduced, reduced_width, reduced_height)
}

/// Preview for data that is already RGB (cpp == 3), so there is nothing to
/// demosaic. `Fast` takes every other pixel; `Quality` box-averages to fit
/// `max_px`. Applies white balance and the color matrix, plus gamma for
/// `Fast`.
fn decimate_linear_rgb(
    raw: &mut rawler::RawImage,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let cpp = raw.cpp;
    let wb = neutral_if_non_finite(raw.wb_coeffs);
    let (width, height) = (raw.width, raw.height);

    let cam2rgb = build_cam2rgb(raw);

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, height),
    ));
    // Out-of-bounds indexing would panic, fatal under wasm32 `panic=abort`.
    if cpp == 0 || !area_fits(area, width, height) {
        return None;
    }

    let RawImageData::Float(data) = &mut raw.data else {
        return None; // apply_scaling always leaves Float data
    };
    if data.len() != width * height * cpp {
        return None;
    }
    let (x0, y0) = (area.p.x, area.p.y);
    let step = match mode {
        DemosaicMode::Fast => 2,
        DemosaicMode::Quality => 1,
    };
    let (out_w, out_h) = match mode {
        DemosaicMode::Fast => (area.d.w / step, area.d.h / step),
        DemosaicMode::Quality => {
            let (w, h) = fit_within(area.d.w as u32, area.d.h as u32, max_px);
            (w as usize, h as usize)
        }
    };
    let bpp = mode.bytes_per_pixel();

    // Accumulate linear values first so `Quality`'s denoise can see the whole
    // buffer, then render in a second pass.
    let mut linear = vec![[0f32; 3]; out_w * out_h];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let mut rgb = [0f32; 3];
            let (sx0, sx1, sy0, sy1) = if mode == DemosaicMode::Quality {
                (
                    ox * area.d.w / out_w,
                    ((ox + 1) * area.d.w / out_w).max(ox * area.d.w / out_w + 1),
                    oy * area.d.h / out_h,
                    ((oy + 1) * area.d.h / out_h).max(oy * area.d.h / out_h + 1),
                )
            } else {
                (ox * step, ox * step + 1, oy * step, oy * step + 1)
            };
            let mut count: f32 = 0.0;
            for sy in sy0..sy1.min(area.d.h as usize) {
                for sx in sx0..sx1.min(area.d.w as usize) {
                    let base =
                        ((y0 as usize + sy) * width as usize + x0 as usize + sx) * cpp as usize;
                    for (ch, slot) in rgb.iter_mut().enumerate() {
                        *slot += data[base + ch] * wb.get(ch).copied().unwrap_or(1.0);
                    }
                    count += 1.0;
                }
            }
            if mode == DemosaicMode::Quality {
                for slot in &mut rgb {
                    *slot /= count.max(1.0);
                }
            }
            linear[oy * out_w + ox] = rgb;
        }
    }

    // Auto-denoise, `Quality` only. White balance is already folded in here,
    // unlike `demosaic_cfa`; both still denoise before the color matrix.
    if mode == DemosaicMode::Quality {
        linear = crate::develop::denoise_linear_rgb_buffer(
            crate::image_decode::AUTO_RAW_DENOISE_STRENGTH,
            out_w,
            out_h,
            &linear,
        );
    }

    let mut rgba = vec![0u8; out_w * out_h * bpp];
    for (i, &rgb) in linear.iter().enumerate() {
        let idx = i * bpp;
        match mode {
            DemosaicMode::Fast => {
                let px = render_rgb_sample(rgb, &cam2rgb);
                rgba[idx..idx + 4].copy_from_slice(&px);
            }
            DemosaicMode::Quality => {
                let px = render_rgb_sample_linear_bytes(rgb, &cam2rgb);
                rgba[idx..idx + 8].copy_from_slice(&px);
            }
        }
    }

    Some((out_w as u32, out_h as u32, rgba))
}
