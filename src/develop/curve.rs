// SPDX-License-Identifier: MIT OR Apache-2.0

//! The point tone curve, as in Lightroom's Curve panel: a master RGB curve
//! and one curve per channel, each a list of 0..=255 points joined by a
//! monotone cubic. [`ToneCurve::lut`] bakes all four into one table that the
//! CPU pipeline and the shader both read, so they can't drift apart.

use serde::{Deserialize, Serialize};

/// The most points one curve holds, endpoints included.
pub const MAX_POINTS: usize = 16;

/// Entries in a baked curve table, one per 8-bit input level.
pub const LUT_SIZE: usize = 256;

/// One curve: points `(input, output)` on 0..=255, sorted by strictly
/// increasing input, at least two of them, with the slots past them zero.
/// Input below the first point or above the last holds that point's output.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(from = "Vec<[u8; 2]>", into = "Vec<[u8; 2]>")]
pub struct Curve {
    n: u8,
    pts: [[u8; 2]; MAX_POINTS],
}

impl Default for Curve {
    fn default() -> Self {
        Curve::LINEAR
    }
}

const fn curve<const N: usize>(p: [[u8; 2]; N]) -> Curve {
    let mut pts = [[0u8; 2]; MAX_POINTS];
    let mut i = 0;
    while i < N {
        pts[i] = p[i];
        i += 1;
    }
    Curve { n: N as u8, pts }
}

impl Curve {
    pub const LINEAR: Curve = curve([[0, 0], [255, 255]]);
    /// Lightroom's Medium Contrast point curve.
    pub const MEDIUM_CONTRAST: Curve = curve([
        [0, 0],
        [32, 22],
        [64, 56],
        [128, 128],
        [192, 196],
        [255, 255],
    ]);
    /// Lightroom's Strong Contrast point curve.
    pub const STRONG_CONTRAST: Curve = curve([
        [0, 0],
        [32, 16],
        [64, 50],
        [128, 128],
        [192, 202],
        [255, 255],
    ]);

    pub fn points(&self) -> &[[u8; 2]] {
        &self.pts[..self.n as usize]
    }

    pub fn is_linear(&self) -> bool {
        *self == Curve::LINEAR
    }

    /// Adds a point and returns its index. `None` when the curve is full or
    /// a point already sits at input `x`.
    pub fn insert(&mut self, x: u8, y: u8) -> Option<usize> {
        let n = self.n as usize;
        if n >= MAX_POINTS {
            return None;
        }
        let i = match self.points().binary_search_by_key(&x, |p| p[0]) {
            Ok(_) => return None,
            Err(i) => i,
        };
        self.pts.copy_within(i..n, i + 1);
        self.pts[i] = [x, y];
        self.n += 1;
        Some(i)
    }

    /// Removes point `i`, unless only two points are left.
    pub fn remove(&mut self, i: usize) {
        let n = self.n as usize;
        if n <= 2 || i >= n {
            return;
        }
        self.pts.copy_within(i + 1..n, i);
        // Unused slots stay zero, so the derived `PartialEq` and `Hash` see
        // only the points.
        self.pts[n - 1] = [0, 0];
        self.n -= 1;
    }

    /// Moves point `i` to `(x, y)`, with `x` kept strictly between its
    /// neighbors' inputs so the order holds.
    pub fn move_point(&mut self, i: usize, x: u8, y: u8) {
        let n = self.n as usize;
        if i >= n {
            return;
        }
        let lo = if i == 0 { 0 } else { self.pts[i - 1][0] + 1 };
        let hi = if i + 1 == n {
            255
        } else {
            self.pts[i + 1][0] - 1
        };
        self.pts[i] = [x.clamp(lo, hi), y];
    }

    /// The curve's output on 0..=1 for input `x` on 0..=1.
    pub fn eval(&self, x: f32) -> f32 {
        Spline::new(self).eval(x)
    }
}

