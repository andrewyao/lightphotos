//! Auto Tone: pick Develop slider values from a photo's histogram.
//!
//! Exposure, Blacks, Whites, and Contrast are solved by bisection against our
//! real tone pipeline ([`develop::apply_linear`], or `apply_raw_display` for
//! RAW), so results match what the shader draws. Highlights, Shadows, and
//! Vibrance have no target to solve for and use hand-picked rules adapted from
//! RapidRAW. RapidRAW's constants don't carry over because its sliders work
//! differently.

use crate::develop::{self, Adjustments, EXPOSURE_RANGE, TONE_RANGE};
use crate::image_decode::PixelFormat;

/// One bin per 8-bit display level.
const BINS: usize = 256;

/// Display-space target for the median.
const TARGET_MEDIAN: f32 = 0.5;
/// Targets for the 1st and 99th percentiles. Aiming at exactly 0 and 1 would
/// clip 1% of the frame at each end.
const TARGET_BLACK: f32 = 0.01;
const TARGET_WHITE: f32 = 0.99;
/// Below this spread between the 1st and 99th percentiles, Contrast is added.
const TARGET_RANGE: f32 = 220.0 / 255.0;
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
/// `develop.rs`. Linear-light code uses Rec.709 instead.
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

/// Auto Tone adjustments for a downscaled linear-light sample. `format` picks
/// the display pipeline. An empty sample returns the defaults.
pub(crate) fn analyze(samples: &[[f32; 3]], format: PixelFormat) -> Adjustments {
    let Some(hist) = Histogram::build(samples, format) else {
        return Adjustments::default();
    };

    // Measured before any adjustment, so the guards reflect the original photo.
    let highlight_fraction = hist.fraction_above(HIGHLIGHT_LEVEL);
    let clipped_fraction = hist.fraction_above(CLIPPED_LEVEL);
    let shadow_fraction = hist.fraction_below(SHADOW_LEVEL);
    let identity = Adjustments::default();
    let white_point = hist.percentile(0.99, &identity);

    // A hot photo gets no more light, however dark its median.
    let hot = white_point > HOT_WHITE_POINT
        || highlight_fraction > HIGHLIGHT_FRACTION
        || clipped_fraction > CLIPPED_FRACTION;

    let mut adj = Adjustments::default();

    // Solve one slider at a time with the others held, then repeat. The
    // repeats make it converge, whatever the order.
    let exposure_range = if hot {
        *EXPOSURE_RANGE.start()..=0.0
    } else {
        EXPOSURE_RANGE
    };
    for _ in 0..REFINE_PASSES {
        solve(
            &mut adj,
            exposure_range.clone(),
            TARGET_MEDIAN,
            |a| &mut a.exposure,
            |a| hist.percentile(0.50, a),
        );
        solve(
            &mut adj,
            TONE_RANGE,
            TARGET_BLACK,
            |a| &mut a.blacks,
            |a| hist.percentile(0.01, a),
        );
        solve(
            &mut adj,
            TONE_RANGE,
            TARGET_WHITE,
            |a| &mut a.whites,
            |a| hist.percentile(0.99, a),
        );
        // Contrast only covers spread the endpoints couldn't reach, so a
        // well-exposed photo keeps Contrast at zero.
        let spread = |a: &Adjustments| hist.percentile(0.99, a) - hist.percentile(0.01, a);
        adj.contrast = 0.0;
        if spread(&adj) < TARGET_RANGE {
            solve(
                &mut adj,
                0.0..=MAX_AUTO_CONTRAST,
                TARGET_RANGE,
                |a| &mut a.contrast,
                spread,
            );
            // On a frame already blowing out, contrast mostly adds blown pixels.
            if highlight_fraction > HIGHLIGHT_FRACTION {
                adj.contrast *= 0.5;
            }
        }
    }

    // Hand-picked rules with eyeballed constants. Retune by eye.
    if shadow_fraction > SHADOW_FRACTION {
        adj.shadows = (shadow_fraction * 40.0).min(50.0);
    }
    if highlight_fraction > HIGHLIGHT_FRACTION {
        adj.highlights = -(highlight_fraction * 120.0).min(70.0);
    }
    if hist.mean_sat < 0.2 {
        adj.vibrance = ((0.2 - hist.mean_sat) * 120.0).min(*TONE_RANGE.end());
    }

    adj.exposure = adj
        .exposure
        .clamp(*EXPOSURE_RANGE.start(), *EXPOSURE_RANGE.end());
    adj.contrast = adj.contrast.clamp(*TONE_RANGE.start(), *TONE_RANGE.end());
    adj
}

