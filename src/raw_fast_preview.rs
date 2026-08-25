// SPDX-License-Identifier: GPL-3.0-or-later

//! A fast, lower-fidelity RAW preview: quarter-resolution Bayer binning (or
//! decimation for already-linear sensor data) instead of full PPG demosaic.
//! That `Fast` tier is what every caller uses today; the same Bayer entry
//! point can also dispatch to rawler's full-res `PPGDemosaic`
//! (`DemosaicMode::Quality`), which exists but isn't wired to any call site
//! yet — see `DemosaicMode`'s own doc comment.
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

/// `Fast` = rawler's `Superpixel3Channel` (quarter-res 2x2 bin, matches
/// this file's pre-Task-4 hand-rolled output). `Quality` = rawler's
/// `PPGDemosaic` (full-res, real edge-directed interpolation — the same
/// algorithm `decode_raw_nonmac`, the native non-mac path, already uses
/// via `RawDevelop`). `Quality` isn't wired into any call site yet — see
/// `plans/raw-decode-rapidraw-parity-design.md`'s explicit scope note.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DemosaicMode {
    Fast,
    #[allow(dead_code)]
    Quality,
}

/// Precomputed sRGB-gamma lookup table, built once on first use — the
/// spike's own finding: both algorithms below landed at the same
/// ~300-320ms/megapixel regardless of approach, and replacing three
/// per-pixel gamma-function calls with an array index was what actually
/// mattered (a real, measured 6.3x speedup came almost entirely from this
/// LUT, not either algorithm's own work). The LUT bakes in `rawler`'s own
/// `srgb_apply_gamma` — the real piecewise sRGB transfer function (linear
/// segment below a crossover point, then a power curve with sRGB's actual
/// gain/offset constants) rather than a flat `1/2.2` approximation, which
/// this file used before — the LUT-as-perf-trick and the curve-it-encodes
/// are independent choices; only the latter changed here.
const GAMMA_LUT_SIZE: usize = 4097;
static GAMMA_LUT: std::sync::OnceLock<[u8; GAMMA_LUT_SIZE]> = std::sync::OnceLock::new();

