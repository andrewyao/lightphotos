// SPDX-License-Identifier: MIT OR Apache-2.0

//! The base layer: a low-frequency estimate of the photo around each pixel,
//! for Clarity and Dehaze. It lives on a fixed grid in source UV whose short
//! side is [`GRID_SHORT`] cells at every image size, so a thumbnail, the
//! Loupe's preview and a full-size export see the same field. It is built
//! from the pre-tone pixel (CA removed, touch-ups blended, before white
//! balance), so no slider drag invalidates it.
//!
//! `base.wgsl` builds the same grid on the GPU and `baseAt` in
//! loupe_common.wgsl reads it. Both do this module's arithmetic in the same
//! order with no hardware filtering, so the two agree to f32 rounding.

/// Cells on the grid's short side, for every image size. Mirrored as the
/// grid size in `Renderer::set_image`.
pub const GRID_SHORT: u32 = 256;
/// The most cells on the grid's long side, which bounds a panorama's grid.
pub const GRID_LONG_MAX: u32 = 4096;

/// Gaussian half-kernel at sigma 2 cells (0.8% of the short side), center
/// first. Mirrored in base.wgsl; a test rebuilds both from [`gauss_half`].
pub const GAUSS_MID: [f32; 7] = [
    0.19967563,
    0.17621312,
    0.12110939,
    0.064825185,
    0.027023157,
    0.008773135,
    0.002218196,
];
/// The same at sigma 8 cells (3% of the short side).
pub const GAUSS_WIDE: [f32; 25] = [
    0.049976755,
    0.04958783,
    0.04843913,
    0.046583455,
    0.04410433,
    0.041109756,
    0.03772443,
    0.034081183,
    0.030312434,
    0.026542453,
    0.022881025,
    0.019418873,
    0.016225077,
    0.013346383,
    0.010808231,
    0.008617073,
    0.006763618,
    0.005226519,
    0.0039761257,
    0.0029779808,
    0.0021958253,
    0.0015939983,
    0.0011391791,
    0.0008015126,
    0.0005551916,
];

/// Below this the detail ratio stops trusting a luma, in gamma-2.2 units.
const DETAIL_EPS: f32 = 1.0 / 255.0;

/// The grid for an image: short side [`GRID_SHORT`], long side by aspect,
/// capped at [`GRID_LONG_MAX`]. Only [`Grid::for_image`] makes one, so every
/// grid has nonzero sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    w: u32,
    h: u32,
}

impl Grid {
    /// `None` for an image with a zero side.
    pub fn for_image(w: u32, h: u32) -> Option<Grid> {
        let (short, long) = (w.min(h) as u64, w.max(h) as u64);
        if short == 0 {
            return None;
        }
        let cells = (GRID_SHORT as u64 * long + short / 2) / short;
        let cells = cells.clamp(GRID_SHORT as u64, GRID_LONG_MAX as u64) as u32;
        Some(if w >= h {
            Grid {
                w: cells,
                h: GRID_SHORT,
            }
        } else {
            Grid {
                w: GRID_SHORT,
                h: cells,
            }
        })
    }

    pub fn width(&self) -> u32 {
        self.w
    }

    pub fn height(&self) -> u32 {
        self.h
    }
}

/// The base at one source UV. Every value is pre-tone, so no slider moves it.
#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct BaseTexel {
    /// Gamma-2.2 Rec.709 luma, Gaussian sigma 2 cells. Clarity's base.
    pub mid: f32,
    /// The same at sigma 8 cells. Dehaze transmission and wide Clarity.
    pub wide: f32,
    /// min(r, g, b) of each cell's mean, linear, sigma 2 cells. Dehaze's
    /// dark channel.
    pub dark: f32,
}

/// One [`BaseTexel`] per grid cell, row-major.
pub struct BaseLayer {
    grid: Grid,
    cells: Vec<BaseTexel>,
}