/// Copy Auto Tone's sliders onto `base`. Crop, white balance, saturation, and
/// denoise keep the user's values.
pub(crate) fn merge(base: &Adjustments, auto: &Adjustments) -> Adjustments {
    Adjustments {
        exposure: auto.exposure,
        contrast: auto.contrast,
        highlights: auto.highlights,
        shadows: auto.shadows,
        whites: auto.whites,
        blacks: auto.blacks,
        vibrance: auto.vibrance,
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

    fn median_under(samples: &[[f32; 3]], adj: &Adjustments) -> f32 {
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
        out[out.len() / 2]
    }

    #[test]
    fn a_well_exposed_ramp_is_left_nearly_alone() {
        let adj = analyze(&ramp(0.0, 1.0, 512), PixelFormat::Srgb8);
        assert!(adj.exposure.abs() < 0.15, "exposure {}", adj.exposure);
        assert!(adj.blacks.abs() < 10.0, "blacks {}", adj.blacks);
        assert!(adj.whites.abs() < 10.0, "whites {}", adj.whites);
    }

    #[test]
    fn an_underexposed_photo_gets_positive_exposure() {
        let dark = ramp(0.0, 0.35, 512);
        let adj = analyze(&dark, PixelFormat::Srgb8);
        assert!(adj.exposure > 0.5, "expected a lift, got {}", adj.exposure);
        let median = median_under(&dark, &adj);
        assert!(
            (median - TARGET_MEDIAN).abs() < 0.1,
            "median after auto: {median}"
        );
    }

    #[test]
    fn an_overexposed_photo_is_never_brightened() {
        // Mostly blown, so the hot guard must keep exposure at or below zero.
        let hot = ramp(0.75, 1.0, 512);
        let adj = analyze(&hot, PixelFormat::Srgb8);
        assert!(
            adj.exposure <= 0.0,
            "expected no lift, got {}",
            adj.exposure
        );
    }

    #[test]
    fn a_flat_photo_has_its_endpoints_pulled_out() {
        let fogged = ramp(0.35, 0.62, 512);
        let adj = analyze(&fogged, PixelFormat::Srgb8);
        assert!(adj.blacks < -20.0, "blacks {}", adj.blacks);
        assert!(adj.whites > 20.0, "whites {}", adj.whites);
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
        let adj = analyze(&photo, PixelFormat::Srgb8);
        let median = percentile_under(&photo, &adj, 0.50);
        let black = percentile_under(&photo, &adj, 0.01);
        let white = percentile_under(&photo, &adj, 0.99);
        assert!((median - TARGET_MEDIAN).abs() < 0.06, "median {median}");
        assert!((black - TARGET_BLACK).abs() < 0.06, "black point {black}");
        assert!((white - TARGET_WHITE).abs() < 0.06, "white point {white}");
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
    /// real pixels so a wrong histogram can't pass.
    #[test]
    fn the_solver_lands_on_its_targets_for_a_saturated_photo() {
        let photo = saturated(0.10, 0.55, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        let median = percentile_under(&photo, &adj, 0.50);
        let black = percentile_under(&photo, &adj, 0.01);
        let white = percentile_under(&photo, &adj, 0.99);
        assert!((median - TARGET_MEDIAN).abs() < 0.06, "median {median}");
        assert!((black - TARGET_BLACK).abs() < 0.06, "black point {black}");
        assert!((white - TARGET_WHITE).abs() < 0.06, "white point {white}");
    }

    /// `saturated` in linear camera RGB, as `raw/preview.rs` returns it.
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

    /// The full solve on the RAW pipeline, which the browser build's RAW Loupe
    /// uses. Native macOS never reaches it because ImageIO returns sRGB.
    #[test]
    fn the_solver_lands_on_its_targets_for_a_raw_photo() {
        let photo = raw_saturated(0.005, 0.6, 512);
        let adj = analyze(&photo, PixelFormat::LinearF16);
        let median = raw_percentile_under(&photo, &adj, 0.50);
        let black = raw_percentile_under(&photo, &adj, 0.01);
        let white = raw_percentile_under(&photo, &adj, 0.99);
        assert!((median - TARGET_MEDIAN).abs() < 0.06, "median {median}");
        assert!((black - TARGET_BLACK).abs() < 0.06, "black point {black}");
        assert!((white - TARGET_WHITE).abs() < 0.06, "white point {white}");
    }

    #[test]
    fn an_underexposed_raw_photo_gets_positive_exposure() {
        let dark = raw_saturated(0.002, 0.30, 512);
        let adj = analyze(&dark, PixelFormat::LinearF16);
        assert!(adj.exposure > 0.5, "expected a lift, got {}", adj.exposure);
        let median = raw_percentile_under(&dark, &adj, 0.50);
        assert!(
            (median - TARGET_MEDIAN).abs() < 0.06,
            "median after auto: {median}"
        );
    }

    #[test]
    fn contrast_stays_off_when_the_endpoints_suffice() {
        assert_eq!(
            analyze(&ramp(0.0, 1.0, 512), PixelFormat::Srgb8).contrast,
            0.0
        );
    }

    #[test]
    fn contrast_is_capped() {
        let very_flat = ramp(0.47, 0.53, 512);
        let adj = analyze(&very_flat, PixelFormat::Srgb8);
        assert!(
            adj.contrast <= MAX_AUTO_CONTRAST,
            "contrast {}",
            adj.contrast
        );
    }

    #[test]
    fn a_desaturated_photo_gets_vibrance() {
        let gray = ramp(0.1, 0.9, 512);
        assert!(analyze(&gray, PixelFormat::Srgb8).vibrance > 0.0);
    }

    #[test]
    fn a_crushed_photo_lifts_shadows() {
        // Two thirds of the frame sits below the shadow threshold.
        let mut samples = flat(0.02, 400);
        samples.extend(ramp(0.4, 0.9, 200));
        let adj = analyze(&samples, PixelFormat::Srgb8);
        assert!(adj.shadows > 0.0, "shadows {}", adj.shadows);
    }

    #[test]
    fn an_empty_sample_is_identity() {
        assert!(analyze(&[], PixelFormat::Srgb8).is_identity());
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
    fn every_slider_stays_inside_its_range() {
        for (lo, hi) in [(0.0, 0.02), (0.98, 1.0), (0.49, 0.51), (0.0, 1.0)] {
            let adj = analyze(&ramp(lo, hi, 256), PixelFormat::Srgb8);
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
