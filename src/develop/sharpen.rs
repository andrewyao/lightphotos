// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sharpening: Lightroom's Detail-panel sharpening as an unsharp mask on
//! the luma of the pre-tone pixel, and the output sharpening an export adds
//! for its medium after it is resized.
//!
//! The Radius is in pixels of a frame whose short side is [`REF_SHORT`],
//! about a 24 MP photo, and scales with the image actually rendered. A
//! preview at fit then sharpens what the export does when scaled to the
//! same size, rather than its own texels three times as hard.

use serde::{Deserialize, Serialize};

use super::base::luma_gamma;
use super::image_ops::Sample;

/// The short side the Radius slider's pixels are measured on.
pub const REF_SHORT: f32 = 4000.0;
pub const AMOUNT_RANGE: std::ops::RangeInclusive<f32> = 0.0..=150.0;
pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 0.5..=3.0;
/// The blur's tap spacing as a share of the radius: a 5x5 grid reaching
/// 1.5 radii each way.
const TAP_SPACING: f32 = 0.75;
/// The widest tap spacing, in texels. Each bilinear tap covers 2x2
/// texels, so wider steps would skip texels and alias fine texture into
/// the blur; a large Radius on a frame over 24 MP stops widening here.
const MAX_STEP: f32 = 1.5;
/// Below this the luma ratio stops trusting a pixel, as in `base`.
const EPS: f32 = 1.0 / 255.0;

/// Lightroom's sharpening sliders and defaults. Amount 0 is off.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Sharpening {
    pub amount: f32,
    /// Pixels on a [`REF_SHORT`] frame.
    pub radius: f32,
    /// 0 holds back the strong edges where halos show; 100 sharpens them
    /// fully.
    pub detail: f32,
    /// 0 sharpens everywhere; higher limits it to ever stronger edges.
    pub masking: f32,
}

impl Default for Sharpening {
    fn default() -> Self {
        Self {
            amount: 0.0,
            radius: 1.0,
            detail: 25.0,
            masking: 0.0,
        }
    }
}

impl Sharpening {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// The derived numbers for one render of a `w x h` image, laid out as part
/// of the shader's `Adjust` uniform. Must match its `sharp_*` fields.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Sharpen {
    /// Amount / 100; 0 is off.
    pub gain: f32,
    /// The blur's tap spacing in texels of the rendered image.
    pub step: f32,
    /// The most one pixel's luma moves, from Detail.
    pub limit: f32,
    /// The edge strength below which Masking holds sharpening back; 0 is
    /// no mask.
    pub mask: f32,
}

impl Sharpen {
    /// Each slider is clamped to its range first, since a sidecar can hold
    /// anything.
    pub fn new(s: &Sharpening, w: f32, h: f32) -> Self {
        let gain = s.amount.clamp(*AMOUNT_RANGE.start(), *AMOUNT_RANGE.end()) / 100.0;
        let short = w.min(h);
        if gain == 0.0 || short <= 0.0 {
            return Self::default();
        }
        let radius = s.radius.clamp(*RADIUS_RANGE.start(), *RADIUS_RANGE.end());
        let detail = s.detail.clamp(0.0, 100.0) / 100.0;
        Self {
            gain,
            step: (radius * TAP_SPACING * short / REF_SHORT).min(MAX_STEP),
            limit: 0.02 + 0.98 * detail * detail,
            mask: s.masking.clamp(0.0, 100.0) / 100.0 * 0.08,
        }
    }

    pub fn is_off(&self) -> bool {
        self.gain == 0.0
    }

    /// The unsharp mask: `pre`'s luma moved away from `blur`'s, applied to
    /// `out` as one gain so the hue stays. `pre` is the pixel the blur was
    /// taken around, `out` that pixel after the other local edits. Must
    /// match `sharpenAdjust` in loupe_common.wgsl.
    pub fn apply(&self, out: [f32; 3], pre: [f32; 3], blur: [f32; 3]) -> [f32; 3] {
        let l = luma_gamma(pre);
        let diff = l - luma_gamma(blur);
        let mut d = diff.clamp(-self.limit, self.limit);
        if self.mask > 0.0 {
            let t = ((diff.abs() - self.mask * 0.5) / (self.mask * 0.5)).clamp(0.0, 1.0);
            d *= t * t * (3.0 - 2.0 * t);
        }
        let target = (l + self.gain * d).max(0.0);
        let g = ((target + EPS) / (l + EPS)).powf(2.2);
        out.map(|c| c * g)
    }
}

