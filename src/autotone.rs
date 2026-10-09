//! Auto Tone: pick Develop slider values from a photo's histogram.
//!
//! First it sees where the histogram's hill sits, by its median. A hill near
//! the middle has its range, the bars left after trimming thin edges,
//! centred by Exposure and widened to 5%..95% by Highlights and Shadows, with
//! Whites and Blacks only for what those can't reach. A hill that sits too
//! dark or too bright has its median slid to the middle, which moves its pile
//! off the wall, then Highlights and Shadows fix what that move did.
//!
//! Slider values are solved by bisection against our real tone pipeline
//! ([`develop::apply_linear`], or `apply_raw_display` for RAW), so results
//! match what the shader draws.

use crate::decode::image_decode::PixelFormat;
use crate::develop::{self, Adjustments, EXPOSURE_RANGE, TONE_RANGE};

/// One bin per 8-bit display level.
const BINS: usize = 256;

/// Display-space targets for the range's middle and ends, x1 and x2.
const TARGET_MIDDLE: f32 = 0.5;
const TARGET_LOW: f32 = 0.05;
const TARGET_HIGH: f32 = 0.95;
/// A bar shorter than this share of the average bar height is left out of
/// the range when it sits at either edge.
const EDGE_THRESHOLD: f32 = 0.04;
/// The range always leaves out at least this share of pixels at each end, so
/// a lamp or a glint tall enough to pass the threshold can't set an edge.
const EDGE_MIN_TRIM: f32 = 0.005;
/// Cap on added Contrast. Without it, any flat photo whose endpoints can't
/// stretch far enough would end up at +100.
const MAX_AUTO_CONTRAST: f32 = 50.0;
/// Passes over the solved sliders. Each slider shifts the others, so one pass
/// isn't enough. Three settles well inside `edit_signature`'s 1e-3 rounding.
const REFINE_PASSES: u32 = 3;

// Display levels for blown, clipped, and crushed pixels, and the fractions of
// the frame past which each counts. A "hot" photo gets no extra Exposure.
const HIGHLIGHT_LEVEL: f32 = 240.0 / 255.0;
const CLIPPED_LEVEL: f32 = 250.0 / 255.0;
const SHADOW_LEVEL: f32 = 32.0 / 255.0;
const HOT_WHITE_POINT: f32 = 245.0 / 255.0;
const HIGHLIGHT_FRACTION: f32 = 0.02;
const CLIPPED_FRACTION: f32 = 0.005;
const SHADOW_FRACTION: f32 = 0.05;

/// A hill whose median sits below `DARK_MEDIAN` or above `BRIGHT_MEDIAN` is
/// too dark or too bright. Between them, it is normal.
const DARK_MEDIAN: f32 = 0.35;
const BRIGHT_MEDIAN: f32 = 0.65;

/// How close an end must land to its target before Whites or Blacks, or
/// Contrast, is asked to help. Half an 8-bit level.
const NEAR_ENOUGH: f32 = 0.5 / 255.0;

/// Bisection steps per slider. 24 halvings of a ±100 range reach 1e-5.
const SOLVE_STEPS: u32 = 24;

/// Bisection steps for [`fit_to_level`]'s 0..1 scale factor.
const FIT_STEPS: u32 = 16;

/// A photo's display-luma histogram. Each bin keeps one linear-light colour
/// that stands in for its pixels, so trying a candidate `Adjustments` pushes
/// 256 colours through the pipeline instead of the whole photo.
///
/// The stand-in must be a colour, not a gray. Pure red displays at luma 0.299,
/// but a gray with red's linear luma displays at about 0.578. Bins are re-sorted
/// by output luma for each candidate, because adjustments can reorder colours.
struct Histogram {
    count: [f32; BINS],
    /// Each bin's mean linear colour, scaled by [`fit_to_level`]. Black for
    /// empty bins.
    lin: [[f32; 3]; BINS],
    total: f32,
    /// Mean of `(max - min) / max` over display-space pixels.
    mean_sat: f32,
    format: PixelFormat,
}

/// Rec.601 luma for display-space values, the same weights as Vibrance in
/// `develop/mod.rs`. Linear-light code uses Rec.709 instead.
fn luma(px: [f32; 3]) -> f32 {
    0.299 * px[0] + 0.587 * px[1] + 0.114 * px[2]
}

/// Linear-light pixel to display colour under `adj`. Matches what
/// `recompute_histogram` plots in the Develop panel.
fn display(adj: &Adjustments, format: PixelFormat, px: [f32; 3]) -> [f32; 3] {
    match format {
        PixelFormat::Srgb8 => {
            let out = develop::apply_linear(adj, px);
            [
                out[0].max(0.0).powf(1.0 / 2.2),
                out[1].max(0.0).powf(1.0 / 2.2),
                out[2].max(0.0).powf(1.0 / 2.2),
            ]
        }
        PixelFormat::LinearF16 => develop::apply_raw_display(adj, px),
    }
}

fn respond(adj: &Adjustments, format: PixelFormat, linear: [f32; 3]) -> f32 {
    luma(display(adj, format, linear))
}