/// The texels `[first, end)` along one axis that cell `i` of `cells` covers
/// over `size` texels, and each one's overlap with the cell in units of
/// `1 / cells` texel. A cell's overlaps sum to `size`. Must match
/// `cellSpan` and `overlap` in base.wgsl.
fn cell_span(i: u64, cells: u64, size: u64) -> (u64, u64) {
    (i * size / cells, ((i + 1) * size).div_ceil(cells))
}

fn overlap(x: u64, i: u64, cells: u64, size: u64) -> u64 {
    ((x + 1) * cells).min((i + 1) * size) - (x * cells).max(i * size)
}

impl BaseLayer {
    /// The exact area average of `pre(x, y)`, the linear pre-tone pixel at
    /// texel `(x, y)`'s center, into each cell, then the separable blurs.
    /// Each texel is read once. `None` for a zero side.
    pub fn build(w: u32, h: u32, pre: impl Fn(u32, u32) -> [f32; 3]) -> Option<BaseLayer> {
        let grid = Grid::for_image(w, h)?;
        let (gw, gh) = (grid.w as usize, grid.h as usize);
        let (w64, h64, gw64, gh64) = (w as u64, h as u64, grid.w as u64, grid.h as u64);
        let columns: Vec<(u64, u64)> = (0..gw64).map(|i| cell_span(i, gw64, w64)).collect();

        let mut sums = vec![[0f32; 3]; gw * gh];
        let mut row = vec![[0f32; 3]; w as usize];
        let mut row_sums = vec![[0f32; 3]; gw];
        for y in 0..h {
            for (x, px) in row.iter_mut().enumerate() {
                *px = pre(x as u32, y);
            }
            for (i, (&(x0, x1), rs)) in columns.iter().zip(row_sums.iter_mut()).enumerate() {
                *rs = [0.0; 3];
                for x in x0..x1 {
                    let ox = overlap(x, i as u64, gw64, w64) as f32;
                    let p = row[x as usize];
                    for c in 0..3 {
                        rs[c] += ox * p[c];
                    }
                }
            }
            let y64 = y as u64;
            let (j0, j1) = (y64 * gh64 / h64, ((y64 + 1) * gh64 - 1) / h64);
            for j in j0..=j1 {
                let oy = overlap(y64, j, gh64, h64) as f32;
                let cells = &mut sums[j as usize * gw..][..gw];
                for (cell, rs) in cells.iter_mut().zip(&row_sums) {
                    for c in 0..3 {
                        cell[c] += oy * rs[c];
                    }
                }
            }
        }

        let area = w as f32 * h as f32;
        let means = sums.iter().map(|s| s.map(|v| v / area));
        let (luma, dark): (Vec<f32>, Vec<f32>) = means
            .map(|m| (luma_gamma(m), m[0].min(m[1]).min(m[2])))
            .unzip();
        let blur = |src: &[f32], kernel: &[f32]| {
            let h_pass = blur_axis(src, gw, gh, kernel, true);
            blur_axis(&h_pass, gw, gh, kernel, false)
        };
        let mid = blur(&luma, &GAUSS_MID);
        let wide = blur(&luma, &GAUSS_WIDE);
        let dark = blur(&dark, &GAUSS_MID);
        let cells = mid
            .iter()
            .zip(&wide)
            .zip(&dark)
            .map(|((&mid, &wide), &dark)| BaseTexel { mid, wide, dark })
            .collect();
        Some(BaseLayer { grid, cells })
    }

    pub fn grid(&self) -> Grid {
        self.grid
    }

