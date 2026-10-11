// SPDX-License-Identifier: MIT OR Apache-2.0

//! Color Grading: a tint and a lightness shift for the shadows, midtones and
//! highlights, plus one for the whole photo, as Lightroom's wheels. It runs
//! in OkLab on the gamma-space pixel after Saturation, so it tints a
//! monochrome photo too (split toning). Blending widens the overlap between
//! the three ranges and Balance moves their meeting point.
//!
//! [`GradeOps`] turns the sliders into the numbers the per-pixel math needs.
//! The CPU bake reads it and [`super::GpuAdjust`] carries it whole to
//! `colorGrade` in loupe_common.wgsl.

use serde::{Deserialize, Serialize};

use super::color::{hue_rgb, oklab, oklab_to_linear};
use super::is_zero;

/// One Color Grading wheel.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Zone {
    Shadows,
    Midtones,
    Highlights,
    Global,
}

impl Zone {
    pub const ALL: [Zone; 4] = [
        Zone::Shadows,
        Zone::Midtones,
        Zone::Highlights,
        Zone::Global,
    ];
}

/// A wheel's position and its luminance slider.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct Wheel {
    /// Degrees around the wheel, 0..360, 0 at red as on an HSV wheel.
    #[serde(skip_serializing_if = "is_zero")]
    pub hue: f32,
    /// Distance from the centre, 0..100.
    #[serde(skip_serializing_if = "is_zero")]
    pub saturation: f32,
    /// -100..100.
    #[serde(skip_serializing_if = "is_zero")]
    pub luminance: f32,
}

impl Wheel {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// Lightroom's Color Grading, with its ranges and defaults.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct ColorGrade {
    #[serde(skip_serializing_if = "Wheel::is_zero")]
    pub shadows: Wheel,
    #[serde(skip_serializing_if = "Wheel::is_zero")]
    pub midtones: Wheel,
    #[serde(skip_serializing_if = "Wheel::is_zero")]
    pub highlights: Wheel,
    #[serde(skip_serializing_if = "Wheel::is_zero")]
    pub global: Wheel,
    /// How far the three ranges overlap, 0..100.
    pub blending: f32,
    /// -100 gives the shadows more of the range, +100 the highlights.
    pub balance: f32,
}

impl Default for ColorGrade {
    fn default() -> Self {
        Self {
            shadows: Wheel::default(),
            midtones: Wheel::default(),
            highlights: Wheel::default(),
            global: Wheel::default(),
            blending: 50.0,
            balance: 0.0,
        }
    }
}

impl ColorGrade {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub fn wheel_mut(&mut self, zone: Zone) -> &mut Wheel {
        match zone {
            Zone::Shadows => &mut self.shadows,
            Zone::Midtones => &mut self.midtones,
            Zone::Highlights => &mut self.highlights,
            Zone::Global => &mut self.global,
        }
    }

    /// The wheels in [`Zone::ALL`] order.
    pub fn wheels(&self) -> [Wheel; 4] {
        [self.shadows, self.midtones, self.highlights, self.global]
    }

    /// True when grading changes nothing: no wheel has saturation or
    /// luminance. A hue alone, or Blending and Balance, move nothing.
    pub fn is_neutral(&self) -> bool {
        self.wheels()
            .iter()
            .all(|w| w.saturation == 0.0 && w.luminance == 0.0)
    }
}

/// OkLab chroma of a wheel at saturation 100.
const SAT_MAX: f32 = 0.09;
/// OkLab lightness change of a wheel at luminance ±100.
const LUM_MAX: f32 = 0.12;

/// The derived numbers for one render, laid out as the tail of the shader's
/// `Adjust` uniform after the colour tools. Must match its `grade_*`
/// fields.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GradeOps {
    /// 1 when grading changes something, 0 to skip it.
    pub on: f32,
    /// The OkLab lightness where the shadow and highlight ranges meet.
    pub mid: f32,
    /// How far each range reaches past `mid`.
    pub width: f32,
    pub _pad: f32,
    /// Per wheel in [`Zone::ALL`] order: the OkLab `a` and `b` it adds, the
    /// lightness it adds, and a pad.
    pub wheels: [[f32; 4]; 4],
}

impl GradeOps {
    /// Each slider is clamped first, since a sidecar can hold anything.
    pub fn new(g: &ColorGrade) -> Self {
        if g.is_neutral() {
            return Self::default();
        }
        let width = 0.15 + g.blending.clamp(0.0, 100.0) / 100.0 * 0.5;
        Self {
            on: 1.0,
            mid: 0.5 - g.balance.clamp(-100.0, 100.0) / 100.0 * 0.25,
            width,
            _pad: 0.0,
            wheels: g.wheels().map(|w| {
                let h = wheel_hue(w.hue);
                let s = w.saturation.clamp(0.0, 100.0) / 100.0 * SAT_MAX;
                let l = w.luminance.clamp(-100.0, 100.0) / 100.0 * LUM_MAX;
                [s * h.cos(), s * h.sin(), l, 0.0]
            }),
        }
    }

