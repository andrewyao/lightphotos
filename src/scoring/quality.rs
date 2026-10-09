// SPDX-License-Identifier: MIT OR Apache-2.0

//! A 0-100 quality score per photo. Classical measurements on the pixels
//! catch defects (missed focus, clipping, darkness, noise, a blink). On
//! macOS 15 and later Vision's aesthetics score sets the base those defects
//! scale down; elsewhere the base is a flat midpoint, so the score separates
//! sound frames from broken ones but cannot judge composition.
//!
//! Pure functions on every platform.

use serde::{Deserialize, Serialize};

/// Long side the measurements run at. Larger inputs are box-averaged down
/// first, so a score means the same thing whatever size the preview came in.
pub const ANALYSIS_PX: u32 = 1024;

/// What the eye geometry says about a photo, once every face has been scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeState {
    /// Every detected eye is open.
    Open,
    /// At least one detected eye is closed.
    Closed,
}

/// Pixel measurements of one rendered photo.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Technical {
    /// `[0, 1]`. The 90th percentile over a tile grid of Laplacian energy
    /// relative to the tile's own contrast, so the sharpest region decides
    /// and a low-contrast scene is not mistaken for a soft one.
    pub focus: f32,
    /// Fraction of pixels with luma at or above 250.
    pub clip_hi: f32,
    /// Fraction of pixels with luma at or below 5.
    pub clip_lo: f32,
    /// `[0, 1]`.
    pub mean_luma: f32,
    /// Estimated noise standard deviation in 8-bit luma levels, measured on
    /// the flattest tiles.
    pub noise_sigma: f32,
}

/// Vision's aesthetics observation. macOS 15 and later only.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aesthetics {
    /// `[-1, 1]`.
    pub overall: f32,
    /// A receipt, screenshot or document rather than a photograph.
    pub utility: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Basis {
    TechnicalOnly,
    WithAesthetics,
}

/// A defect that lowered the score. Each has one row in [`CURVES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Penalty {
    SoftFocus,
    HighlightsClipped,
    ShadowsCrushed,
    Dark,
    Bright,
    Noisy,
    EyesClosed,
    /// Vision judged it a utility image, not a photograph.
    Utility,
}

/// One penalty that fired and the points it took off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deduction {
    pub penalty: Penalty,
    pub points: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityScore {
    /// `0..=100`.
    pub value: u8,
    pub basis: Basis,
    /// `0..=100`, what the score started from before any deduction. `None`
    /// on a score stored before it was recorded, which has no breakdown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<u8>,
    /// Largest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deductions: Vec<Deduction>,
}

#[cfg(test)]
impl QualityScore {
    pub fn penalties(&self) -> Vec<Penalty> {
        self.deductions.iter().map(|d| d.penalty).collect()
    }
}

/// Base without aesthetics. A flawless frame scores 50, which leaves room to
/// read a technical-only score next to an aesthetic one without the two
/// looking equally confident.
const TECHNICAL_BASE: f32 = 0.5;

/// A penalty is listed once it costs more than this much of the score.
const LISTED_BELOW: f32 = 0.95;

struct Inputs<'a> {
    t: &'a Technical,
    a: Option<&'a Aesthetics>,
    eyes: Option<EyeState>,
}

/// One penalty's curve. The factor is 1 at `ok` or on its good side, falls
/// linearly to `floor` at `bad`, and stays there past it. `ok` may sit above
/// or below `bad`, so "higher is better" and "lower is better" share a shape.
/// `measure` is `None` when the input is unknown, which costs nothing.
struct Curve {
    penalty: Penalty,
    measure: fn(&Inputs) -> Option<f32>,
    ok: f32,
    bad: f32,
    floor: f32,
}

fn flag(on: bool) -> f32 {
    if on {
        1.0
    } else {
        0.0
    }
}

