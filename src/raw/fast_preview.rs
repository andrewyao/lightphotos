// SPDX-License-Identifier: GPL-3.0-or-later

//! Two RAW preview tiers, both dispatched off `bin_bayer_quarter_res`'s and
//! `decimate_linear_rgb`'s `mode: DemosaicMode` parameter — wired to
//! `wasm_worker.rs`'s two job kinds (Grid `Thumb` vs. Loupe `Preview`):
//!
//! - `Fast` (Grid/`Thumb`): quarter-resolution Bayer binning (or decimation
//!   for already-linear sensor data) instead of full PPG demosaic, with
//!   output baked to sRGB8 (`PixelFormat::Srgb8`) via a CPU lookup table
//!   (real sRGB gamma plus a display brightness/contrast boost). Ported from
//!   `tools/wasm-decode-probe`'s throwaway spike (`wasm-decode-probe-spike`
//!   branch, `src/lib.rs`), where it measured ~6.3x faster than
//!   `RawDevelop`'s default `Quality`/PPG path — fast enough that the wasm
//!   port plan picked it as the interactive-browsing default.
//! - `Quality` (Loupe/`Preview`): rawler's full-res `PPGDemosaic` — real
//!   edge-directed interpolation. Measured 4-5x *slower* than native in that
//!   same spike, which is fine here since it's one photo at a time, not a
//!   grid flood. Output stops at linear camera-RGB (`PixelFormat::LinearF16`,
//!   no gamma/boost baked in) — `renderer.rs` uploads it as an `Rgba16Float`
//!   texture and `raw_shader.wgsl` (not the CPU LUT) does the sRGB gamma plus
//!   boost on the GPU, a two-stage CPU-decode/GPU-tonemap split.
//!
//! `#[cfg(not(target_os = "macos"))]`, like the rest of the non-mac RAW
//! decode: this isn't wasm32-specific code, it's just currently only wired
//! into the app via wasm32's decode path (`app/web.rs`) — nothing stops a
//! future native non-mac caller from using it too. Exception: the
//! bytes-based entry points are also gated on `feature = "raw-probe"` so
//! `decode_probe.rs` can call them from a mac dev build (see their own
//! comments for why).

// ## Pipeline position
// - `decode_raw_fast_from_bytes` is Pipeline 1's/Pipeline 2's wasm32
//   fallback for a RAW file: `wasm_worker.rs`'s `decode` calls it when
//   `quality` is `false` (Grid `JobKind::Thumb`, and the Loupe's first-paint
//   `JobKind::Speed`), after the cheap embedded-preview extractors have
//   already been tried and failed.
// - `decode_raw_quality_from_bytes` is the same fallback for
//   `quality == true` (Loupe `JobKind::Preview`/`Full`) — the tier that
//   feeds `renderer.rs`'s `raw_shader.wgsl` path.
// - Neither function runs on macOS or native Linux/Windows — those
//   platforms' RAW decode goes through `raw/nonmac_decode.rs`'s
//   `decode_raw_nonmac` (native) or ImageIO (mac) instead.
// - See `ARCHITECTURE.md`.

use crate::image_decode::{fit_within, DecodedImage, PixelFormat};

/// `Fast` = rawler's `Superpixel3Channel` (quarter-res 2x2 bin, matches this
/// file's earlier hand-rolled output), wired to Grid/`JobKind::Thumb` jobs,
/// output `PixelFormat::Srgb8`. `Quality` = rawler's `PPGDemosaic` (full-res,
/// real edge-directed interpolation — the same algorithm the native non-mac
/// path, `decode_raw_nonmac`, already uses via `RawDevelop`), wired to
/// Loupe/`JobKind::Preview` jobs via `decode_raw_quality_from_bytes`, output
/// `PixelFormat::LinearF16`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DemosaicMode {
    Fast,
    Quality,
}

impl DemosaicMode {
    /// Bytes per pixel this mode's `bin_bayer_quarter_res`/`decimate_linear_rgb`
    /// write: `Fast` → 4 (u8 sRGB RGBA), `Quality` → 8 (`half::f16` linear
    /// RGBA). Drives both the output buffer's allocation size and
    /// `apply_orientation`'s byte-swap stride, so the two never drift apart.
    fn bytes_per_pixel(self) -> usize {
        match self {
            DemosaicMode::Fast => 4,
            DemosaicMode::Quality => 8,
        }
    }
}

