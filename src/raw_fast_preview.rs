// SPDX-License-Identifier: GPL-3.0-or-later

//! A fast, lower-fidelity RAW preview: quarter-resolution Bayer binning (or
//! decimation for already-linear sensor data) instead of full PPG demosaic.
//! Ported from `tools/wasm-decode-probe`'s throwaway spike
//! (`wasm-decode-probe-spike` branch, `src/lib.rs`), where it was measured
//! at ~6.3x faster than `RawDevelop`'s default `Quality`/PPG path — the
//! wasm port plan's M3 explicitly chose this as the interactive-browsing
//! default (fast, close to native speed) over full PPG (measured 4-5x
//! *slower* than native, fails the port's own performance goal outright).
//! Full PPG stays available via `image_decode::decode` for a later
//! export-time/"full quality" opt-in — not wired to anything yet.
//!
//! `#[cfg(not(target_os = "macos"))]`, like the rest of the non-mac RAW
//! decode: not wasm32-specific code, just currently only wired into the app
//! via wasm32's decode path (`app/web.rs`) — nothing stops a future native
//! non-mac caller from using this too. Exception: `decode_raw_fast_from_bytes`
//! is also gated on `feature = "raw-probe"` to allow `decode_probe.rs` to call it
//! from a mac dev build (see that function's own comment for the rationale).

use crate::image_decode::{fit_within, DecodedImage};

/// Precomputed 1/2.2-gamma lookup table, built once on first use — the
/// spike's own finding: both algorithms below landed at the same
/// ~300-320ms/megapixel regardless of approach, and replacing three
/// per-pixel `powf()` calls with an array index was what actually mattered
/// (a real, measured 6.3x speedup came almost entirely from this LUT, not
/// either algorithm's own work).
const GAMMA_LUT_SIZE: usize = 4097;
static GAMMA_LUT: std::sync::OnceLock<[u8; GAMMA_LUT_SIZE]> = std::sync::OnceLock::new();

fn to_srgb_u8(v: f32) -> u8 {
    let lut = GAMMA_LUT.get_or_init(|| {
        let mut table = [0u8; GAMMA_LUT_SIZE];
        for (i, entry) in table.iter_mut().enumerate() {
            let linear = i as f32 / (GAMMA_LUT_SIZE - 1) as f32;
            *entry = (linear.powf(1.0 / 2.2) * 255.0).round() as u8;
        }
        table
    });
    let idx = (v.clamp(0.0, 1.0) * (GAMMA_LUT_SIZE - 1) as f32).round() as usize;
    lut[idx]
}

/// Decode a RAW file's bytes into a fast preview, resized to fit `max_px`
/// (Lanczos3, same as every other decode path in this crate — see
/// `image_decode::decode_jpeg_png_tiff_from_bytes`). Both the grid (small
/// target) and the Loupe (larger target) call this identically; the
/// quarter-res output is generated once regardless of `max_px` and then
/// resized down if still larger, rather than having two separate
/// resolution-tiered fast paths — simpler, and the quarter-res generation
/// itself is already the fast part.
//
// Gated on `feature = "raw-probe"` as well as `not(target_os = "macos")` so
// `decode_probe.rs`'s golden-hash regression tests can call this same
// function from a mac dev build via `cargo test --bin decode_probe
// --features raw-probe` — exact same reasoning and pattern as
// `image_decode.rs`'s `decode_raw_via_rawler` (see that function's doc
// comment). On mac+raw-probe this also compiles into the *main*
// `lightphotos` binary, where nothing calls it — only `decode_probe.rs`'s
// own copy of this module does.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    let params = rawler::decoders::RawDecodeParams::default();
    let orientation = real_orientation(&source, &params);

    let raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rawler::decode(&source, &params)))
        .map_err(|_| "panicked during RAW decode".to_string())?
        .map_err(|e| e.to_string())?;

    let (w, h, rgba) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| fast_preview(&raw, orientation)))
        .map_err(|_| "panicked during fast preview".to_string())?
        .ok_or_else(|| format!("unsupported RAW layout (cpp={})", raw.cpp))?;

    if w == 0 || h == 0 {
        return Err("fast preview has zero dimension".to_string());
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
    })
}

/// `rawler` 0.7.2's `RawImage.orientation` is hardcoded to `Normal`
/// regardless of the file's real EXIF tag (confirmed by reading
/// `rawimage.rs:389,478` — an upstream bug, not guessed at). Fetches
/// orientation the way RapidRAW's own calling code does instead: a separate
/// `raw_metadata()` call reading `.exif.orientation`.
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