/// Every penalty in one place, so tuning happens here. `src/bin/score_probe.rs`
/// prints the measurements against a folder's star ratings to tune them by.
const CURVES: &[Curve] = &[
    Curve {
        penalty: Penalty::SoftFocus,
        measure: |i| Some(i.t.focus),
        ok: 0.55,
        bad: 0.15,
        floor: 0.4,
    },
    Curve {
        penalty: Penalty::HighlightsClipped,
        measure: |i| Some(i.t.clip_hi),
        ok: 0.01,
        bad: 0.15,
        floor: 0.6,
    },
    Curve {
        penalty: Penalty::ShadowsCrushed,
        measure: |i| Some(i.t.clip_lo),
        ok: 0.03,
        bad: 0.30,
        floor: 0.7,
    },
    Curve {
        penalty: Penalty::Dark,
        measure: |i| Some(i.t.mean_luma),
        ok: 0.22,
        bad: 0.06,
        floor: 0.5,
    },
    Curve {
        penalty: Penalty::Bright,
        measure: |i| Some(i.t.mean_luma),
        ok: 0.78,
        bad: 0.95,
        floor: 0.5,
    },
    Curve {
        penalty: Penalty::Noisy,
        measure: |i| Some(i.t.noise_sigma),
        ok: 2.0,
        bad: 8.0,
        floor: 0.6,
    },
    Curve {
        penalty: Penalty::EyesClosed,
        measure: |i| i.eyes.map(|e| flag(e == EyeState::Closed)),
        ok: 0.0,
        bad: 1.0,
        floor: 0.3,
    },
    Curve {
        penalty: Penalty::Utility,
        measure: |i| i.a.map(|a| flag(a.utility)),
        ok: 0.0,
        bad: 1.0,
        floor: 0.3,
    },
];

fn factor(c: &Curve, x: f32) -> f32 {
    let t = ((x - c.ok) / (c.bad - c.ok)).clamp(0.0, 1.0);
    1.0 - t * (1.0 - c.floor)
}

/// The base times every penalty's factor. The base is the aesthetics score
/// mapped to `[0, 1]` when there is one, else [`TECHNICAL_BASE`].
pub fn score(t: &Technical, a: Option<&Aesthetics>, eyes: Option<EyeState>) -> QualityScore {
    let (base, basis) = match a {
        Some(a) if a.overall.is_finite() => (
            ((a.overall + 1.0) / 2.0).clamp(0.0, 1.0),
            Basis::WithAesthetics,
        ),
        _ => (TECHNICAL_BASE, Basis::TechnicalOnly),
    };
    let inputs = Inputs { t, a, eyes };
    let points = |v: f32| (v * 100.0).round().clamp(0.0, 100.0) as u8;
    let mut value = base;
    let mut deductions = Vec::new();
    for c in CURVES {
        let Some(x) = (c.measure)(&inputs).filter(|x| x.is_finite()) else {
            continue;
        };
        let f = factor(c, x);
        let before = value;
        value *= f;
        if f < LISTED_BELOW {
            deductions.push(Deduction {
                penalty: c.penalty,
                points: points(before) - points(value),
            });
        }
    }
    // Stable, so equal deductions keep CURVES order.
    deductions.sort_by(|a, b| b.points.cmp(&a.points));
    QualityScore {
        value: points(value),
        basis,
        base: Some(points(base)),
        deductions,
    }
}

/// Focus is graded over this many tiles a side; noise over twice as many, so
/// enough flat tiles exist to find.
const FOCUS_GRID: usize = 8;
const NOISE_GRID: usize = 16;
/// Tiles flatter than this (luma standard deviation, 8-bit levels) carry no
/// edges to judge focus by.
const FLAT_TILE_STD: f32 = 3.0;
/// Keeps the contrast normalization finite on near-flat tiles.
const CONTRAST_FLOOR: f32 = 4.0;
/// Where the squashed focus crosses `1 - 1/e`.
const FOCUS_SCALE: f32 = 0.35;
/// Variance of the 4-neighbor Laplacian of unit white noise (4^2 + 4 * 1),
/// padded by half again because the noise estimate runs low on textured
/// frames, and an underestimate would pass grain off as detail.
const LAPLACIAN_NOISE_GAIN: f32 = 30.0;