    /// The base at source UV: the four cells around `uv * grid - 0.5`,
    /// clamped at the edge, blended in f32. Must match `baseAt` in
    /// loupe_common.wgsl.
    pub fn at_uv(&self, u: f32, v: f32) -> BaseTexel {
        let (gw, gh) = (self.grid.w as i64, self.grid.h as i64);
        let sx = u * gw as f32 - 0.5;
        let sy = v * gh as f32 - 0.5;
        let (fx, fy) = (sx.floor(), sy.floor());
        let (tx, ty) = (sx - fx, sy - fy);
        // Saturating casts: a NaN or huge UV lands on an edge cell.
        let (ix, iy) = (fx as i64, fy as i64);
        let at = |x: i64, y: i64| {
            let (x, y) = (x.clamp(0, gw - 1), y.clamp(0, gh - 1));
            self.cells[(y * gw + x) as usize]
        };
        let (a, b) = (at(ix, iy), at(ix.saturating_add(1), iy));
        let (c, d) = (
            at(ix, iy.saturating_add(1)),
            at(ix.saturating_add(1), iy.saturating_add(1)),
        );
        let lerp = |f: fn(&BaseTexel) -> f32| {
            let top = f(&a) * (1.0 - tx) + f(&b) * tx;
            let bottom = f(&c) * (1.0 - tx) + f(&d) * tx;
            top * (1.0 - ty) + bottom * ty
        };
        BaseTexel {
            mid: lerp(|t| t.mid),
            wide: lerp(|t| t.wide),
            dark: lerp(|t| t.dark),
        }
    }
}

/// One separable pass of the half-kernel `kernel` along x (`horizontal`) or
/// y, taps clamped at the edge, summed from the most negative tap up. Must
/// match `fs_base_blur_h` and `fs_base_blur_v` in base.wgsl.
fn blur_axis(src: &[f32], gw: usize, gh: usize, kernel: &[f32], horizontal: bool) -> Vec<f32> {
    let r = kernel.len() as i64 - 1;
    let (gw_i, gh_i) = (gw as i64, gh as i64);
    let mut out = vec![0f32; gw * gh];
    for y in 0..gh_i {
        for x in 0..gw_i {
            let mut acc = 0f32;
            for k in -r..=r {
                let (sx, sy) = if horizontal {
                    ((x + k).clamp(0, gw_i - 1), y)
                } else {
                    (x, (y + k).clamp(0, gh_i - 1))
                };
                acc += kernel[k.unsigned_abs() as usize] * src[(sy * gw_i + sx) as usize];
            }
            out[(y * gw_i + x) as usize] = acc;
        }
    }
    out
}

/// A normalized Gaussian half-kernel of `N` taps at `sigma`, center first:
/// `w[0] + 2 * (w[1] + ... + w[N-1]) == 1`. Computed in f64 so the tables
/// are reproducible.
pub fn gauss_half<const N: usize>(sigma: f64) -> [f32; N] {
    let raw: [f64; N] = std::array::from_fn(|k| (-((k * k) as f64) / (2.0 * sigma * sigma)).exp());
    let total = raw.iter().skip(1).sum::<f64>() * 2.0 + raw.first().copied().unwrap_or(0.0);
    raw.map(|v| (v / total) as f32)
}

/// Gamma-2.2 Rec.709 luma of a linear pixel. Must match `lumaGamma` in
/// loupe_common.wgsl.
pub fn luma_gamma(rgb: [f32; 3]) -> f32 {
    let luma = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    luma.max(0.0).powf(1.0 / 2.2)
}

