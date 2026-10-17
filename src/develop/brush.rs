// SPDX-License-Identifier: MIT OR Apache-2.0

//! A brush mask's strokes rasterized into a [`Coverage`], which then renders
//! exactly as a detected mask's does: the Loupe's shader and every CPU bake
//! sample the same bytes, so they agree by construction.
//!
//! A stroke is a row of round dabs along its path. A painting dab adds
//! `flow · w · (1 − a)` to the coverage `a` under it, so overlapping dabs
//! build toward 1 and never past it; an erasing dab multiplies `a` by
//! `1 − flow · w`. `w` is the dab's falloff, the radial mask's
//! [`falloff`](super::mask::falloff). Each segment of the path is cut into
//! equal steps no longer than a quarter radius, so a segment's dabs depend
//! only on its two ends, and a stroke stamped a point at a time while it is
//! painted comes out byte for byte as the whole stroke stamped at once.
//!
//! Rasters live in memory only: the strokes are the source of truth, and a
//! small cache keyed by their content keeps every render after the first
//! from repeating the work.

use std::sync::{Arc, Mutex, PoisonError};

use super::coverage::{Coverage, MAX_COVERAGE_SIDE};
use super::mask::{falloff, Stroke};
use crate::hash::Fnv1a;

/// The most texels one brush raster holds: 1024², about 1254x836 on a 3:2
/// photo. A soft brush is no sharper at more, and it keeps the Loupe's
/// coverage buffer small; see `coverage::MAX_COVERAGE_BYTES`.
pub const MAX_BRUSH_TEXELS: usize = 1 << 20;

/// The most texel visits one raster spends on its dabs. A sidecar's strokes
/// come from whoever supplied the folder, and 20,000 photo-wide dabs would
/// otherwise take minutes. Real painting stays far below: a 500-dab stroke
/// of a tenth of the shorter side visits about 16 million.
const MAX_STAMP_TEXELS: u64 = 1 << 28;

/// Dabs along a segment sit at most this share of the radius apart.
const DAB_SPACING: f32 = 0.25;

/// How many rasters the cache keeps, newest first. Enough for every brush
/// mask a photo can hold, so an edit to one never evicts another.
const CACHE_ENTRIES: usize = super::MAX_MASKS * 2;

/// The raster size for a photo whose width over height is `aspect`: as many
/// texels as [`MAX_BRUSH_TEXELS`] allows in that shape, each side at least 1
/// and at most [`MAX_COVERAGE_SIDE`].
pub fn raster_size(aspect: f32) -> (u32, u32) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect as f64
    } else {
        1.0
    };
    let area = MAX_BRUSH_TEXELS as f64;
    let side = |v: f64| (v.round() as u32).clamp(1, MAX_COVERAGE_SIDE);
    if aspect >= 1.0 {
        let w = side((area * aspect).sqrt());
        (w, side(w as f64 / aspect))
    } else {
        let h = side((area / aspect).sqrt());
        (side(h as f64 * aspect), h)
    }
}

/// A raster being painted: coverage in floats, so a low flow's small steps
/// aren't lost to 8-bit rounding until the end.
pub struct BrushRaster {
    width: u32,
    height: u32,
    alpha: Vec<f32>,
    /// Shorter-side units per UV, along u and v.
    scale: [f32; 2],
    /// Texel visits left before dabs are skipped.
    budget: u64,
}

impl BrushRaster {
    /// An empty raster for a photo of `aspect`.
    pub fn new(aspect: f32) -> BrushRaster {
        let (width, height) = raster_size(aspect);
        let a = width as f32 / height as f32;
        let scale = if a >= 1.0 { [a, 1.0] } else { [1.0, 1.0 / a] };
        BrushRaster {
            width,
            height,
            alpha: vec![0.0; width as usize * height as usize],
            scale,
            budget: MAX_STAMP_TEXELS,
        }
    }

    /// `strokes` painted in order onto an empty raster for `aspect`.
    pub fn of(strokes: &[Stroke], aspect: f32) -> BrushRaster {
        let mut r = BrushRaster::new(aspect);
        for s in strokes {
            r.stamp(s, 0);
        }
        r
    }

    /// Stamps `stroke`'s dabs from point `from` on: the dab at that point
    /// when it is the first, then every segment that ends past it. Stamping
    /// points `0..n` and then `n..` equals stamping `0..` at once.
    pub fn stamp(&mut self, stroke: &Stroke, from: usize) {
        if from == 0 {
            if let Some(&p) = stroke.points.first() {
                self.dab(stroke, p);
            }
        }
        let radius = self.radius(stroke);
        let step = radius * DAB_SPACING;
        for i in from.max(1)..stroke.points.len() {
            let (Some(&a), Some(&b)) = (stroke.points.get(i - 1), stroke.points.get(i)) else {
                break;
            };
            let len = ((b[0] - a[0]) * self.scale[0]).hypot((b[1] - a[1]) * self.scale[1]);
            // Clamped points keep `len` below 30 shorter sides, and `step`
            // is at least half a texel, so the count stays small and finite.
            let n = (len / step).ceil().clamp(1.0, 1e6) as u32;
            for k in 1..=n {
                let t = k as f32 / n as f32;
                self.dab(stroke, [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]);
            }
        }
    }

