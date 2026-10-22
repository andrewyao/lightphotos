// SPDX-License-Identifier: MIT OR Apache-2.0

//! The base layer: a low-frequency estimate of the photo around each pixel,
//! for Clarity and Dehaze. It lives on a fixed grid in source UV whose short
//! side is [`GRID_SHORT`] cells at every image size, so a thumbnail, the
//! Loupe's preview and a full-size export see the same field. It is built
//! from the pre-tone pixel (CA removed, touch-ups blended, before white
//! balance), so no slider drag invalidates it.
//!
//! The fine layer is the same build on a [`FINE_SHORT`] grid, read only for
//! its `mid`: Texture's local mean, a few pixels wide on a full-size photo
//! but the same share of the frame at every size.
//!
//! `base.wgsl` builds the same grid on the GPU and `baseAt` in
//! loupe_common.wgsl reads it. Both do this module's arithmetic in the same
//! order with no hardware filtering, so the two agree to f32 rounding.

/// Cells on the grid's short side, for every image size. Mirrored as the
/// grid size in `Renderer::set_image`.
pub const GRID_SHORT: u32 = 256;
/// The most cells on the grid's long side, which bounds a panorama's grid.
pub const GRID_LONG_MAX: u32 = 4096;
/// Cells on the fine grid's short side: its `mid` blur (sigma 2 cells) is
/// 0.26% of the short side, about 10 px on a 24 MP photo.
pub const FINE_SHORT: u32 = 768;
/// The fine grid's long-side cap, WebGPU's smallest texture limit.
pub const FINE_LONG_MAX: u32 = 8192;

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

/// The fewest texel rows one thread's band of the layer build covers.
const BAND_TEXEL_ROWS: u64 = 32;

/// Below this the detail ratio stops trusting a luma, in gamma-2.2 units.
const DETAIL_EPS: f32 = 1.0 / 255.0;

/// A layer's grid for an image: a fixed short side, the long side by
/// aspect up to a cap. Only [`Grid::for_image`] and [`Grid::fine`] make
/// one, so every grid has nonzero sides.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Grid {
    w: u32,
    h: u32,
}

impl Grid {
    /// The base layer's grid. `None` for an image with a zero side.
    pub fn for_image(w: u32, h: u32) -> Option<Grid> {
        Self::sized(GRID_SHORT, GRID_LONG_MAX, w, h)
    }

    /// The fine layer's grid, no finer than the image itself, which bounds
    /// the cost on a thumbnail. `None` for an image with a zero side.
    pub fn fine(w: u32, h: u32) -> Option<Grid> {
        Self::sized(FINE_SHORT.min(w.min(h)), FINE_LONG_MAX, w, h)
    }