/// Precomputed sRGB-gamma + display-boost lookup table, built once on first
/// use. Worth explaining why this exists: the early spike found both
/// algorithms below landed at the same ~300-320ms/megapixel regardless of
/// approach, and swapping three per-pixel gamma-function calls for an array
/// index was what actually moved the needle — most of the measured 6.3x
/// speedup came from this LUT, not either demosaic algorithm's own work. The
/// LUT chains two independent curves: `rawler`'s own `srgb_apply_gamma` (the
/// real piecewise sRGB transfer function, replacing a flat `1/2.2`
/// approximation this file used before) and then
/// `image_decode::apply_raw_preview_boost` (a brightness/contrast display
/// transform — without it, a linear-matrix RAW conversion looks flatter and
/// darker than a camera JPEG, by design). The LUT-as-perf-trick and the
/// curves it encodes are separate decisions; `decode_raw_nonmac` (native
/// non-mac) applies the exact same two curves through its own LUT, since it
/// can't share this `OnceLock`'d one (different cfg gate, different call
/// shape).
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

/// Decodes a RAW file's bytes into the quarter-res `Fast`/sRGB8 preview
/// (`DemosaicMode::Fast`), resized to fit `max_px` (Lanczos3, same as every
/// other decode path in this crate — see
/// `image_decode::decode_jpeg_png_tiff_from_bytes`). Grid-only — the Loupe
/// calls [`decode_raw_quality_from_bytes`] instead. Its name, signature, and
/// output shape are kept stable across the `Fast`/`Quality` split below,
/// since `decode_probe.rs`'s golden-hash regression tests call it by name.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Fast)
}

/// Decodes a RAW file's bytes into the `Quality`/PPG preview
/// (`DemosaicMode::Quality`) — see this module's doc comment for what that
/// means. Loupe-only. Output is `PixelFormat::LinearF16`, resized to fit
/// `max_px` via [`resize_linear_f16`] (a box-filter downsample on
/// linear-light data — actually more correct than resizing after gamma
/// encoding, not just a workaround). The `image` crate's own Lanczos3 resize
/// can't represent `half::f16` data, so this needed its own implementation —
/// but it still has to happen: an *unbounded* full-native-resolution
/// `Quality` upload once broke the Loupe's zoom transform. The Loupe's
/// `zoom` is screen-px-per-image-px, and a same-photo sharper-tier upload
/// deliberately reuses it rather than refitting (`app/thumbs.rs::upload_shown`)
/// — correct only when the landing tiers are close in resolution, which
/// `Fast` (already bounded to `max_px`) and an unbounded `Quality` are not.
/// The pixel-dimension jump between them made the reused `zoom` show a
/// wrongly zoomed-in crop the instant `Quality` landed.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_quality_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Quality)
}