    /// The stroke's radius in shorter-side units, at least half a texel so
    /// every dab lands on one.
    fn radius(&self, stroke: &Stroke) -> f32 {
        let texel = 1.0 / self.width.min(self.height) as f32;
        stroke.radius.max(texel * 0.5)
    }

    /// One dab at `center`, in source UV.
    fn dab(&mut self, stroke: &Stroke, center: [f32; 2]) {
        if !(center[0].is_finite() && center[1].is_finite()) {
            return;
        }
        let radius = self.radius(stroke);
        let (w, h) = (self.width as f32, self.height as f32);
        // The dab's reach in texels along each axis.
        let reach = [radius / self.scale[0] * w, radius / self.scale[1] * h];
        let cx = center[0] * w - 0.5;
        let cy = center[1] * h - 0.5;
        let x0 = (cx - reach[0]).floor().max(0.0);
        let x1 = (cx + reach[0]).ceil().min(w - 1.0);
        let y0 = (cy - reach[1]).floor().max(0.0);
        let y1 = (cy + reach[1]).ceil().min(h - 1.0);
        if !(x0 <= x1 && y0 <= y1) {
            return;
        }
        let (x0, x1, y0, y1) = (x0 as usize, x1 as usize, y0 as usize, y1 as usize);
        let visits = ((x1 - x0 + 1) * (y1 - y0 + 1)) as u64;
        if visits > self.budget {
            self.budget = 0;
            return;
        }
        self.budget -= visits;
        let flow = stroke.flow;
        let row = self.width as usize;
        // Texel to shorter-side units from the dab's center.
        let unit = [self.scale[0] / w, self.scale[1] / h];
        for y in y0..=y1 {
            let dy = (y as f32 - cy) * unit[1];
            let line = &mut self.alpha[y * row + x0..=y * row + x1];
            for (x, a) in (x0..=x1).zip(line) {
                let dx = (x as f32 - cx) * unit[0];
                let wgt = flow * falloff(dx.hypot(dy) / radius, stroke.feather);
                if wgt <= 0.0 {
                    continue;
                }
                if stroke.erase {
                    *a *= 1.0 - wgt;
                } else {
                    *a += wgt * (1.0 - *a);
                }
            }
        }
    }

    /// The raster as a coverage, each texel rounded to a byte.
    pub fn coverage(&self) -> Coverage {
        let alpha: Vec<u8> = self
            .alpha
            .iter()
            .map(|a| (a.clamp(0.0, 1.0) * 255.0).round() as u8)
            .collect();
        // Both sides are within `MAX_COVERAGE_SIDE` by `raster_size`, and
        // `alpha` holds a byte per texel, so this never fails.
        Coverage::fit(self.width, self.height, &alpha).unwrap_or_else(|_| Coverage::nothing())
    }
}

/// The cache: (content key, raster), newest first.
static CACHE: Mutex<Vec<(u64, Arc<Coverage>)>> = Mutex::new(Vec::new());

/// What a raster of `strokes` at `aspect` depends on: every stroke number
/// and the raster's size.
fn key(strokes: &[Stroke], aspect: f32) -> u64 {
    let (w, h) = raster_size(aspect);
    let mut hash = Fnv1a::new();
    hash.write(&w.to_le_bytes());
    hash.write(&h.to_le_bytes());
    for s in strokes {
        hash.write(&[u8::from(s.erase)]);
        hash.write(&(s.points.len() as u64).to_le_bytes());
        let settings = [s.radius, s.feather, s.flow];
        for v in settings.iter().chain(s.points.iter().flatten()) {
            hash.write(&v.to_bits().to_le_bytes());
        }
    }
    hash.finish()
}

/// The coverage `strokes` paint on a photo of `aspect`, from the cache when
/// it holds them. No strokes cover nothing: a 1x1 zero raster, so an
/// inverted empty brush still covers everything, as Lightroom's does.
pub fn coverage(strokes: &[Stroke], aspect: f32) -> Arc<Coverage> {
    if strokes.is_empty() {
        return Arc::new(Coverage::nothing());
    }
    let key = key(strokes, aspect);
    if let Some(hit) = cached(key) {
        return hit;
    }
    let raster = Arc::new(BrushRaster::of(strokes, aspect).coverage());
    remember_key(key, Arc::clone(&raster));
    raster
}