/// Measure opaque sRGB8 RGBA pixels, row-major. A degenerate or short buffer
/// measures as all zeros.
#[hotpath::measure]
pub fn technical(rgba: &[u8], width: u32, height: u32) -> Technical {
    let Some((luma, w, h)) = analysis_luma(rgba, width, height) else {
        return Technical::default();
    };
    let n = (w * h) as f32;
    let (mut hi, mut lo, mut sum) = (0usize, 0usize, 0.0f64);
    for &y in &luma {
        hi += usize::from(y >= 250.0);
        lo += usize::from(y <= 5.0);
        sum += y as f64;
    }
    let noise_sigma = noise_sigma(&luma, w, h);
    Technical {
        focus: focus(&luma, w, h, noise_sigma),
        clip_hi: hi as f32 / n,
        clip_lo: lo as f32 / n,
        mean_luma: (sum / n as f64 / 255.0) as f32,
        noise_sigma,
    }
}

/// Rec. 709 luma in 8-bit levels, box-averaged by a whole factor so the long
/// side is at most [`ANALYSIS_PX`].
fn analysis_luma(rgba: &[u8], width: u32, height: u32) -> Option<(Vec<f32>, usize, usize)> {
    let (sw, sh) = (width as usize, height as usize);
    if sw == 0 || sh == 0 || rgba.len() < sw * sh * 4 {
        return None;
    }
    let block = (width.max(height).div_ceil(ANALYSIS_PX)).max(1) as usize;
    let (w, h) = (sw / block, sh / block);
    if w < 3 || h < 3 {
        return None;
    }
    let mut out = vec![0.0f32; w * h];
    let inv = 1.0 / (block * block) as f32;
    for (oy, row) in out.chunks_exact_mut(w).enumerate() {
        for (ox, px) in row.iter_mut().enumerate() {
            let mut acc = 0.0f32;
            for y in oy * block..(oy + 1) * block {
                let base = (y * sw + ox * block) * 4;
                for p in rgba[base..base + block * 4].chunks_exact(4) {
                    acc += 0.2126 * p[0] as f32 + 0.7152 * p[1] as f32 + 0.0722 * p[2] as f32;
                }
            }
            *px = acc * inv;
        }
    }
    Some((out, w, h))
}

/// Which tile of a `grid` x `grid` split pixel `(x, y)` falls in.
fn tile_of(x: usize, y: usize, w: usize, h: usize, grid: usize) -> usize {
    (y * grid / h) * grid + x * grid / w
}

/// Immerkær's fast noise estimate per tile, then a low percentile, because
/// texture inflates the estimate and the flattest tiles are closest to the
/// truth.
fn noise_sigma(luma: &[f32], w: usize, h: usize) -> f32 {
    let mut sums = vec![(0.0f64, 0usize); NOISE_GRID * NOISE_GRID];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let at = |dx: isize, dy: isize| {
                luma[(y as isize + dy) as usize * w + (x as isize + dx) as usize]
            };
            let v = at(-1, -1) + at(1, -1) + at(-1, 1) + at(1, 1)
                - 2.0 * (at(0, -1) + at(-1, 0) + at(1, 0) + at(0, 1))
                + 4.0 * at(0, 0);
            let s = &mut sums[tile_of(x, y, w, h, NOISE_GRID)];
            s.0 += v.abs() as f64;
            s.1 += 1;
        }
    }
    let scale = (std::f64::consts::PI / 2.0).sqrt() / 6.0;
    let per_tile: Vec<f32> = sums
        .iter()
        .filter(|s| s.1 > 0)
        .map(|s| (scale * s.0 / s.1 as f64) as f32)
        .collect();
    percentile(per_tile, 0.1)
}

