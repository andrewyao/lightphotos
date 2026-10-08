// SPDX-License-Identifier: MIT OR Apache-2.0

//! Measures lateral chromatic aberration: how much the lens magnifies red
//! and blue relative to green. The result is a [`CaScale`] that the Loupe
//! shader and `image_ops::bake_edited` both undo the same way.

use crate::develop::CaScale;
use crate::image_decode::DecodedImage;
use crate::image_ops::pixel_linear;

/// The largest scale `measure` reports. Real lenses stay well inside it, so
/// anything past it is a bad fit, not aberration.
const MAX_SCALE: f32 = 0.005;
/// Edges inside this fraction of the half-diagonal move too little to measure.
const INNER_RADIUS: f32 = 0.35;
/// Half the length of the profile compared along the radius, in pixels.
const HALF_WINDOW: i32 = 6;
/// The farthest a channel is searched along the radius, in pixels.
const MAX_SHIFT: f32 = 3.0;
const SHIFT_STEP: f32 = 0.25;
/// Pixels kept clear of the border so every profile tap is inside the photo.
const MARGIN: f32 = HALF_WINDOW as f32 + MAX_SHIFT + 2.0;
/// The weakest radial green gradient, in linear light per pixel, worth measuring.
const MIN_GRADIENT: f32 = 0.01;
/// How many of the strongest edges are measured.
const MAX_EDGES: usize = 4000;
/// Fewer edges than this and the photo measures zero.
const MIN_EDGES: usize = 40;

/// Measure `img`'s lateral chromatic aberration. Returns zero scales when the
/// photo has too few sharp radial edges to tell.
///
/// At edges far from the center where green changes along the radius, it
/// finds the subpixel shift along the radius that best lines red (then
/// blue) up with green, and fits `shift = k * radius` through the origin.
#[hotpath::measure]
pub fn measure(img: &DecodedImage) -> CaScale {
    let (w, h) = (img.width as f32, img.height as f32);
    if w < 2.0 * MARGIN || h < 2.0 * MARGIN {
        return CaScale::default();
    }
    let edges = radial_edges(img);
    if edges.len() < MIN_EDGES {
        return CaScale::default();
    }
    CaScale {
        red: fit_scale(img, &edges, 0),
        blue: fit_scale(img, &edges, 2),
    }
}

/// A point with a strong radial green edge, with its radius and outward unit
/// direction, in pixels from the center.
struct Edge {
    x: f32,
    y: f32,
    r: f32,
    dx: f32,
    dy: f32,
}

/// The strongest radial green edges in the outer part of the photo, at most
/// [`MAX_EDGES`] of them.
fn radial_edges(img: &DecodedImage) -> Vec<(f32, Edge)> {
    let (w, h) = (img.width as f32, img.height as f32);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let r_max = cx.hypot(cy);
    let stride = (img.width.max(img.height) / 1500).max(2) as usize;
    let mut edges = Vec::new();
    for y in (MARGIN as u32..(h - MARGIN) as u32).step_by(stride) {
        for x in (MARGIN as u32..(w - MARGIN) as u32).step_by(stride) {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let r = (px - cx).hypot(py - cy);
            if r < INNER_RADIUS * r_max {
                continue;
            }
            let (dx, dy) = ((px - cx) / r, (py - cy) / r);
            let g = |t: f32, s: f32| bilinear(img, px + t * dx - s * dy, py + t * dy + s * dx, 1);
            let radial = (g(1.0, 0.0) - g(-1.0, 0.0)) / 2.0;
            let tangential = (g(0.0, 1.0) - g(0.0, -1.0)) / 2.0;
            if radial.abs() < MIN_GRADIENT || radial.abs() < 2.0 * tangential.abs() {
                continue;
            }
            // A clipped channel has lost the edge's shape.
            if pixel_linear(img, x, y).iter().any(|&c| c >= 0.98) {
                continue;
            }
            edges.push((
                radial.abs(),
                Edge {
                    x: px,
                    y: py,
                    r,
                    dx,
                    dy,
                },
            ));
        }
    }
    if edges.len() > MAX_EDGES {
        edges.select_nth_unstable_by(MAX_EDGES, |a, b| b.0.total_cmp(&a.0));
        edges.truncate(MAX_EDGES);
    }
    edges
}

/// Channel `c`'s scale relative to green, fitted over `edges`.
fn fit_scale(img: &DecodedImage, edges: &[(f32, Edge)], c: usize) -> f32 {
    let samples: Vec<(f32, f32)> = edges
        .iter()
        .filter_map(|(_, e)| radial_shift(img, e, c).map(|s| (e.r, s)))
        .collect();
    if samples.len() < MIN_EDGES {
        return 0.0;
    }
    let fit = |pts: &[(f32, f32)]| {
        let (sr, rr) = pts
            .iter()
            .fold((0.0, 0.0), |(sr, rr), &(r, s)| (sr + s * r, rr + r * r));
        sr / rr
    };
    let k = fit(&samples);
    // One round of outlier rejection at three median absolute deviations.
    let mut residuals: Vec<f32> = samples.iter().map(|&(r, s)| (s - k * r).abs()).collect();
    let mid = residuals.len() / 2;
    let mad = *residuals.select_nth_unstable_by(mid, f32::total_cmp).1;
    let kept: Vec<(f32, f32)> = samples
        .into_iter()
        .filter(|&(r, s)| (s - k * r).abs() <= 3.0 * mad.max(0.05))
        .collect();
    if kept.len() < MIN_EDGES {
        return 0.0;
    }
    fit(&kept).clamp(-MAX_SCALE, MAX_SCALE)
}

