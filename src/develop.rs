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

    // Tone: eight −100..=100 (or −5..=5) sliders, quantized to 1e-3.
    let tone = [
        adj.temp, adj.tint, adj.exposure, adj.contrast, adj.highlights, adj.shadows,
        adj.whites, adj.blacks,
    ];
    for v in tone {
        h.write(&((v * 1000.0).round() as i32).to_le_bytes());
    }

    // Crop: normalize a full-frame `Some` to `None` so it hashes like no crop,
    // then quantize the four normalized coords to 1e-5.
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 100_000.0).round()) as u32;
    match adj.crop {
        Some(c)
            if c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0 =>
        {
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
/// 12 × f32 = 48 bytes, already a multiple of 16 so no trailing pad is needed —
/// but uniform buffers require 16-byte alignment, so keep the field count a
/// multiple of 4 if you ever add fields.
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
        }
    }
}

impl From<&Adjustments> for GpuAdjust {
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

    // 5. Convert working → linear.
    let to_linear = |x: f32| x.max(0.0).powf(2.2);
    r = to_linear(rg);
    g = to_linear(gg);
    b = to_linear(bg);

    // 6. Clamp final to 0..1.
    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crop(l: f32, t: f32, r: f32, b: f32) -> Crop {
        Crop { left: l, top: t, right: r, bottom: b }
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
        let full = Adjustments { crop: Some(crop(0.0, 0.0, 1.0, 1.0)), ..Default::default() };
        assert_eq!(edit_signature(&full, 0), base);
    }

    #[test]
    fn edit_signature_differs_on_crop() {
        let a = Adjustments::default();
        let b = Adjustments { crop: Some(crop(0.1, 0.1, 0.9, 0.9)), ..Default::default() };
        assert_ne!(edit_signature(&a, 0), edit_signature(&b, 0));
    }

    #[test]
    fn edit_signature_differs_on_tone() {
        let a = Adjustments::default();
        let b = Adjustments { exposure: 0.5, ..Default::default() };
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
        let a = Adjustments { exposure: 1.0, ..Default::default() };
        let b = Adjustments { exposure: 1.0 + 1e-5, ..Default::default() };
        assert_eq!(edit_signature(&a, 0), edit_signature(&b, 0));
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
}