/// log2 of a pixel's luma over its local base, both gamma 2.2. Must match
/// `log2Detail` in loupe_common.wgsl.
pub fn log2_detail(pre: [f32; 3], base: f32) -> f32 {
    ((luma_gamma(pre) + DETAIL_EPS) / (base + DETAIL_EPS)).log2()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::develop::image_ops::fit_long_edge;
    use crate::develop::{srgb_decode, srgb_encode};

    #[test]
    fn the_kernel_tables_are_their_gaussians() {
        assert_eq!(GAUSS_MID, gauss_half::<7>(2.0));
        assert_eq!(GAUSS_WIDE, gauss_half::<25>(8.0));
    }

    #[test]
    fn the_grid_keeps_the_short_side_and_the_aspect() {
        let size = |w, h| Grid::for_image(w, h).map(|g| (g.width(), g.height()));
        assert_eq!(size(6000, 4000), Some((384, 256)));
        assert_eq!(size(96, 64), Some((384, 256)));
        assert_eq!(size(4000, 6000), Some((256, 384)));
        assert_eq!(size(100, 100), Some((256, 256)));
        assert_eq!(size(60000, 1000), Some((GRID_LONG_MAX, 256)));
        assert_eq!(size(0, 100), None);
        assert_eq!(size(100, 0), None);
    }

    /// The overlap weights of every cell sum to the image's area, so a flat
    /// photo averages to itself, whether a cell spans many texels or a
    /// fraction of one.
    #[test]
    fn a_flat_photo_has_a_flat_base() {
        let px = [0.3, 0.5, 0.1];
        for (w, h) in [(7, 5), (96, 64), (1000, 333)] {
            let base = BaseLayer::build(w, h, |_, _| px).unwrap();
            for t in &base.cells {
                assert!((t.mid - luma_gamma(px)).abs() < 1e-5, "{w}x{h}: {t:?}");
                assert!((t.wide - luma_gamma(px)).abs() < 1e-5, "{w}x{h}: {t:?}");
                assert!((t.dark - 0.1).abs() < 1e-5, "{w}x{h}: {t:?}");
            }
        }
    }

    #[test]
    fn at_uv_at_a_cell_center_is_that_cell() {
        let (w, h) = (300, 200);
        let base = BaseLayer::build(w, h, |x, y| {
            let v = (x as f32 / w as f32) * (y as f32 / h as f32);
            [v, v * 0.5, 1.0 - v]
        })
        .unwrap();
        let g = base.grid();
        for (i, j) in [(0, 0), (17, 3), (g.width() - 1, g.height() - 1), (200, 100)] {
            let (u, v) = (
                (i as f32 + 0.5) / g.width() as f32,
                (j as f32 + 0.5) / g.height() as f32,
            );
            let want = base.cells[(j * g.width() + i) as usize];
            let got = base.at_uv(u, v);
            for (a, b) in [
                (got.mid, want.mid),
                (got.wide, want.wide),
                (got.dark, want.dark),
            ] {
                assert!((a - b).abs() < 1e-6, "cell ({i}, {j}): {got:?} vs {want:?}");
            }
        }
    }

    /// A 2400x1600 photo and its 600x400 export-style reduction have the
    /// same base, because the grid is a fraction of the photo, not texels.
    #[test]
    fn the_base_does_not_depend_on_resolution() {
        let (w, h) = (2400u32, 1600u32);
        let tau = std::f32::consts::TAU;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (u, v) = (x as f32 / w as f32, y as f32 / h as f32);
                let rgb = [
                    0.05 + 0.6 * u,
                    0.1 + 0.5 * v * (0.5 + 0.5 * (tau * u * 3.0).cos()),
                    0.3 + 0.25 * (tau * (u * 5.0 + v * 4.0)).sin(),
                ];
                rgba.extend(rgb.map(|c| (srgb_encode(c) * 255.0).round() as u8));
                rgba.push(255);
            }
        }
        fn decode(rgba: &[u8], w: u32) -> impl Fn(u32, u32) -> [f32; 3] + '_ {
            move |x, y| {
                let i = ((y * w + x) * 4) as usize;
                [0, 1, 2].map(|c| srgb_decode(rgba[i + c] as f32 / 255.0))
            }
        }
        let full = BaseLayer::build(w, h, decode(&rgba, w)).unwrap();
        let (sw, sh, small_rgba) = fit_long_edge(w, h, rgba.clone(), 600);
        assert_eq!((sw, sh), (600, 400));
        let small = BaseLayer::build(sw, sh, decode(&small_rgba, sw)).unwrap();

        assert_eq!(full.grid(), small.grid());
        let worst = |f: fn(&BaseTexel) -> f32| {
            full.cells
                .iter()
                .zip(&small.cells)
                .map(|(a, b)| (f(a) - f(b)).abs())
                .fold(0f32, f32::max)
        };
        let (mid, wide, dark) = (worst(|t| t.mid), worst(|t| t.wide), worst(|t| t.dark));
        assert!(mid < 1e-3 && wide < 1e-3, "mid {mid}, wide {wide}");
        // `dark` is linear, where `fit_long_edge`'s sRGB8 rounding is up to
        // 2.2e-3 at 0.34, and the edge cells repeat one rounded row.
        assert!(dark < 1.5e-3, "dark {dark}");
    }
}