/// Per tile: the Laplacian's standard deviation over the tile's own
/// contrast, each less what `noise_sigma` alone would give it. The 90th percentile
/// across tiles that have any contrast, squashed into `[0, 1]`.
fn focus(luma: &[f32], w: usize, h: usize, noise_sigma: f32) -> f32 {
    #[derive(Clone, Copy, Default)]
    struct Acc {
        lap: f64,
        lap2: f64,
        y: f64,
        y2: f64,
        n: usize,
    }
    let mut tiles = vec![Acc::default(); FOCUS_GRID * FOCUS_GRID];
    for y in 1..h - 1 {
        for x in 1..w - 1 {
            let i = y * w + x;
            let c = luma[i];
            let lap = (luma[i - w] + luma[i + w] + luma[i - 1] + luma[i + 1] - 4.0 * c) as f64;
            let t = &mut tiles[tile_of(x, y, w, h, FOCUS_GRID)];
            t.lap += lap;
            t.lap2 += lap * lap;
            t.y += c as f64;
            t.y2 += (c as f64) * (c as f64);
            t.n += 1;
        }
    }
    let noise_var = LAPLACIAN_NOISE_GAIN * noise_sigma * noise_sigma;
    let ratios: Vec<f32> = tiles
        .iter()
        .filter(|t| t.n > 1)
        .filter_map(|t| {
            let n = t.n as f64;
            let var = |s: f64, s2: f64| ((s2 - s * s / n) / n).max(0.0) as f32;
            let std = (var(t.y, t.y2) - noise_sigma * noise_sigma).max(0.0).sqrt();
            (std >= FLAT_TILE_STD).then(|| {
                let edges = (var(t.lap, t.lap2) - noise_var).max(0.0).sqrt();
                edges / (std + CONTRAST_FLOOR)
            })
        })
        .collect();
    if ratios.is_empty() {
        return 0.0;
    }
    1.0 - (-percentile(ratios, 0.9) / FOCUS_SCALE).exp()
}