/// Records that `strokes` at `aspect` rasterize to `raster`, for a stroke
/// painted a dab at a time: the next render of those strokes reuses it.
pub fn remember(strokes: &[Stroke], aspect: f32, raster: Arc<Coverage>) {
    remember_key(key(strokes, aspect), raster);
}

fn cached(key: u64) -> Option<Arc<Coverage>> {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let at = cache.iter().position(|(k, _)| *k == key)?;
    let entry = cache.remove(at);
    let hit = Arc::clone(&entry.1);
    cache.insert(0, entry);
    Some(hit)
}

fn remember_key(key: u64, raster: Arc<Coverage>) {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.retain(|(k, _)| *k != key);
    cache.insert(0, (key, raster));
    cache.truncate(CACHE_ENTRIES);
}

/// Appends `p` to a stroke being recorded unless it lies closer than a
/// quarter radius to the last point kept, in shorter-side units on a photo
/// of `aspect`. Points round to five decimals, so a sidecar holds about 20
/// bytes a point and what the Loupe stamps is what is saved. Returns
/// whether `p` was kept.
pub fn push_point(stroke: &mut Stroke, p: [f32; 2], aspect: f32) -> bool {
    let p = p.map(|v| (v * 100_000.0).round() / 100_000.0);
    if !(p[0].is_finite() && p[1].is_finite()) {
        return false;
    }
    if let Some(last) = stroke.points.last() {
        let (sx, sy) = if aspect >= 1.0 {
            (aspect, 1.0)
        } else {
            (1.0, 1.0 / aspect)
        };
        let d = ((p[0] - last[0]) * sx).hypot((p[1] - last[1]) * sy);
        let far_enough = d >= stroke.radius * DAB_SPACING;
        if !far_enough {
            return false;
        }
    }
    stroke.points.push(p);
    true
}

