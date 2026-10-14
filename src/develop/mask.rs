// SPDX-License-Identifier: MIT OR Apache-2.0

//! Local adjustments: gradient masks that each carry a few tone and colour
//! sliders. A mask's weight at a pixel scales its [`LocalAmounts`], and the
//! weighted amounts of every mask add to the global sliders of the same name
//! before the tone pipeline reads them. Shapes live in 0..1 source UV, before
//! crop and straighten, so neither moves a mask, the same rule [`TouchUp`]
//! follows.
//!
//! [`TouchUp`]: super::TouchUp

use serde::{Deserialize, Serialize};

use super::{Adjustments, SliderId, EXPOSURE_RANGE, TONE_RANGE};

/// The most masks one photo can hold: the GPU buffer's size.
pub const MAX_MASKS: usize = 16;

/// The smallest and largest radial radius, as a fraction of the photo's
/// shorter side.
pub const RADIUS_RANGE: std::ops::RangeInclusive<f32> = 1e-4..=4.0;

/// How far off the photo a mask's points may sit, in UV. A gradient may start
/// or end past an edge; anything further is no different on screen, and the
/// bound keeps squared distances finite.
const COORD_RANGE: std::ops::RangeInclusive<f32> = -10.0..=11.0;

/// One local adjustment.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Mask {
    pub shape: MaskShape,
    /// Swaps where the mask applies and where it doesn't.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub invert: bool,
    #[serde(default, skip_serializing_if = "LocalAmounts::is_zero")]
    pub amounts: LocalAmounts,
}

/// Where a mask applies. Points are 0..1 source UV; lengths are fractions of
/// the photo's shorter side, so a circle stays round on any aspect.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum MaskShape {
    /// Full strength on the `start` side, fading to nothing at `end`, as a
    /// Lightroom linear gradient drags.
    Linear { start: [f32; 2], end: [f32; 2] },
    /// Full strength inside an ellipse of `radii` turned by `angle` radians,
    /// fading out over the outer `feather` share of each radius.
    Radial {
        center: [f32; 2],
        radii: [f32; 2],
        #[serde(default)]
        angle: f32,
        #[serde(default)]
        feather: f32,
    },
}

/// The sliders a mask carries, in the global sliders' units and ranges.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct LocalAmounts {
    #[serde(skip_serializing_if = "is_zero")]
    pub temp: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub tint: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub exposure: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub contrast: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub highlights: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub shadows: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub whites: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub blacks: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub saturation: f32,
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// One mask slider: the panel draws [`LOCAL_SLIDERS`] in order, and loading,
/// clamping and the GPU copy walk the same table.
pub struct LocalSlider {
    pub id: SliderId,
    pub field: fn(&mut LocalAmounts) -> &mut f32,
    pub range: std::ops::RangeInclusive<f32>,
    pub decimals: usize,
}

const fn local(id: SliderId, field: fn(&mut LocalAmounts) -> &mut f32) -> LocalSlider {
    LocalSlider {
        id,
        field,
        range: TONE_RANGE,
        decimals: 0,
    }
}

pub const LOCAL_SLIDERS: [LocalSlider; 9] = [
    local(SliderId::Temp, |a| &mut a.temp),
    local(SliderId::Tint, |a| &mut a.tint),
    LocalSlider {
        range: EXPOSURE_RANGE,
        decimals: 2,
        ..local(SliderId::Exposure, |a| &mut a.exposure)
    },
    local(SliderId::Contrast, |a| &mut a.contrast),
    local(SliderId::Highlights, |a| &mut a.highlights),
    local(SliderId::Shadows, |a| &mut a.shadows),
    local(SliderId::Whites, |a| &mut a.whites),
    local(SliderId::Blacks, |a| &mut a.blacks),
    local(SliderId::Saturation, |a| &mut a.saturation),
];

impl LocalSlider {
    pub fn get(&self, amounts: &LocalAmounts) -> f32 {
        let mut a = *amounts;
        *(self.field)(&mut a)
    }
}