/// Nearest-rank percentile, `q` in `[0, 1]`. `0.0` for an empty list.
fn percentile(mut v: Vec<f32>, q: f32) -> f32 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(f32::total_cmp);
    let rank = ((v.len() - 1) as f32 * q).round() as usize;
    v[rank]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean() -> Technical {
        Technical {
            focus: 0.9,
            clip_hi: 0.0,
            clip_lo: 0.0,
            mean_luma: 0.45,
            noise_sigma: 1.0,
        }
    }

    #[test]
    fn a_blink_loses_to_a_softer_open_eyed_frame() {
        let sharp = clean();
        let soft = Technical {
            focus: 0.4,
            ..clean()
        };
        let blink = score(&sharp, None, Some(EyeState::Closed));
        let open = score(&soft, None, Some(EyeState::Open));
        assert!(blink.value < open.value, "blink {blink:?} open {open:?}");
        assert_eq!(blink.penalties(), vec![Penalty::EyesClosed]);
        assert_eq!(open.penalties(), vec![Penalty::SoftFocus]);
    }

    #[test]
    fn the_breakdown_adds_up_to_the_score_largest_first() {
        let t = Technical {
            focus: 0.3,
            clip_hi: 0.05,
            ..clean()
        };
        let a = Aesthetics {
            overall: 0.6,
            utility: false,
        };
        let s = score(&t, Some(&a), Some(EyeState::Closed));
        assert_eq!(s.base, Some(80));
        assert_eq!(
            s.penalties(),
            vec![
                Penalty::EyesClosed,
                Penalty::SoftFocus,
                Penalty::HighlightsClipped
            ]
        );
        let taken: u32 = s.deductions.iter().map(|d| u32::from(d.points)).sum();
        assert_eq!(taken + u32::from(s.value), 80, "{s:?}");
    }

    #[test]
    fn unknown_eyes_cost_nothing() {
        assert_eq!(
            score(&clean(), None, None),
            score(&clean(), None, Some(EyeState::Open))
        );
    }

    #[test]
    fn a_clipped_frame_loses_to_its_well_exposed_twin() {
        let clipped = Technical {
            clip_hi: 0.12,
            mean_luma: 0.7,
            ..clean()
        };
        let a = Aesthetics {
            overall: 0.4,
            utility: false,
        };
        let worse = score(&clipped, Some(&a), None);
        let better = score(&clean(), Some(&a), None);
        assert!(worse.value < better.value, "{worse:?} vs {better:?}");
        assert_eq!(worse.penalties(), vec![Penalty::HighlightsClipped]);
        assert!(better.deductions.is_empty());
    }

    #[test]
    fn a_flawless_frame_scores_the_base() {
        let tech = score(&clean(), None, Some(EyeState::Open));
        assert_eq!((tech.value, tech.basis), (50, Basis::TechnicalOnly));
        let best = Aesthetics {
            overall: 1.0,
            utility: false,
        };
        let aes = score(&clean(), Some(&best), None);
        assert_eq!((aes.value, aes.basis), (100, Basis::WithAesthetics));
    }

    #[test]
    fn a_utility_image_is_held_low_whatever_its_aesthetics() {
        let receipt = Aesthetics {
            overall: 1.0,
            utility: true,
        };
        let s = score(&clean(), Some(&receipt), None);
        assert!(s.value <= 30, "{s:?}");
        assert_eq!(s.penalties(), vec![Penalty::Utility]);
    }

    #[test]
    fn every_basis_stays_in_range_at_the_extremes() {
        let extremes = [f32::NEG_INFINITY, -1e9, -1.0, 0.0, 0.5, 1.0, 1e9, f32::NAN];
        for &x in &extremes {
            let t = Technical {
                focus: x,
                clip_hi: x,
                clip_lo: x,
                mean_luma: x,
                noise_sigma: x,
            };
            for a in [
                None,
                Some(Aesthetics {
                    overall: x,
                    utility: false,
                }),
                Some(Aesthetics {
                    overall: x,
                    utility: true,
                }),
            ] {
                for eyes in [None, Some(EyeState::Open), Some(EyeState::Closed)] {
                    let s = score(&t, a.as_ref(), eyes);
                    assert!(s.value <= 100, "{x} gave {s:?}");
                }
            }
        }
    }

    #[test]
    fn the_worst_frame_still_scores_above_zero_and_lists_every_defect() {
        let t = Technical {
            focus: 0.0,
            clip_hi: 1.0,
            clip_lo: 1.0,
            mean_luma: 0.0,
            noise_sigma: 50.0,
        };
        let s = score(&t, None, Some(EyeState::Closed));
        assert!(s.value < 5, "{s:?}");
        assert!(!s.penalties().contains(&Penalty::Bright));
        assert!(s.penalties().contains(&Penalty::SoftFocus));
        assert!(s.penalties().contains(&Penalty::EyesClosed));
    }

    #[test]
    fn a_stored_score_round_trips_through_json() {
        let s = score(
            &Technical {
                focus: 0.2,
                ..clean()
            },
            None,
            Some(EyeState::Closed),
        );
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains("\"soft_focus\""), "{json}");
        assert_eq!(serde_json::from_str::<QualityScore>(&json).unwrap(), s);
    }

    /// A deterministic scene: overlapping rectangles of varied gray on a
    /// mid-gray ground, so there are hard edges and flat regions both.
    fn scene(w: u32, h: u32) -> Vec<u8> {
        let mut seed = 0x2545_f491_u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let mut gray = vec![110u8; (w * h) as usize];
        for _ in 0..120 {
            let (x0, y0) = (next() % w, next() % h);
            let (rw, rh) = (8 + next() % (w / 6), 8 + next() % (h / 6));
            let v = (40 + next() % 160) as u8;
            for y in y0..(y0 + rh).min(h) {
                for x in x0..(x0 + rw).min(w) {
                    gray[(y * w + x) as usize] = v;
                }
            }
        }
        gray.iter().flat_map(|&v| [v, v, v, 255]).collect()
    }

    fn gaussian_blur(rgba: &[u8], w: u32, h: u32, sigma: f32) -> Vec<u8> {
        let r = (sigma * 3.0).ceil() as i32;
        let k: Vec<f32> = (-r..=r)
            .map(|i| (-(i * i) as f32 / (2.0 * sigma * sigma)).exp())
            .collect();
        let ksum: f32 = k.iter().sum();
        let (w, h) = (w as i32, h as i32);
        let pass = |src: &[f32], horizontal: bool| -> Vec<f32> {
            let mut out = vec![0.0; src.len()];
            for y in 0..h {
                for x in 0..w {
                    let mut acc = 0.0;
                    for (j, kv) in k.iter().enumerate() {
                        let d = j as i32 - r;
                        let (sx, sy) = if horizontal {
                            ((x + d).clamp(0, w - 1), y)
                        } else {
                            (x, (y + d).clamp(0, h - 1))
                        };
                        acc += kv * src[(sy * w + sx) as usize];
                    }
                    out[(y * w + x) as usize] = acc / ksum;
                }
            }
            out
        };
        let gray: Vec<f32> = rgba.chunks_exact(4).map(|p| p[0] as f32).collect();
        let blurred = pass(&pass(&gray, true), false);
        blurred
            .iter()
            .flat_map(|&v| {
                let v = v.round() as u8;
                [v, v, v, 255]
            })
            .collect()
    }

    fn push_exposure(rgba: &[u8], stops: f32) -> Vec<u8> {
        let gain = 2f32.powf(stops);
        rgba.chunks_exact(4)
            .flat_map(|p| {
                let lin = (p[0] as f32 / 255.0).powf(2.2) * gain;
                let v = (lin.min(1.0).powf(1.0 / 2.2) * 255.0).round() as u8;
                [v, v, v, 255]
            })
            .collect()
    }

    fn add_noise(rgba: &[u8], amplitude: f32) -> Vec<u8> {
        let mut seed = 0x9e37_79b9_u32;
        rgba.chunks_exact(4)
            .flat_map(|p| {
                // Sum of uniforms, roughly Gaussian.
                let mut n = 0.0;
                for _ in 0..4 {
                    seed ^= seed << 13;
                    seed ^= seed >> 17;
                    seed ^= seed << 5;
                    n += (seed as f32 / u32::MAX as f32) - 0.5;
                }
                let v = (p[0] as f32 + n * amplitude).clamp(0.0, 255.0) as u8;
                [v, v, v, 255]
            })
            .collect()
    }

    const W: u32 = 640;
    const H: u32 = 480;

    #[test]
    fn a_blurred_copy_measures_softer() {
        let sharp = scene(W, H);
        let soft = gaussian_blur(&sharp, W, H, 2.5);
        let (s, b) = (technical(&sharp, W, H), technical(&soft, W, H));
        assert!(s.focus > 0.6, "sharp scene focus {}", s.focus);
        assert!(b.focus < s.focus - 0.3, "sharp {s:?} blurred {b:?}");
    }

    #[test]
    fn three_stops_over_raises_clipped_highlights() {
        let base = scene(W, H);
        let over = push_exposure(&base, 3.0);
        let (b, o) = (technical(&base, W, H), technical(&over, W, H));
        assert!(b.clip_hi < 0.01, "{b:?}");
        assert!(o.clip_hi > 0.2, "{o:?}");
        assert!(o.mean_luma > b.mean_luma);
    }

    #[test]
    fn grain_raises_the_noise_estimate_without_reading_as_focus() {
        let base = scene(W, H);
        let soft = gaussian_blur(&base, W, H, 2.5);
        let grainy = add_noise(&soft, 20.0);
        let (s, g) = (technical(&soft, W, H), technical(&grainy, W, H));
        assert!(s.noise_sigma < 1.0, "{s:?}");
        assert!(g.noise_sigma > 4.0, "{g:?}");
        assert!(
            g.focus < s.focus + 0.15,
            "noise must not pass for detail: {s:?} vs {g:?}"
        );
    }

    #[test]
    fn a_flat_image_has_no_focus_and_no_noise() {
        let flat: Vec<u8> = [128u8, 128, 128, 255].repeat((W * H) as usize);
        let t = technical(&flat, W, H);
        assert_eq!((t.focus, t.noise_sigma, t.clip_hi), (0.0, 0.0, 0.0));
        assert!((t.mean_luma - 128.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn the_measurements_do_not_depend_on_the_preview_size() {
        let small = scene(W, H);
        let big: Vec<u8> = (0..H * 2)
            .flat_map(|y| {
                let row = &small[((y / 2) * W * 4) as usize..((y / 2 + 1) * W * 4) as usize];
                row.chunks_exact(4)
                    .flat_map(|p| [p, p].concat())
                    .collect::<Vec<u8>>()
            })
            .collect();
        let at_2x = technical(&big, W * 2, H * 2);
        let at_1x = technical(&small, W, H);
        assert!((at_2x.mean_luma - at_1x.mean_luma).abs() < 1e-3);
        assert!(at_2x.focus > 0.6, "{at_2x:?}");
    }

    #[test]
    fn degenerate_input_measures_as_zeros() {
        assert_eq!(technical(&[], 0, 0), Technical::default());
        assert_eq!(technical(&[0, 0, 0, 255], 10, 10), Technical::default());
    }
}
