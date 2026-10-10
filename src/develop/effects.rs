// SPDX-License-Identifier: MIT OR Apache-2.0

//! Post-crop Vignette and Grain, the two edits that depend on where a pixel
//! sits in the frame. Both run last, on the gamma-space pixel after colour.
//!
//! [`Effects`] turns the sliders into the numbers the per-pixel math needs.
//! The CPU bake reads it directly and [`super::GpuAdjust`] carries it whole
//! to `effects` in loupe_common.wgsl, so the two never derive them twice.
//!
//! Grain lives on a lattice in canvas UV with a fixed cell count across the
//! short side, so a thumbnail, the Loupe and a full-size export see the same
//! pattern. Each octave fades by the share of a cell one output pixel
//! covers, the amplitude a box average of that many cells keeps, so the
//! Loupe at fit shows about what the export shows scaled to the same size.

use serde::{Deserialize, Serialize};

use super::Crop;

/// Lightroom's post-crop vignette, with its slider ranges and defaults.
/// Amount -100 darkens the corners to black, +100 lightens them to white.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Vignette {
    pub amount: f32,
    /// How far from the center the falloff sits, 0..100.
    pub midpoint: f32,
    /// -100 is a rounded rectangle, 0 an ellipse with the crop's shape,
    /// +100 a circle.
    pub roundness: f32,
    /// How wide the falloff is, 0..100.
    pub feather: f32,
}

impl Default for Vignette {
    fn default() -> Self {
        Self {
            amount: 0.0,
            midpoint: 50.0,
            roundness: 0.0,
            feather: 50.0,
        }
    }
}

impl Vignette {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Lightroom's film grain: luminance noise with these sliders and defaults.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(default)]
pub struct Grain {
    pub amount: f32,
    /// 0 is the finest grain, 100 the coarsest.
    pub size: f32,
    /// 0 is even grain, 100 mixes in a finer, irregular octave.
    pub roughness: f32,
}

impl Default for Grain {
    fn default() -> Self {
        Self {
            amount: 0.0,
            size: 25.0,
            roughness: 50.0,
        }
    }
}

impl Grain {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// Grain cells across the canvas's short side at Size 0. Size 100 is 8x
/// coarser. At 25 a 4000 px short side has cells about 3.4 px wide.
const GRAIN_CELLS_FINEST: f32 = 2000.0;
/// The second octave's frequency over the first. Not a whole number, so
/// the two lattices never line up.
const GRAIN_OCTAVE: f32 = 2.31;
/// The gamma-space swing of Grain at 100 on a midtone.
const GRAIN_STRENGTH: f32 = 0.3;

/// The derived numbers for one render of a `w x h` canvas, laid out as the
/// tail of the shader's `Adjust` uniform. Must match its `vig_*` and
/// `grain_*` fields.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Effects {
    /// Amount / 100; 0 is off.
    pub vig_amount: f32,
    /// The superellipse exponent: 2 for an ellipse, up to 6 for a rounded
    /// rectangle.
    pub vig_exponent: f32,
    /// The falloff runs from `vig_lo` to `vig_hi` in distance units, where
    /// 1 is the shape's edge.
    pub vig_lo: f32,
    pub vig_hi: f32,
    /// The shape's half-axes in canvas UV.
    pub vig_axis: [f32; 2],
    /// The crop's center in canvas UV.
    pub vig_center: [f32; 2],
    /// Gamma-space swing at a midtone; 0 is off.
    pub grain_amount: f32,
    /// The second octave's weight, 0..1.
    pub grain_rough: f32,
    /// Grain cells per canvas UV along x and y.
    pub grain_scale: [f32; 2],
}

impl Effects {
    /// `crop` is in canvas UV, `None` for the whole frame. Each slider is
    /// clamped to its range first, since a sidecar can hold anything.
    pub fn new(vignette: &Vignette, grain: &Grain, crop: Option<Crop>, w: f32, h: f32) -> Self {
        let mut fx = Self::default();
        let vig_amount = vignette.amount.clamp(-100.0, 100.0);
        let grain_amount = grain.amount.clamp(0.0, 100.0);
        let c = crop.unwrap_or(Crop {
            left: 0.0,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        });
        if vig_amount != 0.0 && w > 0.0 && h > 0.0 {
            let round = vignette.roundness.clamp(-100.0, 100.0) / 100.0;
            let circle = round.max(0.0);
            let (hw, hh) = ((c.right - c.left) * w / 2.0, (c.bottom - c.top) * h / 2.0);
            // A circle as wide as the crop's area, so +100 keeps the same
            // amount of the frame inside it.
            let r = (hw * hh).max(0.0).sqrt();
            let ax = hw + (r - hw) * circle;
            let ay = hh + (r - hh) * circle;
            let mid = 0.25 + vignette.midpoint.clamp(0.0, 100.0) / 100.0;
            let width = 0.05 + vignette.feather.clamp(0.0, 100.0) / 100.0 * 1.5;
            fx.vig_amount = vig_amount / 100.0;
            fx.vig_exponent = 2.0 + 4.0 * (-round).max(0.0);
            fx.vig_lo = mid - width / 2.0;
            fx.vig_hi = mid + width / 2.0;
            fx.vig_axis = [(ax / w).max(1e-6), (ay / h).max(1e-6)];
            fx.vig_center = [(c.left + c.right) / 2.0, (c.top + c.bottom) / 2.0];
        }
        if grain_amount != 0.0 && w > 0.0 && h > 0.0 {
            let cells = GRAIN_CELLS_FINEST / (grain.size.clamp(0.0, 100.0) / 100.0 * 3.0).exp2();
            let short = w.min(h);
            fx.grain_amount = grain_amount / 100.0 * GRAIN_STRENGTH;
            fx.grain_scale = [cells * w / short, cells * h / short];
            fx.grain_rough = grain.roughness.clamp(0.0, 100.0) / 100.0;
        }
        fx
    }