impl LocalAmounts {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }

    /// The global sliders a mask's amounts add to.
    pub fn of(adj: &Adjustments) -> Self {
        Self {
            temp: adj.temp,
            tint: adj.tint,
            exposure: adj.exposure,
            contrast: adj.contrast,
            highlights: adj.highlights,
            shadows: adj.shadows,
            whites: adj.whites,
            blacks: adj.blacks,
            saturation: adj.saturation,
        }
    }

    /// `self + d · weight` for every slider.
    fn add_weighted(&mut self, d: &LocalAmounts, weight: f32) {
        for s in &LOCAL_SLIDERS {
            *(s.field)(self) += s.get(d) * weight;
        }
    }

    /// Each slider held to its range. Must match the clamps in `localTone`
    /// in loupe_common.wgsl.
    pub fn clamped(mut self) -> Self {
        for s in &LOCAL_SLIDERS {
            let v = (s.field)(&mut self);
            *v = v.clamp(*s.range.start(), *s.range.end());
        }
        self
    }

    fn all_finite(&self) -> bool {
        LOCAL_SLIDERS.iter().all(|s| s.get(self).is_finite())
    }
}

impl Mask {
    /// A new linear mask with no amounts yet.
    pub fn linear(start: [f32; 2], end: [f32; 2]) -> Self {
        Mask {
            shape: MaskShape::Linear { start, end },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// A new radial mask with no amounts yet and Lightroom's default feather.
    pub fn radial(center: [f32; 2], radii: [f32; 2]) -> Self {
        Mask {
            shape: MaskShape::Radial {
                center,
                radii,
                angle: 0.0,
                feather: 0.5,
            },
            invert: false,
            amounts: LocalAmounts::default(),
        }
    }

    /// This mask with every number in range, or `None` if one isn't a
    /// number at all. Masks come from sidecars and drive scripts.
    pub fn sanitized(self) -> Option<Mask> {
        let point = |p: [f32; 2]| {
            p.iter()
                .all(|v| v.is_finite())
                .then(|| p.map(|v| v.clamp(*COORD_RANGE.start(), *COORD_RANGE.end())))
        };
        if !self.amounts.all_finite() {
            return None;
        }
        let shape = match self.shape {
            MaskShape::Linear { start, end } => MaskShape::Linear {
                start: point(start)?,
                end: point(end)?,
            },
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            } => {
                if !radii.iter().all(|r| r.is_finite())
                    || !angle.is_finite()
                    || !feather.is_finite()
                {
                    return None;
                }
                MaskShape::Radial {
                    center: point(center)?,
                    radii: radii.map(|r| r.clamp(*RADIUS_RANGE.start(), *RADIUS_RANGE.end())),
                    angle: angle.rem_euclid(std::f32::consts::TAU),
                    feather: feather.clamp(0.0, 1.0),
                }
            }
        };
        Some(Mask {
            shape,
            invert: self.invert,
            amounts: self.amounts.clamped(),
        })
    }
}

/// How much of `mask` applies at source UV `uv`, 0..1, on a photo whose
/// width over height is `aspect`. Must match `maskWeight` in
/// loupe_common.wgsl.
pub fn mask_weight(mask: &Mask, uv: [f32; 2], aspect: f32) -> f32 {
    // UV to units of the shorter side.
    let scale = if aspect >= 1.0 {
        [aspect, 1.0]
    } else {
        [1.0, 1.0 / aspect]
    };
    let w = match mask.shape {
        MaskShape::Linear { start, end } => {
            let dir = [
                (end[0] - start[0]) * scale[0],
                (end[1] - start[1]) * scale[1],
            ];
            let len2 = dir[0] * dir[0] + dir[1] * dir[1];
            // A gradient with no length has no direction, so it covers nothing,
            // inverted or not.
            if len2 < 1e-12 {
                return 0.0;
            }
            let p = [(uv[0] - start[0]) * scale[0], (uv[1] - start[1]) * scale[1]];
            let t = ((p[0] * dir[0] + p[1] * dir[1]) / len2).clamp(0.0, 1.0);
            1.0 - t * t * (3.0 - 2.0 * t)
        }
        MaskShape::Radial {
            center,
            radii,
            angle,
            feather,
        } => {
            let q = [
                (uv[0] - center[0]) * scale[0],
                (uv[1] - center[1]) * scale[1],
            ];
            let (s, c) = angle.sin_cos();
            let x = (c * q[0] + s * q[1]) / radii[0];
            let y = (c * q[1] - s * q[0]) / radii[1];
            falloff(x.hypot(y), feather)
        }
    };
    if mask.invert {
        1.0 - w
    } else {
        w
    }
}

/// 1 inside `1 - feather` of the radius, 0 at and past the radius, and a
/// smoothstep between. A zero feather is a hard edge, not a division by zero.
/// Must match `maskFalloff` in loupe_common.wgsl.
fn falloff(d: f32, feather: f32) -> f32 {
    if d >= 1.0 {
        return 0.0;
    }
    if d <= 1.0 - feather {
        return 1.0;
    }
    let t = (1.0 - d) / feather;
    t * t * (3.0 - 2.0 * t)
}

/// The sliders at source UV `uv`: `base` plus every mask's weighted amounts,
/// each held to its range. Must match `localTone` in loupe_common.wgsl.
pub fn local_tone(base: LocalAmounts, masks: &[Mask], uv: [f32; 2], aspect: f32) -> LocalAmounts {
    let mut out = base;
    for m in masks {
        let w = mask_weight(m, uv, aspect);
        if w > 0.0 {
            out.add_weighted(&m.amounts, w);
        }
    }
    out.clamped()
}

/// [`Mask`] as laid out in the GPU storage buffer. Must match `Mask` in
/// loupe_common.wgsl.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuMask {
    /// Linear: start, end. Radial: center, radii.
    pub geom: [f32; 4],
    /// Kind (0 linear, 1 radial), invert (0 or 1), angle, feather.
    pub params: [f32; 4],
    /// Exposure, contrast, highlights, shadows.
    pub a0: [f32; 4],
    /// Whites, blacks, temp, tint.
    pub a1: [f32; 4],
    /// Saturation, then padding.
    pub a2: [f32; 4],
}

