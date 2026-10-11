// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Color Mixer: hue, saturation and luminance for eight hue bands, as in
//! Lightroom. It runs in OkLCh on the gamma-space pixel after the point
//! curve, so a band's hue is the hue a person sees, and each pixel's
//! weights over the bands fall off as a raised cosine between neighbouring
//! band centres, so no seam shows between two bands.
//!
//! [`ColorOps`] turns the sliders into the numbers the per-pixel math needs.
//! The CPU bake reads it and [`super::GpuAdjust`] carries it whole to
//! `colorMix` in loupe_common.wgsl, so the two never derive them twice.

use serde::{Deserialize, Serialize};

use super::is_zero;

/// The eight hue bands, in Lightroom's order.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Band {
    Red,
    Orange,
    Yellow,
    Green,
    Aqua,
    Blue,
    Purple,
    Magenta,
}

impl Band {
    pub const ALL: [Band; 8] = [
        Band::Red,
        Band::Orange,
        Band::Yellow,
        Band::Green,
        Band::Aqua,
        Band::Blue,
        Band::Purple,
        Band::Magenta,
    ];
}

/// Which of a band's three values a Color Mixer slider moves.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum MixAxis {
    Hue,
    Saturation,
    Luminance,
}

impl MixAxis {
    pub const ALL: [MixAxis; 3] = [MixAxis::Hue, MixAxis::Saturation, MixAxis::Luminance];
}

/// One band's three sliders, each -100..100.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct BandShift {
    #[serde(skip_serializing_if = "is_zero")]
    pub hue: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub saturation: f32,
    #[serde(skip_serializing_if = "is_zero")]
    pub luminance: f32,
}

impl BandShift {
    pub fn is_zero(&self) -> bool {
        *self == Self::default()
    }
}

/// The Color Mixer's 24 sliders, one [`BandShift`] per [`Band`].
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(default)]
pub struct ColorMixer {
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub red: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub orange: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub yellow: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub green: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub aqua: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub blue: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub purple: BandShift,
    #[serde(skip_serializing_if = "BandShift::is_zero")]
    pub magenta: BandShift,
}

impl ColorMixer {
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }

    /// The bands in [`Band::ALL`] order.
    pub fn bands(&self) -> [BandShift; 8] {
        [
            self.red,
            self.orange,
            self.yellow,
            self.green,
            self.aqua,
            self.blue,
            self.purple,
            self.magenta,
        ]
    }
}

/// OkLCh hue, in radians, of each band's centre: the pure sRGB colour at
/// HSV hue 0, 30, 60, 120, 180, 225, 270 and 315 degrees, linearized with
/// the 2.2 gamma the pipeline works in. They rise once around the circle.
pub const BAND_HUES: [f32; 8] = [
    0.51023, 0.92832, 1.91583, 2.48701, -2.88383, -1.67724, -1.15023, -0.29032,
];

/// Hue rotation at ±100, in radians (about 29 degrees).
const HUE_MAX: f32 = 0.5;
/// OkLab lightness change at ±100 on a fully coloured pixel of lightness 1.
const LUM_MAX: f32 = 0.18;
/// OkLCh chroma at and above which a pixel takes a band's full hue and
/// luminance shift. Duller pixels take less, so grays, whose hue means
/// nothing, stay put.
const CHROMA_FULL: f32 = 0.12;

/// The derived numbers for one render, laid out as the tail of the shader's
/// `Adjust` uniform after the effects. Must match its `mix_*` fields. Every
/// array is 8 floats, two `vec4`s on the GPU, so the layout needs no padding
/// between them.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct ColorOps {
    /// 1 when the mixer changes something, 0 to skip it.
    pub mix_on: f32,
    pub _pad: [f32; 3],
    /// [`BAND_HUES`].
    pub band_hue: [f32; 8],
    /// Hue rotation per band, radians.
    pub mix_hue: [f32; 8],
    /// Chroma gain minus 1 per band, -1..1.
    pub mix_sat: [f32; 8],
    /// OkLab lightness change per band at full chroma and lightness 1.
    pub mix_lum: [f32; 8],
}

impl ColorOps {
    /// Each slider is clamped first, since a sidecar can hold anything. A
    /// monochrome photo skips the mixer, as Lightroom swaps it for the B&W
    /// mix.
    pub fn new(mixer: &ColorMixer, monochrome: bool) -> Self {
        if mixer.is_neutral() || monochrome {
            return Self::default();
        }
        let bands = mixer.bands();
        let unit = |v: f32| v.clamp(-100.0, 100.0) / 100.0;
        Self {
            mix_on: 1.0,
            _pad: [0.0; 3],
            band_hue: BAND_HUES,
            mix_hue: bands.map(|b| unit(b.hue) * HUE_MAX),
            mix_sat: bands.map(|b| unit(b.saturation)),
            mix_lum: bands.map(|b| unit(b.luminance) * LUM_MAX),
        }
    }