/// Scale `px` down until it displays at luma `level`, keeping its hue.
///
/// Gamma encoding is concave, so a bin's mean linear colour displays up to
/// 0.08 brighter than the bin's mean display level. Uncorrected, every
/// percentile reads high and Exposure stops short. Never scales up: if `px`
/// is already at or below `level`, it is returned unchanged.
fn fit_to_level(format: PixelFormat, px: [f32; 3], level: f32) -> [f32; 3] {
    let identity = Adjustments::default();
    let scaled = |k: f32| [px[0] * k, px[1] * k, px[2] * k];
    if respond(&identity, format, px) <= level + 1e-6 {
        return px;
    }
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..FIT_STEPS {
        let mid = 0.5 * (lo + hi);
        if respond(&identity, format, scaled(mid)) < level {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    scaled(0.5 * (lo + hi))
}

impl Histogram {
    fn build(samples: &[[f32; 3]], format: PixelFormat) -> Option<Histogram> {
        if samples.is_empty() {
            return None;
        }
        let identity = Adjustments::default();
        let mut count = [0f32; BINS];
        let mut lin_sum = [[0f32; 3]; BINS];
        let mut level_sum = [0f64; BINS];
        let mut sat_sum = 0f32;

        for &px in samples {
            let shown = display(&identity, format, px);
            let v = luma(shown).clamp(0.0, 1.0);
            let bin = ((v * (BINS - 1) as f32).round() as usize).min(BINS - 1);
            count[bin] += 1.0;
            level_sum[bin] += f64::from(v);
            for c in 0..3 {
                lin_sum[bin][c] += px[c].max(0.0);
            }

            let cmax = shown[0].max(shown[1]).max(shown[2]);
            let cmin = shown[0].min(shown[1]).min(shown[2]);
            if cmax > 0.0 {
                sat_sum += (cmax - cmin) / cmax;
            }
        }

        let total = samples.len() as f32;
        let mut lin = [[0f32; 3]; BINS];
        for i in 0..BINS {
            if count[i] > 0.0 {
                let mean = [
                    lin_sum[i][0] / count[i],
                    lin_sum[i][1] / count[i],
                    lin_sum[i][2] / count[i],
                ];
                lin[i] = fit_to_level(format, mean, (level_sum[i] / f64::from(count[i])) as f32);
            }
        }
        Some(Histogram {
            count,
            lin,
            total,
            mean_sat: sat_sum / total,
            format,
        })
    }

    /// Display luma at cumulative fraction `p` under `adj`.
    fn percentile(&self, p: f32, adj: &Adjustments) -> f32 {
        let mut output = [(0.0f32, 0.0f32); BINS];
        let mut len = 0;
        for (bin, &count) in self.count.iter().enumerate() {
            if count > 0.0 {
                output[len] = (respond(adj, self.format, self.lin[bin]), count);
                len += 1;
            }
        }
        let output = &mut output[..len];
        output.sort_unstable_by(|a, b| a.0.total_cmp(&b.0));
        let target = self.total * p;
        let mut cumulative = 0.0;
        for &(level, count) in output.iter() {
            cumulative += count;
            if cumulative >= target {
                return level;
            }
        }
        output.last().map_or(0.0, |entry| entry.0)
    }

    /// Where the unedited hill sits.
    fn hill(&self) -> Hill {
        let median = self.percentile(0.50, &Adjustments::default());
        if median < DARK_MEDIAN {
            Hill::Dark
        } else if median > BRIGHT_MEDIAN {
            Hill::Bright
        } else {
            Hill::Normal
        }
    }

    /// The range as pixel fractions for [`Histogram::percentile`]: walk in
    /// from each side past bars under [`EDGE_THRESHOLD`] of the average
    /// height. Measured on the unedited photo and then followed through each
    /// guess, because edits reshape bars and would move the edges.
    fn edges(&self) -> (f32, f32) {
        let threshold = EDGE_THRESHOLD * self.total / BINS as f32;
        let first = self.count.iter().position(|&c| c >= threshold).unwrap_or(0);
        let last = self
            .count
            .iter()
            .rposition(|&c| c >= threshold)
            .unwrap_or(BINS - 1);
        // Aim inside the edge bar, so the percentile lands on it rather than
        // on the empty bar next to it.
        let below: f32 = self.count[..first].iter().sum::<f32>() + 0.5 * self.count[first];
        let above: f32 = self.count[last + 1..].iter().sum::<f32>() + 0.5 * self.count[last];
        (
            (below / self.total).max(EDGE_MIN_TRIM),
            1.0 - (above / self.total).max(EDGE_MIN_TRIM),
        )
    }

    /// Fraction of pixels at or above display level `level`.
    fn fraction_above(&self, level: f32) -> f32 {
        let first = ((level * (BINS - 1) as f32).ceil() as usize).min(BINS - 1);
        self.count[first..].iter().sum::<f32>() / self.total
    }

    /// Fraction of pixels at or below display level `level`.
    fn fraction_below(&self, level: f32) -> f32 {
        let last = ((level * (BINS - 1) as f32).floor() as usize).min(BINS - 1);
        self.count[..=last].iter().sum::<f32>() / self.total
    }
}

/// Bisect `field` over `range` until `measure` hits `target`. `measure` must
/// not decrease as the field grows. Out-of-reach targets land on the nearest
/// end of `range`.
fn solve(
    adj: &mut Adjustments,
    range: std::ops::RangeInclusive<f32>,
    target: f32,
    field: fn(&mut Adjustments) -> &mut f32,
    measure: impl Fn(&Adjustments) -> f32,
) -> f32 {
    let (mut lo, mut hi) = (*range.start(), *range.end());
    for _ in 0..SOLVE_STEPS {
        let mid = 0.5 * (lo + hi);
        *field(adj) = mid;
        if measure(adj) < target {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let out = 0.5 * (lo + hi);
    *field(adj) = out;
    out
}

/// If `measure` is above `level`, solve `field` within `range` to bring it
/// down to `level`. Otherwise leave `adj` alone.
fn hold_below(
    adj: &mut Adjustments,
    range: std::ops::RangeInclusive<f32>,
    level: f32,
    field: fn(&mut Adjustments) -> &mut f32,
    measure: impl Fn(&Adjustments) -> f32,
) {
    if measure(adj) > level {
        solve(adj, range, level, field, measure);
    }
}

/// [`hold_below`] from the other side.
fn hold_above(
    adj: &mut Adjustments,
    range: std::ops::RangeInclusive<f32>,
    level: f32,
    field: fn(&mut Adjustments) -> &mut f32,
    measure: impl Fn(&Adjustments) -> f32,
) {
    if measure(adj) < level {
        solve(adj, range, level, field, measure);
    }
}

/// How Exposure centers a photo whose hill sits near the middle. A setting,
/// because each has a trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Centering {
    /// Put the middle of the range, x1 to x2, at 50%. Keeps the hill's shape
    /// where it is, whatever the mix of dark and bright inside it.
    #[default]
    Range,
    /// Put the middle pixel at 50%. Follows where most of the photo is, so a
    /// photo that is mostly shadow comes out brighter.
    Median,
}

impl Centering {
    /// `prefs` key holding the saved choice.
    const PREF_KEY: &'static str = "autotone_centering";

    fn as_str(self) -> &'static str {
        match self {
            Centering::Range => "range",
            Centering::Median => "median",
        }
    }

    fn parse(s: &str) -> Option<Centering> {
        match s.trim() {
            "range" => Some(Centering::Range),
            "median" => Some(Centering::Median),
            _ => None,
        }
    }

    /// The saved choice, or [`Centering::Range`] when none is saved.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Centering {
        crate::persist::prefs::load(Centering::PREF_KEY)
            .and_then(|s| Centering::parse(&s))
            .unwrap_or_default()
    }

    /// Remember the choice for the next launch. A storage failure costs one
    /// relaunch's choice, so it is logged, not surfaced.
    pub(crate) fn save(self) {
        // A test must never write the developer's own settings.
        if cfg!(test) {
            return;
        }
        if let Err(e) = crate::persist::prefs::save(Centering::PREF_KEY, self.as_str()) {
            eprintln!("[lightphotos] could not save the Auto Tone centering: {e}");
        }
    }
}

/// Where a photo's histogram hill sits before any edit. Each place gets its
/// own recipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hill {
    Dark,
    Normal,
    Bright,
}