/// Shared body behind [`decode_raw_fast_from_bytes`]/[`decode_raw_quality_from_bytes`].
//
// Gated on `feature = "raw-probe"` as well as `not(target_os = "macos")` so
// `decode_probe.rs`'s golden-hash regression tests can call this same
// function from a mac dev build, via `cargo test --bin decode_probe
// --features raw-probe` — same reasoning and pattern as
// `image_decode.rs`'s `decode_raw_via_rawler` (see that function's doc
// comment). Note this module isn't declared in `main.rs` at all, so it never
// reaches the *main* `lightphotos` binary: the only two things that pull it
// in are `wasm_worker.rs` (wasm32-gated — the sole production caller) and
// `decode_probe.rs`'s own `#[path]` re-declaration.
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

    let mut raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rawler::decode(&source, &params)))
        .map_err(|_| "panicked during RAW decode".to_string())?
        .map_err(|e| e.to_string())?;

    // `apply_scaling()` hits a bare `todo!()` for `BlackIsZero` (rawler
    // 0.7.2, `rawimage.rs:510`) — that's a panic, not an error. The
    // `catch_unwind` guards in this function only actually help on native:
    // the sole production target here is `wasm32-unknown-unknown`, which
    // builds with `panic=abort`, so *any* panic below takes down the whole
    // decode worker with no recoverable error. Every unsupported layout
    // therefore has to be turned away by an explicit check before the
    // offending call, not caught after the fact — see
    // `is_supported_bayer_layout` for the same reasoning applied to the
    // `Demosaic` impls.
    if !matches!(
        raw.photometric,
        RawPhotometricInterpretation::Cfa(_) | RawPhotometricInterpretation::LinearRaw
    ) {
        return Err(format!("unsupported RAW layout ({})", describe_photometric(&raw.photometric)));
    }

    let (w, h, rgba) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        raw.apply_scaling().map_err(|e| e.to_string())?;
        fast_preview(&mut raw, orientation, mode, max_px).ok_or_else(|| {
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

/// A short, human-readable tag for an error message — `RawPhotometricInterpretation`
/// isn't `Display`, and its `Debug` for the `Cfa` case would dump the whole
/// 48x48 pattern matrix. The CFA's own name (e.g. `"RGGB"`, or X-Trans's
/// 36-char name) is the part that's actually worth showing.
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
/// `rawimage.rs:389,478` — a real upstream bug, not guessed at). Works
/// around it with a separate `raw_metadata()` call that reads
/// `.exif.orientation` directly instead.
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
/// cpp == 4) rather than guessing at a wrong image. `mode` picks both the
/// demosaic algorithm and the output pixel encoding (see `DemosaicMode`'s own
/// doc comment) — threaded straight through to both branches and to
/// `apply_orientation`'s byte stride.
fn fast_preview(
    raw: &mut rawler::RawImage,
    orientation: rawler::decoders::Orientation,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => bin_bayer_quarter_res(raw, mode, max_px),
        3 => decimate_linear_rgb(raw, mode, max_px),
        _ => None,
    }?;
    Some(apply_orientation(orientation, w, h, rgba, mode.bytes_per_pixel()))
}

/// Applies `rawler::decoders::Orientation`'s flip/transpose to a tightly
/// packed pixel buffer — the sensor buffer is always laid out in the
/// camera's native orientation regardless of how the photo was actually
/// held. Per `Orientation::to_flips`'s own doc comment, flips must happen
/// before the transpose for a correct result. `bpp` (bytes/pixel — 4 for
/// `Fast`'s sRGB8, 8 for `Quality`'s linear f16, see `DemosaicMode::
/// bytes_per_pixel`) is the only layout assumption this needs: every swap/
/// copy below is otherwise encoding-agnostic.
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

/// White balance is applied to demosaiced RGB below, matching rawler's
/// `RawDevelop` ordering. It must not be applied to the mosaic before PPG,
/// because PPG's edge decisions are channel-dependent.
///
/// One deliberate behavior change from that older closure: it hard-clamped
/// each sample to `[0,1]` *before* applying `wb`, so nothing above white
/// ever reached the color matrix. `apply_scaling()`'s `correct_blacklevel_cfa`
/// only clips negatives, and this pass doesn't clamp either, so with a real
/// camera's `wb_coeffs` (something like `[2.0, 1.0, 1.4]`, not all 1.0)
/// samples now legitimately exceed 1.0 and get `apply_cam2rgb`'s
/// `clip_euclidean_norm_avg` soft rolloff instead of a hard pre-clamp.
/// That's the intended pipeline, matching rawler's own `RawDevelop`: a soft
/// highlight rolloff that keeps hue near white, rather than clipping each
/// channel independently before the matrix ever sees it.
///
/// Also inherited from `correct_blacklevel_cfa`, and also matching
/// `RawDevelop`: there's no divide-by-zero guard on `white - black` (the old
/// closure's `.max(1.0)` on the denominator went away with the closure). A
/// file claiming `white == black` yields inf/NaN samples — which stay
/// non-fatal (`to_srgb_u8`'s `clamp` passes NaN through and the `as usize`
/// cast saturates to 0), so worst case it's a black/garbage image, never a
/// panic.
///
/// Deliberately *not* a flat `for (idx, v) in samples.iter_mut()` loop with
/// `idx / width` / `idx % width` per sample: on a 24MP sensor that's ~50M
/// integer divisions, and it would also touch masked/optical-black rows
/// outside the active area that nothing downstream ever reads. This walks
/// only the active-area rows, hoisting one 48-wide coefficient table per row
/// (48 is `rawler::CFA`'s own pattern-matrix period, so this is exact for
/// any CFA size it supports, X-Trans included), so the inner loop is just a
/// multiply and a wrapping counter with no division at all.
///
/// Box-filter downsample of a `DemosaicMode::Quality` linear `half::f16`
/// RGBA buffer to fit `max_px` — the f16-data equivalent of
/// `image::imageops::resize`, which can't represent this layout. Used by
/// [`decode_raw_quality_from_bytes`] (see its own doc comment for why this
/// has to happen at all, not just how). A no-op — returns `rgba` unchanged —
/// when `(w, h)` already fits, same contract as `fit_within`'s own callers.
///
/// Same box-average shape as `image_ops.rs`'s `resize_luma` (integer source
/// range per destination pixel, exact for any ratio), but averaging
/// *linear-light* samples here, which is the photometrically correct thing
/// to do — averaging already-gamma-encoded u8 values instead darkens edges.
/// So this isn't only a workaround for the missing library resize; it's
/// arguably better than the `Fast` tier's post-gamma Lanczos3 resize.
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

/// `area` must lie inside `width * height` — `bin_bayer_quarter_res` checks
/// that before calling.
/// Camera-RGB → sRGB matrix, built once per image. Same algorithm `rawler`'s
/// own `imgop::raw::map_3ch_to_rgb` uses internally — that function is
/// `pub(crate)`-restricted inside `rawler`, so it's not callable from here,
/// and this just replicates its ~10-line body from the public primitives
/// it's built from. Returns `None` if `raw.color_matrix` has no calibration
/// data for this camera; callers then fall back to WB-only output, same as
/// `RawDevelop`'s own behavior on a missing matrix.
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

/// Pipeline stage note: the `clip_euclidean_norm_avg` call below is a soft
/// highlight rolloff applied right after the color-matrix multiply. It's
/// folded into this one function rather than split into its own, since
/// there's no separate exposure/gain step in this codebase to share a
/// boundary with.
///
/// Applies `build_cam2rgb`'s matrix to one WB-corrected camera-RGB sample,
/// including that soft highlight-rolloff clip (`clip_euclidean_norm_avg`)
/// rather than a hard `.clamp` — it behaves better near white.
fn apply_cam2rgb(cam2rgb: &[[f32; 4]; 3], rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let srgb = [
        cam2rgb[0][0] * r + cam2rgb[0][1] * g + cam2rgb[0][2] * b,
        cam2rgb[1][0] * r + cam2rgb[1][1] * g + cam2rgb[1][2] * b,
        cam2rgb[2][0] * r + cam2rgb[2][1] * g + cam2rgb[2][2] * b,
    ];
    rawler::imgop::raw::clip_euclidean_norm_avg(&srgb)
}

/// Renders one linear camera-RGB sample (already white-balanced, already
/// black/white-level normalized by `raw.apply_scaling()` before demosaic) to
/// display RGBA8: color matrix (if the camera has calibration data) -> gamma,
/// alpha fixed opaque. Shared by both `bin_bayer_quarter_res` and
/// `decimate_linear_rgb` — used to be duplicated inline in both.
fn render_rgb_sample(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 4] {
    let srgb = match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    };
    [to_srgb_u8(srgb[0]), to_srgb_u8(srgb[1]), to_srgb_u8(srgb[2]), 255]
}