impl From<&Mask> for GpuMask {
    fn from(m: &Mask) -> Self {
        let invert = if m.invert { 1.0 } else { 0.0 };
        let (geom, params) = match m.shape {
            MaskShape::Linear { start, end } => (
                [start[0], start[1], end[0], end[1]],
                [0.0, invert, 0.0, 0.0],
            ),
            MaskShape::Radial {
                center,
                radii,
                angle,
                feather,
            } => (
                [center[0], center[1], radii[0], radii[1]],
                [1.0, invert, angle, feather],
            ),
        };
        let a = &m.amounts;
        GpuMask {
            geom,
            params,
            a0: [a.exposure, a.contrast, a.highlights, a.shadows],
            a1: [a.whites, a.blacks, a.temp, a.tint],
            a2: [a.saturation, 0.0, 0.0, 0.0],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn radial(feather: f32) -> Mask {
        Mask::radial([0.5, 0.5], [0.2, 0.1]).with_feather(feather)
    }

    impl Mask {
        fn with_feather(mut self, f: f32) -> Self {
            if let MaskShape::Radial { feather, .. } = &mut self.shape {
                *feather = f;
            }
            self
        }
        fn inverted(self) -> Self {
            Mask {
                invert: true,
                ..self
            }
        }
    }

    #[test]
    fn a_linear_mask_is_full_before_start_and_gone_past_end() {
        let m = Mask::linear([0.2, 0.5], [0.6, 0.5]);
        assert_eq!(mask_weight(&m, [0.1, 0.3], 1.5), 1.0);
        assert_eq!(mask_weight(&m, [0.9, 0.7], 1.5), 0.0);
        assert!((mask_weight(&m, [0.4, 0.5], 1.5) - 0.5).abs() < 1e-6);
        let inv = m.inverted();
        assert_eq!(mask_weight(&inv, [0.1, 0.3], 1.5), 0.0);
        assert_eq!(mask_weight(&inv, [0.9, 0.7], 1.5), 1.0);
    }

    #[test]
    fn a_linear_mask_with_no_length_covers_nothing_even_inverted() {
        let m = Mask::linear([0.4, 0.4], [0.4, 0.4]);
        assert_eq!(mask_weight(&m, [0.4, 0.4], 1.0), 0.0);
        assert_eq!(mask_weight(&m.inverted(), [0.1, 0.9], 1.0), 0.0);
    }

    #[test]
    fn a_radial_mask_is_full_at_its_center_and_gone_past_its_edge() {
        for feather in [0.0, 0.5, 1.0] {
            let m = radial(feather);
            assert_eq!(mask_weight(&m, [0.5, 0.5], 1.5), 1.0, "feather {feather}");
            // 0.2 of the shorter side, along x, is 0.2 / 1.5 in UV on a 3:2 photo.
            let edge = 0.5 + 0.2 / 1.5;
            assert_eq!(mask_weight(&m, [edge + 1e-4, 0.5], 1.5), 0.0);
            assert_eq!(mask_weight(&m.inverted(), [edge + 1e-4, 0.5], 1.5), 1.0);
            assert_eq!(mask_weight(&m.inverted(), [0.5, 0.5], 1.5), 0.0);
        }
        let hard = radial(0.0);
        assert_eq!(mask_weight(&hard, [0.5 + 0.19 / 1.5, 0.5], 1.5), 1.0);
        let soft = radial(0.5);
        let mid = mask_weight(&soft, [0.5 + 0.15 / 1.5, 0.5], 1.5);
        assert!((mid - 0.5).abs() < 1e-5, "{mid}");
    }

    #[test]
    fn a_radial_mask_turns_with_its_angle_and_measures_in_pixels() {
        let flat = radial(0.0);
        // 0.15 down is past the 0.1 radius until the ellipse turns upright.
        assert_eq!(mask_weight(&flat, [0.5, 0.65], 1.0), 0.0);
        let mut upright = flat;
        if let MaskShape::Radial { angle, .. } = &mut upright.shape {
            *angle = std::f32::consts::FRAC_PI_2;
        }
        assert_eq!(mask_weight(&upright, [0.5, 0.65], 1.0), 1.0);
        // On a tall photo the shorter side is the width, so a UV step in v
        // covers more of it.
        let circle = Mask::radial([0.5, 0.5], [0.2, 0.2]).with_feather(0.0);
        assert_eq!(mask_weight(&circle, [0.5, 0.62], 0.5), 0.0);
        assert_eq!(mask_weight(&circle, [0.5, 0.58], 0.5), 1.0);
        assert_eq!(mask_weight(&circle, [0.68, 0.5], 0.5), 1.0);
    }

    #[test]
    fn local_tone_adds_weighted_amounts_and_clamps() {
        let mut m = Mask::radial([0.5, 0.5], [0.3, 0.3]).with_feather(0.0);
        m.amounts.exposure = 4.0;
        m.amounts.temp = -40.0;
        let base = LocalAmounts {
            exposure: 3.0,
            temp: 10.0,
            ..LocalAmounts::default()
        };
        let inside = local_tone(base, &[m, m], [0.5, 0.5], 1.0);
        assert_eq!(inside.exposure, 5.0, "held to the slider's range");
        assert_eq!(inside.temp, -70.0);
        let outside = local_tone(base, &[m], [0.0, 0.0], 1.0);
        assert_eq!(outside, base);
    }

    #[test]
    fn sanitizing_drops_non_numbers_and_clamps_the_rest() {
        let mut nan = radial(0.5);
        nan.amounts.exposure = f32::NAN;
        assert_eq!(nan.sanitized(), None);
        let inf = Mask::linear([f32::INFINITY, 0.0], [0.5, 0.5]);
        assert_eq!(inf.sanitized(), None);

        let wild = Mask {
            shape: MaskShape::Radial {
                center: [1e30, -0.5],
                radii: [-1.0, 99.0],
                angle: -1.0,
                feather: 7.0,
            },
            invert: true,
            amounts: LocalAmounts {
                exposure: 50.0,
                saturation: -500.0,
                ..LocalAmounts::default()
            },
        };
        let Some(Mask {
            shape:
                MaskShape::Radial {
                    center,
                    radii,
                    angle,
                    feather,
                },
            invert,
            amounts,
        }) = wild.sanitized()
        else {
            panic!("a finite mask survives");
        };
        assert_eq!(center, [11.0, -0.5]);
        assert_eq!(radii, [1e-4, 4.0]);
        assert!((0.0..std::f32::consts::TAU).contains(&angle));
        assert_eq!(feather, 1.0);
        assert!(invert);
        assert_eq!((amounts.exposure, amounts.saturation), (5.0, -100.0));
    }

    #[test]
    fn a_mask_round_trips_through_its_tagged_json() {
        let mut m = radial(0.3);
        m.amounts.shadows = 20.0;
        let json = serde_json::to_string(&m).unwrap();
        assert!(json.contains(r#""kind":"radial""#), "{json}");
        assert!(
            !json.contains("invert"),
            "a default field stays out: {json}"
        );
        let back: Mask = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }
}