/// The unedited photo's numbers that both recipes start from.
struct Original {
    highlight_fraction: f32,
    clipped_fraction: f32,
    shadow_fraction: f32,
    /// The range's ends as pixel fractions, from [`Histogram::edges`].
    low: f32,
    high: f32,
    exposure_range: std::ops::RangeInclusive<f32>,
}

impl Original {
    /// Measured before any adjustment, so the guards reflect the original photo.
    fn measure(hist: &Histogram) -> Original {
        let highlight_fraction = hist.fraction_above(HIGHLIGHT_LEVEL);
        let clipped_fraction = hist.fraction_above(CLIPPED_LEVEL);
        let white_point = hist.percentile(0.99, &Adjustments::default());
        // A hot photo gets no more light, however dark its median.
        let hot = white_point > HOT_WHITE_POINT
            || highlight_fraction > HIGHLIGHT_FRACTION
            || clipped_fraction > CLIPPED_FRACTION;
        let (low, high) = hist.edges();
        Original {
            highlight_fraction,
            clipped_fraction,
            shadow_fraction: hist.fraction_below(SHADOW_LEVEL),
            low,
            high,
            exposure_range: if hot {
                *EXPOSURE_RANGE.start()..=0.0
            } else {
                EXPOSURE_RANGE
            },
        }
    }
}

/// Auto Tone adjustments for a downscaled linear-light sample. `format` picks
/// the display pipeline, and `centering` how a normal photo is centered. An
/// empty sample returns the defaults.
#[hotpath::measure]
pub(crate) fn analyze(
    samples: &[[f32; 3]],
    format: PixelFormat,
    centering: Centering,
) -> Adjustments {
    let Some(hist) = Histogram::build(samples, format) else {
        return Adjustments::default();
    };

    let original = Original::measure(&hist);
    let mut adj = match hist.hill() {
        Hill::Normal => normal(&hist, &original, centering),
        Hill::Dark | Hill::Bright => lopsided(&hist, &original),
    };

    // A hand-picked rule with an eyeballed constant. Retune by eye.
    if hist.mean_sat < 0.2 {
        adj.vibrance = ((0.2 - hist.mean_sat) * 120.0).min(*TONE_RANGE.end());
    }

    adj.exposure = adj
        .exposure
        .clamp(*EXPOSURE_RANGE.start(), *EXPOSURE_RANGE.end());
    adj.contrast = adj.contrast.clamp(*TONE_RANGE.start(), *TONE_RANGE.end());
    adj
}

/// A hill that already sits near the middle. Exposure centers it by
/// `centering`, then Highlights puts its top at [`TARGET_HIGH`] and Shadows
/// puts its bottom at [`TARGET_LOW`]. Whites and Blacks only cover what those
/// two can't reach, because they are the sliders a person reaches for last.
/// Contrast widens a photo still too narrow.
fn normal(hist: &Histogram, original: &Original, centering: Centering) -> Adjustments {
    let x1 = |a: &Adjustments| hist.percentile(original.low, a);
    let x2 = |a: &Adjustments| hist.percentile(original.high, a);
    let mut adj = Adjustments::default();

    // Solve one slider at a time with the others held, then repeat. The
    // repeats make it converge.
    for _ in 0..REFINE_PASSES {
        solve(
            &mut adj,
            original.exposure_range.clone(),
            TARGET_MIDDLE,
            |a| &mut a.exposure,
            |a| match centering {
                Centering::Range => 0.5 * (x1(a) + x2(a)),
                Centering::Median => hist.percentile(0.50, a),
            },
        );
        adj.whites = 0.0;
        solve(&mut adj, TONE_RANGE, TARGET_HIGH, |a| &mut a.highlights, x2);
        if (x2(&adj) - TARGET_HIGH).abs() > NEAR_ENOUGH {
            solve(&mut adj, TONE_RANGE, TARGET_HIGH, |a| &mut a.whites, x2);
        }
        adj.blacks = 0.0;
        solve(&mut adj, TONE_RANGE, TARGET_LOW, |a| &mut a.shadows, x1);
        if (x1(&adj) - TARGET_LOW).abs() > NEAR_ENOUGH {
            solve(&mut adj, TONE_RANGE, TARGET_LOW, |a| &mut a.blacks, x1);
        }
        // Contrast only covers spread the ends couldn't reach, so a
        // well-exposed photo keeps Contrast at zero.
        let spread = |a: &Adjustments| x2(a) - x1(a);
        adj.contrast = 0.0;
        if spread(&adj) < TARGET_HIGH - TARGET_LOW - NEAR_ENOUGH {
            solve(
                &mut adj,
                0.0..=MAX_AUTO_CONTRAST,
                TARGET_HIGH - TARGET_LOW,
                |a| &mut a.contrast,
                spread,
            );
            // On a frame already blowing out, contrast mostly adds blown pixels.
            if original.highlight_fraction > HIGHLIGHT_FRACTION {
                adj.contrast *= 0.5;
            }
        }
    }
    adj
}

