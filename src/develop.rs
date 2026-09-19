// SPDX-License-Identifier: GPL-3.0-or-later

//! The edit model. [`Adjustments`] is what the catalog stores, [`GpuAdjust`]
//! is its uniform-buffer copy, and [`apply_linear`] is the CPU tone pipeline
//! used for export, edited thumbnails, and the histogram. The Loupe runs the
//! same math in `shader.wgsl`; the two must stay in sync.

use serde::{Deserialize, Serialize};

/// Range of every tone slider except exposure and denoise. 0 is no change.
pub const TONE_RANGE: std::ops::RangeInclusive<f32> = -100.0..=100.0;
/// Exposure range in stops.
pub const EXPOSURE_RANGE: std::ops::RangeInclusive<f32> = -5.0..=5.0;
/// Denoise strength range. 0 is off.
pub const DENOISE_RANGE: std::ops::RangeInclusive<f32> = 0.0..=100.0;

/// One Develop slider. The panel draws `SLIDERS` in order and keyboard focus
/// indexes it, so both always agree.
pub struct Slider {
    pub section: Section,
    pub id: SliderId,
    pub field: fn(&mut Adjustments) -> &mut f32,
    pub range: std::ops::RangeInclusive<f32>,
    pub decimals: usize,
    /// Keyboard nudge size.
    pub step: f32,
}