    /// Color Grading on one gamma-space pixel. Must match `colorGrade` in
    /// loupe_common.wgsl.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.on == 0.0 {
            return rgb;
        }
        let mut lab = oklab(rgb.map(|x| x.max(0.0).powf(2.2)));
        let (m, w) = (self.mid, self.width);
        let ws = 1.0 - smooth(m - w, m + w * 0.25, lab[0]);
        let wh = smooth(m - w * 0.25, m + w, lab[0]);
        let wm = (1.0 - ws - wh).max(0.0);
        for (k, weight) in self.wheels.iter().zip([ws, wm, wh, 1.0]) {
            lab[1] += k[0] * weight;
            lab[2] += k[1] * weight;
            lab[0] += k[2] * weight;
        }
        oklab_to_linear(lab).map(|x| x.max(0.0).powf(1.0 / 2.2))
    }
}

/// The OkLCh hue, in radians, of the pure sRGB colour at HSV hue `deg`, so a
/// wheel's red tints toward the red a person sees.
fn wheel_hue(deg: f32) -> f32 {
    let [_, a, b] = oklab(hue_rgb(deg).map(|c| c.powf(2.2)));
    b.atan2(a)
}

/// Must match `smoothRamp` in loupe_common.wgsl.
fn smooth(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lab(rgb: [f32; 3]) -> [f32; 3] {
        oklab(rgb.map(|x| x.max(0.0).powf(2.2)))
    }

    fn grade(shadows: Wheel, highlights: Wheel) -> ColorGrade {
        ColorGrade {
            shadows,
            highlights,
            ..Default::default()
        }
    }

    fn tint(hue: f32, saturation: f32) -> Wheel {
        Wheel {
            hue,
            saturation,
            luminance: 0.0,
        }
    }

    /// Catches a wheel that tints the wrong range, or the wrong way.
    #[test]
    fn shadows_and_highlights_tint_their_own_tones() {
        let ops = GradeOps::new(&grade(tint(220.0, 80.0), tint(40.0, 80.0)));
        let dark = lab(ops.apply([0.1; 3]));
        let light = lab(ops.apply([0.9; 3]));
        assert!(dark[2] < -0.03, "shadows go blue: {dark:?}");
        assert!(light[2] > 0.03, "highlights go orange: {light:?}");
        let blue_only = GradeOps::new(&grade(tint(220.0, 80.0), Wheel::default()));
        let light = lab(blue_only.apply([0.9; 3]));
        assert!(light[1].abs() < 0.01 && light[2].abs() < 0.01, "{light:?}");
    }

    #[test]
    fn balance_moves_the_meeting_point() {
        let mut g = grade(tint(220.0, 100.0), Wheel::default());
        let mid = [0.45; 3];
        let at = |g: &ColorGrade| lab(GradeOps::new(g).apply(mid))[2];
        let even = at(&g);
        g.balance = 100.0;
        let to_highlights = at(&g);
        g.balance = -100.0;
        let to_shadows = at(&g);
        assert!(
            to_shadows < even && even < to_highlights,
            "{to_shadows} {even} {to_highlights}"
        );
    }

    #[test]
    fn neutral_is_identity_and_luminance_lifts() {
        let mut g = ColorGrade {
            balance: 40.0,
            ..Default::default()
        };
        g.midtones.hue = 120.0;
        assert!(g.is_neutral());
        assert_eq!(GradeOps::new(&g), GradeOps::default());
        assert_eq!(GradeOps::default().apply([0.3, 0.4, 0.5]), [0.3, 0.4, 0.5]);
        g.global.luminance = 100.0;
        let out = GradeOps::new(&g).apply([0.4; 3]);
        assert!(out[0] > 0.45 && (out[0] - out[2]).abs() < 1e-3, "{out:?}");
    }

    #[test]
    fn out_of_range_sliders_clamp() {
        let g = ColorGrade {
            shadows: Wheel {
                hue: -720.5,
                saturation: f32::INFINITY,
                luminance: -1e9,
            },
            blending: 900.0,
            balance: f32::NEG_INFINITY,
            ..Default::default()
        };
        let ops = GradeOps::new(&g);
        assert_eq!(ops.wheels[0][2], -LUM_MAX);
        assert_eq!(ops.width, 0.65);
        assert_eq!(ops.mid, 0.75);
        assert!(ops.apply([0.2, 0.5, 0.7]).iter().all(|v| v.is_finite()));
    }
}