/// `render_rgb_sample`'s counterpart for `DemosaicMode::Quality`: color
/// matrix (including the highlight rolloff `apply_cam2rgb` already applies),
/// same as above, but it stops there — no gamma, no
/// `image_decode::apply_raw_preview_boost`, no u8 quantize, no exposure gain
/// (see `render_rgb_sample`'s doc comment for why there's no gain multiply
/// here either). Feeds `renderer.rs`'s `Rgba16Float` texture; `raw_shader.wgsl`
/// does the real sRGB gamma plus display brightness/contrast boost on the GPU
/// instead — a CPU-decode/GPU-tonemap split.
fn render_rgb_sample_linear(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [f32; 3] {
    match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    }
}

/// [`render_rgb_sample_linear`], packed as 8 little-endian `half::f16` bytes
/// (R, G, B, A — alpha always opaque) ready for a direct `Vec<u8>` buffer
/// write, the same shape `render_rgb_sample`'s `[u8; 4]` serves for `Fast`.
fn render_rgb_sample_linear_bytes(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 8] {
    let [r, g, b] = render_rgb_sample_linear(rgb, cam2rgb);
    let mut out = [0u8; 8];
    out[0..2].copy_from_slice(&half::f16::from_f32(r).to_le_bytes());
    out[2..4].copy_from_slice(&half::f16::from_f32(g).to_le_bytes());
    out[4..6].copy_from_slice(&half::f16::from_f32(b).to_le_bytes());
    out[6..8].copy_from_slice(&half::f16::from_f32(1.0).to_le_bytes());
    out
}

