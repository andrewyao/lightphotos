// SPDX-License-Identifier: MIT OR Apache-2.0

//! The develop model — the single source of truth for non-destructive edits.
//!
//! [`Adjustments`] is the persisted/edited value (serde, stored in the catalog).
//! [`GpuAdjust`] is its packed `#[repr(C)]` mirror, uploaded as a uniform.
//! [`apply_linear`] is the CPU tone pipeline used for the live histogram; it is a
//! faithful mirror of the WGSL fragment shader.
//!
//! Ranges: tone sliders are −100..=100 with 0 = identity; exposure is −5..=5 stops
//! with 0 = identity. `Adjustments::default()` is the identity edit, which
//! `is_identity()` reports so the catalog can skip serializing it.

use serde::{Deserialize, Serialize};

/// Inclusive range for the −100..=100 tone sliders (temp/tint/contrast/etc.).
pub const TONE_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
/// Inclusive range for exposure, in stops.
pub const EXPOSURE_RANGE: std::ops::RangeInclusive<f32> = -5.0..=5.0;
/// Inclusive range for denoise strength (one-directional: 0 = off).
pub const DENOISE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=100.0;

/// True when a serde-skippable f32 field is at its identity value.
fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// A normalized crop rectangle (0..1 of the image, origin top-left). Reserved for
/// a future crop phase — there is no crop UI yet, but the field/type exist so the
/// catalog schema and uniform layout don't need re-migrating later.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// The non-destructive Basic-tone edit applied to one image.
///
/// Every f32 field is `skip_serializing_if` zero so an identity image serializes
/// to an empty object; combined with `is_identity()` the catalog omits it entirely.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub struct Adjustments {
    /// White-balance temperature, −100 (cool) ..=100 (warm).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub temp: f32,
    /// White-balance tint, −100 (green) ..=100 (magenta).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tint: f32,
    /// Exposure in stops, −5..=5.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exposure: f32,
    /// Contrast, −100..=100 (S-curve strength about mid-gray).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub contrast: f32,
    /// Highlights, −100 (recover) ..=100 (lift).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub highlights: f32,
    /// Shadows, −100 (deepen) ..=100 (lift).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shadows: f32,
    /// Whites endpoint, −100..=100.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub whites: f32,
    /// Blacks endpoint, −100..=100.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub blacks: f32,
    /// Vibrance: adaptive saturation, weighted toward less-saturated pixels
    /// (protects skin tones / already-vivid colors), −100..=100.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vibrance: f32,
    /// Saturation: uniform chroma scale, −100 (grayscale) ..=100 (max).
    #[serde(default, skip_serializing_if = "is_zero")]
    pub saturation: f32,
    /// Edge-aware denoise strength, 0 (off) ..=100.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denoise: f32,
    /// Reserved crop rectangle for a future phase; never set by current UI.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
}

impl Adjustments {
    /// True when this edit is the identity (all tone fields zero, no crop). Used
    /// by the catalog's `skip_serializing_if` to keep identity images out of the
    /// file. Compares against the derived default so new fields can never be
    /// forgotten here.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// A copy carrying only the tone sliders — crop is dropped. Used to copy
    /// develop settings from one photo onto others without touching their crops
    /// (the roadmap: "not cropping adjustment, only the sliders").
    pub fn tone_only(&self) -> Adjustments {
        Adjustments {
            crop: None,
            ..*self
        }
    }
}