    pub fn is_off(&self) -> bool {
        self.vig_amount == 0.0 && self.grain_amount == 0.0
    }

    /// Vignette, then Grain, on gamma-space `rgb` at canvas UV `(u, v)`.
    /// `footprint` is one output pixel's size in canvas UV along x and y.
    /// Must match `effects` in loupe_common.wgsl.
    pub fn apply(&self, rgb: [f32; 3], u: f32, v: f32, footprint: [f32; 2]) -> [f32; 3] {
        let mut c = rgb.map(|x| x.clamp(0.0, 1.0));
        if self.vig_amount != 0.0 {
            let m = self.vignette_mask(u, v);
            let dark = 1.0 + self.vig_amount.min(0.0) * m;
            let light = self.vig_amount.max(0.0) * m;
            c = c.map(|x| x * dark + (1.0 - x) * light);
        }
        if self.grain_amount != 0.0 {
            let fp = (footprint[0] * self.grain_scale[0]).max(footprint[1] * self.grain_scale[1]);
            let n = self.grain_noise(u, v, fp);
            let luma = (0.299 * c[0] + 0.587 * c[1] + 0.114 * c[2]).clamp(0.0, 1.0);
            let weight = 0.25 + 3.0 * luma * (1.0 - luma);
            c = c.map(|x| x + self.grain_amount * n * weight);
        }
        c
    }

    /// 0 inside the vignette, 1 past its falloff. Must match `vignetteMask`
    /// in loupe_common.wgsl.
    fn vignette_mask(&self, u: f32, v: f32) -> f32 {
        let n = self.vig_exponent;
        let x = ((u - self.vig_center[0]) / self.vig_axis[0])
            .abs()
            .max(1e-6);
        let y = ((v - self.vig_center[1]) / self.vig_axis[1])
            .abs()
            .max(1e-6);
        let d = if n == 2.0 {
            (x * x + y * y).sqrt()
        } else {
            (x.powf(n) + y.powf(n)).powf(1.0 / n)
        };
        let t = ((d - self.vig_lo) / (self.vig_hi - self.vig_lo)).clamp(0.0, 1.0);
        t * t * (3.0 - 2.0 * t)
    }