impl From<Vec<[u8; 2]>> for Curve {
    /// Sorts, drops repeated inputs and keeps at most [`MAX_POINTS`]. Fewer
    /// than two points is [`Curve::LINEAR`], so a hand-edited sidecar can't
    /// break a render.
    fn from(mut v: Vec<[u8; 2]>) -> Self {
        v.sort_by_key(|p| p[0]);
        v.dedup_by_key(|p| p[0]);
        if v.len() < 2 {
            return Curve::LINEAR;
        }
        thin(&mut v);
        let mut c = Curve {
            n: v.len() as u8,
            pts: [[0; 2]; MAX_POINTS],
        };
        c.pts[..v.len()].copy_from_slice(&v);
        c
    }
}

impl From<Curve> for Vec<[u8; 2]> {
    fn from(c: Curve) -> Self {
        c.points().to_vec()
    }
}

/// Drops interior points until `v` fits [`MAX_POINTS`], each time the one
/// whose removal changes the line through its neighbors least.
fn thin(v: &mut Vec<[u8; 2]>) {
    while v.len() > MAX_POINTS {
        let off = |i: usize| {
            let ([x0, y0], [x, y], [x1, y1]) = (v[i - 1], v[i], v[i + 1]);
            let t = (x as f32 - x0 as f32) / (x1 as f32 - x0 as f32);
            (y0 as f32 + (y1 as f32 - y0 as f32) * t - y as f32).abs()
        };
        let Some(i) = (1..v.len() - 1).min_by(|&a, &b| off(a).total_cmp(&off(b))) else {
            break;
        };
        v.remove(i);
    }
}

/// A [`Curve`] ready to evaluate: points on 0..=1 and their tangents, chosen
/// by Fritsch–Carlson so the curve never overshoots between points.
struct Spline {
    n: usize,
    x: [f32; MAX_POINTS],
    y: [f32; MAX_POINTS],
    m: [f32; MAX_POINTS],
}

impl Spline {
    fn new(c: &Curve) -> Spline {
        let n = c.n as usize;
        let (mut x, mut y, mut m) = ([0f32; MAX_POINTS], [0f32; MAX_POINTS], [0f32; MAX_POINTS]);
        for (i, p) in c.points().iter().enumerate() {
            x[i] = p[0] as f32 / 255.0;
            y[i] = p[1] as f32 / 255.0;
        }
        let mut d = [0f32; MAX_POINTS];
        for k in 0..n - 1 {
            d[k] = (y[k + 1] - y[k]) / (x[k + 1] - x[k]);
        }
        m[0] = d[0];
        m[n - 1] = d[n - 2];
        for k in 1..n - 1 {
            m[k] = if d[k - 1] * d[k] <= 0.0 {
                0.0
            } else {
                (d[k - 1] + d[k]) / 2.0
            };
        }
        for k in 0..n - 1 {
            if d[k] == 0.0 {
                m[k] = 0.0;
                m[k + 1] = 0.0;
                continue;
            }
            let (a, b) = (m[k] / d[k], m[k + 1] / d[k]);
            let s = a * a + b * b;
            if s > 9.0 {
                let tau = 3.0 / s.sqrt();
                m[k] = tau * a * d[k];
                m[k + 1] = tau * b * d[k];
            }
        }
        Spline { n, x, y, m }
    }

    fn eval(&self, v: f32) -> f32 {
        let n = self.n;
        if v <= self.x[0] {
            return self.y[0];
        }
        if v >= self.x[n - 1] {
            return self.y[n - 1];
        }
        let k = self.x[1..n].partition_point(|&xk| xk < v);
        let h = self.x[k + 1] - self.x[k];
        let t = (v - self.x[k]) / h;
        let (t2, t3) = (t * t, t * t * t);
        let out = (2.0 * t3 - 3.0 * t2 + 1.0) * self.y[k]
            + (t3 - 2.0 * t2 + t) * h * self.m[k]
            + (-2.0 * t3 + 3.0 * t2) * self.y[k + 1]
            + (t3 - t2) * h * self.m[k + 1];
        out.clamp(0.0, 1.0)
    }
}