/// A stable 64-bit signature of an image's rendered edits (tone + crop + manual
/// 90° rotation). Used as part of the thumbnail-texture cache key so a thumbnail
/// is invalidated and re-baked whenever any edit changes. Floats are quantized so
/// sub-threshold jitter doesn't churn the cache, and an identity edit (no tone, no
/// crop, `rot == 0`) always yields the same value so unedited photos share one key.
pub(crate) fn edit_signature(adj: &Adjustments, rot: u8) -> u64 {
    use crate::hash::Fnv1a;

    let mut h = Fnv1a::new();

    // Tone: eleven sliders (−100..=100, −5..=5, or 0..=100), quantized to 1e-3.
    let tone = [
        adj.temp,
        adj.tint,
        adj.exposure,
        adj.contrast,
        adj.highlights,
        adj.shadows,
        adj.whites,
        adj.blacks,
        adj.vibrance,
        adj.saturation,
        adj.denoise,
    ];
    for v in tone {
        h.write(&((v * 1000.0).round() as i32).to_le_bytes());
    }

    // Crop: normalize a full-frame `Some` to `None` so it hashes like no crop,
    // then quantize the four normalized coords to 1e-5.
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 100_000.0).round()) as u32;
    match adj.crop {
        Some(c) if c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0 => {
            h.write(&[1]); // crop present
            for v in [c.left, c.top, c.right, c.bottom] {
                h.write(&q(v).to_le_bytes());
            }
        }
        _ => h.write(&[0]), // no crop / full-frame
    }

    // Manual rotation in 90° steps.
    h.write(&[rot % 4]);

    h.finish()
}

/// Packed uniform mirror of [`Adjustments`], uploaded to the fragment shader.
///
/// 20 × f32 = 80 bytes; uniform buffers require 16-byte alignment, so keep the
/// field count a multiple of 4 if you ever add fields. `texel_w`/`texel_h` are
/// not mirrored from any `Adjustments` field — they're the shown image's
/// per-texel UV size (1/width, 1/height), filled in by `App::gpu_adjust` so
/// the shader's denoise taps can offset by whole texels. `_pad0`/`_pad1` are
/// unused, just alignment filler.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuAdjust {
    pub exposure: f32,
    pub contrast: f32,
    pub highlights: f32,
    pub shadows: f32,
    pub whites: f32,
    pub blacks: f32,
    pub temp: f32,
    pub tint: f32,
    pub crop_l: f32,
    pub crop_t: f32,
    pub crop_r: f32,
    pub crop_b: f32,
    pub denoise: f32,
    pub vibrance: f32,
    pub saturation: f32,
    pub texel_w: f32,
    pub texel_h: f32,
    pub _pad0: f32,
    pub _pad1: f32,
    pub _pad2: f32,
}

impl Default for GpuAdjust {
    /// Identity: all tone zero, crop covering the full frame.
    fn default() -> Self {
        Self {
            exposure: 0.0,
            contrast: 0.0,
            highlights: 0.0,
            shadows: 0.0,
            whites: 0.0,
            blacks: 0.0,
            temp: 0.0,
            tint: 0.0,
            crop_l: 0.0,
            crop_t: 0.0,
            crop_r: 1.0,
            crop_b: 1.0,
            denoise: 0.0,
            vibrance: 0.0,
            saturation: 0.0,
            texel_w: 1.0,
            texel_h: 1.0,
            _pad0: 0.0,
            _pad1: 0.0,
            _pad2: 0.0,
        }
    }
}

impl From<&Adjustments> for GpuAdjust {
    /// `texel_w`/`texel_h` are left at their `Default` placeholder here — the
    /// real per-image value is filled in by `App::gpu_adjust`, since this impl
    /// only sees `Adjustments`, not the shown image's pixel dimensions.
    fn from(a: &Adjustments) -> Self {
        let (crop_l, crop_t, crop_r, crop_b) = match a.crop {
            Some(c) => (c.left, c.top, c.right, c.bottom),
            None => (0.0, 0.0, 1.0, 1.0),
        };
        Self {
            exposure: a.exposure,
            contrast: a.contrast,
            highlights: a.highlights,
            shadows: a.shadows,
            whites: a.whites,
            blacks: a.blacks,
            temp: a.temp,
            tint: a.tint,
            crop_l,
            crop_t,
            crop_r,
            crop_b,
            denoise: a.denoise,
            vibrance: a.vibrance,
            saturation: a.saturation,
            ..Self::default()
        }
    }
}