    /// The Color Mixer on one gamma-space pixel. Must match `colorMix` in
    /// loupe_common.wgsl.
    pub fn apply(&self, rgb: [f32; 3]) -> [f32; 3] {
        if self.mix_on == 0.0 {
            return rgb;
        }
        let [l, a, b] = oklab(rgb.map(|x| x.max(0.0).powf(2.2)));
        let c = (a * a + b * b).sqrt();
        // A gray has no hue to read, and atan2(0, 0) is undefined on some
        // GPUs; every shift below leaves it alone anyway.
        if c < 1e-6 {
            return rgb;
        }
        let h = b.atan2(a);
        let (i, s) = self.segment(h);
        let j = (i + 1) % 8;
        let blend = |p: &[f32; 8]| p[i] + (p[j] - p[i]) * s;
        let cw = (c / CHROMA_FULL).min(1.0);
        let h2 = h + blend(&self.mix_hue) * cw;
        let c2 = c * (1.0 + blend(&self.mix_sat)).max(0.0);
        let l2 = l + blend(&self.mix_lum) * cw * l.max(0.05).sqrt();
        let lin = oklab_to_linear([l2, c2 * h2.cos(), c2 * h2.sin()]);
        lin.map(|x| x.max(0.0).powf(1.0 / 2.2))
    }

    /// The band segment hue `h` falls in, and how far along it, eased:
    /// `(i, s)` puts weight `1 - s` on band `i` and `s` on the next.
    fn segment(&self, h: f32) -> (usize, f32) {
        let hues = &self.band_hue;
        for i in 0..8 {
            let a = hues[i];
            let span = wrap_tau(hues[(i + 1) % 8] - a);
            let d = wrap_tau(h - a);
            if d < span {
                let t = d / span;
                return (i, 0.5 - 0.5 * (std::f32::consts::PI * t).cos());
            }
        }
        // Rounding can leave a hair between two segments; band 0 is where
        // the last one ends.
        (0, 0.0)
    }
}

/// `x` into 0..TAU. Written out rather than `rem_euclid` so it rounds as
/// the shader's `wrapTau` does.
fn wrap_tau(x: f32) -> f32 {
    let tau = std::f32::consts::TAU;
    x - tau * (x / tau).floor()
}

/// Linear sRGB to OkLab (Björn Ottosson, 2020). Must match `oklab` in
/// loupe_common.wgsl.
fn oklab(rgb: [f32; 3]) -> [f32; 3] {
    let [r, g, b] = rgb;
    let l = 0.412_221_47 * r + 0.536_332_55 * g + 0.051_445_995 * b;
    let m = 0.211_903_5 * r + 0.680_699_5 * g + 0.107_396_96 * b;
    let s = 0.088_302_46 * r + 0.281_718_85 * g + 0.629_978_7 * b;
    let [l, m, s] = [l, m, s].map(cbrt);
    [
        0.210_454_26 * l + 0.793_617_8 * m - 0.004_072_047 * s,
        1.977_998_5 * l - 2.428_592_2 * m + 0.450_593_7 * s,
        0.025_904_037 * l + 0.782_771_77 * m - 0.808_675_77 * s,
    ]
}

/// OkLab to linear sRGB. Must match `oklabToLinear` in loupe_common.wgsl.
fn oklab_to_linear(lab: [f32; 3]) -> [f32; 3] {
    let [l, a, b] = lab;
    let l_ = l + 0.396_337_78 * a + 0.215_803_76 * b;
    let m_ = l - 0.105_561_346 * a - 0.063_854_17 * b;
    let s_ = l - 0.089_484_18 * a - 1.291_485_5 * b;
    let [l, m, s] = [l_, m_, s_].map(|v| v * v * v);
    [
        4.076_741_7 * l - 3.307_711_6 * m + 0.230_969_94 * s,
        -1.268_438 * l + 2.609_757_4 * m - 0.341_319_38 * s,
        -0.004_196_086_3 * l - 0.703_418_6 * m + 1.707_614_7 * s,
    ]
}