/// Which of a [`ToneCurve`]'s four curves.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Channel {
    #[default]
    Rgb,
    Red,
    Green,
    Blue,
}

impl Channel {
    pub const ALL: [Channel; 4] = [Channel::Rgb, Channel::Red, Channel::Green, Channel::Blue];
}

/// The master curve and the three channel curves. The master runs first,
/// then each channel's own curve.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct ToneCurve {
    #[serde(default, skip_serializing_if = "Curve::is_linear")]
    pub rgb: Curve,
    #[serde(default, skip_serializing_if = "Curve::is_linear")]
    pub red: Curve,
    #[serde(default, skip_serializing_if = "Curve::is_linear")]
    pub green: Curve,
    #[serde(default, skip_serializing_if = "Curve::is_linear")]
    pub blue: Curve,
}

/// A baked [`ToneCurve`]: entry `i` holds the red, green and blue output for
/// input `i / 255`, and a fourth unused lane so the shader can read it as
/// `vec4`s.
pub type Lut = [[f32; 4]; LUT_SIZE];

impl ToneCurve {
    pub fn is_linear(&self) -> bool {
        *self == ToneCurve::default()
    }

    pub fn get(&self, ch: Channel) -> &Curve {
        match ch {
            Channel::Rgb => &self.rgb,
            Channel::Red => &self.red,
            Channel::Green => &self.green,
            Channel::Blue => &self.blue,
        }
    }

    pub fn get_mut(&mut self, ch: Channel) -> &mut Curve {
        match ch {
            Channel::Rgb => &mut self.rgb,
            Channel::Red => &mut self.red,
            Channel::Green => &mut self.green,
            Channel::Blue => &mut self.blue,
        }
    }

    /// The four curves composed into one table. Must match how
    /// `toneCurve` in loupe_common.wgsl reads it.
    pub fn lut(&self) -> Lut {
        let master = Spline::new(&self.rgb);
        let chans = [&self.red, &self.green, &self.blue].map(Spline::new);
        let mut lut = [[0f32; 4]; LUT_SIZE];
        for (i, e) in lut.iter_mut().enumerate() {
            let v = master.eval(i as f32 / (LUT_SIZE - 1) as f32);
            for c in 0..3 {
                e[c] = chans[c].eval(v);
            }
        }
        lut
    }
}