/// Dispatches on `raw.cpp`: `bin_bayer_quarter_res` for undemosaiced Bayer
/// mosaics (cpp == 1 — ARW/CR2/NEF and most camera RAW), `decimate_linear_rgb`
/// for already-demosaiced/linear data (cpp == 3 — some DNGs, which skip
/// Bayer interpolation entirely). `None` for anything else (e.g. monochrome,
/// cpp == 4) rather than guessing at a wrong image.
fn fast_preview(
    raw: &rawler::RawImage,
    orientation: rawler::decoders::Orientation,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => bin_bayer_quarter_res(raw),
        3 => decimate_linear_rgb(raw),
        _ => None,
    }?;
    Some(apply_orientation(orientation, w, h, rgba))
}

/// Applies `rawler::decoders::Orientation`'s flip/transpose to an RGBA8
/// buffer — the sensor buffer is always laid out in the camera's native
/// orientation regardless of how the photo was actually held. Per
/// `Orientation::to_flips`'s own doc comment, flips must happen before the
/// transpose for a correct result.
fn apply_orientation(
    orientation: rawler::decoders::Orientation,
    w: u32,
    h: u32,
    mut rgba: Vec<u8>,
) -> (u32, u32, Vec<u8>) {
    let (transpose, flip_h, flip_v) = orientation.to_flips();
    let (w, h) = (w as usize, h as usize);

    if flip_h {
        for row in 0..h {
            let start = row * w * 4;
            for col in 0..w / 2 {
                let (a, b) = (start + col * 4, start + (w - 1 - col) * 4);
                for c in 0..4 {
                    rgba.swap(a + c, b + c);
                }
            }
        }
    }
    if flip_v {
        for row in 0..h / 2 {
            let (a, b) = (row * w * 4, (h - 1 - row) * w * 4);
            for c in 0..w * 4 {
                rgba.swap(a + c, b + c);
            }
        }
    }
    if transpose {
        let mut out = vec![0u8; rgba.len()];
        for row in 0..h {
            for col in 0..w {
                let src = (row * w + col) * 4;
                let dst = (col * h + row) * 4;
                out[dst..dst + 4].copy_from_slice(&rgba[src..src + 4]);
            }
        }
        return (h as u32, w as u32, out);
    }

    (w as u32, h as u32, rgba)
}

/// Camera-RGB → sRGB matrix, built once per image. Same algorithm `rawler`'s
/// own `imgop::raw::map_3ch_to_rgb` uses internally (that function itself is
/// `pub(crate)`-restricted inside `rawler`, so not callable from here — this
/// replicates its ~10-line body from the public primitives it's built from).
/// `None` if `raw.color_matrix` has no calibration data for this camera;
/// callers fall back to WB-only output, same as `RawDevelop`'s own behavior
/// on a missing matrix.
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

/// Applies `build_cam2rgb`'s matrix to one WB-corrected camera-RGB sample,
/// including the soft highlight-rolloff clip (`clip_euclidean_norm_avg`)
/// rather than a hard `.clamp` — better behavior near white.
fn apply_cam2rgb(cam2rgb: &[[f32; 4]; 3], rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let srgb = [
        cam2rgb[0][0] * r + cam2rgb[0][1] * g + cam2rgb[0][2] * b,
        cam2rgb[1][0] * r + cam2rgb[1][1] * g + cam2rgb[1][2] * b,
        cam2rgb[2][0] * r + cam2rgb[2][1] * g + cam2rgb[2][2] * b,
    ];
    rawler::imgop::raw::clip_euclidean_norm_avg(&srgb)
}

/// Renders one linear camera-RGB sample (already white-balanced) to
/// display RGBA8: color matrix (if the camera has calibration data) ->
/// gamma, alpha fixed opaque. Shared by both `bin_bayer_quarter_res` and
/// `decimate_linear_rgb` — was duplicated inline in both before this.
fn render_rgb_sample(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 4] {
    let srgb = match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    };
    [to_srgb_u8(srgb[0]), to_srgb_u8(srgb[1]), to_srgb_u8(srgb[2]), 255]
}