/// A hill that sits too dark or too bright. Exposure slides its median to the
/// middle,
/// which moves the pile off the wall, then Highlights and Shadows fix what
/// that move did, in the order a person edits: Highlights recovers what a
/// lift blew, and Shadows lifts what a cut crushed. Whites, Blacks and
/// Contrast stay at zero, as they do when a person makes this edit.
fn lopsided(hist: &Histogram, original: &Original) -> Adjustments {
    // How much of the frame may end up near white or black. A near-white sky
    // is recovered even if the photo came that way, unless it is clipped and
    // past saving. Shadows only lifts what Auto Tone itself crushed.
    let highlight_cap = original.clipped_fraction.max(HIGHLIGHT_FRACTION);
    let shadow_cap = original.shadow_fraction.max(SHADOW_FRACTION);
    let blown = |a: &Adjustments| hist.percentile(1.0 - highlight_cap, a);
    let crushed = |a: &Adjustments| hist.percentile(shadow_cap, a);

    let mut adj = Adjustments::default();

    for _ in 0..REFINE_PASSES {
        solve(
            &mut adj,
            original.exposure_range.clone(),
            TARGET_MIDDLE,
            |a| &mut a.exposure,
            |a| hist.percentile(0.50, a),
        );

        adj.highlights = 0.0;
        hold_below(
            &mut adj,
            *TONE_RANGE.start()..=0.0,
            HIGHLIGHT_LEVEL,
            |a| &mut a.highlights,
            blown,
        );

        adj.shadows = 0.0;
        hold_above(
            &mut adj,
            0.0..=*TONE_RANGE.end(),
            SHADOW_LEVEL,
            |a| &mut a.shadows,
            crushed,
        );
    }
    adj
}