    /// Two octaves of value noise, each faded by `fp`, the cells one output
    /// pixel covers. Must match `grainNoise` in loupe_common.wgsl.
    fn grain_noise(&self, u: f32, v: f32, fp: f32) -> f32 {
        let (px, py) = (u * self.grain_scale[0], v * self.grain_scale[1]);
        let n1 = value_noise(px, py, 0) / fp.max(1.0);
        let r = self.grain_rough;
        if r == 0.0 {
            return n1;
        }
        let n2 = value_noise(px * GRAIN_OCTAVE, py * GRAIN_OCTAVE, OCTAVE_SALT)
            / (fp * GRAIN_OCTAVE).max(1.0);
        (n1 + r * n2) / (1.0 + r * r).sqrt()
    }
}

/// PCG's output hash. Integer math, so the CPU and the GPU agree exactly.
/// Must match `pcgHash` in loupe_common.wgsl.
fn pcg_hash(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// The second octave's salt, so its lattice values differ from the first's.
const OCTAVE_SALT: u32 = 0x9e37_79b9;

/// A value in -1..1 for lattice point `(x, y)` of the octave salted `salt`.
fn lattice(x: i32, y: i32, salt: u32) -> f32 {
    let h = pcg_hash((x as u32) ^ pcg_hash((y as u32) ^ salt));
    (h >> 8) as f32 * (2.0 / 16_777_215.0) - 1.0
}

/// Smooth value noise at lattice coordinates `(x, y)`: the four corner
/// values blended with smoothstep weights, so it has no seams. Must match
/// `valueNoise` in loupe_common.wgsl.
fn value_noise(x: f32, y: f32, salt: u32) -> f32 {
    let (fx, fy) = (x.floor(), y.floor());
    let (ix, iy) = (fx as i32, fy as i32);
    let (tx, ty) = (x - fx, y - fy);
    let (sx, sy) = (tx * tx * (3.0 - 2.0 * tx), ty * ty * (3.0 - 2.0 * ty));
    let a = lattice(ix, iy, salt);
    let b = lattice(ix.wrapping_add(1), iy, salt);
    let c = lattice(ix, iy.wrapping_add(1), salt);
    let d = lattice(ix.wrapping_add(1), iy.wrapping_add(1), salt);
    let top = a + (b - a) * sx;
    let bottom = c + (d - c) * sx;
    top + (bottom - top) * sy
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(vignette: Vignette, grain: Grain) -> Effects {
        Effects::new(&vignette, &grain, None, 1500.0, 1000.0)
    }

    #[test]
    fn default_sliders_are_off_and_change_nothing() {
        let e = fx(Vignette::default(), Grain::default());
        assert!(e.is_off());
        let px = [0.2, 0.5, 0.9];
        assert_eq!(e.apply(px, 0.0, 0.0, [1e-3; 2]), px);
    }

    #[test]
    fn a_negative_vignette_darkens_corners_and_spares_the_center() {
        let e = fx(
            Vignette {
                amount: -100.0,
                ..Default::default()
            },
            Grain::default(),
        );
        let gray = [0.5; 3];
        assert_eq!(e.apply(gray, 0.5, 0.5, [0.0; 2]), gray);
        let corner = e.apply(gray, 0.0, 0.0, [0.0; 2]);
        assert!(corner[0] < 0.05, "{corner:?}");
        let edge = e.apply(gray, 0.0, 0.5, [0.0; 2])[0];
        assert!(edge > corner[0] && edge < 0.5, "{edge}");
    }

    #[test]
    fn a_positive_vignette_lightens_toward_white() {
        let e = fx(
            Vignette {
                amount: 100.0,
                ..Default::default()
            },
            Grain::default(),
        );
        let corner = e.apply([0.2; 3], 0.0, 0.0, [0.0; 2]);
        assert!(corner[0] > 0.95, "{corner:?}");
    }

    /// The vignette follows the crop: its center is the crop's center, not
    /// the photo's.
    #[test]
    fn the_vignette_centers_on_the_crop() {
        let crop = Crop {
            left: 0.5,
            top: 0.0,
            right: 1.0,
            bottom: 1.0,
        };
        let v = Vignette {
            amount: -100.0,
            ..Default::default()
        };
        let e = Effects::new(&v, &Grain::default(), Some(crop), 1500.0, 1000.0);
        assert_eq!(e.apply([0.5; 3], 0.75, 0.5, [0.0; 2]), [0.5; 3]);
        assert!(e.apply([0.5; 3], 0.5, 0.0, [0.0; 2])[0] < 0.1);
    }

    /// A round vignette reaches as far along both axes in pixels; the
    /// default one is stretched to the frame.
    #[test]
    fn roundness_100_is_a_circle_in_pixels() {
        let at = |round: f32, u: f32, v: f32| {
            let e = fx(
                Vignette {
                    amount: -100.0,
                    roundness: round,
                    ..Default::default()
                },
                Grain::default(),
            );
            e.apply([0.5; 3], u, v, [0.0; 2])[0]
        };
        // 300 px from the center along x and along y on a 1500 x 1000 frame.
        let (dx, dy) = (300.0 / 1500.0, 300.0 / 1000.0);
        let (x, y) = (at(100.0, 0.5 + dx, 0.5), at(100.0, 0.5, 0.5 + dy));
        assert!((x - y).abs() < 1e-5, "{x} {y}");
        let (x, y) = (at(0.0, 0.5 + dx, 0.5), at(0.0, 0.5, 0.5 + dy));
        assert!(x > y, "an ellipse reaches further along the long side");
    }

    #[test]
    fn grain_is_luminance_noise_that_averages_to_about_zero() {
        let e = fx(
            Vignette::default(),
            Grain {
                amount: 100.0,
                ..Default::default()
            },
        );
        let (mut sum, mut moved) = (0.0f64, 0);
        let n = 200;
        for j in 0..n {
            for i in 0..n {
                let (u, v) = (i as f32 / n as f32, j as f32 / n as f32);
                let out = e.apply([0.5; 3], u, v, [0.0; 2]);
                assert_eq!(out[0], out[1]);
                assert_eq!(out[1], out[2]);
                sum += (out[0] - 0.5) as f64;
                moved += usize::from((out[0] - 0.5).abs() > 0.01);
            }
        }
        assert!(moved > n * n / 2, "{moved}");
        assert!(
            (sum / (n * n) as f64).abs() < 0.01,
            "{}",
            sum / (n * n) as f64
        );
    }

    /// The same canvas point gets the same grain at any image size, and a
    /// pixel covering many cells gets less of it.
    #[test]
    fn grain_is_fixed_to_the_frame_and_fades_with_the_footprint() {
        let g = Grain {
            amount: 100.0,
            ..Default::default()
        };
        let small = Effects::new(&Vignette::default(), &g, None, 600.0, 400.0);
        let big = Effects::new(&Vignette::default(), &g, None, 6000.0, 4000.0);
        let at = |e: &Effects, fp: f32| e.apply([0.5; 3], 0.31, 0.62, [fp, fp])[0] - 0.5;
        assert_eq!(at(&small, 0.0), at(&big, 0.0));
        let fine = at(&big, 1.0 / 6000.0);
        let coarse = at(&big, 1.0 / 600.0);
        assert!(fine != 0.0);
        assert!(coarse.abs() < fine.abs(), "{coarse} {fine}");
    }

    #[test]
    fn value_noise_has_no_seams_at_cell_edges() {
        for i in -3..3 {
            let x = i as f32;
            let below = value_noise(x - 1e-4, 0.37, 0);
            let above = value_noise(x + 1e-4, 0.37, 0);
            assert!((below - above).abs() < 1e-3, "{x}: {below} {above}");
        }
    }

    /// A sidecar's `1e39` reads as an infinite f32.
    #[test]
    fn infinite_sliders_from_a_sidecar_stay_finite() {
        let e = fx(
            Vignette {
                amount: f32::INFINITY,
                midpoint: f32::NEG_INFINITY,
                roundness: f32::INFINITY,
                feather: f32::INFINITY,
            },
            Grain {
                amount: f32::INFINITY,
                size: f32::INFINITY,
                roughness: f32::NEG_INFINITY,
            },
        );
        for (u, v) in [(0.0, 0.0), (0.5, 0.5), (0.9, 0.2)] {
            let out = e.apply([0.5; 3], u, v, [1e-3; 2]);
            assert!(out.iter().all(|c| c.is_finite()), "{out:?}");
        }
    }

    #[test]
    fn extreme_sliders_and_a_zero_frame_stay_finite() {
        for (w, h) in [(0.0, 0.0), (1.0, 1.0), (5000.0, 1.0)] {
            let e = Effects::new(
                &Vignette {
                    amount: -100.0,
                    midpoint: 0.0,
                    roundness: -100.0,
                    feather: 0.0,
                },
                &Grain {
                    amount: 100.0,
                    size: 100.0,
                    roughness: 100.0,
                },
                None,
                w,
                h,
            );
            for (u, v) in [(0.0, 0.0), (0.5, 0.5), (1.0, 0.3)] {
                let out = e.apply([0.5; 3], u, v, [1.0, 1.0]);
                assert!(out.iter().all(|c| c.is_finite()), "{w}x{h}: {out:?}");
            }
        }
    }
}