/// Applies a baked curve to one display-space value of channel `c`,
/// interpolating between entries. Must match `toneCurve` in
/// loupe_common.wgsl.
pub fn apply(lut: &Lut, c: usize, v: f32) -> f32 {
    let x = v.clamp(0.0, 1.0) * (LUT_SIZE - 1) as f32;
    let i = (x as usize).min(LUT_SIZE - 2);
    let t = x - i as f32;
    lut[i][c] + (lut[i + 1][c] - lut[i][c]) * t
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_linear_curve_bakes_to_the_identity() {
        let lut = ToneCurve::default().lut();
        for (i, e) in lut.iter().enumerate() {
            let x = i as f32 / 255.0;
            for c in 0..3 {
                assert!((e[c] - x).abs() < 1e-6, "entry {i}: {e:?}");
            }
        }
        assert!((apply(&lut, 1, 0.3) - 0.3).abs() < 1e-6);
    }

    #[test]
    fn presets_pass_through_their_points_and_never_fall() {
        for c in [Curve::MEDIUM_CONTRAST, Curve::STRONG_CONTRAST] {
            for p in c.points() {
                let got = c.eval(p[0] as f32 / 255.0) * 255.0;
                assert!((got - p[1] as f32).abs() < 1e-3, "{p:?} -> {got}");
            }
            let mut last = 0.0;
            for i in 0..=1000 {
                let y = c.eval(i as f32 / 1000.0);
                assert!(y >= last - 1e-6, "falls at {i}");
                last = y;
            }
        }
    }

    #[test]
    fn a_contrast_curve_darkens_shadows_and_brightens_highlights() {
        let c = Curve::STRONG_CONTRAST;
        assert!(c.eval(0.2) < 0.2);
        assert!(c.eval(0.8) > 0.8);
    }

    #[test]
    fn a_channel_curve_moves_only_its_channel() {
        let mut tc = ToneCurve::default();
        tc.red.insert(128, 180);
        let lut = tc.lut();
        assert!(apply(&lut, 0, 0.5) > 0.6);
        assert!((apply(&lut, 1, 0.5) - 0.5).abs() < 1e-6);
        assert!((apply(&lut, 2, 0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn the_master_curve_feeds_the_channel_curves() {
        let tc = ToneCurve {
            rgb: curve([[0, 0], [255, 128]]),
            red: curve([[0, 0], [128, 255], [255, 255]]),
            ..Default::default()
        };
        let lut = tc.lut();
        // White becomes half by the master, then full by red's curve.
        assert!((lut[255][0] - 1.0).abs() < 1e-3);
        assert!((lut[255][1] - 128.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn points_keep_their_order_when_moved() {
        let mut c = Curve::MEDIUM_CONTRAST;
        c.move_point(2, 250, 10);
        assert_eq!(c.points()[2], [127, 10]);
        c.move_point(0, 40, 30);
        assert_eq!(c.points()[0], [31, 30]);
        assert_eq!(c.insert(128, 0), None, "a point is already at 128");
        assert_eq!(c.insert(100, 90), Some(2));
        assert_eq!(c.points()[3], [127, 10]);
        c.remove(2);
        assert_eq!(c.points().len(), 6);
    }

    #[test]
    fn removing_every_added_point_is_linear_again() {
        let mut c = Curve::LINEAR;
        c.insert(128, 200);
        c.insert(60, 20);
        c.remove(1);
        c.remove(1);
        assert_eq!(c, Curve::LINEAR);
        assert!(c.is_linear());
    }

    #[test]
    fn the_last_two_points_cannot_be_removed() {
        let mut c = Curve::LINEAR;
        c.remove(0);
        assert_eq!(c, Curve::LINEAR);
    }

    #[test]
    fn a_full_curve_takes_no_more_points() {
        let mut c = Curve::LINEAR;
        for x in 1..=(MAX_POINTS as u8 - 2) {
            assert!(c.insert(x * 10, x * 10).is_some());
        }
        assert_eq!(c.insert(200, 200), None);
    }

    #[test]
    fn serializes_as_a_point_list_and_skips_linear_curves() {
        let tc = ToneCurve {
            green: Curve::MEDIUM_CONTRAST,
            ..Default::default()
        };
        let json = serde_json::to_string(&tc).unwrap();
        assert_eq!(
            json,
            r#"{"green":[[0,0],[32,22],[64,56],[128,128],[192,196],[255,255]]}"#
        );
        assert_eq!(serde_json::from_str::<ToneCurve>(&json).unwrap(), tc);
        assert!(serde_json::from_str::<ToneCurve>("{}").unwrap().is_linear());
    }

    #[test]
    fn a_bad_point_list_loads_as_something_renderable() {
        let c: Curve = serde_json::from_str("[[9,9]]").unwrap();
        assert_eq!(c, Curve::LINEAR);
        let c: Curve = serde_json::from_str("[[255,255],[0,10],[0,20]]").unwrap();
        assert_eq!(c.points(), &[[0, 10], [255, 255]]);
        let many: Vec<[u8; 2]> = (0..40u8).map(|i| [i * 6, i * 6]).collect();
        assert_eq!(Curve::from(many).points().len(), MAX_POINTS);
    }
}