/// Gaussian weight of a tap by squared grid distance, at sigma 1/0.75 grid
/// steps, so the blur's sigma is the radius. Must match `sharpenWeight` in
/// loupe_common.wgsl.
fn tap_weight(dx: i32, dy: i32) -> f32 {
    match dx * dx + dy * dy {
        0 => 1.0,
        1 => 0.754_840_3,
        2 => 0.569_782_8,
        4 => 0.324_652_5,
        5 => 0.245_061_4,
        8 => 0.105_399_2,
        _ => 0.0,
    }
}

/// The weighted mean of `tap(dx, dy)` over the 5x5 grid, summed row by row
/// from the top left as the shader sums it. `tap` reads `Sharpen::step`
/// texels apart.
pub fn blur(tap: impl Fn(i32, i32) -> [f32; 3]) -> [f32; 3] {
    let (mut sum, mut wsum) = ([0f32; 3], 0f32);
    for dy in -2..=2 {
        for dx in -2..=2 {
            let w = tap_weight(dx, dy);
            let t = tap(dx, dy);
            for c in 0..3 {
                sum[c] += w * t[c];
            }
            wsum += w;
        }
    }
    sum.map(|s| s / wsum)
}

/// What an export is sharpened for, after it is resized.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OutputMedia {
    #[default]
    Off,
    Screen,
    Matte,
    Glossy,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum OutputAmount {
    Low,
    #[default]
    Standard,
    High,
}

/// Lightroom's export sharpening: a medium and an amount.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(default)]
pub struct OutputSharpening {
    pub media: OutputMedia,
    pub amount: OutputAmount,
}

impl OutputSharpening {
    /// The blur sigma in output pixels and the gain. Paper hides fine
    /// sharpening that a screen shows, matte more than glossy, so print
    /// gets a wider radius and more of it.
    fn kernel(self) -> Option<(f32, f32)> {
        let (sigma, base) = match self.media {
            OutputMedia::Off => return None,
            OutputMedia::Screen => (0.7, 0.5),
            OutputMedia::Glossy => (1.0, 0.8),
            OutputMedia::Matte => (1.3, 1.1),
        };
        let scale = match self.amount {
            OutputAmount::Low => 0.6,
            OutputAmount::Standard => 1.0,
            OutputAmount::High => 1.5,
        };
        Some((sigma, base * scale))
    }

