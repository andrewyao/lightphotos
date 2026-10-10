// SPDX-License-Identifier: MIT OR Apache-2.0

//! Texture, Clarity and Dehaze: the edits that compare a pixel with its
//! neighbourhood. They run on the pre-tone pixel (after the touch-ups,
//! before white balance), so every tone slider acts on their result.
//!
//! All three read the [`BaseLayer`](super::BaseLayer), so each works on the
//! same share of the frame at every image size and the Loupe at fit shows
//! what the export does. Clarity scales the pixel's luma against the base
//! layer's `mid`, a local mean about 1% of the frame wide. Texture does the
//! same against the fine layer's `mid`, a third of that. Dehaze removes a
//! grey veil as thick as `dark`, the local dark channel. Texture and Clarity
//! scale all three channels by one gain, which keeps the pixel's hue.
//!
//! Must match `localAdjust` in loupe_common.wgsl.

use super::base::{log2_detail, BaseTexel};
use super::Adjustments;

/// The most a detail ratio counts, in stops of gamma luma, so one hot
/// pixel next to black can't blow up the gain.
const DETAIL_LIMIT: f32 = 1.5;
/// Dehaze at 100 removes this share of the dark channel.
const DEHAZE_STRENGTH: f32 = 0.8;
/// The least transmission Dehaze divides by.
const DEHAZE_FLOOR: f32 = 0.2;
/// Dehaze at -100 mixes in this much of the haze colour.
const HAZE_ADD: f32 = 0.5;
/// The haze added by a negative Dehaze, linear.
const HAZE: f32 = 0.7;

/// Texture, Clarity and Dehaze on `pre`, the linear pre-tone pixel. `base`
/// is the base layer at the pixel and `fine_mid` the fine layer's `mid`
/// there; `None` leaves out the sliders that read it.
pub fn apply(
    adj: &Adjustments,
    pre: [f32; 3],
    base: Option<BaseTexel>,
    fine_mid: Option<f32>,
) -> [f32; 3] {
    // A sidecar can hold anything; the shader gets these clamped too.
    let [texture, clarity, dehaze] =
        [adj.texture, adj.clarity, adj.dehaze].map(|v| v.clamp(-100.0, 100.0) / 100.0);
    let mut out = pre;
    // Stops of gamma luma to add, from Clarity and Texture together.
    let mut gain = 0.0;
    if let Some(base) = base {
        if clarity != 0.0 {
            let d = log2_detail(pre, base.mid).clamp(-DETAIL_LIMIT, DETAIL_LIMIT);
            // Most in the midtones, none at black or white, which keeps
            // halos off skies and shadows.
            let m = base.mid.clamp(0.0, 1.0);
            let weight = 1.0 - (2.0 * m - 1.0) * (2.0 * m - 1.0);
            gain += clarity * d * weight;
        }
        if dehaze > 0.0 {
            // Remove a grey veil as thick as the local dark channel, and
            // stretch what is left back up to white. No thicker than the
            // pixel's own darkest channel, which would go below black.
            let darkest = out[0].min(out[1]).min(out[2]).max(0.0);
            let veil = (DEHAZE_STRENGTH * dehaze * base.dark.clamp(0.0, 1.0)).min(darkest);
            let t = (1.0 - veil).max(DEHAZE_FLOOR);
            out = out.map(|c| (c - veil) / t);
        } else if dehaze < 0.0 {
            let k = -dehaze * HAZE_ADD;
            out = out.map(|c| c + (HAZE - c) * k);
        }
    }
    if let Some(fine) = fine_mid {
        gain += texture * log2_detail(pre, fine).clamp(-DETAIL_LIMIT, DETAIL_LIMIT);
    }
    if gain != 0.0 {
        // A gamma-luma gain of 2^gain is 2^(2.2 * gain) in linear light.
        let g = (2.2 * gain).exp2();
        out = out.map(|c| c * g);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::base::luma_gamma;

    fn adj(texture: f32, clarity: f32, dehaze: f32) -> Adjustments {
        Adjustments {
            texture,
            clarity,
            dehaze,
            ..Default::default()
        }
    }

    fn base(mid: f32, dark: f32) -> Option<BaseTexel> {
        Some(BaseTexel {
            mid,
            wide: mid,
            dark,
        })
    }

    #[test]
    fn zero_sliders_change_nothing() {
        let px = [0.3, 0.1, 0.05];
        let got = apply(&adj(0.0, 0.0, 0.0), px, base(0.2, 0.1), Some(0.5));
        assert_eq!(got, px);
    }

    /// A pixel brighter than its neighbourhood gets brighter with Clarity
    /// and goes back to the neighbourhood at -100, keeping its hue.
    #[test]
    fn clarity_stretches_a_pixel_from_its_base_and_minus_100_flattens_it() {
        let px = [0.30, 0.20, 0.10];
        let mid = 0.4;
        assert!(luma_gamma(px) > mid);
        let up = apply(&adj(0.0, 100.0, 0.0), px, base(mid, 0.0), None);
        assert!(luma_gamma(up) > luma_gamma(px));
        assert!((up[0] / up[1] - px[0] / px[1]).abs() < 1e-5, "hue kept");
        let flat = apply(&adj(0.0, -100.0, 0.0), px, base(0.5, 0.0), None);
        assert!(
            (luma_gamma(flat) - 0.5).abs() < 1e-3,
            "{}",
            luma_gamma(flat)
        );
    }

    #[test]
    fn texture_minus_100_takes_the_fine_mean() {
        let got = apply(&adj(-100.0, 0.0, 0.0), [0.4, 0.3, 0.2], None, Some(0.45));
        assert!((luma_gamma(got) - 0.45).abs() < 1e-3);
    }

    #[test]
    fn dehaze_keeps_white_and_darkens_a_hazy_gray() {
        let a = adj(0.0, 0.0, 100.0);
        let white = apply(&a, [1.0; 3], base(1.0, 0.6), None);
        assert!(white.iter().all(|c| (c - 1.0).abs() < 1e-5), "{white:?}");
        let gray = apply(&a, [0.6; 3], base(0.8, 0.6), None);
        assert!(gray[0] < 0.6 * 0.5, "{gray:?}");
        let clear = apply(&a, [0.6; 3], base(0.8, 0.0), None);
        assert_eq!(clear, [0.6; 3], "no dark channel, no haze to remove");
        // A pixel darker than the veil keeps its colour order rather than
        // going below black.
        let dark = apply(&a, [0.5, 0.35, 0.10], base(0.5, 0.3), None);
        assert!(dark.iter().all(|&c| c >= 0.0), "{dark:?}");
        assert!(dark[0] > dark[1] && dark[1] > dark[2], "{dark:?}");
        let hazier = apply(&adj(0.0, 0.0, -100.0), [0.1; 3], base(0.3, 0.1), None);
        assert!(hazier[0] > 0.1);
    }

    /// A sidecar's `1e39` reads as an infinite f32.
    #[test]
    fn infinite_sliders_from_a_sidecar_stay_finite() {
        let a = adj(f32::INFINITY, f32::NEG_INFINITY, f32::INFINITY);
        let got = apply(&a, [0.3, 0.2, 0.1], base(0.4, 0.2), Some(0.25));
        assert!(got.iter().all(|c| c.is_finite()), "{got:?}");
    }
}