// MUST stay in sync with the fs_main pipeline in shader.wgsl
//
/// Apply the tone pipeline to one linear-light RGB pixel (0..1+; values may
/// exceed 1.0 mid-pipeline). Returns linear RGB clamped to 0..1.
///
/// This is the CPU mirror of the WGSL fragment shader (Task 2 writes the GPU
/// copy). Fidelity to Lightroom exactly is NOT required — plausible and
/// well-behaved is the goal. Keep it simple so the two copies stay in lockstep.
pub fn apply_linear(adj: &Adjustments, rgb: [f32; 3]) -> [f32; 3] {
    let [mut r, mut g, mut b] = rgb;

    // 1. White balance: turn temp/tint (−100..100) into gentle per-channel
    //    linear gains. Warm (temp>0) raises R and lowers B; tint>0 shifts toward
    //    magenta (lowers G), tint<0 toward green (raises G). Coefficients are
    //    modest (±0.3 at the extremes) so the slider feels like a trim, not a wrench.
    let t = adj.temp / 100.0;
    let ti = adj.tint / 100.0;
    let r_gain = 1.0 + t * 0.3;
    let b_gain = 1.0 - t * 0.3;
    let g_gain = 1.0 - ti * 0.15;
    r *= r_gain;
    g *= g_gain;
    b *= b_gain;

    // 2. Exposure: a stop is a doubling of linear light.
    let e = 2f32.powf(adj.exposure);
    r *= e;
    g *= e;
    b *= e;

    // 3. Convert linear → working gamma (≈ perceptual). Clamp negatives first so
    //    powf is well-defined.
    let to_gamma = |x: f32| x.max(0.0).powf(1.0 / 2.2);
    let mut rg = to_gamma(r);
    let mut gg = to_gamma(g);
    let mut bg = to_gamma(b);

    // 4. Perceptual tone ops, in gamma space.
    let tone = |v: f32| -> f32 {
        let mut x = v;

        // Blacks/whites: shift the endpoints. Blacks pulls the bottom, whites
        // pushes the top; each ±100 maps to a ±0.2 endpoint move.
        let blacks = adj.blacks / 100.0 * 0.2;
        let whites = adj.whites / 100.0 * 0.2;
        // Remap [0,1] so 0 → -blacks (lift/deepen the floor) and 1 → 1+whites.
        x = (x - (-blacks)) / ((1.0 + whites) - (-blacks));

        // Contrast: S-curve pivoting at mid-gray (0.5). ±100 → ±0.5 strength.
        let c = adj.contrast / 100.0 * 0.5;
        if c != 0.0 {
            // Smooth, monotonic S/inverse-S about 0.5.
            let d = x - 0.5;
            x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
        }

        // Shadows: luminance-masked lift/compress of the lower range. Mask is 1
        // near black, 0 by mid-gray.
        let s = adj.shadows / 100.0 * 0.3;
        if s != 0.0 {
            let mask = (1.0 - x.clamp(0.0, 1.0)).powf(2.0); // strongest near 0
            x += s * mask;
        }

        // Highlights: luminance-masked lift/compress of the upper range. Mask is 1
        // near white, 0 by mid-gray.
        let h = adj.highlights / 100.0 * 0.3;
        if h != 0.0 {
            let mask = x.clamp(0.0, 1.0).powf(2.0); // strongest near 1
            x += h * mask;
        }

        x
    };
    rg = tone(rg);
    gg = tone(gg);
    bg = tone(bg);

    // 4.5. Vibrance/saturation: a cross-channel chroma scale about luma, in
    // gamma (working) space. Saturation scales chroma uniformly; vibrance
    // scales adaptively by the pixel's current saturation (mostly leaves
    // already-vivid pixels alone, pushes near-gray ones harder), the usual
    // "protect skin tones" behavior. MUST stay in sync with the equivalent
    // block in fs_main in shader.wgsl.
    let luma = 0.299 * rg + 0.587 * gg + 0.114 * bg;
    let sat_total = 1.0 + adj.saturation / 100.0;
    let cmax = rg.max(gg).max(bg);
    let cmin = rg.min(gg).min(bg);
    let cur_sat = if cmax > 0.0 {
        (cmax - cmin) / cmax
    } else {
        0.0
    };
    let vib_factor = 1.0 + adj.vibrance / 100.0 * (1.0 - cur_sat);
    let total = sat_total * vib_factor;
    rg = luma + (rg - luma) * total;
    gg = luma + (gg - luma) * total;
    bg = luma + (bg - luma) * total;

    // 5. Convert working → linear.
    let to_linear = |x: f32| x.max(0.0).powf(2.2);
    r = to_linear(rg);
    g = to_linear(gg);
    b = to_linear(bg);

    // 6. Clamp final to 0..1.
    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

/// Given a linear-light RGB pixel (as sampled straight from a decode, before
/// any adjustments) that the user says should be neutral gray, solve for the
/// `temp`/`tint` values that neutralize it — inverting the white-balance
/// gains at the top of [`apply_linear`] (`r_gain = 1+0.3t`,
/// `g_gain = 1-0.15ti`, `b_gain = 1-0.3t`):
///
/// ```text
/// r*(1+0.3t)  = b*(1-0.3t)        =>  t  = (b - r) / (0.3*(r + b))
/// r*(1+0.3t)  = g*(1-0.15*ti)     =>  ti = (1 - r*(1+0.3t)/g) / 0.15
/// ```
///
/// Every later pipeline stage (exposure, contrast, highlights/shadows,
/// whites/blacks) treats equal-valued channels identically — a uniform scale
/// or the same per-channel function — so a pixel neutralized this way stays
/// neutral through the rest of the pipeline regardless of the image's other
/// current adjustments. Returns `None` for pixels too dark to solve
/// reliably (near-zero denominators would otherwise blow up `t`/`ti`).
pub fn neutralize_gray(rgb: [f32; 3]) -> Option<(f32, f32)> {
    let [r, g, b] = rgb;
    const EPS: f32 = 0.02;
    if r + b < EPS || g < EPS {
        return None;
    }
    let t = ((b - r) / (0.3 * (r + b))).clamp(-1.0, 1.0);
    let ti = ((1.0 - r * (1.0 + 0.3 * t) / g) / 0.15).clamp(-1.0, 1.0);
    Some((t * 100.0, ti * 100.0))
}

/// Neighbor radius (in taps) for [`denoise_sample`]'s fixed 5×5 kernel. Does
/// NOT scale with the denoise slider — only the range-weight sigma does — so
/// cost stays bounded and predictable at any strength.
pub(crate) const DENOISE_RADIUS: i32 = 2;

/// Hand-baked σ=1.0 Gaussian spatial weight for the 5×5 denoise kernel,
/// indexed by squared tap distance. A literal table avoids a runtime `exp()`
/// call for a term that never depends on live pixel data or the slider.
/// MUST stay in sync with `spatialWeight` in shader.wgsl.
fn spatial_weight(dx: i32, dy: i32) -> f32 {
    match dx * dx + dy * dy {
        0 => 1.0,
        1 => 0.606531,
        2 => 0.367879,
        4 => 0.135335,
        5 => 0.082085,
        8 => 0.018316,
        _ => 0.0,
    }
}

/// Edge-aware (bilateral-style) denoise of one linear-light RGB pixel.
/// `sample(dx, dy)` fetches the neighbor at integer offset `(dx, dy)` from the
/// center (both in `-DENOISE_RADIUS..=DENOISE_RADIUS`); the caller owns
/// clamping/indexing into whatever buffer it has (e.g. clamp-to-edge against
/// image bounds). At `adj.denoise <= 0.0` this is a hard identity — returns
/// `sample(0, 0)` unchanged with no extra math — since every existing catalog
/// entry has `denoise == 0` today and must render byte-identical to before
/// this function existed.
///
/// MUST stay in sync with the `denoise > 0.0` branch of `fs_main` in
/// shader.wgsl.
pub(crate) fn denoise_sample(adj: &Adjustments, sample: impl Fn(i32, i32) -> [f32; 3]) -> [f32; 3] {
    if adj.denoise <= 0.0 {
        return sample(0, 0);
    }
    let center = sample(0, 0);
    let sigma_r = 0.02 + adj.denoise / 100.0 * 0.30;
    let sigma_r2 = sigma_r * sigma_r;
    let (mut sum, mut wsum) = ([0f32; 3], 0f32);
    for dy in -DENOISE_RADIUS..=DENOISE_RADIUS {
        for dx in -DENOISE_RADIUS..=DENOISE_RADIUS {
            let tap = sample(dx, dy);
            let d = [tap[0] - center[0], tap[1] - center[1], tap[2] - center[2]];
            let diff2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            // wsum can't be 0: (dx,dy)=(0,0) always contributes weight 1.0.
            let w = spatial_weight(dx, dy) / (1.0 + diff2 / sigma_r2);
            for c in 0..3 {
                sum[c] += w * tap[c];
            }
            wsum += w;
        }
    }
    [sum[0] / wsum, sum[1] / wsum, sum[2] / wsum]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(l: f32, t: f32, r: f32, b: f32) -> Crop {
        Crop {
            left: l,
            top: t,
            right: r,
            bottom: b,
        }
    }

    #[test]
    fn edit_signature_stable() {
        let mut a = Adjustments::default();
        a.exposure = 1.5;
        a.crop = Some(crop(0.1, 0.2, 0.8, 0.9));
        assert_eq!(edit_signature(&a, 1), edit_signature(&a, 1));
    }

    #[test]
    fn edit_signature_identity_is_fixed() {
        // Identity adjustments + no crop + rot 0 must always hash the same, and a
        // full-frame `Some` crop must hash identically to `None`.
        let base = edit_signature(&Adjustments::default(), 0);
        let full = Adjustments {
            crop: Some(crop(0.0, 0.0, 1.0, 1.0)),
            ..Default::default()
        };
        assert_eq!(edit_signature(&full, 0), base);
    }

    #[test]
    fn edit_signature_differs_on_crop() {
        let a = Adjustments::default();
        let b = Adjustments {
            crop: Some(crop(0.1, 0.1, 0.9, 0.9)),
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn edit_signature_differs_on_tone() {
        let a = Adjustments::default();
        let b = Adjustments {
            exposure: 0.5,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn edit_signature_differs_on_rotation() {
        let a = Adjustments::default();
        assert_ne!(edit_signature(&a, 0), edit_signature(&a, 1));
    }

    #[test]
    fn edit_signature_quantizes() {
        // Sub-quantum jitter (< 1e-3 tone, < 1e-5 crop) hashes identically.
        let a = Adjustments {
            exposure: 1.0,
            ..Default::default()
        };
        let b = Adjustments {
            exposure: 1.0 + 1e-5,
            ..Default::default()
        };
        assert_eq!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn denoise_zero_is_passthrough() {
        let adj = Adjustments::default();
        // A closure that would clearly change the result if the neighborhood
        // loop ran at all: every non-center tap is wildly different.
        let out = denoise_sample(&adj, |dx, dy| {
            if (dx, dy) == (0, 0) {
                [0.2, 0.3, 0.4]
            } else {
                [1.0, 0.0, 0.0]
            }
        });
        assert_eq!(out, [0.2, 0.3, 0.4]);
    }

    #[test]
    fn denoise_smooths_flat_noise() {
        // Center is an outlier against an otherwise-uniform neighborhood — a
        // real blend should pull the result away from the raw center value.
        let adj = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let out = denoise_sample(&adj, |dx, dy| {
            if (dx, dy) == (0, 0) {
                [1.0, 1.0, 1.0]
            } else {
                [0.0, 0.0, 0.0]
            }
        });
        assert!(
            out[0] < 1.0 && out[0] > 0.0,
            "expected a blend, got {out:?}"
        );
    }

    #[test]
    fn denoise_preserves_hard_edge() {
        // A step edge: left half 0.0, right half 1.0 (dx >= 0 is "right").
        // The denoised center (on the boundary, dx=0 counted as right/1.0)
        // should stay closer to its own side than a plain unweighted average
        // of all 25 taps would (which is exactly 0.5 minus the center column).
        let adj = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let step = |dx: i32, _dy: i32| -> [f32; 3] {
            let v = if dx < 0 { 0.0 } else { 1.0 };
            [v, v, v]
        };
        let out = denoise_sample(&adj, step);
        // Plain average over the 5x5 (dx in -2..=2) step pattern: 3/5 columns
        // are 1.0 (dx=0,1,2), 2/5 are 0.0 (dx=-1,-2) -> 0.6.
        let plain_avg = 0.6;
        assert!(
            (out[0] - 1.0).abs() < (out[0] - plain_avg).abs(),
            "expected range weighting to favor the pixel's own side, got {out:?}"
        );
    }

    #[test]
    fn edit_signature_changes_with_denoise() {
        let a = Adjustments::default();
        let b = Adjustments {
            denoise: 40.0,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn is_identity_false_when_denoise_set() {
        let a = Adjustments {
            denoise: 1.0,
            ..Default::default()
        };
        assert!(!a.is_identity());
    }

    #[test]
    fn tone_only_drops_crop_keeps_sliders() {
        let a = Adjustments {
            exposure: 1.5,
            contrast: 20.0,
            crop: Some(crop(0.1, 0.1, 0.9, 0.9)),
            ..Default::default()
        };
        let t = a.tone_only();
        assert_eq!(t.crop, None);
        assert_eq!(t.exposure, 1.5);
        assert_eq!(t.contrast, 20.0);
    }

    #[test]
    fn vibrance_saturation_zero_is_identity() {
        // At the default (all-zero) adjustments, `sat_total` and `vib_factor`
        // both evaluate to 1.0, so the new 4.5 step is a no-op — the whole
        // pipeline should round-trip a non-gray pixel back to itself (modulo
        // gamma round-trip float error).
        let adj = Adjustments::default();
        let px = [0.6, 0.3, 0.2];
        let out = apply_linear(&adj, px);
        for i in 0..3 {
            assert!(
                (out[i] - px[i]).abs() < 1e-5,
                "channel {i}: {} vs {}",
                out[i],
                px[i]
            );
        }
    }

    #[test]
    fn saturation_pushes_channels_from_luma() {
        let base = Adjustments::default();
        let saturated = Adjustments {
            saturation: 80.0,
            ..Default::default()
        };
        let px = [0.6, 0.4, 0.4];
        let out_base = apply_linear(&base, px);
        let out_sat = apply_linear(&saturated, px);
        let spread = |o: [f32; 3]| (o[0] - o[1]).abs() + (o[1] - o[2]).abs() + (o[0] - o[2]).abs();
        assert!(
            spread(out_sat) > spread(out_base),
            "expected more saturation to widen channel spread: base={out_base:?} sat={out_sat:?}"
        );
    }

    #[test]
    fn edit_signature_differs_on_vibrance_and_saturation() {
        let a = Adjustments::default();
        let v = Adjustments {
            vibrance: 30.0,
            ..Default::default()
        };
        let s = Adjustments {
            saturation: 30.0,
            ..Default::default()
        };
        assert_ne!(edit_signature(&a, 0), edit_signature(&v, 0));
        assert_ne!(edit_signature(&a, 0), edit_signature(&s, 0));
        assert_ne!(edit_signature(&v, 0), edit_signature(&s, 0));
    }

    #[test]
    fn is_identity_false_when_vibrance_or_saturation_set() {
        assert!(!Adjustments {
            vibrance: 1.0,
            ..Default::default()
        }
        .is_identity());
        assert!(!Adjustments {
            saturation: 1.0,
            ..Default::default()
        }
        .is_identity());
    }

    #[test]
    fn neutralize_gray_recovers_neutral() {
        let (t, ti) = neutralize_gray([0.5, 0.5, 0.5]).expect("should solve");
        assert!(t.abs() < 1e-3, "expected ~0 temp, got {t}");
        assert!(ti.abs() < 1e-3, "expected ~0 tint, got {ti}");
    }

    #[test]
    fn neutralize_gray_recovers_warm_cast() {
        // A warm-cast pixel (more red, less blue than a neutral gray): solve
        // for temp/tint, then re-run the raw white-balance gain formula
        // (mirroring apply_linear's step 1) and confirm it lands on gray.
        let px = [0.6, 0.5, 0.4];
        let (t, ti) = neutralize_gray(px).expect("should solve");
        let tt = t / 100.0;
        let tit = ti / 100.0;
        let r = px[0] * (1.0 + tt * 0.3);
        let g = px[1] * (1.0 - tit * 0.15);
        let b = px[2] * (1.0 - tt * 0.3);
        assert!((r - g).abs() < 1e-4, "r={r} g={g}");
        assert!((g - b).abs() < 1e-4, "g={g} b={b}");
    }

    #[test]
    fn neutralize_gray_none_when_too_dark() {
        assert_eq!(neutralize_gray([0.0, 0.0, 0.0]), None);
    }
}