/// Whether rawler's 3-channel Bayer `Demosaic` impls can actually handle this
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
/// The earlier hand-rolled loop needed none of this: it used
/// `cfa.color_at()`, which degrades to a color-artifacted (but non-crashing)
/// image on any pattern. Returning `None` here replaces that graceful
/// degradation — a wrong-but-visible image was never the contract we want; a
/// clear error is.
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

// TODO(x-trans): this file's X-Trans path (this function,
// `downsample_xtrans_mosaic`, `map_xtrans_coord` below) still has open
// correctness findings from roborev review history on this file: (1)
// `map_xtrans_coord` doesn't correctly translate an active-area origin that
// falls inside a skipped tile into reduced-space, so a Fuji file with a
// nonzero active-area offset can crop more of the first CFA tile than it
// should; (2) `bin_bayer_quarter_res` runs X-Trans data through
// `RawImage::apply_scaling()`, which calls the Bayer-only
// `correct_blacklevel_cfa` — that collapses X-Trans's 6x6 per-phase
// black-level table down to a single repeated value, losing per-phase black
// level and risking pattern noise/color casts; (3) `downsample_xtrans_mosaic`
// clamps a partial trailing block's out-of-range coordinates to the last
// valid row/column, which is generally a different CFA phase than the output
// position it's filling in, corrupting color along the bottom/right edge.
// Deferred, not blocking: `thumbnail.rs`'s `rawler_full_image_from_bytes`
// (rawler's own `Decoder::full_image()`) now covers the common case for RAF
// files ahead of this path — this demosaic code only still runs when a RAF
// has no usable embedded image (corrupted file, decode error) — but it
// remains genuinely wrong on those inputs when it does run.
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
        assert_eq!(neutral_if_non_finite([2.0, 1.0, 0.5, 1.0]), [2.0, 1.0, 0.5, 1.0]);
    }

    #[test]
    fn xtrans_bounded_sampling_preserves_all_cfa_phases() {
        // Column-independent so each output column's average collapses to
        // the same expression as the row it belongs to.
        let (width, height) = (72, 72);
        let source: Vec<f32> = (0..width * height).map(|index| (index / width) as f32).collect();
        let (reduced, reduced_width, reduced_height) =
            downsample_xtrans_mosaic(&source, width, height, 12);

        assert_eq!((reduced_width, reduced_height), (12, 12));

        // reduction = 6, tile_step = 36: tile_row 0's phase_row `p` averages
        // source rows {p, p+6, .., p+30}; tile_row 1's averages {36+p, ..,
        // 66+p}. Confirms the fix aggregates every sub-tile in the block
        // (the bug only ever read the block's first sub-tile, i.e. row `p`
        // alone).
        for phase_row in 0..6 {
            let block0_mean: f32 = (0..6).map(|i| (phase_row + i * 6) as f32).sum::<f32>() / 6.0;
            let block1_mean: f32 = (0..6).map(|i| (36 + phase_row + i * 6) as f32).sum::<f32>() / 6.0;
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

/// `area` lies wholly inside a `width * height` buffer. `checked_add` rather
/// than `+`: `usize` is 32 bits on wasm32, so a garbage active-area tag could
/// wrap a plain addition back into the valid range and defeat the check.
fn area_fits(area: rawler::imgop::Rect, width: usize, height: usize) -> bool {
    area.p.x.checked_add(area.d.w).is_some_and(|x1| x1 <= width)
        && area.p.y.checked_add(area.d.h).is_some_and(|y1| y1 <= height)
}

fn map_xtrans_coord(coord: usize, tile_step: usize) -> usize {
    let tile = coord / tile_step;
    let offset = coord % tile_step;
    tile * 6 + if offset < 6 { offset } else { 6 }
}

/// Bayer-CFA preview via rawler's own `Demosaic` trait: `Fast` uses
/// `Superpixel3Channel` (quarter-res 2x2 bin, no interpolation — matches this
/// file's earlier hand-rolled output byte-for-byte), `Quality` uses
/// `PPGDemosaic` (full-res, edge-directed interpolation). See `fast_preview`'s
/// doc comment for when this applies, and `is_supported_bayer_layout` for the
/// layouts this turns away rather than handing to a panicking demosaic.
pub(crate) fn bin_bayer_quarter_res(
    raw: &mut rawler::RawImage,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::imgop::sensor::{bayer::{ppg::PPGDemosaic, superpixel::Superpixel3Channel, Demosaic}, xtrans::xtrans_fast::XtransFastDemosaic};
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
    // Captured before `area` is potentially reassigned below (X-Trans
    // reduced-space remapping) — `demosaiced`'s own buffer origin sits at
    // this rect's top-left (it was demosaiced against `area` as its ROI, at
    // whatever `area` was *then*, which for the non-X-Trans case never
    // changes anyway), and the `crop_area` handling further down needs the
    // true active-area rect to `adapt()` against, matching what `RawDevelop`
    // itself adapts crop_area to.
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
    // `mem::take`, not `clone`: the full-resolution sensor buffer is ~96MB on
    // a 24MP RAW, and cloning it would double peak heap (especially painful
    // on wasm32's 32-bit heap) plus cost a full memcpy, for no reason —
    // nothing reads `raw.data` after this point (`build_cam2rgb` below only
    // touches `raw.color_matrix`).
    let pixels = if is_xtrans {
        let max_dim = width.max(height);
        let requested = max_px.max(1) as usize;
        // Preserve every position in each 6x6 CFA tile while averaging
        // equivalent phases across each skipped region (see
        // `downsample_xtrans_mosaic`'s own doc comment). A pixel stride
        // rounded to six would keep one CFA phase and corrupt the mosaic.
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
        // Return the first reduced coordinate whose retained source coordinate
        // is at or after `coord`. Coordinates in the skipped part of a tile
        // must advance to the next retained tile, rather than rounding down
        // and accidentally including optical-border samples.
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
        XtransFastDemosaic::new().demosaic(&pixels, &cfa, &colors, area)
    } else {
        match mode {
            DemosaicMode::Fast => Superpixel3Channel::new().demosaic(&pixels, &cfa, &colors, area),
            DemosaicMode::Quality => PPGDemosaic::new().demosaic(&pixels, &cfa, &colors, area),
        }
    };

    // `RawDevelop`'s `CropDefault` step (native's own pipeline, `RawDevelop::
    // default()`) crops to `raw.crop_area.or(active_area)` after demosaic —
    // the DNG "recommended default display" rectangle, usually a touch
    // tighter than the full active area (trims a residual sensor-edge/mask
    // margin the active-area crop alone doesn't). This file only ever
    // cropped to `active_area` (as the demosaic ROI, matching `RawDevelop`'s
    // separate `CropActiveArea` step) and never applied this second, tighter
    // crop at all — confirmed via a real Sony ARW to make the wasm32 Loupe
    // measurably darker than native/Linux even after the embedded-JPEG
    // substitution bug (a different issue) was fixed, since that extra
    // margin's typically-darker pixels get averaged into the GPU's mip chain
    // whenever the fitted view is downscaled. Mirrors `RawDevelop`'s own
    // `adapt`/`scale(0.5)` logic exactly (`vendor/rawler-0.7.2/src/imgop/
    // develop.rs`'s `CropDefault` block): `crop_area` is in full-sensor
    // coordinates, `adapt`ed to `original_active_area`-relative ones since
    // that's where `demosaiced`'s own origin sits (it was demosaiced against
    // `area` as its ROI), then halved if this is the quarter-res `Fast`
    // superpixel bin (X-Trans and `Quality`/PPG are both full-res, no scale).
    // X-Trans excluded: its ROI passed to `.demosaic()` above is `area` as
    // reassigned into *reduced* (downsampled) space a few lines up, not
    // `original_active_area` — adapting `crop_area` against the wrong
    // reference rect would misplace the crop entirely. X-Trans's demosaic
    // path already has its own open, separately-tracked correctness issues
    // (see the TODO on `is_supported_xtrans_layout`); not compounding that
    // here.
    let demosaiced = match (!is_xtrans).then(|| raw.crop_area.or(Some(original_active_area))).flatten() {
        Some(mut crop) if crop.d != rawler::imgop::Dim2::new(demosaiced.width, demosaiced.height) => {
            crop = crop.adapt(&original_active_area);
            if mode == DemosaicMode::Fast {
                crop.scale(0.5);
            }
            // Clamp rather than trust the file: a crop rect that doesn't
            // actually fit inside what got demosaiced (bad/inconsistent
            // metadata) must degrade to "no crop", not panic or produce a
            // nonsensical sub-rect.
            let fits = crop.p.x + crop.d.w <= demosaiced.width && crop.p.y + crop.d.h <= demosaiced.height;
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

    // Automatic decode-time denoise — see `AUTO_RAW_DENOISE_STRENGTH`'s own
    // doc comment (`raw/nonmac_decode.rs`) for why this exists. `Quality`
    // only, applied here to the demosaiced camera-native samples
    // (pre-white-balance, pre-color-matrix) rather than after rendering —
    // the earliest linear-light point available, and (unlike
    // `decode_raw_nonmac`'s post-hoc pass on a finished sRGB image) no
    // gamma round-trip needed since this data is already linear. `Fast`
    // skips this: its quarter-resolution 2x2 bin already gets free noise
    // reduction from averaging.
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

/// For each retained 6x6 CFA tile, averages every same-phase sample across
/// the whole `reduction`x`reduction` grid of sub-tiles the tile stands in
/// for, rather than reading only the tile's own single sub-tile position.
/// Point-sampling one sub-tile per block (the previous approach) concatenated
/// 6-pixel strips from source positions `reduction`x6 pixels apart directly
/// against each other, discarding everything between them — real image
/// content, not just high-frequency detail — which showed up as periodic
/// geometric distortion/banding in the output. Averaging keeps the CFA phase
/// at each output position correct (every contributor shares that phase, six
/// rows/columns apart) while actually representing the region it stands in
/// for. `.min(height - 1)`/`.min(width - 1)` clamps a partial trailing block
/// to its last valid row/column, same as the point-sample version did — a
/// harmless edge duplication, not a correctness issue.
fn downsample_xtrans_mosaic(source: &[f32], width: usize, height: usize, max_px: u32) -> (Vec<f32>, usize, usize) {
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

/// Already-demosaiced/linear RGB preview (cpp == 3 — no CFA, no
/// interpolation to do): white balance, color matrix and gamma (per-channel
/// black/white-level normalize already happened upstream, in
/// `apply_scaling()`), plus simple 2x2 nearest-neighbor decimation rather
/// than an averaging box filter — this data has no CFA-driven reason to
/// average 4 samples together. `mode` has no demosaic-algorithm effect here
/// (there's no CFA to interpolate), only an output-encoding one — same
/// `Fast`/`Quality` branch `bin_bayer_quarter_res` makes.
fn decimate_linear_rgb(
    raw: &mut rawler::RawImage,
    mode: DemosaicMode,
    max_px: u32,
) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let cpp = raw.cpp;
    let wb = neutral_if_non_finite(raw.wb_coeffs);
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

    // Accumulate into a linear-light buffer first, render in a second pass —
    // split out (rather than rendering inline, the way this loop used to)
    // so `Quality`'s automatic denoise below has a whole-buffer neighbor
    // lookup to run against; `Fast` pays the extra `Vec` but no extra math.
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
                    let base = ((y0 as usize + sy) * width as usize + x0 as usize + sx) * cpp as usize;
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

    // Automatic decode-time denoise — see `AUTO_RAW_DENOISE_STRENGTH`'s own
    // doc comment (`raw/nonmac_decode.rs`) and `bin_bayer_quarter_res`'s
    // matching comment for why this exists and why `Fast` skips it. Applied
    // here after white balance (unlike `bin_bayer_quarter_res`, which
    // denoises before it) since this loop already folds `wb` into the
    // accumulation above — still before the color matrix, still linear
    // light, so the difference doesn't change what the filter is smoothing.
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