/// A cube root as the shader takes it, through `pow` on the magnitude.
fn cbrt(v: f32) -> f32 {
    v.signum() * v.abs().powf(1.0 / 3.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hsv(deg: f32) -> [f32; 3] {
        let hp = deg / 60.0;
        let x = 1.0 - (hp % 2.0 - 1.0).abs();
        match hp as u32 {
            0 => [1.0, x, 0.0],
            1 => [x, 1.0, 0.0],
            2 => [0.0, 1.0, x],
            3 => [0.0, x, 1.0],
            4 => [x, 0.0, 1.0],
            _ => [1.0, 0.0, x],
        }
    }

    fn lch(rgb: [f32; 3]) -> [f32; 3] {
        let [l, a, b] = oklab(rgb.map(|x| x.max(0.0).powf(2.2)));
        [l, (a * a + b * b).sqrt(), b.atan2(a)]
    }

    fn mixer(band: Band, shift: BandShift) -> ColorMixer {
        let mut m = ColorMixer::default();
        *match band {
            Band::Red => &mut m.red,
            Band::Orange => &mut m.orange,
            Band::Yellow => &mut m.yellow,
            Band::Green => &mut m.green,
            Band::Aqua => &mut m.aqua,
            Band::Blue => &mut m.blue,
            Band::Purple => &mut m.purple,
            Band::Magenta => &mut m.magenta,
        } = shift;
        m
    }

    /// Catches a band centre typed wrong, or listed out of order.
    #[test]
    fn band_hues_are_the_hsv_hues_in_oklch() {
        for (deg, want) in [0.0, 30.0, 60.0, 120.0, 180.0, 225.0, 270.0, 315.0]
            .into_iter()
            .zip(BAND_HUES)
        {
            let got = lch(hsv(deg))[2];
            assert!((got - want).abs() < 1e-4, "{deg}: {got} vs {want}");
        }
        let total: f32 = (0..8)
            .map(|i| wrap_tau(BAND_HUES[(i + 1) % 8] - BAND_HUES[i]))
            .sum();
        assert!((total - std::f32::consts::TAU).abs() < 1e-4, "{total}");
    }

    #[test]
    fn oklab_round_trips_and_keeps_gray_neutral() {
        for c in [[0.1, 0.5, 0.9], [1.0, 0.0, 0.0], [0.02, 0.03, 0.01]] {
            let back = oklab_to_linear(oklab(c));
            for i in 0..3 {
                assert!((back[i] - c[i]).abs() < 1e-4, "{c:?} {back:?}");
            }
        }
        let [l, a, b] = oklab([0.18; 3]);
        assert!(a.abs() < 1e-4 && b.abs() < 1e-4 && l > 0.5, "{l} {a} {b}");
    }

    /// Catches a mixer that leaks into the bands it should leave alone.
    #[test]
    fn a_band_moves_its_colours_and_leaves_far_ones_and_grays() {
        let ops = ColorOps::new(
            &mixer(
                Band::Blue,
                BandShift {
                    saturation: -100.0,
                    luminance: -60.0,
                    ..Default::default()
                },
            ),
            false,
        );
        let blue = [0.15, 0.3, 0.9];
        let out = ops.apply(blue);
        assert!(lch(out)[1] < 0.01, "blue keeps chroma {:?}", lch(out));
        for far in [[0.9, 0.2, 0.1], [0.9, 0.85, 0.2], [0.5, 0.5, 0.5]] {
            let got = ops.apply(far);
            for i in 0..3 {
                assert!((got[i] - far[i]).abs() < 1e-3, "{far:?} -> {got:?}");
            }
        }
    }

    /// Catches a hard edge between bands: a hue sweep moves smoothly. The
    /// jump is measured in linear light, since the gamma encoding is
    /// steep where a channel leaves 0.
    #[test]
    fn a_hue_sweep_has_no_seam() {
        let ops = ColorOps::new(
            &mixer(
                Band::Green,
                BandShift {
                    hue: 100.0,
                    saturation: 100.0,
                    luminance: 100.0,
                },
            ),
            false,
        );
        let linear = |rgb: [f32; 3]| ops.apply(rgb).map(|x| x.powf(2.2));
        let mut prev = linear(hsv(0.0).map(|x| 0.2 + 0.6 * x));
        for k in 1..=3600 {
            let out = linear(hsv(k as f32 / 10.0 % 360.0).map(|x| 0.2 + 0.6 * x));
            let jump = (0..3).map(|i| (out[i] - prev[i]).abs()).fold(0.0, f32::max);
            assert!(jump < 0.02, "seam at {} degrees: {jump}", k as f32 / 10.0);
            prev = out;
        }
    }

    #[test]
    fn hue_shift_turns_the_band_and_monochrome_skips_the_mixer() {
        let m = mixer(
            Band::Red,
            BandShift {
                hue: 100.0,
                ..Default::default()
            },
        );
        // A red with room to turn inside sRGB, so no channel clips.
        let red = [0.7, 0.3, 0.28];
        let before = lch(red)[2];
        let after = lch(ColorOps::new(&m, false).apply(red))[2];
        assert!(after - before > 0.3, "{before} -> {after}");
        assert_eq!(ColorOps::new(&m, true), ColorOps::default());
        assert_eq!(
            ColorOps::new(&ColorMixer::default(), false),
            ColorOps::default()
        );
    }

    #[test]
    fn out_of_range_sliders_clamp() {
        let m = mixer(
            Band::Aqua,
            BandShift {
                hue: f32::INFINITY,
                saturation: -1e9,
                luminance: 500.0,
            },
        );
        let ops = ColorOps::new(&m, false);
        assert_eq!(ops.mix_hue[4], HUE_MAX);
        assert_eq!(ops.mix_sat[4], -1.0);
        assert_eq!(ops.mix_lum[4], LUM_MAX);
        assert!(ops.apply([0.1, 0.8, 0.8]).iter().all(|v| v.is_finite()));
    }
}