const fn tone(
    section: Section,
    id: SliderId,
    field: fn(&mut Adjustments) -> &mut f32,
) -> Slider {
    Slider {
        section,
        id,
        field,
        range: TONE_RANGE,
        decimals: 0,
        step: 1.0,
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Section {
    WhiteBalance,
    Tone,
    Presence,
    Detail,
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum SliderId {
    Temp,
    Tint,
    Exposure,
    Contrast,
    Highlights,
    Shadows,
    Whites,
    Blacks,
    Vibrance,
    Saturation,
    Denoise,
}

pub const SLIDERS: [Slider; 11] = [
    tone(Section::WhiteBalance, SliderId::Temp, |a| &mut a.temp),
    tone(Section::WhiteBalance, SliderId::Tint, |a| &mut a.tint),
    Slider {
        section: Section::Tone,
        id: SliderId::Exposure,
        field: |a| &mut a.exposure,
        range: EXPOSURE_RANGE,
        decimals: 2,
        step: 0.05,
    },
    tone(Section::Tone, SliderId::Contrast, |a| &mut a.contrast),
    tone(Section::Tone, SliderId::Highlights, |a| &mut a.highlights),
    tone(Section::Tone, SliderId::Shadows, |a| &mut a.shadows),
    tone(Section::Tone, SliderId::Whites, |a| &mut a.whites),
    tone(Section::Tone, SliderId::Blacks, |a| &mut a.blacks),
    tone(Section::Presence, SliderId::Vibrance, |a| &mut a.vibrance),
    tone(Section::Presence, SliderId::Saturation, |a| &mut a.saturation),
    Slider {
        section: Section::Detail,
        id: SliderId::Denoise,
        field: |a| &mut a.denoise,
        range: DENOISE_RANGE,
        decimals: 0,
        step: 1.0,
    },
];

/// One spot-heal: copy a soft circle from `source` onto `center`. Coordinates
/// are 0..1 of the unrotated image, so the edit works at any size or rotation.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct TouchUp {
    pub center: [f32; 2],
    pub radius: f32,
    pub source: [f32; 2],
    pub feather: f32,
    pub delta: [f32; 3],
}

fn is_zero(v: &f32) -> bool {
    *v == 0.0
}

/// A crop rectangle in 0..1 image coordinates, origin top-left.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
pub struct Crop {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// The edit applied to one image. `Default` is no edit. Zero fields are left
/// out when serialized, so an unedited image writes nothing.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
pub struct Adjustments {
    /// Negative is cooler, positive warmer.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub temp: f32,
    /// Negative is greener, positive more magenta.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub tint: f32,
    /// In stops.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub exposure: f32,
    /// S-curve strength around mid-gray.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub contrast: f32,
    /// Negative recovers highlights, positive brightens them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub highlights: f32,
    /// Negative deepens shadows, positive lifts them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub shadows: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub whites: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub blacks: f32,
    /// Saturation that acts mostly on dull pixels, sparing vivid colours and skin.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub vibrance: f32,
    /// -100 is grayscale.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub saturation: f32,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub denoise: f32,
    /// `None` means the full frame.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crop: Option<Crop>,
}

impl Adjustments {
    /// True for no edit. The catalog skips storing these.
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }

    /// A copy without the crop, for pasting settings onto other photos.
    pub fn tone_only(&self) -> Adjustments {
        Adjustments {
            crop: None,
            ..*self
        }
    }
}

/// A hash of everything that changes how an image renders, used in the
/// thumbnail cache key. Floats are rounded so tiny jitter doesn't re-bake, and
/// every unedited photo gets the same value.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn edit_signature(adj: &Adjustments, rot: u8) -> u64 {
    edit_signature_with_touchups(adj, &[], rot)
}

pub(crate) fn edit_signature_with_touchups(
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> u64 {
    use crate::hash::Fnv1a;

    let mut h = Fnv1a::new();

    // Tone sliders, rounded to 1e-3.
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

    for t in touchups {
        for v in [
            t.center[0],
            t.center[1],
            t.radius,
            t.source[0],
            t.source[1],
            t.feather,
        ] {
            h.write(&((v * 100_000.0).round() as i32).to_le_bytes());
        }
        for v in t.delta {
            h.write(&((v * 100_000.0).round() as i32).to_le_bytes());
        }
    }

    // A full-frame crop hashes the same as no crop. Coordinates round to 1e-5.
    let q = |v: f32| ((v.clamp(0.0, 1.0) * 100_000.0).round()) as u32;
    match adj.crop {
        Some(c) if c.left > 0.0 || c.top > 0.0 || c.right < 1.0 || c.bottom < 1.0 => {
            h.write(&[1]);
            for v in [c.left, c.top, c.right, c.bottom] {
                h.write(&q(v).to_le_bytes());
            }
        }
        _ => h.write(&[0]),
    }

    h.write(&[rot % 4]);

    h.finish()
}

/// [`TouchUp`] as laid out in the GPU storage buffer.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct GpuTouchUp {
    pub center_radius_feather: [f32; 4],
    pub source: [f32; 2],
    pub _pad: [f32; 2],
    pub delta: [f32; 4],
}

impl From<&TouchUp> for GpuTouchUp {
    fn from(t: &TouchUp) -> Self {
        Self {
            center_radius_feather: [t.center[0], t.center[1], t.radius, t.feather],
            source: t.source,
            _pad: [0.0; 2],
            delta: [t.delta[0], t.delta[1], t.delta[2], 0.0],
        }
    }
}

/// [`Adjustments`] as the shader's uniform buffer. Uniforms need 16-byte
/// alignment, so keep the field count a multiple of 4. `texel_w`/`texel_h` are
/// 1/width and 1/height of the shown image, set by `App::gpu_adjust` so
/// denoise can step by whole texels.
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
    /// Leaves `texel_w`/`texel_h` at 1.0; `App::gpu_adjust` sets them.
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

/// The exposure slider's curve, on one linear-light pixel. A plain `2^stops`
/// gain would clip bright areas flat. Brightening instead bends luma toward
/// white and grows chroma more slowly, so highlights roll off and go pale.
/// Shadows get close to the full stops; highlights get much less. Darkening
/// is a plain gain, which recovers RAW values above white. Adapted from
/// RapidRAW's `apply_filmic_exposure`.
///
/// Must match `filmicExposure` in shader.wgsl and raw_shader.wgsl.
fn filmic_exposure(rgb: [f32; 3], stops: f32) -> [f32; 3] {
    // Share of the change that goes through the curve. The rest is plain gain.
    const MIX: f32 = 0.95;
    const MIDTONE: f32 = 1.2;
    // The curve's fixed point. It sits just above white so a pixel at 1.0
    // still responds to the slider.
    const ANCHOR: f32 = 1.06;

    if stops == 0.0 {
        return rgb;
    }

    // Plain gain scales RAW values above white too and keeps colour ratios.
    if stops < 0.0 {
        let gain = stops.exp2();
        return rgb.map(|v| v * gain);
    }

    // Rec.709 luma, because these values are linear light.
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    if luma.abs() < 1e-5 {
        return rgb;
    }

    let scale = (stops * (1.0 - MIX)).exp2();
    // k == 1 is no change, k < 1 lifts, k > 1 darkens.
    let k = (-stops * MIX * MIDTONE).exp2();

    // The curve repeats every ANCHOR, so RAW values above white keep being
    // shaped. sRGB input never passes the anchor, so `base` stays 0 there.
    let la = luma.abs();
    let base = (la / ANCHOR).floor() * ANCHOR;
    let norm = (la - base) / ANCHOR;
    let shaped = norm / (norm + (1.0 - norm) * k);
    let new_luma = luma.signum() * (base + shaped * ANCHOR) * scale;

    // Chroma grows more slowly than luma, and slower still near white. The
    // `max(0.0)` keeps `powf` from returning NaN on a negative base.
    let luma_scale = (new_luma / luma).max(0.0);
    let w = new_luma.clamp(0.0, 2.0) * 0.5;
    let dyn_exp = 0.95 + (0.65 - 0.95) * w;
    // Ramps from no effect at 0 stops, so the slider has no jump.
    let rolloff = 1.0 / (1.0 + (new_luma - 0.9).max(0.0) * 2.0 * stops.min(1.0));
    let chroma_scale = luma_scale.powf(dyn_exp) * rolloff;

    [
        new_luma + (rgb[0] - luma) * chroma_scale,
        new_luma + (rgb[1] - luma) * chroma_scale,
        new_luma + (rgb[2] - luma) * chroma_scale,
    ]
}

/// Apply every edit to one linear-light pixel. Input may exceed 1.0; output is
/// linear RGB clamped to 0..1.
///
/// Must match `fs_main` in shader.wgsl. Keep it simple so the two stay in sync.
pub fn apply_linear(adj: &Adjustments, rgb: [f32; 3]) -> [f32; 3] {
    apply_linear_impl(adj, rgb, false)
}

/// `apply_linear` as raw_shader.wgsl runs it. Returns display values, not
/// linear, because the RAW shader's render target is not an sRGB surface.
pub fn apply_raw_display(adj: &Adjustments, rgb: [f32; 3]) -> [f32; 3] {
    apply_linear_impl(adj, rgb, true)
}

fn apply_linear_impl(adj: &Adjustments, rgb: [f32; 3], raw_display: bool) -> [f32; 3] {
    let [mut r, mut g, mut b] = rgb;

    // White balance as small per-channel gains, at most ±0.3.
    let t = adj.temp / 100.0;
    let ti = adj.tint / 100.0;
    let r_gain = 1.0 + t * 0.3;
    let b_gain = 1.0 - t * 0.3;
    let g_gain = 1.0 - ti * 0.15;
    r *= r_gain;
    g *= g_gain;
    b *= b_gain;

    let exposed = filmic_exposure([r, g, b], adj.exposure);
    r = exposed[0];
    g = exposed[1];
    b = exposed[2];

    // Tone and colour steps below work in gamma space. The RAW path uses
    // raw_shader.wgsl's display curve instead of plain 2.2 gamma.
    let to_gamma = |x: f32| {
        if raw_display {
            let srgb = if x <= 0.0031308 {
                x.max(0.0) * 12.92
            } else {
                1.055 * x.max(0.0).powf(1.0 / 2.4) - 0.055
            };
            let brightened = srgb.clamp(0.0, 1.0).powf(1.0 / 1.1);
            let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
            (brightened + (contrast_curve - brightened) * 0.75).clamp(0.0, 1.0)
        } else {
            x.max(0.0).powf(1.0 / 2.2)
        }
    };
    let mut rg = to_gamma(r);
    let mut gg = to_gamma(g);
    let mut bg = to_gamma(b);

    let tone = |v: f32| -> f32 {
        let mut x = v;

        let blacks = adj.blacks / 100.0 * 0.2;
        let whites = adj.whites / 100.0 * 0.2;
        // Map input `-blacks` to 0 and `1 - whites` to 1. Positive Whites
        // brightens the top end, as in Lightroom.
        x = (x + blacks) / ((1.0 - whites) + blacks);

        // S-curve around 0.5.
        let c = adj.contrast / 100.0 * 0.5;
        if c != 0.0 {
            let d = x - 0.5;
            x = 0.5 + d + c * d * (1.0 - 4.0 * d * d);
        }

        // Shadows and Highlights are weighted toward black and white respectively.
        let s = adj.shadows / 100.0 * 0.3;
        if s != 0.0 {
            let mask = (1.0 - x.clamp(0.0, 1.0)).powf(2.0);
            x += s * mask;
        }

        let h = adj.highlights / 100.0 * 0.3;
        if h != 0.0 {
            let mask = x.clamp(0.0, 1.0).powf(2.0);
            x += h * mask;
        }

        x
    };
    rg = tone(rg);
    gg = tone(gg);
    bg = tone(bg);

    // Vibrance and saturation scale chroma around Rec.601 luma. Vibrance is
    // weaker on pixels that are already saturated.
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

    if raw_display {
        return [rg.clamp(0.0, 1.0), gg.clamp(0.0, 1.0), bg.clamp(0.0, 1.0)];
    }

    let to_linear = |x: f32| x.max(0.0).powf(2.2);
    r = to_linear(rg);
    g = to_linear(gg);
    b = to_linear(bg);

    [r.clamp(0.0, 1.0), g.clamp(0.0, 1.0), b.clamp(0.0, 1.0)]
}

/// The `(temp, tint)` that turn an unedited linear-light pixel gray, by
/// inverting the white-balance gains in [`apply_linear`]:
///
/// ```text
/// r*(1+0.3t)  = b*(1-0.3t)        =>  t  = (b - r) / (0.3*(r + b))
/// r*(1+0.3t)  = g*(1-0.15*ti)     =>  ti = (1 - r*(1+0.3t)/g) / 0.15
/// ```
///
/// Later steps keep gray pixels gray, so other edits don't matter. `None` when
/// the pixel is too dark to solve.
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

/// Denoise kernel radius (5x5). Fixed so cost doesn't grow with strength;
/// the slider only changes how much neighbors blend.
pub(crate) const DENOISE_RADIUS: i32 = 2;

/// Gaussian (sigma 1) weight by squared tap distance, precomputed.
/// Must match `spatialWeight` in shader.wgsl.
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

/// Edge-preserving (bilateral) denoise of one linear-light pixel.
/// `sample(dx, dy)` returns the neighbor at that offset; the caller handles
/// edges. Returns `sample(0, 0)` exactly when denoise is off.
///
/// Must match the denoise branch of `fs_main` in shader.wgsl.
pub(crate) fn denoise_sample(adj: &Adjustments, sample: impl Fn(i32, i32) -> [f32; 3]) -> [f32; 3] {
    denoise_sample_with_strength(adj.denoise, sample)
}

/// [`denoise_sample`] with a plain strength, for the RAW decoders' fixed
/// denoise pass.
pub(crate) fn denoise_sample_with_strength(
    strength: f32,
    sample: impl Fn(i32, i32) -> [f32; 3],
) -> [f32; 3] {
    if strength <= 0.0 {
        return sample(0, 0);
    }
    let center = sample(0, 0);
    let sigma_r = 0.02 + strength / 100.0 * 0.30;
    let sigma_r2 = sigma_r * sigma_r;
    let (mut sum, mut wsum) = ([0f32; 3], 0f32);
    for dy in -DENOISE_RADIUS..=DENOISE_RADIUS {
        for dx in -DENOISE_RADIUS..=DENOISE_RADIUS {
            let tap = sample(dx, dy);
            let d = [tap[0] - center[0], tap[1] - center[1], tap[2] - center[2]];
            let diff2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            // The center tap has weight 1.0, so `wsum` is never 0.
            let w = spatial_weight(dx, dy) / (1.0 + diff2 / sigma_r2);
            for c in 0..3 {
                sum[c] += w * tap[c];
            }
            wsum += w;
        }
    }
    [sum[0] / wsum, sum[1] / wsum, sum[2] / wsum]
}

/// Denoise a whole packed linear-light buffer, clamping at its edges. The
/// `rawler` RAW decoders (browser, Linux, Windows) call this after demosaic
/// because rawler doesn't denoise; ImageIO on macOS does its own. Returns a
/// copy of `buf` when `strength <= 0.0` or the size is bad.
pub(crate) fn denoise_linear_rgb_buffer(
    strength: f32,
    width: usize,
    height: usize,
    buf: &[[f32; 3]],
) -> Vec<[f32; 3]> {
    if strength <= 0.0 || width == 0 || height == 0 || buf.len() < width * height {
        return buf.to_vec();
    }
    (0..height)
        .flat_map(|y| {
            (0..width).map(move |x| {
                denoise_sample_with_strength(strength, |dx, dy| {
                    let sx = (x as i64 + dx as i64).clamp(0, width as i64 - 1) as usize;
                    let sy = (y as i64 + dy as i64).clamp(0, height as i64 - 1) as usize;
                    buf[sy * width + sx]
                })
            })
        })
        .collect()
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
        // A full-frame crop must hash the same as no crop.
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

    fn luma709(px: [f32; 3]) -> f32 {
        0.2126 * px[0] + 0.7152 * px[1] + 0.0722 * px[2]
    }

    #[test]
    fn filmic_exposure_zero_is_identity() {
        let px = [0.6, 0.3, 0.2];
        assert_eq!(filmic_exposure(px, 0.0), px);
        let out = apply_linear(&Adjustments::default(), px);
        for i in 0..3 {
            assert!((out[i] - px[i]).abs() < 1e-5, "channel {i}: {} vs {}", out[i], px[i]);
        }
    }

    #[test]
    fn filmic_exposure_colored_highlights_are_continuous_at_zero() {
        for px in [[1.0, 1.0, 0.0], [1.5, 1.0, 0.5], [3.0, 2.0, 1.0]] {
            for stops in [-1e-4, -1e-5, 1e-5, 1e-4] {
                let out = filmic_exposure(px, stops);
                for i in 0..3 {
                    assert!((out[i] - px[i]).abs() < 10.0 * stops.abs(),
                        "discontinuity for {px:?} at {stops}: {out:?}");
                }
            }
        }
    }

    #[test]
    fn negative_exposure_recovers_raw_highlights_in_display_pipeline() {
        for pipeline in [apply_linear, apply_raw_display] {
            // Multiples of ANCHOR and values several stops above white.
            for level in [1.06, 2.12, 4.24, 8.0, 16.0] {
                let mut previous = 1.0;
                for step in 0..=50 {
                    let adj = Adjustments {
                        exposure: -(step as f32) / 10.0,
                        ..Default::default()
                    };
                    let out = pipeline(&adj, [level; 3]);
                    assert!(out.iter().all(|v| v.is_finite()));
                    assert!(out[0] <= previous + 1e-6);
                    assert!((out[0] - out[1]).abs() < 1e-6);
                    assert!((out[0] - out[2]).abs() < 1e-6);
                    previous = out[0];
                }
                assert!(previous > 0.0 && previous < 0.9,
                    "highlight {level} failed to recover at -5 stops: {previous}");
            }
        }
    }

    #[test]
    fn filmic_exposure_is_monotonic_in_the_slider() {
        let px = [0.2, 0.2, 0.2];
        let mut prev = f32::NEG_INFINITY;
        for stops in [-5.0, -3.0, -1.0, 0.0, 1.0, 3.0, 5.0] {
            let out = luma709(filmic_exposure(px, stops));
            assert!(out > prev, "luma fell going to {stops} stops: {out} after {prev}");
            prev = out;
        }
    }

    #[test]
    fn filmic_exposure_lifts_shadows_more_than_highlights() {
        // A plain `2^stops` gain would give both the same ratio.
        let shadow = [0.05, 0.05, 0.05];
        let highlight = [0.9, 0.9, 0.9];
        let shadow_gain = luma709(filmic_exposure(shadow, 1.0)) / luma709(shadow);
        let highlight_gain = luma709(filmic_exposure(highlight, 1.0)) / luma709(highlight);
        assert!(
            shadow_gain > highlight_gain,
            "expected shadows to gain more than highlights: {shadow_gain} vs {highlight_gain}"
        );
    }

    #[test]
    fn filmic_exposure_desaturates_as_it_brightens() {
        let px = [0.5, 0.1, 0.1];
        let chroma_ratio = |p: [f32; 3]| {
            let l = luma709(p);
            (p[0] - l).abs() / l
        };
        let before = chroma_ratio(px);
        let after = chroma_ratio(filmic_exposure(px, 2.0));
        assert!(
            after < before,
            "expected brightening to desaturate: {after} vs {before}"
        );
    }

    #[test]
    fn filmic_exposure_is_finite_at_the_edges() {
        // Guards the `powf` NaN case: black, near-anchor white, and a pure
        // single-channel colour.
        for px in [[0.0, 0.0, 0.0], [1.0, 1.0, 1.0], [1.0, 0.0, 0.0], [2.0, 2.0, 2.0]] {
            for stops in [-5.0, -1.0, 0.0, 1.0, 5.0] {
                let out = filmic_exposure(px, stops);
                assert!(
                    out.iter().all(|v| v.is_finite()),
                    "non-finite output for {px:?} at {stops} stops: {out:?}"
                );
            }
        }
    }

    #[test]
    fn whites_brightens_the_top_end() {
        // Lightroom's direction: positive Whites brightens, negative recovers.
        let bright = [0.8, 0.8, 0.8];
        let up = apply_linear(&Adjustments { whites: 60.0, ..Default::default() }, bright);
        let down = apply_linear(&Adjustments { whites: -60.0, ..Default::default() }, bright);
        assert!(up[0] > bright[0], "whites +60 should brighten: {}", up[0]);
        assert!(down[0] < bright[0], "whites -60 should recover: {}", down[0]);
    }

    #[test]
    fn blacks_lifts_the_floor() {
        let dark = [0.03, 0.03, 0.03];
        let up = apply_linear(&Adjustments { blacks: 60.0, ..Default::default() }, dark);
        let down = apply_linear(&Adjustments { blacks: -60.0, ..Default::default() }, dark);
        assert!(up[0] > dark[0], "blacks +60 should lift: {}", up[0]);
        assert!(down[0] < dark[0], "blacks -60 should crush: {}", down[0]);
    }

    #[test]
    fn denoise_zero_is_passthrough() {
        let adj = Adjustments::default();
        // Any blending with these neighbors would change the result.
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
        // A step edge at dx = 0. The result must stay nearer its own side
        // than a plain average would.
        let adj = Adjustments {
            denoise: 100.0,
            ..Default::default()
        };
        let step = |dx: i32, _dy: i32| -> [f32; 3] {
            let v = if dx < 0 { 0.0 } else { 1.0 };
            [v, v, v]
        };
        let out = denoise_sample(&adj, step);
        // 3 of 5 columns are 1.0.
        let plain_avg = 0.6;
        assert!(
            (out[0] - 1.0).abs() < (out[0] - plain_avg).abs(),
            "expected range weighting to favor the pixel's own side, got {out:?}"
        );
    }

    #[test]
    fn denoise_sample_with_strength_matches_denoise_sample_at_the_same_value() {
        let adj = Adjustments {
            denoise: 60.0,
            ..Default::default()
        };
        let sample = |dx: i32, dy: i32| -> [f32; 3] {
            let v = ((dx + dy) as f32) * 0.05 + 0.5;
            [v, v, v]
        };
        assert_eq!(
            denoise_sample(&adj, sample),
            denoise_sample_with_strength(60.0, sample)
        );
    }

    #[test]
    fn denoise_linear_rgb_buffer_smooths_an_isolated_outlier() {
        let (w, h) = (3, 3);
        let mut buf = vec![[0.0f32; 3]; w * h];
        buf[4] = [1.0, 1.0, 1.0];
        let out = denoise_linear_rgb_buffer(100.0, w, h, &buf);
        assert!(
            out[4][0] < 1.0 && out[4][0] > 0.0,
            "expected the outlier pulled toward its neighbors, got {:?}",
            out[4]
        );
        // Edge preservation keeps dark pixels from jumping toward the outlier.
        assert!(
            out[0][0] < 0.5,
            "expected a far corner to stay close to its own dark value, got {:?}",
            out[0]
        );
    }

    #[test]
    fn denoise_linear_rgb_buffer_zero_strength_is_identity() {
        let buf = vec![[0.2, 0.3, 0.4], [1.0, 0.0, 0.0]];
        let out = denoise_linear_rgb_buffer(0.0, 2, 1, &buf);
        assert_eq!(out, buf);
    }

    #[test]
    fn denoise_linear_rgb_buffer_clamps_at_edges_without_panicking() {
        let buf = vec![
            [0.1, 0.1, 0.1],
            [0.9, 0.9, 0.9],
            [0.5, 0.5, 0.5],
            [0.3, 0.3, 0.3],
        ];
        let out = denoise_linear_rgb_buffer(50.0, 2, 2, &buf);
        assert_eq!(out.len(), 4);
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
        // Apply the solved gains the way `apply_linear` does and check for gray.
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