/// How far along the radius channel `c` sits from green at `e`, in pixels:
/// `c` at radius `r + shift` looks like green at `r`. `None` when `c` has no
/// edge there or the best match is at the search's end.
fn radial_shift(img: &DecodedImage, e: &Edge, c: usize) -> Option<f32> {
    let at = |ch: usize, t: f32| bilinear(img, e.x + t * e.dx, e.y + t * e.dy, ch);
    let green = normalized((-HALF_WINDOW..=HALF_WINDOW).map(|t| at(1, t as f32)))?;
    let steps = (MAX_SHIFT / SHIFT_STEP) as i32;
    let mut costs = Vec::with_capacity((2 * steps + 1) as usize);
    for i in -steps..=steps {
        let s = i as f32 * SHIFT_STEP;
        let other = normalized((-HALF_WINDOW..=HALF_WINDOW).map(|t| at(c, t as f32 + s)))?;
        let cost: f32 = green
            .iter()
            .zip(&other)
            .map(|(a, b)| (a - b) * (a - b))
            .sum();
        costs.push(cost);
    }
    let best = (0..costs.len()).min_by(|&a, &b| costs[a].total_cmp(&costs[b]))?;
    if best == 0 || best == costs.len() - 1 {
        return None;
    }
    // Parabola through the best step and its neighbors.
    let (l, m, r) = (costs[best - 1], costs[best], costs[best + 1]);
    let denom = l - 2.0 * m + r;
    let offset = if denom > 0.0 {
        0.5 * (l - r) / denom
    } else {
        0.0
    };
    Some((best as i32 - steps) as f32 * SHIFT_STEP + offset * SHIFT_STEP)
}

/// `values` shifted to mean 0 and scaled to unit length, so a channel's
/// brightness and contrast don't count, only its shape. `None` for a flat run.
fn normalized(values: impl Iterator<Item = f32>) -> Option<Vec<f32>> {
    let mut v: Vec<f32> = values.collect();
    let mean = v.iter().sum::<f32>() / v.len() as f32;
    v.iter_mut().for_each(|x| *x -= mean);
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm < 1e-3 {
        return None;
    }
    v.iter_mut().for_each(|x| *x /= norm);
    Some(v)
}

/// Channel `c` of `img` at pixel coordinates `(x, y)`, where pixel `i`'s
/// center is `i + 0.5`, interpolated in linear light and clamped at the edge.
fn bilinear(img: &DecodedImage, x: f32, y: f32, c: usize) -> f32 {
    let (fx, fy) = (x - 0.5, y - 0.5);
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let (mx, my) = (img.width as i64 - 1, img.height as i64 - 1);
    let p = |x: f32, y: f32| {
        let (x, y) = (
            (x as i64).clamp(0, mx) as u32,
            (y as i64).clamp(0, my) as u32,
        );
        pixel_linear(img, x, y)[c]
    };
    let top = p(x0, y0) * (1.0 - tx) + p(x0 + 1.0, y0) * tx;
    let bottom = p(x0, y0 + 1.0) * (1.0 - tx) + p(x0 + 1.0, y0 + 1.0) * tx;
    top * (1.0 - ty) + bottom * ty
}

/// Test photos with known aberration, shared with `image_ops`'s tests.
#[cfg(test)]
pub(crate) mod fixture {
    use crate::develop::CaScale;
    use crate::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};

    /// A soft gray checkerboard in linear light, about 0.15..0.85.
    pub(crate) fn chart(x: f32, y: f32) -> f32 {
        let p = std::f32::consts::TAU / 48.0;
        0.5 + 0.35 * (3.0 * (x * p).sin() * (y * p).sin()).tanh()
    }

    /// A `w x h` sRGB8 photo of [`chart`] whose red and blue the lens
    /// magnified by `1 + ca.red` and `1 + ca.blue` about the center.
    pub(crate) fn photo(w: u32, h: u32, ca: CaScale) -> DecodedImage {
        let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
        let encode = |v: f32| (v.powf(1.0 / 2.2) * 255.0).round() as u8;
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let (px, py) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
                let at = |k: f32| chart(cx + px / (1.0 + k), cy + py / (1.0 + k));
                rgba.extend([
                    encode(at(ca.red)),
                    encode(at(0.0)),
                    encode(at(ca.blue)),
                    255,
                ]);
            }
        }
        DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::photo;
    use super::*;
    use crate::image_decode::{DecodedImageFields, PixelFormat};

    #[test]
    fn recovers_the_red_and_blue_magnification() {
        let truth = CaScale {
            red: 0.002,
            blue: -0.0015,
        };
        let got = measure(&photo(1200, 800, truth));
        assert!((got.red - truth.red).abs() < 1e-4, "{got:?}");
        assert!((got.blue - truth.blue).abs() < 1e-4, "{got:?}");
    }

    #[test]
    fn a_clean_lens_measures_about_zero() {
        let got = measure(&photo(1200, 800, CaScale::default()));
        assert!(got.red.abs() < 1e-4 && got.blue.abs() < 1e-4, "{got:?}");
    }

    #[test]
    fn a_flat_photo_measures_zero() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 64,
            height: 48,
            rgba: vec![128; 64 * 48 * 4],
            pixel_format: PixelFormat::Srgb8,
        });
        assert_eq!(measure(&img), CaScale::default());
    }

    #[test]
    fn an_empty_photo_measures_zero() {
        let img = DecodedImage::new_tracked(DecodedImageFields {
            width: 0,
            height: 0,
            rgba: Vec::new(),
            pixel_format: PixelFormat::Srgb8,
        });
        assert_eq!(measure(&img), CaScale::default());
    }
}