fn to_srgb_u8(v: f32) -> u8 {
    let lut = GAMMA_LUT.get_or_init(|| {
        let mut table = [0u8; GAMMA_LUT_SIZE];
        for (i, entry) in table.iter_mut().enumerate() {
            let linear = i as f32 / (GAMMA_LUT_SIZE - 1) as f32;
            *entry = (rawler::imgop::srgb::srgb_apply_gamma(linear) * 255.0).round() as u8;
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
// comment). Note this module isn't declared in `main.rs` at all, so it never
// reaches the *main* `lightphotos` binary: the only two things that pull it
// in are `wasm_worker.rs` (wasm32-gated — the sole production caller) and
// `decode_probe.rs`'s own `#[path]` re-declaration.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    use rawler::rawimage::RawPhotometricInterpretation;

    let source = rawler::rawsource::RawSource::new_from_slice(bytes);
    let params = rawler::decoders::RawDecodeParams::default();
    let orientation = real_orientation(&source, &params);

    let mut raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rawler::decode(&source, &params)))
        .map_err(|_| "panicked during RAW decode".to_string())?
        .map_err(|e| e.to_string())?;

    // `apply_scaling()` hits a bare `todo!()` for `BlackIsZero` (rawler
    // 0.7.2, `rawimage.rs:510`) — a panic, not an error. The `catch_unwind`
    // guards in this function are load-bearing on native only: the sole
    // production target here is `wasm32-unknown-unknown`, which is
    // `panic=abort`, so *any* panic below traps the whole decode worker
    // module with no recoverable error. Every unsupported layout therefore
    // has to be turned away by an explicit check before the offending call,
    // not caught after the fact — see `is_supported_bayer_layout` for the
    // same reasoning applied to the `Demosaic` impls.
    if !matches!(
        raw.photometric,
        RawPhotometricInterpretation::Cfa(_) | RawPhotometricInterpretation::LinearRaw
    ) {
        return Err(format!("unsupported RAW layout ({})", describe_photometric(&raw.photometric)));
    }

    let (w, h, rgba) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        raw.apply_scaling().map_err(|e| e.to_string())?;
        fast_preview(&mut raw, orientation).ok_or_else(|| {
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

/// A short, human-readable tag for an error message — `RawPhotometricInterpretation`
/// isn't `Display`, and its `Debug` for the `Cfa` case would dump the whole
/// 48x48 pattern matrix. The CFA's own name (e.g. `"RGGB"`, or X-Trans's
/// 36-char name) is the part that's actually diagnostic here.
fn describe_photometric(photometric: &rawler::rawimage::RawPhotometricInterpretation) -> String {
    use rawler::rawimage::RawPhotometricInterpretation as P;
    match photometric {
        P::Cfa(config) => format!("cfa={}", config.cfa.name),
        P::LinearRaw => "linear".to_string(),
        P::BlackIsZero => "black-is-zero".to_string(),
    }
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
    raw: &mut rawler::RawImage,
    orientation: rawler::decoders::Orientation,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => bin_bayer_quarter_res(raw, DemosaicMode::Fast),
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

/// Multiplies each raw sample by its CFA-position's white-balance
/// coefficient, in place. Pre-demosaic, the same position this file's
/// pre-refactor hand-rolled `normalize` closure applied WB (there it was
/// fused with black/white-normalize; this is its own pass now that
/// black/white-normalize is `apply_scaling()`'s job instead).
///
/// One deliberate behavior change from that closure, per
/// `plans/raw-decode-rapidraw-parity-design.md`: it hard-clamped each sample
/// to `[0,1]` *before* applying `wb`, so nothing above white ever reached the
/// color matrix. `apply_scaling()`'s `correct_blacklevel_cfa` clips negatives
/// only, and this pass doesn't clamp either, so with a real camera's
/// `wb_coeffs` (`[2.0, 1.0, 1.4]`-ish, not all 1.0) samples now legitimately
/// exceed 1.0 and get `apply_cam2rgb`'s `clip_euclidean_norm_avg` soft
/// rolloff instead of a hard pre-clamp. That's the intended pipeline (it's
/// what RapidRaw and rawler's own `RawDevelop` do): a soft highlight rolloff
/// that keeps hue near white, rather than clipping each channel
/// independently before the matrix ever sees it.
///
/// Also inherited from `correct_blacklevel_cfa`, and also matching
/// `RawDevelop`: no divide-by-zero guard on `white - black` (the old
/// closure's `.max(1.0)` on the denominator is gone with the closure). A file
/// claiming `white == black` yields inf/NaN samples — which stay non-fatal
/// (`to_srgb_u8`'s `clamp` passes NaN through and the `as usize` cast
/// saturates to 0), so it's a black/garbage image, never a panic.
///
/// Deliberately *not* a flat `for (idx, v) in samples.iter_mut()` loop with
/// `idx / width` / `idx % width` per sample: on a 24MP sensor that's ~50M
/// integer divisions, and it also touches masked/optical-black rows outside
/// the active area that nothing downstream ever reads. This walks only the
/// active-area rows, hoisting one 48-wide coefficient table per row (48 is
/// `rawler::CFA`'s own pattern-matrix period, so this is exact for any CFA
/// size it supports, X-Trans included) so the inner loop is a multiply and a
/// wrapping counter with no division at all.
///
/// `area` must lie inside `width * height` — `bin_bayer_quarter_res` checks
/// that before calling.
fn apply_white_balance_in_place(
    samples: &mut [f32],
    width: usize,
    area: rawler::imgop::Rect,
    cfa: &rawler::CFA,
    wb: [f32; 4],
) {
    const PERIOD: usize = 48;
    for row in area.p.y..area.p.y + area.d.h {
        let mut coeffs = [1f32; PERIOD];
        for (col, slot) in coeffs.iter_mut().enumerate() {
            // `wb` only has 4 slots; a CYGM/RGBE pattern's color codes run
            // past that, so fall back to 1.0 rather than panicking on an
            // out-of-range index (unreachable for the RGGB-family patterns
            // `is_supported_bayer_layout` lets through, but this must not be
            // the thing that traps a `panic=abort` wasm worker).
            *slot = wb.get(cfa.color_at(row, col)).copied().unwrap_or(1.0);
        }
        let base = row * width;
        let mut phase = area.p.x % PERIOD;
        for v in &mut samples[base + area.p.x..base + area.p.x + area.d.w] {
            *v *= coeffs[phase];
            phase = if phase + 1 == PERIOD { 0 } else { phase + 1 };
        }
    }
}

/// Same idea for already-demosaiced linear data (`cpp == 3`, no CFA — each
/// sample's channel is just its offset within the pixel, cycling R,G,B).
/// Same active-area-only, division-free shape as the CFA version above, for
/// the same reasons; `area` must lie inside `width * height`.
fn apply_white_balance_linear_in_place(
    samples: &mut [f32],
    width: usize,
    cpp: usize,
    area: rawler::imgop::Rect,
    wb: [f32; 4],
) {
    for row in area.p.y..area.p.y + area.d.h {
        let base = (row * width + area.p.x) * cpp;
        for pixel in samples[base..base + area.d.w * cpp].chunks_exact_mut(cpp) {
            for (ch, v) in pixel.iter_mut().enumerate() {
                *v *= wb.get(ch).copied().unwrap_or(1.0);
            }
        }
    }
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

/// Pipeline stage note: the `clip_euclidean_norm_avg` call below is this
/// file's counterpart to RapidRaw's standalone `highlight_compression`
/// stage — a soft highlight rolloff applied right after the color-matrix
/// multiply, same position RapidRaw's version occupies. Folded into this
/// one function rather than split into its own, since there's no separate
/// exposure/gain step in this codebase to share a boundary with (unlike
/// an earlier, discarded attempt at this file that had one — see
/// `plans/raw-decode-rapidraw-parity-design.md`'s reset note).
///
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

/// Whether rawler's 3-channel `Demosaic` impls can actually handle this
/// layout. Both of them *panic* rather than erroring on anything else, and
/// this file's only production target (`wasm32-unknown-unknown`) is
/// `panic=abort`, so an unsupported layout has to be turned away here:
///
/// - `Superpixel3Channel::demosaic` panics when `colors.plane_count() != 3`,
///   panics when `!cfa.is_rgb()`, and hits `_ => unreachable!()` for any RGB
///   CFA whose (ROI-shifted) name isn't exactly one of the four 2x2
///   RGGB-family patterns. Fuji X-Trans clears `is_rgb()` — its 36-char name
///   is all R/G/B — and lands squarely on that `unreachable!()`.
/// - `PPGDemosaic::demosaic` guards only `is_rgb()`, so X-Trans gets past it
///   and into pattern math that assumes a 2x2 tiling.
/// - Both index the sample buffer at `roi`'s coordinates unchecked, so an
///   active area larger than the decoded buffer is a slice-bounds panic.
///
/// The name has to be checked *after* `cfa.shift(roi.p.x, roi.p.y)`, because
/// that's the CFA the demosaic impls actually match on — an odd active-area
/// origin rotates e.g. RGGB into GRBG.
///
/// The pre-refactor hand-rolled loop needed none of this: it used
/// `cfa.color_at()`, which degrades to a color-artifacted (but non-crashing)
/// image on any pattern. Returning `None` here is the replacement for that
/// graceful degradation — a wrong-but-visible image was never the contract,
/// a clear error is.
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

/// `area` lies wholly inside a `width * height` buffer. `checked_add` rather
/// than `+`: `usize` is 32 bits on wasm32, so a garbage active-area tag could
/// wrap a plain addition back into the valid range and defeat the check.
fn area_fits(area: rawler::imgop::Rect, width: usize, height: usize) -> bool {
    area.p.x.checked_add(area.d.w).is_some_and(|x1| x1 <= width)
        && area.p.y.checked_add(area.d.h).is_some_and(|y1| y1 <= height)
}

/// Bayer-CFA preview via rawler's own `Demosaic` trait: `Fast` uses
/// `Superpixel3Channel` (quarter-res 2x2 bin, no interpolation — matches this
/// file's pre-Task-4 hand-rolled output byte-for-byte), `Quality` uses
/// `PPGDemosaic` (full-res, edge-directed interpolation). See `fast_preview`'s
/// doc comment for when this applies, and `is_supported_bayer_layout` for the
/// layouts this turns away rather than handing to a panicking demosaic.
pub(crate) fn bin_bayer_quarter_res(raw: &mut rawler::RawImage, mode: DemosaicMode) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::imgop::sensor::bayer::{ppg::PPGDemosaic, superpixel::Superpixel3Channel, Demosaic};
    use rawler::pixarray::Pix2D;
    use rawler::rawimage::RawPhotometricInterpretation;
    use rawler::RawImageData;

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return None;
    };
    let cfa = config.cfa.clone();
    let colors = config.colors.clone();
    let wb = raw.wb_coeffs;
    let (width, height) = (raw.width, raw.height);

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, height),
    ));
    if !is_supported_bayer_layout(&cfa, &colors, area, width, height) {
        return None;
    }

    let RawImageData::Float(data) = &mut raw.data else {
        return None; // apply_scaling always leaves Float data
    };
    if data.len() != width * height {
        return None;
    }
    apply_white_balance_in_place(data, width, area, &cfa, wb);

    // `mem::take`, not `clone`: the full-resolution sensor buffer is ~96MB on
    // a 24MP RAW, and cloning it doubled peak heap (on wasm32's 32-bit heap
    // especially) plus a full memcpy, for no reason — nothing reads
    // `raw.data` after this point (`build_cam2rgb` below only touches
    // `raw.color_matrix`).
    let pixels = Pix2D::new_with(std::mem::take(data), width, height);

    let demosaiced = match mode {
        DemosaicMode::Fast => Superpixel3Channel::new().demosaic(&pixels, &cfa, &colors, area),
        DemosaicMode::Quality => PPGDemosaic::new().demosaic(&pixels, &cfa, &colors, area),
    };

    let cam2rgb = build_cam2rgb(raw);
    let (w, h) = (demosaiced.width, demosaiced.height);
    let mut rgba = vec![0u8; w * h * 4];
    for (i, &rgb) in demosaiced.pixels().iter().enumerate() {
        let px = render_rgb_sample(rgb, &cam2rgb);
        rgba[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    Some((w as u32, h as u32, rgba))
}

/// Already-demosaiced/linear RGB preview (cpp == 3 — no CFA, no
/// interpolation to do): white balance, color matrix and gamma (per-channel
/// black/white-level normalize already happened upstream, in
/// `apply_scaling()`), plus simple 2x2 nearest-neighbor decimation rather
/// than an averaging box filter — this data has no CFA-driven reason to
/// average 4 samples together.
fn decimate_linear_rgb(raw: &mut rawler::RawImage) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let cpp = raw.cpp;
    let wb = raw.wb_coeffs;
    let (width, height) = (raw.width, raw.height);

    // Computed before the mutable borrow of `raw.data` below for
    // borrow-checker reasons only — `build_cam2rgb` only reads
    // `raw.color_matrix`.
    let cam2rgb = build_cam2rgb(raw);

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, height),
    ));
    // Same reasoning as `is_supported_bayer_layout`'s bounds check: an active
    // area outside the decoded buffer would be a slice-bounds panic below,
    // and `panic=abort` on wasm32 makes that fatal to the whole worker.
    if cpp == 0 || !area_fits(area, width, height) {
        return None;
    }

    let RawImageData::Float(data) = &mut raw.data else {
        return None; // apply_scaling always leaves Float data
    };
    if data.len() != width * height * cpp {
        return None;
    }
    apply_white_balance_linear_in_place(data, width, cpp, area, wb);

    let (x0, y0) = (area.p.x, area.p.y);
    let (out_w, out_h) = (area.d.w / 2, area.d.h / 2);
    let mut rgba = vec![0u8; out_w * out_h * 4];

    for oy in 0..out_h {
        for ox in 0..out_w {
            let (row, col) = (y0 + oy * 2, x0 + ox * 2);
            let base = (row * width + col) * cpp;
            let idx = (oy * out_w + ox) * 4;
            let mut rgb = [0f32; 3];
            for (ch, slot) in rgb.iter_mut().enumerate() {
                *slot = data[base + ch];
            }
            let px = render_rgb_sample(rgb, &cam2rgb);
            rgba[idx..idx + 4].copy_from_slice(&px);
        }
    }

    Some((out_w as u32, out_h as u32, rgba))
}