/// Bayer-CFA quarter-res preview: bin each 2x2 Bayer block into one output
/// pixel, no interpolation. See `fast_preview`'s doc comment for when this
/// applies.
fn bin_bayer_quarter_res(raw: &rawler::RawImage) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let sample_at = |idx: usize| -> f32 {
        match &raw.data {
            RawImageData::Integer(v) => v[idx] as f32,
            RawImageData::Float(v) => v[idx],
        }
    };
    let expected_len = raw.width * raw.height;
    let actual_len = match &raw.data {
        RawImageData::Integer(v) => v.len(),
        RawImageData::Float(v) => v.len(),
    };
    if actual_len != expected_len {
        return None;
    }

    let black = raw.blacklevel.as_bayer_array();
    let white = raw.whitelevel.as_bayer_array();
    let wb = raw.wb_coeffs;
    let cfa = &raw.camera.cfa;

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(raw.width, raw.height),
    ));
    let (x0, y0) = (area.p.x - (area.p.x % 2), area.p.y - (area.p.y % 2));
    let (aw, ah) = (area.d.w - (area.d.w % 2), area.d.h - (area.d.h % 2));

    let out_w = aw / 2;
    let out_h = ah / 2;
    let mut rgba = vec![0u8; out_w * out_h * 4];
    let cam2rgb = build_cam2rgb(raw);

    let normalize = |row: usize, col: usize| -> (usize, f32) {
        let ch = cfa.color_at(row, col);
        let v = sample_at(row * raw.width + col);
        let denom = (white[ch] - black[ch]).max(1.0);
        let n = ((v - black[ch]) / denom).clamp(0.0, 1.0) * wb[ch];
        (ch, n.clamp(0.0, 1.0))
    };

    for oy in 0..out_h {
        for ox in 0..out_w {
            let (row, col) = (y0 + oy * 2, x0 + ox * 2);
            let mut rgb = [0f32; 3];
            let mut g_count = 0f32;
            for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let (ch, n) = normalize(row + dr, col + dc);
                match ch {
                    0 => rgb[0] = n,
                    2 => rgb[2] = n,
                    _ => {
                        rgb[1] += n;
                        g_count += 1.0;
                    }
                }
            }
            if g_count > 0.0 {
                rgb[1] /= g_count;
            }
            let px = render_rgb_sample(rgb, &cam2rgb);
            let idx = (oy * out_w + ox) * 4;
            rgba[idx..idx + 4].copy_from_slice(&px);
        }
    }

    Some((out_w as u32, out_h as u32, rgba))
}

/// Already-demosaiced/linear RGB preview (cpp == 3 — no CFA, no
/// interpolation to do): per-channel black/white-level + white-balance
/// normalize and gamma, plus simple 2x2 nearest-neighbor decimation rather
/// than an averaging box filter — this data has no CFA-driven reason to
/// average 4 samples together.
fn decimate_linear_rgb(raw: &rawler::RawImage) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let sample_at = |idx: usize| -> f32 {
        match &raw.data {
            RawImageData::Integer(v) => v[idx] as f32,
            RawImageData::Float(v) => v[idx],
        }
    };
    let expected_len = raw.width * raw.height * raw.cpp;
    let actual_len = match &raw.data {
        RawImageData::Integer(v) => v.len(),
        RawImageData::Float(v) => v.len(),
    };
    if actual_len != expected_len {
        return None;
    }

    let black = raw.blacklevel.as_bayer_array();
    let white = raw.whitelevel.as_bayer_array();
    let wb = raw.wb_coeffs;

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(raw.width, raw.height),
    ));
    let (x0, y0) = (area.p.x, area.p.y);
    let (out_w, out_h) = (area.d.w / 2, area.d.h / 2);
    let mut rgba = vec![0u8; out_w * out_h * 4];
    let cam2rgb = build_cam2rgb(raw);

    for oy in 0..out_h {
        for ox in 0..out_w {
            let (row, col) = (y0 + oy * 2, x0 + ox * 2);
            let base = (row * raw.width + col) * raw.cpp;
            let idx = (oy * out_w + ox) * 4;
            let mut rgb = [0f32; 3];
            for (ch, slot) in rgb.iter_mut().enumerate() {
                let v = sample_at(base + ch);
                let denom = (white[ch] - black[ch]).max(1.0);
                *slot = ((v - black[ch]) / denom).clamp(0.0, 1.0) * wb[ch];
            }
            let px = render_rgb_sample(rgb, &cam2rgb);
            rgba[idx..idx + 4].copy_from_slice(&px);
        }
    }

    Some((out_w as u32, out_h as u32, rgba))
}