/// Copy Auto Tone's sliders onto `base`. Crop, white balance, saturation, and
/// denoise keep the user's values, and so does vibrance on a B&W photo, where
/// it has no effect and would only surface on switching back to color.
pub(crate) fn merge(base: &Adjustments, auto: &Adjustments) -> Adjustments {
    let vibrance = if base.is_monochrome() {
        base.vibrance
    } else {
        auto.vibrance
    };
    Adjustments {
        exposure: auto.exposure,
        contrast: auto.contrast,
        highlights: auto.highlights,
        shadows: auto.shadows,
        whites: auto.whites,
        blacks: auto.blacks,
        vibrance,
        ..*base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` gray linear-light samples whose display luma runs from `lo` to `hi`.
    fn ramp(lo: f32, hi: f32, n: usize) -> Vec<[f32; 3]> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                let display = lo + (hi - lo) * t;
                let linear = display.powf(2.2);
                [linear; 3]
            })
            .collect()
    }

    fn flat(display: f32, n: usize) -> Vec<[f32; 3]> {
        vec![[display.powf(2.2); 3]; n]
    }

    /// A dark pile with a long tail toward bright: display luma
    /// `top * t^steep`. The larger `steep`, the tighter the pile.
    fn dark_pile(top: f32, steep: i32, n: usize) -> Vec<[f32; 3]> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                [(top * t.powi(steep)).powf(2.2); 3]
            })
            .collect()
    }

    /// The mirror of [`dark_pile`]: a bright pile with a tail toward dark.
    fn bright_pile(bottom: f32, steep: i32, n: usize) -> Vec<[f32; 3]> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                [(1.0 - (1.0 - bottom) * t.powi(steep)).powf(2.2); 3]
            })
            .collect()
    }

    #[test]
    fn hills_are_classified_by_where_they_sit() {
        let hill = |samples: &[[f32; 3]]| {
            Histogram::build(samples, PixelFormat::Srgb8)
                .unwrap()
                .hill()
        };
        assert_eq!(hill(&ramp(0.1, 0.3, 256)), Hill::Dark);
        assert_eq!(hill(&ramp(0.3, 0.7, 256)), Hill::Normal);
        assert_eq!(hill(&ramp(0.7, 0.9, 256)), Hill::Bright);
        assert_eq!(hill(&dark_pile(0.7, 3, 256)), Hill::Dark);
        assert_eq!(hill(&bright_pile(0.3, 2, 256)), Hill::Bright);
    }

    /// A dark pile is slid to the middle, and the lift doesn't blow the top.
    #[test]
    fn a_dark_pile_is_brought_to_the_middle() {
        // About four stops under, within reach of +5 Exposure.
        let photo = dark_pile(0.9, 3, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        let median = percentile_under(&photo, &adj, 0.50);
        assert!(
            (median - TARGET_MIDDLE).abs() < 0.06,
            "median {median} in {adj:?}"
        );
        let blown = fraction_above_under(&photo, &adj, HIGHLIGHT_LEVEL);
        assert!(
            blown <= HIGHLIGHT_FRACTION + 0.01,
            "{blown} blown in {adj:?}"
        );
    }

    /// A bright pile is slid off the right wall to the middle.
    #[test]
    fn a_bright_pile_is_brought_to_the_middle() {
        let photo = bright_pile(0.3, 2, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.exposure < 0.0, "exposure {}", adj.exposure);
        let median = percentile_under(&photo, &adj, 0.50);
        assert!(
            (median - TARGET_MIDDLE).abs() < 0.06,
            "median {median} in {adj:?}"
        );
        let blown = fraction_above_under(&photo, &adj, HIGHLIGHT_LEVEL);
        assert!(
            blown <= HIGHLIGHT_FRACTION + 0.01,
            "{blown} blown in {adj:?}"
        );
    }

    /// The range's middle and ends landed on their targets.
    fn assert_lands((x1, x2): (f32, f32), tolerance: f32) {
        let middle = 0.5 * (x1 + x2);
        assert!(
            (middle - TARGET_MIDDLE).abs() < tolerance,
            "middle {middle}"
        );
        assert!((x1 - TARGET_LOW).abs() < tolerance, "x1 {x1}");
        assert!((x2 - TARGET_HIGH).abs() < tolerance, "x2 {x2}");
    }

    #[test]
    fn a_well_exposed_ramp_is_left_nearly_alone() {
        let adj = analyze(&ramp(0.0, 1.0, 512), PixelFormat::Srgb8, Centering::Range);
        assert!(adj.exposure.abs() < 0.15, "exposure {}", adj.exposure);
        assert!(adj.blacks.abs() < 10.0, "blacks {}", adj.blacks);
        assert!(adj.whites.abs() < 10.0, "whites {}", adj.whites);
    }

    #[test]
    fn an_underexposed_photo_gets_positive_exposure() {
        let dark = ramp(0.0, 0.35, 512);
        let adj = analyze(&dark, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.exposure > 0.5, "expected a lift, got {}", adj.exposure);
        let median = percentile_under(&dark, &adj, 0.50);
        assert!((median - TARGET_MIDDLE).abs() < 0.06, "median {median}");
    }

    #[test]
    fn an_overexposed_photo_is_never_brightened() {
        // Mostly blown, so the hot guard must keep exposure at or below zero.
        let hot = ramp(0.75, 1.0, 512);
        let adj = analyze(&hot, PixelFormat::Srgb8, Centering::Range);
        assert!(
            adj.exposure <= 0.0,
            "expected no lift, got {}",
            adj.exposure
        );
    }

    #[test]
    fn a_flat_photo_has_its_endpoints_pulled_out() {
        let fogged = ramp(0.35, 0.62, 512);
        let adj = analyze(&fogged, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.shadows < -20.0, "shadows {}", adj.shadows);
        assert!(adj.highlights > 20.0, "highlights {}", adj.highlights);
    }

    /// The range's ends, x1 and x2, measured over every real pixel.
    fn range_under(samples: &[[f32; 3]], adj: &Adjustments) -> (f32, f32) {
        let hist = Histogram::build(samples, PixelFormat::Srgb8).unwrap();
        let (lo, hi) = hist.edges();
        (
            percentile_under(samples, adj, lo),
            percentile_under(samples, adj, hi),
        )
    }

    /// A long, thin tail of bars under the height threshold isn't part of
    /// the range.
    #[test]
    fn the_range_drops_sparse_bars_from_each_side() {
        let mut photo = ramp(0.2, 0.6, 20_000);
        for level in 156..=255 {
            photo.extend(flat(level as f32 / 255.0, 2));
        }
        let (x1, x2) = range_under(&photo, &Adjustments::default());
        assert!((x1 - 0.2).abs() < 0.02, "x1 {x1}");
        assert!((x2 - 0.6).abs() < 0.02, "x2 {x2}");
    }

    /// A lamp tall enough to pass the height threshold still doesn't set the
    /// top of the range.
    #[test]
    fn a_small_bright_spot_does_not_set_the_top_edge() {
        let mut photo = ramp(0.2, 0.6, 20_000);
        photo.extend(flat(1.0, 60));
        let (_, x2) = range_under(&photo, &Adjustments::default());
        assert!((x2 - 0.6).abs() < 0.02, "x2 {x2}");
    }

    /// Exposure centres the range, not the median. Most of this photo sits
    /// low in its range, so centring the median would brighten it instead.
    #[test]
    fn exposure_centres_the_range() {
        let mut photo = ramp(0.3, 0.45, 3000);
        photo.extend(ramp(0.45, 0.8, 1000));
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        assert_eq!(hist.hill(), Hill::Normal);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        let (x1, x2) = range_under(&photo, &adj);
        let middle = 0.5 * (x1 + x2);
        assert!(
            (middle - TARGET_MIDDLE).abs() < 0.03,
            "middle {middle} in {adj:?}"
        );
    }

    /// Median centering puts the middle pixel at 50% instead of the range's
    /// middle. Most of this photo sits low in its range, so it comes
    /// out brighter than with range centering.
    #[test]
    fn median_centering_puts_the_middle_pixel_at_half() {
        let mut photo = ramp(0.3, 0.45, 3000);
        photo.extend(ramp(0.45, 0.8, 1000));
        let by_range = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        let by_median = analyze(&photo, PixelFormat::Srgb8, Centering::Median);
        let median = percentile_under(&photo, &by_median, 0.50);
        assert!(
            (median - TARGET_MIDDLE).abs() < 0.03,
            "median {median} in {by_median:?}"
        );
        assert!(
            by_median.exposure > by_range.exposure,
            "{by_median:?} vs {by_range:?}"
        );
    }

    #[test]
    fn centering_round_trips_through_its_saved_name() {
        for c in [Centering::Range, Centering::Median] {
            assert_eq!(Centering::parse(c.as_str()), Some(c));
        }
        assert_eq!(Centering::parse("bogus"), None);
    }

    /// A normal photo's ends go to 5% and 95% through Highlights and
    /// Shadows, the sliders a person reaches for first.
    #[test]
    fn a_normal_photo_sets_its_ends_with_highlights_and_shadows() {
        let photo = ramp(0.15, 0.8, 4000);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert_eq!((adj.whites, adj.blacks), (0.0, 0.0), "{adj:?}");
        assert!(adj.highlights > 0.0 && adj.shadows < 0.0, "{adj:?}");
        let (x1, x2) = range_under(&photo, &adj);
        assert!((x1 - TARGET_LOW).abs() < 0.03, "x1 {x1} in {adj:?}");
        assert!((x2 - TARGET_HIGH).abs() < 0.03, "x2 {x2} in {adj:?}");
    }

    /// Display luma at percentile `p`, measured over every pixel instead of the
    /// histogram's stand-ins.
    fn percentile_under(samples: &[[f32; 3]], adj: &Adjustments, p: f32) -> f32 {
        let mut out: Vec<f32> = samples
            .iter()
            .map(|&px| {
                let o = develop::apply_linear(adj, px);
                luma([
                    o[0].max(0.0).powf(1.0 / 2.2),
                    o[1].max(0.0).powf(1.0 / 2.2),
                    o[2].max(0.0).powf(1.0 / 2.2),
                ])
            })
            .collect();
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out[((out.len() as f32 * p) as usize).min(out.len() - 1)]
    }

    #[test]
    fn the_solver_lands_on_its_targets() {
        let photo = ramp(0.12, 0.78, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert_lands(range_under(&photo, &adj), 0.03);
    }

    /// Gray and strongly coloured pixels over the same display range. Four
    /// rotating hues keep any one channel from matching the luma weights.
    fn saturated(lo: f32, hi: f32, n: usize) -> Vec<[f32; 3]> {
        let hues = [
            [1.0, 0.15, 0.15],
            [0.15, 1.0, 0.15],
            [0.15, 0.15, 1.0],
            [1.0, 1.0, 1.0],
        ];
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                let display = lo + (hi - lo) * t;
                let h = hues[i % hues.len()];
                [
                    (display * h[0]).powf(2.2),
                    (display * h[1]).powf(2.2),
                    (display * h[2]).powf(2.2),
                ]
            })
            .collect()
    }

    #[test]
    fn adjusted_percentiles_follow_colours_when_their_brightness_order_changes() {
        let mut photo = vec![[1.0, 0.0, 0.0]; 40];
        photo.extend(flat(0.26, 60));
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        let adj = Adjustments {
            whites: 100.0,
            ..Default::default()
        };
        assert!(
            respond(&Adjustments::default(), PixelFormat::Srgb8, photo[0])
                > respond(&Adjustments::default(), PixelFormat::Srgb8, photo[99])
        );
        assert!(
            respond(&adj, PixelFormat::Srgb8, photo[0])
                < respond(&adj, PixelFormat::Srgb8, photo[99])
        );
        for p in [0.01, 0.25, 0.50, 0.75, 0.99] {
            let expected = percentile_under(&photo, &adj, p);
            let got = hist.percentile(p, &adj);
            assert!((got - expected).abs() < 0.01, "p={p}: {got} vs {expected}");
        }
    }

    #[test]
    fn contrast_percentiles_follow_reordered_coloured_and_neutral_samples() {
        let mut photo = vec![[1.0, 0.0, 0.0]; 40];
        photo.extend(flat(0.30, 60));
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        let adj = Adjustments {
            contrast: 50.0,
            ..Default::default()
        };
        assert!(
            respond(&Adjustments::default(), PixelFormat::Srgb8, photo[0])
                < respond(&Adjustments::default(), PixelFormat::Srgb8, photo[99])
        );
        assert!(
            respond(&adj, PixelFormat::Srgb8, photo[0])
                > respond(&adj, PixelFormat::Srgb8, photo[99])
        );
        for p in [0.01, 0.25, 0.50, 0.75, 0.99] {
            assert!((hist.percentile(p, &adj) - percentile_under(&photo, &adj, p)).abs() < 1e-4);
        }
    }

    #[test]
    fn fully_saturated_representatives_preserve_exposure_response() {
        for format in [PixelFormat::Srgb8, PixelFormat::LinearF16] {
            for px in [
                [1.0, 0.0, 0.0],
                [0.0, 1.0, 0.0],
                [0.0, 0.0, 1.0],
                [1.0, 1.0, 0.0],
                [1.0, 0.0, 1.0],
                [0.0, 1.0, 1.0],
            ] {
                let hist = Histogram::build(&vec![px; 1000], format).unwrap();
                for exposure in [-2.0, 0.0, 2.0] {
                    let adj = Adjustments {
                        exposure,
                        ..Default::default()
                    };
                    let expected = respond(&adj, format, px);
                    assert!((hist.percentile(0.5, &adj) - expected).abs() < 1e-5);
                }
                let level = respond(&Adjustments::default(), format, px);
                assert_eq!(fit_to_level(format, px, level + 0.01), px);
                assert_eq!(fit_to_level(format, px, level), px);
            }
        }
    }

    /// Each bin's stand-in must display at the bin's own luma. Gray test photos
    /// can't catch a violation; only coloured pixels do.
    #[test]
    fn every_bin_representative_reproduces_its_own_display_luma() {
        let photo = saturated(0.02, 0.98, 2048);
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        let identity = Adjustments::default();
        for bin in 0..BINS {
            if hist.count[bin] == 0.0 {
                continue;
            }
            let level = bin as f32 / (BINS - 1) as f32;
            let got = respond(&identity, PixelFormat::Srgb8, hist.lin[bin]);
            assert!(
                (got - level).abs() < 0.005,
                "bin {bin} stands for {level:.3} but reads back as {got:.3}"
            );
        }
    }

    /// A saturated photo must hit the same targets as a gray one. Measured on
    /// real pixels so a wrong histogram can't pass. Runs the solver directly,
    /// because this photo sits dark enough to take the other recipe.
    #[test]
    fn the_solver_lands_on_its_targets_for_a_saturated_photo() {
        let photo = saturated(0.10, 0.55, 512);
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        let adj = normal(&hist, &Original::measure(&hist), Centering::Range);
        assert_lands(range_under(&photo, &adj), 0.06);
    }

    /// `saturated` in linear camera RGB, as `decode::raw_preview` returns it.
    fn raw_saturated(lo: f32, hi: f32, n: usize) -> Vec<[f32; 3]> {
        let hues = [
            [1.0, 0.15, 0.15],
            [0.15, 1.0, 0.15],
            [0.15, 0.15, 1.0],
            [1.0, 1.0, 1.0],
        ];
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                let v = lo + (hi - lo) * t;
                let h = hues[i % hues.len()];
                [v * h[0], v * h[1], v * h[2]]
            })
            .collect()
    }

    /// `percentile_under` for the RAW display pipeline.
    fn raw_percentile_under(samples: &[[f32; 3]], adj: &Adjustments, p: f32) -> f32 {
        let mut out: Vec<f32> = samples
            .iter()
            .map(|&px| luma(develop::apply_raw_display(adj, px)))
            .collect();
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out[((out.len() as f32 * p) as usize).min(out.len() - 1)]
    }

    fn assert_monotonic(
        what: &str,
        range: std::ops::RangeInclusive<f32>,
        field: fn(&mut Adjustments) -> &mut f32,
        measure: impl Fn(&Adjustments) -> f32,
    ) {
        let mut prev = f32::NEG_INFINITY;
        for step in 0..=128 {
            let t = step as f32 / 128.0;
            let mut adj = Adjustments::default();
            *field(&mut adj) = range.start() + (range.end() - range.start()) * t;
            let v = measure(&adj);
            assert!(
                v >= prev - 1e-4,
                "{what} went backwards at t={t}: {prev} then {v}"
            );
            prev = v;
        }
    }

    /// Bisection needs each measure to never decrease as its slider grows. The
    /// RAW display curve differs from sRGB, so check it. Contrast is checked
    /// against the spread, since single bins don't move monotonically with it.
    #[test]
    fn the_raw_solve_measures_are_monotonic_in_their_own_sliders() {
        let photo = raw_saturated(0.005, 0.6, 512);
        let hist = Histogram::build(&photo, PixelFormat::LinearF16).unwrap();

        assert_monotonic(
            "exposure/median",
            EXPOSURE_RANGE,
            |a| &mut a.exposure,
            |a| hist.percentile(0.50, a),
        );
        assert_monotonic(
            "blacks/1st pct",
            TONE_RANGE,
            |a| &mut a.blacks,
            |a| hist.percentile(0.01, a),
        );
        assert_monotonic(
            "whites/99th pct",
            TONE_RANGE,
            |a| &mut a.whites,
            |a| hist.percentile(0.99, a),
        );
        assert_monotonic(
            "highlights/98th pct",
            TONE_RANGE,
            |a| &mut a.highlights,
            |a| hist.percentile(0.98, a),
        );
        assert_monotonic(
            "shadows/5th pct",
            TONE_RANGE,
            |a| &mut a.shadows,
            |a| hist.percentile(0.05, a),
        );
        assert_monotonic(
            "contrast/spread",
            TONE_RANGE,
            |a| &mut a.contrast,
            |a| hist.percentile(0.99, a) - hist.percentile(0.01, a),
        );
    }

    /// The bin stand-in check on the RAW pipeline, whose output is clamped.
    #[test]
    fn every_bin_representative_reproduces_its_own_display_luma_for_raw() {
        let photo = raw_saturated(0.001, 0.9, 2048);
        let hist = Histogram::build(&photo, PixelFormat::LinearF16).unwrap();
        let identity = Adjustments::default();
        for bin in 0..BINS {
            if hist.count[bin] == 0.0 {
                continue;
            }
            let level = bin as f32 / (BINS - 1) as f32;
            let got = respond(&identity, PixelFormat::LinearF16, hist.lin[bin]);
            assert!(
                (got - level).abs() < 0.005,
                "bin {bin} stands for {level:.3} but reads back as {got:.3}"
            );
        }
    }

    /// `range_under` for the RAW display pipeline.
    fn raw_range_under(samples: &[[f32; 3]], adj: &Adjustments) -> (f32, f32) {
        let hist = Histogram::build(samples, PixelFormat::LinearF16).unwrap();
        let (lo, hi) = hist.edges();
        (
            raw_percentile_under(samples, adj, lo),
            raw_percentile_under(samples, adj, hi),
        )
    }

    /// The normal solve on the RAW pipeline, which the browser build's RAW
    /// Loupe uses. Native macOS never reaches it because ImageIO returns sRGB.
    #[test]
    fn the_solver_lands_on_its_targets_for_a_raw_photo() {
        let photo = raw_saturated(0.005, 0.6, 512);
        let hist = Histogram::build(&photo, PixelFormat::LinearF16).unwrap();
        let adj = normal(&hist, &Original::measure(&hist), Centering::Range);
        assert_lands(raw_range_under(&photo, &adj), 0.06);
    }

    #[test]
    fn an_underexposed_raw_photo_gets_positive_exposure() {
        let dark = raw_saturated(0.002, 0.30, 512);
        let adj = analyze(&dark, PixelFormat::LinearF16, Centering::Range);
        assert!(adj.exposure > 0.5, "expected a lift, got {}", adj.exposure);
        // Only Median promises the middle pixel lands at 50%.
        let adj = analyze(&dark, PixelFormat::LinearF16, Centering::Median);
        let median = raw_percentile_under(&dark, &adj, 0.50);
        assert!((median - TARGET_MIDDLE).abs() < 0.06, "median {median}");
    }

    #[test]
    fn contrast_stays_off_when_the_endpoints_suffice() {
        assert_eq!(
            analyze(&ramp(0.0, 1.0, 512), PixelFormat::Srgb8, Centering::Range).contrast,
            0.0
        );
    }

    #[test]
    fn contrast_is_capped() {
        let very_flat = ramp(0.47, 0.53, 512);
        let adj = analyze(&very_flat, PixelFormat::Srgb8, Centering::Range);
        assert!(
            adj.contrast <= MAX_AUTO_CONTRAST,
            "contrast {}",
            adj.contrast
        );
    }

    #[test]
    fn a_desaturated_photo_gets_vibrance() {
        let gray = ramp(0.1, 0.9, 512);
        assert!(analyze(&gray, PixelFormat::Srgb8, Centering::Range).vibrance > 0.0);
    }

    fn fraction_above_under(samples: &[[f32; 3]], adj: &Adjustments, level: f32) -> f32 {
        let above = (0..samples.len())
            .filter(|&i| percentile_under(&samples[i..=i], adj, 0.0) >= level)
            .count();
        above as f32 / samples.len() as f32
    }

    fn fraction_below_under(samples: &[[f32; 3]], adj: &Adjustments, level: f32) -> f32 {
        let below = (0..samples.len())
            .filter(|&i| percentile_under(&samples[i..=i], adj, 0.0) <= level)
            .count();
        below as f32 / samples.len() as f32
    }

    /// A dark photo with a bright patch. Lifting the median blows the patch,
    /// so Highlights must pull it back.
    #[test]
    fn a_lifted_photo_recovers_its_highlights() {
        let mut photo = ramp(0.0, 0.35, 480);
        photo.extend(ramp(0.7, 0.85, 32));
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.exposure > 0.0, "exposure {}", adj.exposure);
        assert!(adj.highlights < 0.0, "highlights {}", adj.highlights);
        let blown = fraction_above_under(&photo, &adj, HIGHLIGHT_LEVEL);
        assert!(
            blown <= HIGHLIGHT_FRACTION + 0.01,
            "{blown} of the frame blown in {adj:?}"
        );
    }

    /// A normal photo with deep blacks and a bright sky spans more than 5% to
    /// 95%: Highlights pulls the sky down and Shadows lifts the blacks.
    #[test]
    fn a_wide_normal_photo_pulls_highlights_down_and_lifts_shadows() {
        let mut photo = ramp(0.0, 0.85, 450);
        photo.extend(ramp(0.96, 0.99, 60));
        let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
        assert_eq!(hist.hill(), Hill::Normal);
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.highlights < 0.0, "highlights {}", adj.highlights);
        assert!(adj.shadows > 0.0, "shadows {}", adj.shadows);
        assert_eq!((adj.whites, adj.blacks), (0.0, 0.0), "{adj:?}");
    }

    /// A too-bright photo with dark detail just above the shadow level.
    /// Cutting Exposure crushes the detail, so Shadows must lift it back.
    #[test]
    fn a_darkened_photo_lifts_its_shadows() {
        let mut photo = flat(0.02, 8);
        photo.extend(ramp(0.15, 0.18, 80));
        photo.extend(ramp(0.55, 0.97, 432));
        let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
        assert!(adj.exposure < 0.0, "exposure {}", adj.exposure);
        assert!(adj.shadows > 0.0, "shadows {}", adj.shadows);
        let crushed = fraction_below_under(&photo, &adj, SHADOW_LEVEL);
        assert!(
            crushed <= SHADOW_FRACTION + 0.01,
            "{crushed} of the frame crushed in {adj:?}"
        );
    }

    /// Too-dark and too-bright photos are fixed with Exposure, Highlights and
    /// Shadows, the way a person does it by hand.
    #[test]
    fn lopsided_photos_leave_whites_and_blacks_alone() {
        for (lo, hi) in [(0.0, 0.02), (0.98, 1.0), (0.0, 0.35), (0.7, 1.0)] {
            let photo = ramp(lo, hi, 256);
            let hist = Histogram::build(&photo, PixelFormat::Srgb8).unwrap();
            assert_ne!(hist.hill(), Hill::Normal, "{lo}..{hi}");
            let adj = analyze(&photo, PixelFormat::Srgb8, Centering::Range);
            assert_eq!((adj.whites, adj.blacks), (0.0, 0.0), "{lo}..{hi}");
        }
    }

    #[test]
    fn an_empty_sample_is_identity() {
        assert!(analyze(&[], PixelFormat::Srgb8, Centering::Range).is_identity());
    }

    #[test]
    fn merge_keeps_everything_auto_does_not_own() {
        let base = Adjustments {
            temp: 20.0,
            saturation: 15.0,
            denoise: 40.0,
            exposure: 3.0,
            ..Default::default()
        };
        let auto = Adjustments {
            exposure: -1.0,
            blacks: -25.0,
            ..Default::default()
        };
        let out = merge(&base, &auto);
        assert_eq!(out.temp, 20.0);
        assert_eq!(out.saturation, 15.0);
        assert_eq!(out.denoise, 40.0);
        assert_eq!(out.exposure, -1.0);
        assert_eq!(out.blacks, -25.0);
    }

    #[test]
    fn merge_on_a_black_and_white_photo_keeps_it_black_and_white() {
        let base = Adjustments {
            saturation: -100.0,
            vibrance: 5.0,
            ..Default::default()
        };
        let auto = Adjustments {
            exposure: 0.5,
            vibrance: 30.0,
            ..Default::default()
        };
        let out = merge(&base, &auto);
        assert!(out.is_monochrome());
        assert_eq!(out.vibrance, 5.0);
        assert_eq!(out.exposure, 0.5);
    }

    #[test]
    fn every_slider_stays_inside_its_range() {
        for (lo, hi) in [(0.0, 0.02), (0.98, 1.0), (0.49, 0.51), (0.0, 1.0)] {
            let adj = analyze(&ramp(lo, hi, 256), PixelFormat::Srgb8, Centering::Range);
            assert!(EXPOSURE_RANGE.contains(&adj.exposure), "{adj:?}");
            for v in [
                adj.contrast,
                adj.highlights,
                adj.shadows,
                adj.whites,
                adj.blacks,
                adj.vibrance,
            ] {
                assert!(TONE_RANGE.contains(&v), "{v} out of range in {adj:?}");
            }
        }
    }
}