    fn sized(short_cells: u32, long_max: u32, w: u32, h: u32) -> Option<Grid> {
        let (short, long) = (w.min(h) as u64, w.max(h) as u64);
        if short == 0 {
            return None;
        }
        let cells = (short_cells as u64 * long + short / 2) / short;
        let cells = cells.clamp(short_cells as u64, long_max as u64) as u32;
        Some(if w >= h {
            Grid {
                w: cells,
                h: short_cells,
            }
        } else {
            Grid {
                w: short_cells,
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
    /// The same at sigma 8 cells. No edit reads it yet; only the probe and
    /// [`Fields::All`] fill it.
    pub wide: f32,
    /// min(r, g, b) of each cell's mean, linear, sigma 2 cells. Dehaze's
    /// dark channel.
    pub dark: f32,
}

/// Which of a [`BaseTexel`]'s fields a CPU build fills.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fields {
    /// Every field, for the probe.
    All,
    /// What Clarity and Dehaze read.
    MidDark,
    /// What Texture reads from the fine layer.
    Mid,
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
    /// Each texel is read once. `fields` says which blurs to run; the rest
    /// stay 0. `None` for a zero side.
    pub fn build(
        w: u32,
        h: u32,
        fields: Fields,
        pre: impl Fn(u32, u32) -> [f32; 3] + Sync,
    ) -> Option<BaseLayer> {
        Self::build_on(Grid::for_image(w, h)?, w, h, fields, pre)
    }

    /// The fine layer: the same build on the [`Grid::fine`] grid, `mid`
    /// only, since only Texture reads it. The GPU fills all three fields
    /// there; the probe compares `mid`.
    pub fn build_fine(
        w: u32,
        h: u32,
        pre: impl Fn(u32, u32) -> [f32; 3] + Sync,
    ) -> Option<BaseLayer> {
        Self::build_on(Grid::fine(w, h)?, w, h, Fields::Mid, pre)
    }

    fn build_on(
        grid: Grid,
        w: u32,
        h: u32,
        fields: Fields,
        pre: impl Fn(u32, u32) -> [f32; 3] + Sync,
    ) -> Option<BaseLayer> {
        let (gw, gh) = (grid.w as usize, grid.h as usize);
        let (w64, h64, gw64, gh64) = (w as u64, h as u64, grid.w as u64, grid.h as u64);
        // Each cell's texels along x and their overlap weights, the same for
        // every row.
        let columns: Vec<Vec<(usize, f32)>> = (0..gw64)
            .map(|i| {
                let (x0, x1) = cell_span(i, gw64, w64);
                (x0..x1)
                    .map(|x| (x as usize, overlap(x, i, gw64, w64) as f32))
                    .collect()
            })
            .collect();

        // A band of cell rows at a time, on any thread. Each band sums its
        // texel rows top to bottom, the order base.wgsl adds them in, so
        // every cell gets the same f32 sum as a single pass. A band covers
        // at least `BAND_TEXEL_ROWS` texel rows, so only the one texel row a
        // band edge may split is read twice, however dense the grid.
        let band_cells = (BAND_TEXEL_ROWS * gh64).div_ceil(h64).max(1);
        let mut sums = vec![[0f32; 3]; gw * gh];
        let threads = w as usize * h as usize >= super::parallel::THREADS_FROM_PX;
        super::parallel::for_each_chunk(
            threads,
            &mut sums,
            gw * band_cells as usize,
            |band, cells| {
                let j_first = band as u64 * band_cells;
                let j_end = j_first + (cells.len() / gw) as u64;
                let mut row = vec![[0f32; 3]; w as usize];
                let mut row_sums = vec![[0f32; 3]; gw];
                let y_first = cell_span(j_first, gh64, h64).0;
                let y_end = cell_span(j_end - 1, gh64, h64).1;
                for y64 in y_first..y_end {
                    let y = y64 as u32;
                    for (x, px) in row.iter_mut().enumerate() {
                        *px = pre(x as u32, y);
                    }
                    for (column, rs) in columns.iter().zip(row_sums.iter_mut()) {
                        *rs = [0.0; 3];
                        for &(x, ox) in column {
                            let p = row[x];
                            for c in 0..3 {
                                rs[c] += ox * p[c];
                            }
                        }
                    }
                    // The cell rows this texel row overlaps, within the band.
                    let j0 = (y64 * gh64 / h64).max(j_first);
                    let j1 = (((y64 + 1) * gh64 - 1) / h64).min(j_end - 1);
                    for j in j0..=j1 {
                        let oy = overlap(y64, j, gh64, h64) as f32;
                        let at = (j - j_first) as usize * gw;
                        for (cell, rs) in cells[at..at + gw].iter_mut().zip(&row_sums) {
                            for c in 0..3 {
                                cell[c] += oy * rs[c];
                            }
                        }
                    }
                }
            },
        );

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
        let zeros = || vec![0.0; mid.len()];
        let wide = match fields {
            Fields::All => blur(&luma, &GAUSS_WIDE),
            Fields::MidDark | Fields::Mid => zeros(),
        };
        let dark = match fields {
            Fields::All | Fields::MidDark => blur(&dark, &GAUSS_MID),
            Fields::Mid => zeros(),
        };
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
        let gw = self.grid.w as i64;
        let [mid, wide, dark] = bilinear(self.grid.w, self.grid.h, u, v, |x, y| {
            let t = self.cells[(y * gw + x) as usize];
            [t.mid, t.wide, t.dark]
        });
        BaseTexel { mid, wide, dark }
    }
}

/// The four texels around `uv * size - 0.5` of a `w x h` field, clamped at
/// the edge and blended in f32, as `textureSample` filters and `baseAt` in
/// loupe_common.wgsl loads. `at` gets coordinates inside the field.
pub(crate) fn bilinear(
    w: u32,
    h: u32,
    u: f32,
    v: f32,
    at: impl Fn(i64, i64) -> [f32; 3],
) -> [f32; 3] {
    let (w, h) = (w as i64, h as i64);
    let sx = u * w as f32 - 0.5;
    let sy = v * h as f32 - 0.5;
    let (fx, fy) = (sx.floor(), sy.floor());
    let (tx, ty) = (sx - fx, sy - fy);
    // Saturating casts: a NaN, infinite or huge coordinate (a sidecar's
    // runaway CA scale) lands on an edge texel.
    let (ix, iy) = (fx as i64, fy as i64);
    let (ix1, iy1) = (ix.saturating_add(1), iy.saturating_add(1));
    let at = |x: i64, y: i64| at(x.clamp(0, w - 1), y.clamp(0, h - 1));
    let (a, b) = (at(ix, iy), at(ix1, iy));
    let (c, d) = (at(ix, iy1), at(ix1, iy1));
    std::array::from_fn(|k| {
        let top = a[k] * (1.0 - tx) + b[k] * tx;
        let bottom = c[k] * (1.0 - tx) + d[k] * tx;
        top * (1.0 - ty) + bottom * ty
    })
}

/// A test view of the base layer, drawn in place of the photo after the
/// touch-ups so the match test can compare the layer with no slider. Never
/// stored: it is not part of [`Adjustments`](super::Adjustments).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BaseProbe {
    Off,
    Mid,
    Wide,
    Dark,
    Detail,
    /// The fine layer's `mid`.
    Fine,
}

impl BaseProbe {
    /// True for the views of the base layer, false for `Off` and `Fine`.
    pub fn reads_base(self) -> bool {
        !matches!(self, BaseProbe::Off | BaseProbe::Fine)
    }

    /// The probe [`BaseProbe::gpu`] wrote, `Off` for anything else.
    pub fn from_gpu(v: f32) -> Self {
        [
            BaseProbe::Mid,
            BaseProbe::Wide,
            BaseProbe::Dark,
            BaseProbe::Detail,
            BaseProbe::Fine,
        ]
        .into_iter()
        .find(|p| p.gpu() == v)
        .unwrap_or(BaseProbe::Off)
    }

    /// `GpuAdjust::base_probe`.
    pub fn gpu(self) -> f32 {
        match self {
            BaseProbe::Off => 0.0,
            BaseProbe::Mid => 1.0,
            BaseProbe::Wide => 2.0,
            BaseProbe::Dark => 3.0,
            BaseProbe::Detail => 4.0,
            BaseProbe::Fine => 5.0,
        }
    }

    /// The gray linear pixel that replaces `pre`, the pre-tone pixel whose
    /// base is `base` and fine layer `fine`. `dark` goes through gamma 2.2
    /// like the other fields; `Detail` amplifies the pixel's luma over `mid`
    /// 8x, so a half-cell shift or a wrong weight shows. Must match
    /// `baseProbe` in loupe_common.wgsl.
    pub fn apply(self, base: BaseTexel, fine: BaseTexel, pre: [f32; 3]) -> [f32; 3] {
        let v = match self {
            BaseProbe::Off => return pre,
            BaseProbe::Fine => fine.mid,
            BaseProbe::Mid => base.mid,
            BaseProbe::Wide => base.wide,
            BaseProbe::Dark => base.dark.max(0.0).powf(1.0 / 2.2),
            BaseProbe::Detail => 0.5 + 8.0 * (luma_gamma(pre) - base.mid),
        };
        [v; 3]
    }
}

/// One separable pass of the half-kernel `kernel` along x (`horizontal`) or
/// y, taps clamped at the edge, summed from the most negative tap up. Must
/// match `fs_base_blur_h` and `fs_base_blur_v` in base.wgsl. Tap `k` of
/// `0..=2r` is offset `k - r`.
fn blur_axis(src: &[f32], gw: usize, gh: usize, kernel: &[f32], horizontal: bool) -> Vec<f32> {
    let r = kernel.len().saturating_sub(1);
    let mut out = vec![0f32; gw * gh];
    if horizontal {
        for (srow, orow) in src.chunks_exact(gw).zip(out.chunks_exact_mut(gw)) {
            for (x, o) in orow.iter_mut().enumerate() {
                for k in 0..=2 * r {
                    let sx = (x + k).saturating_sub(r).min(gw - 1);
                    *o += kernel[k.abs_diff(r)] * srow[sx];
                }
            }
        }
    } else {
        // Whole rows at a time, each output still summed in tap order.
        for (y, orow) in out.chunks_exact_mut(gw).enumerate() {
            for k in 0..=2 * r {
                let sy = (y + k).saturating_sub(r).min(gh - 1);
                let w = kernel[k.abs_diff(r)];
                for (o, s) in orow.iter_mut().zip(&src[sy * gw..][..gw]) {
                    *o += w * s;
                }
            }
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
    fn the_shader_kernels_are_the_cpu_kernels() {
        let wgsl = include_str!("../shaders/base.wgsl");
        let table = |name: &str| -> Vec<f32> {
            wgsl.split(&format!("const {name} = array<f32, "))
                .nth(1)
                .and_then(|rest| rest.split_once(">(")?.1.split(')').next())
                .unwrap_or_else(|| panic!("{name} in base.wgsl"))
                .split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().expect("kernel literal"))
                .collect()
        };
        assert_eq!(table("GAUSS_MID"), GAUSS_MID);
        assert_eq!(table("GAUSS_WIDE"), GAUSS_WIDE);
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

        let fine = |w, h| Grid::fine(w, h).map(|g| (g.width(), g.height()));
        assert_eq!(fine(6000, 4000), Some((1152, 768)));
        assert_eq!(fine(96, 64), Some((96, 64)), "no finer than the image");
        assert_eq!(fine(60000, 1000), Some((FINE_LONG_MAX, 768)));
    }

    /// The overlap weights of every cell sum to the image's area, so a flat
    /// photo averages to itself, whether a cell spans many texels or a
    /// fraction of one.
    #[test]
    fn a_flat_photo_has_a_flat_base() {
        let px = [0.3, 0.5, 0.1];
        for (w, h) in [(7, 5), (96, 64), (1000, 333)] {
            let base = BaseLayer::build(w, h, Fields::All, |_, _| px).unwrap();
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
        let base = BaseLayer::build(w, h, Fields::All, |x, y| {
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
        let full = BaseLayer::build(w, h, Fields::All, decode(&rgba, w)).unwrap();
        let (sw, sh, small_rgba) = fit_long_edge(w, h, rgba.clone(), 600);
        assert_eq!((sw, sh), (600, 400));
        let small = BaseLayer::build(sw, sh, Fields::All, decode(&small_rgba, sw)).unwrap();

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