/// Ends a stroke being recorded at `p`, the release point, which thinning
/// may have skipped: a stroke always ends where the pointer let go. Returns
/// whether `p` was added.
pub fn finish_at(stroke: &mut Stroke, p: [f32; 2]) -> bool {
    let p = p.map(|v| (v * 100_000.0).round() / 100_000.0);
    if !(p[0].is_finite() && p[1].is_finite()) || stroke.points.last() == Some(&p) {
        return false;
    }
    stroke.points.push(p);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stroke(points: &[[f32; 2]]) -> Stroke {
        Stroke {
            radius: 0.1,
            feather: 0.0,
            flow: 1.0,
            erase: false,
            points: points.to_vec(),
        }
    }

    #[test]
    fn a_dab_is_full_at_its_center_and_nothing_past_its_radius() {
        let c = BrushRaster::of(&[stroke(&[[0.5, 0.5]])], 1.0).coverage();
        assert_eq!(c.at_uv([0.5, 0.5]), 1.0);
        assert_eq!(c.at_uv([0.5 + 0.09, 0.5]), 1.0, "inside a hard edge");
        assert_eq!(c.at_uv([0.5 + 0.12, 0.5]), 0.0, "past the radius");
        assert_eq!(c.at_uv([0.1, 0.1]), 0.0);
    }

    #[test]
    fn a_dab_stays_round_on_a_wide_photo() {
        // 0.1 of the shorter side is 0.1 / 2 in u on a 2:1 photo.
        let c = BrushRaster::of(&[stroke(&[[0.5, 0.5]])], 2.0).coverage();
        assert_eq!(c.at_uv([0.5 + 0.04, 0.5]), 1.0);
        assert_eq!(c.at_uv([0.5 + 0.06, 0.5]), 0.0);
        assert_eq!(c.at_uv([0.5, 0.5 + 0.09]), 1.0);
    }

    #[test]
    fn feather_ramps_from_full_to_nothing() {
        let mut s = stroke(&[[0.5, 0.5]]);
        s.feather = 1.0;
        let c = BrushRaster::of(&[s], 1.0).coverage();
        let near = c.at_uv([0.52, 0.5]);
        let mid = c.at_uv([0.55, 0.5]);
        let far = c.at_uv([0.58, 0.5]);
        assert!(near > mid && mid > far && far > 0.0, "{near} {mid} {far}");
        assert!((mid - 0.5).abs() < 0.05, "half way out is half: {mid}");
    }

    #[test]
    fn flow_builds_toward_one_and_never_past_it() {
        let mut s = stroke(&[[0.5, 0.5]]);
        s.flow = 0.3;
        let once = BrushRaster::of(&[s.clone()], 1.0)
            .coverage()
            .at_uv([0.5, 0.5]);
        assert!((once - 0.3).abs() < 0.01, "{once}");
        let thrice = BrushRaster::of(&[s.clone(), s.clone(), s.clone()], 1.0)
            .coverage()
            .at_uv([0.5, 0.5]);
        assert!((thrice - (1.0 - 0.7f32.powi(3))).abs() < 0.01, "{thrice}");
        let many = vec![s; 100];
        let full = BrushRaster::of(&many, 1.0);
        assert!(full.alpha.iter().all(|&a| a <= 1.0));
        assert_eq!(full.coverage().at_uv([0.5, 0.5]), 1.0);
    }

    #[test]
    fn erasing_removes_what_was_painted() {
        let paint = stroke(&[[0.2, 0.5], [0.8, 0.5]]);
        let mut erase = stroke(&[[0.5, 0.2], [0.5, 0.8]]);
        erase.erase = true;
        erase.radius = 0.05;
        let c = BrushRaster::of(&[paint, erase], 1.0).coverage();
        assert_eq!(c.at_uv([0.3, 0.5]), 1.0, "painted");
        assert_eq!(c.at_uv([0.5, 0.5]), 0.0, "erased where the strokes cross");
    }

    #[test]
    fn no_strokes_cover_nothing() {
        assert!(coverage(&[], 1.5).is_nothing());
        assert!(BrushRaster::new(1.5).coverage().is_nothing());
    }

    #[test]
    fn stamping_a_point_at_a_time_equals_stamping_the_stroke() {
        let full = stroke(&[[0.1, 0.1], [0.3, 0.2], [0.35, 0.6], [0.9, 0.9]]);
        let whole = BrushRaster::of(std::slice::from_ref(&full), 1.5).coverage();
        let mut live = BrushRaster::new(1.5);
        let mut growing = stroke(&[]);
        for (i, p) in full.points.iter().enumerate() {
            growing.points.push(*p);
            live.stamp(&growing, i);
        }
        assert_eq!(live.coverage(), whole);
    }

    #[test]
    fn the_cache_returns_the_same_raster_for_the_same_strokes() {
        let s = [stroke(&[[0.25, 0.75], [0.5, 0.5]])];
        let a = coverage(&s, 1.25);
        let b = coverage(&s, 1.25);
        assert!(Arc::ptr_eq(&a, &b));
        let other = coverage(&s, 0.8);
        assert_ne!(a.id(), other.id(), "another shape is another raster");
    }

    #[test]
    fn raster_sizes_keep_the_shape_within_the_budget() {
        for aspect in [1.0, 1.5, 0.66, 10.0, 0.01, 1e9, 0.0, -1.0, f32::NAN] {
            let (w, h) = raster_size(aspect);
            assert!(w >= 1 && h >= 1, "{aspect}");
            assert!(w <= MAX_COVERAGE_SIDE && h <= MAX_COVERAGE_SIDE, "{aspect}");
            assert!(
                (w as usize * h as usize) <= MAX_BRUSH_TEXELS + 2048,
                "{aspect}"
            );
        }
        assert_eq!(raster_size(1.5), (1254, 836));
    }

    #[test]
    fn thinning_keeps_the_ends_and_drops_points_closer_than_a_quarter_radius() {
        let mut s = stroke(&[]);
        assert!(push_point(&mut s, [0.5, 0.5], 1.0));
        assert!(!push_point(&mut s, [0.51, 0.5], 1.0), "0.01 < 0.025");
        assert!(push_point(&mut s, [0.53, 0.5], 1.0));
        assert!(!push_point(&mut s, [0.54, 0.5], 1.0));
        assert!(finish_at(&mut s, [0.54, 0.5]), "the release point is kept");
        assert!(!finish_at(&mut s, [0.54, 0.5]), "once");
        assert_eq!(s.points, vec![[0.5, 0.5], [0.53, 0.5], [0.54, 0.5]]);
        assert!(push_point(&mut s, [0.123_456_78, 0.9], 1.0));
        assert_eq!(s.points.last(), Some(&[0.12346, 0.9]), "five decimals");
        assert!(!push_point(&mut s, [f32::NAN, 0.9], 1.0));
    }

    #[test]
    fn hostile_strokes_are_bounded_and_never_panic() {
        let wild = Stroke {
            radius: 1.0,
            feather: 0.5,
            flow: 1.0,
            erase: false,
            points: (0..20_000)
                .map(|i| {
                    if i % 2 == 0 {
                        [-10.0, -10.0]
                    } else {
                        [11.0, 11.0]
                    }
                })
                .collect(),
        };
        let start = std::time::Instant::now();
        let r = BrushRaster::of(&[wild], 1.5);
        assert_eq!(r.budget, 0, "the budget ran out");
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
        let odd = Stroke {
            radius: f32::NAN,
            feather: f32::INFINITY,
            flow: -3.0,
            erase: true,
            points: vec![[f32::NAN, 0.5], [0.5, f32::INFINITY], [0.5, 0.5]],
        };
        let _ = BrushRaster::of(&[odd], f32::NAN).coverage();
    }
}