    /// Sharpen opaque sRGB RGBA in place: an unsharp mask on the encoded
    /// samples' luma, blurred by two 5-tap passes clamped at the edges, and
    /// applied to each pixel as one gain. A short buffer is left alone.
    pub fn apply<S: Sample>(self, w: u32, h: u32, rgba: &mut [S]) {
        let Some((sigma, gain)) = self.kernel() else {
            return;
        };
        let (w, h) = (w as usize, h as usize);
        let Some(n) = w.checked_mul(h).filter(|&n| n > 0) else {
            return;
        };
        if n.checked_mul(4).is_none_or(|len| rgba.len() < len) {
            return;
        }
        let luma: Vec<f32> = rgba
            .as_chunks::<4>()
            .0
            .iter()
            .take(n)
            .map(|[r, g, b, _]| {
                (0.299 * r.level() + 0.587 * g.level() + 0.114 * b.level()) / S::MAX
            })
            .collect();
        let weights: [f32; 5] = std::array::from_fn(|k| {
            let d = k as f32 - 2.0;
            (-(d * d) / (2.0 * sigma * sigma)).exp()
        });
        let total: f32 = weights.iter().sum();
        let pass = |src: &[f32], horizontal: bool| -> Vec<f32> {
            (0..n)
                .map(|i| {
                    let (x, y) = (i % w, i / w);
                    weights
                        .iter()
                        .enumerate()
                        .map(|(k, wt)| {
                            let (sx, sy) = if horizontal {
                                ((x + k).saturating_sub(2).min(w - 1), y)
                            } else {
                                (x, (y + k).saturating_sub(2).min(h - 1))
                            };
                            wt * src[sy * w + sx]
                        })
                        .sum::<f32>()
                        / total
                })
                .collect()
        };
        let blur = pass(&pass(&luma, true), false);
        for y in 0..h {
            for x in 0..w {
                let l = luma[y * w + x];
                let target = (l + gain * (l - blur[y * w + x])).clamp(0.0, 1.0);
                let g = (target + EPS) / (l + EPS);
                let i = (y * w + x) * 4;
                for c in &mut rgba[i..i + 3] {
                    *c = c.scaled(g);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn on(amount: f32) -> Sharpening {
        Sharpening {
            amount,
            ..Default::default()
        }
    }

    #[test]
    fn the_weights_are_a_gaussian_one_radius_wide() {
        for (dx, dy) in [(0, 0), (1, 0), (1, 1), (2, 0), (2, 1), (2, 2)] {
            let d2 = (dx * dx + dy * dy) as f32;
            let want = (-d2 * TAP_SPACING * TAP_SPACING / 2.0).exp();
            assert!((tap_weight(dx, dy) - want).abs() < 1e-6, "{dx},{dy}");
        }
    }

    #[test]
    fn amount_0_is_off_and_changes_nothing() {
        let s = Sharpen::new(&Sharpening::default(), 6000.0, 4000.0);
        assert!(s.is_off());
    }

    /// The radius is in pixels of a 4000 px short side, so a quarter-size
    /// preview steps a quarter as far.
    #[test]
    fn the_radius_scales_with_the_image() {
        let full = Sharpen::new(&on(50.0), 6000.0, 4000.0);
        let preview = Sharpen::new(&on(50.0), 1500.0, 1000.0);
        assert!((full.step - 0.75).abs() < 1e-6);
        assert!((preview.step - full.step / 4.0).abs() < 1e-6);
    }

    #[test]
    fn a_pixel_brighter_than_its_blur_gets_brighter_and_keeps_its_hue() {
        let s = Sharpen::new(&on(100.0), 6000.0, 4000.0);
        let pre = [0.4, 0.3, 0.2];
        let out = s.apply(pre, pre, [0.2, 0.15, 0.1]);
        assert!(luma_gamma(out) > luma_gamma(pre));
        assert!((out[0] / out[1] - pre[0] / pre[1]).abs() < 1e-5);
        let flat = s.apply(pre, pre, pre);
        assert_eq!(flat, pre, "no detail, no change");
    }

    #[test]
    fn detail_holds_back_strong_edges_and_masking_skips_weak_ones() {
        let edge = |detail, masking| {
            let s = Sharpen::new(
                &Sharpening {
                    amount: 100.0,
                    detail,
                    masking,
                    ..Default::default()
                },
                6000.0,
                4000.0,
            );
            let pre = [0.5; 3];
            [
                luma_gamma(s.apply(pre, pre, [0.1; 3])) - luma_gamma(pre),
                luma_gamma(s.apply(pre, pre, [0.49; 3])) - luma_gamma(pre),
            ]
        };
        let [strong_low, _] = edge(0.0, 0.0);
        let [strong_high, _] = edge(100.0, 0.0);
        assert!(strong_low < strong_high);
        let [_, weak_unmasked] = edge(100.0, 0.0);
        let [_, weak_masked] = edge(100.0, 100.0);
        assert!(
            weak_masked < weak_unmasked * 0.1,
            "{weak_masked} {weak_unmasked}"
        );
    }

    #[test]
    fn infinite_sliders_from_a_sidecar_stay_finite() {
        let s = Sharpen::new(
            &Sharpening {
                amount: f32::INFINITY,
                radius: f32::NEG_INFINITY,
                detail: f32::INFINITY,
                masking: f32::INFINITY,
            },
            6000.0,
            4000.0,
        );
        let out = s.apply([0.3; 3], [0.3; 3], [0.1; 3]);
        assert!(out.iter().all(|c| c.is_finite()), "{out:?}");
    }

    #[test]
    fn output_sharpening_lifts_an_edge_and_leaves_flat_areas() {
        let (w, h) = (16u32, 8u32);
        let mut rgba = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x < 8 { 80 } else { 160 };
                rgba.extend([v, v, v, 255]);
            }
        }
        let before = rgba.clone();
        let s = OutputSharpening {
            media: OutputMedia::Matte,
            amount: OutputAmount::High,
        };
        s.apply(w, h, &mut rgba);
        let at = |buf: &[u8], x: u32| buf[((4 * w + x) * 4) as usize];
        assert!(at(&rgba, 8) > at(&before, 8), "the bright side of the edge");
        assert!(at(&rgba, 7) < at(&before, 7), "the dark side of the edge");
        assert_eq!(at(&rgba, 1), at(&before, 1), "flat");
        let mut off = before.clone();
        OutputSharpening::default().apply(w, h, &mut off);
        assert_eq!(off, before);
        let mut short = vec![1u8; 10];
        s.apply(w, h, &mut short);
    }
}
