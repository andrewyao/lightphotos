//! Auto Tone: pick Develop slider values from a photo's histogram, the way a
//! person does it by hand.
//!
//! Exposure moves the median to a little above the middle. Highlights then
//! come down and Shadows come up until neither end of the histogram piles up
//! against its wall. Those two moves shift the median, so the loop repeats.
//! Whites, Blacks and Contrast stay at zero.
//!
//! Slider values are solved by bisection against our real tone pipeline
//! ([`develop::Develop`], or `apply_raw_display` for RAW), so results
//! match what the shader draws.

use crate::decode::image_decode::PixelFormat;
use crate::develop::{self, Adjustments, EXPOSURE_RANGE, TONE_RANGE};

/// One bin per 8-bit display level.
const BINS: usize = 256;

/// Where Exposure puts the median: a little over the middle.
const TARGET_MEDIAN: f32 = 0.55;

/// A pile-up is more than [`END_PILE_FRACTION`] of the pixels past one of
/// these display levels, the outer ~6% of each end. The band is that wide
/// because Exposure's highlight shoulder squeezes a lifted bright patch to
/// just under white instead of onto it, so a narrower band misses the pile.
const TOP_LEVEL: f32 = 240.0 / 255.0;
const BOTTOM_LEVEL: f32 = 15.0 / 255.0;
const END_PILE_FRACTION: f32 = 0.01;

/// Passes over the loop. Highlights and Shadows shift the median, so one pass
/// isn't enough. Four settle well inside `edit_signature`'s 1e-3 rounding.
const REFINE_PASSES: u32 = 4;

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
    // Unframed, so Vignette and Grain stay out and the position is unused.
    develop::Develop::new(adj).display_at(format, px, 0.5, 0.5, None)
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

    /// Fraction of pixels at or above display level `level`, unedited.
    fn fraction_above(&self, level: f32) -> f32 {
        let first = ((level * (BINS - 1) as f32).ceil() as usize).min(BINS - 1);
        self.count[first..].iter().sum::<f32>() / self.total
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

/// Auto Tone adjustments for a downscaled linear-light sample. `format` picks
/// the display pipeline. An empty sample returns the defaults.
#[hotpath::measure]
pub(crate) fn analyze(samples: &[[f32; 3]], format: PixelFormat) -> Adjustments {
    let Some(hist) = Histogram::build(samples, format) else {
        return Adjustments::default();
    };

    let median = |a: &Adjustments| hist.percentile(0.50, a);
    // Past this level sits more than END_PILE_FRACTION of the frame, so the
    // top of the histogram has a bump.
    let top = |a: &Adjustments| hist.percentile(1.0 - END_PILE_FRACTION, a);
    let bottom = |a: &Adjustments| hist.percentile(END_PILE_FRACTION, a);
    // Highlights that bottom out can't clear a bump the photo already had, so
    // Exposure backs off only to the original's top, never below it.
    let original_top = hist.fraction_above(TOP_LEVEL).max(END_PILE_FRACTION);
    let top_at_original = |a: &Adjustments| hist.percentile(1.0 - original_top, a);

    let mut adj = Adjustments::default();
    for _ in 0..REFINE_PASSES {
        solve(
            &mut adj,
            EXPOSURE_RANGE,
            TARGET_MEDIAN,
            |a| &mut a.exposure,
            median,
        );

        adj.highlights = 0.0;
        hold_below(
            &mut adj,
            *TONE_RANGE.start()..=0.0,
            TOP_LEVEL,
            |a| &mut a.highlights,
            top,
        );
        hold_below(
            &mut adj,
            EXPOSURE_RANGE,
            TOP_LEVEL,
            |a| &mut a.exposure,
            top_at_original,
        );

        adj.shadows = 0.0;
        hold_above(
            &mut adj,
            0.0..=*TONE_RANGE.end(),
            BOTTOM_LEVEL,
            |a| &mut a.shadows,
            bottom,
        );
    }

    // A hand-picked rule with an eyeballed constant. Retune by eye.
    if hist.mean_sat < 0.2 {
        adj.vibrance = ((0.2 - hist.mean_sat) * 120.0).min(*TONE_RANGE.end());
    }

    adj.exposure = adj
        .exposure
        .clamp(*EXPOSURE_RANGE.start(), *EXPOSURE_RANGE.end());
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
                [develop::srgb_decode(display); 3]
            })
            .collect()
    }

    fn flat(display: f32, n: usize) -> Vec<[f32; 3]> {
        vec![[develop::srgb_decode(display); 3]; n]
    }

    /// A dark pile with a long tail toward bright: display luma
    /// `top * t^steep`. The larger `steep`, the tighter the pile.
    fn dark_pile(top: f32, steep: i32, n: usize) -> Vec<[f32; 3]> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                [develop::srgb_decode(top * t.powi(steep)); 3]
            })
            .collect()
    }

    /// The mirror of [`dark_pile`]: a bright pile with a tail toward dark.
    fn bright_pile(bottom: f32, steep: i32, n: usize) -> Vec<[f32; 3]> {
        (0..n)
            .map(|i| {
                let t = i as f32 / (n - 1) as f32;
                [develop::srgb_decode(1.0 - (1.0 - bottom) * t.powi(steep)); 3]
            })
            .collect()
    }

    /// Display luma at percentile `p`, measured over every pixel instead of the
    /// histogram's stand-ins.
    fn percentile_under(samples: &[[f32; 3]], adj: &Adjustments, p: f32) -> f32 {
        let dev = develop::Develop::new(adj);
        let mut out: Vec<f32> = samples
            .iter()
            .map(|&px| luma(dev.linear_at(px, 0.0, 0.0).map(develop::srgb_encode)))
            .collect();
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out[((out.len() as f32 * p) as usize).min(out.len() - 1)]
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
                h.map(|c| develop::srgb_decode(display * c))
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

    /// A `TonedF16` RAW already carries Apple's curve, so Auto Tone reads it
    /// as it reads an `Srgb8` photo of the same linear values.
    #[test]
    fn a_toned_half_float_photo_analyzes_as_srgb8_does() {
        let photo: Vec<[f32; 3]> = (0..400)
            .map(|i| {
                let v = 0.02 + 0.5 * (i as f32 / 400.0).powi(2);
                [v, v * 0.9, v * 0.7]
            })
            .collect();
        let srgb = analyze(&photo, PixelFormat::Srgb8);
        assert_eq!(analyze(&photo, PixelFormat::TonedF16), srgb);
        assert_ne!(analyze(&photo, PixelFormat::LinearF16), srgb);
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

    /// `saturated` in linear camera RGB, as `decode::rawler::raw_preview` returns it.
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
    /// RAW display curve differs from sRGB, so check it.
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
            "highlights/99th pct",
            TONE_RANGE,
            |a| &mut a.highlights,
            |a| hist.percentile(1.0 - END_PILE_FRACTION, a),
        );
        assert_monotonic(
            "shadows/1st pct",
            TONE_RANGE,
            |a| &mut a.shadows,
            |a| hist.percentile(END_PILE_FRACTION, a),
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

    #[test]
    fn an_underexposed_raw_photo_is_lifted_to_the_target_median() {
        let dark = raw_saturated(0.002, 0.30, 512);
        let adj = analyze(&dark, PixelFormat::LinearF16);
        assert!(adj.exposure > 0.5, "expected a lift, got {}", adj.exposure);
        let median = raw_percentile_under(&dark, &adj, 0.50);
        assert!((median - TARGET_MEDIAN).abs() < 0.06, "median {median}");
    }

    #[test]
    fn a_desaturated_photo_gets_vibrance() {
        let gray = ramp(0.1, 0.9, 512);
        assert!(analyze(&gray, PixelFormat::Srgb8).vibrance > 0.0);
    }

    /// Fraction of pixels displaying at or above `level`, measured over every
    /// pixel instead of the histogram's stand-ins.
    fn fraction_above_under(samples: &[[f32; 3]], adj: &Adjustments, level: f32) -> f32 {
        let above = (0..samples.len())
            .filter(|&i| percentile_under(&samples[i..=i], adj, 0.0) >= level)
            .count();
        above as f32 / samples.len() as f32
    }

    /// [`fraction_above_under`] from the other side.
    fn fraction_below_under(samples: &[[f32; 3]], adj: &Adjustments, level: f32) -> f32 {
        let below = (0..samples.len())
            .filter(|&i| percentile_under(&samples[i..=i], adj, 0.0) <= level)
            .count();
        below as f32 / samples.len() as f32
    }

    /// Slack for the 8-bit bins and the downscaled stand-ins.
    const MEDIAN_TOLERANCE: f32 = 0.05;

    #[test]
    fn a_dark_photo_is_lifted_to_the_target_median() {
        let photo = dark_pile(0.9, 3, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert!(adj.exposure > 0.0, "exposure {}", adj.exposure);
        let median = percentile_under(&photo, &adj, 0.50);
        assert!(
            (median - TARGET_MEDIAN).abs() < MEDIAN_TOLERANCE,
            "median {median} in {adj:?}"
        );
    }

    #[test]
    fn a_bright_photo_is_brought_down_to_the_target_median() {
        let photo = bright_pile(0.3, 2, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert!(adj.exposure < 0.0, "exposure {}", adj.exposure);
        let median = percentile_under(&photo, &adj, 0.50);
        assert!(
            (median - TARGET_MEDIAN).abs() < MEDIAN_TOLERANCE,
            "median {median} in {adj:?}"
        );
    }

    /// A photo with no wall at either end and its median already on target
    /// has nothing to fix.
    #[test]
    fn a_photo_without_pile_ups_keeps_highlights_and_shadows_at_zero() {
        let photo = ramp(0.15, 0.9, 1024);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert_eq!((adj.highlights, adj.shadows), (0.0, 0.0), "{adj:?}");
    }

    /// A dark photo with a bright patch. Lifting the median blows the patch,
    /// so Highlights must pull it back.
    #[test]
    fn a_lifted_photo_recovers_its_highlights() {
        let mut photo = ramp(0.0, 0.35, 480);
        photo.extend(ramp(0.7, 0.85, 32));
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert!(adj.exposure > 0.0, "exposure {}", adj.exposure);
        assert!(adj.highlights < 0.0, "{adj:?}");
        let blown = fraction_above_under(&photo, &adj, TOP_LEVEL);
        assert!(
            blown <= END_PILE_FRACTION + 0.01,
            "{blown} of the frame blown in {adj:?}"
        );
    }

    /// A wall at white that Highlights can clear is cleared.
    #[test]
    fn a_wall_at_white_is_pulled_off_with_highlights() {
        let mut photo = ramp(0.1, 0.7, 900);
        photo.extend(ramp(0.95, 1.0, 100));
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert!(adj.highlights < 0.0, "highlights {}", adj.highlights);
        let blown = fraction_above_under(&photo, &adj, TOP_LEVEL);
        assert!(
            blown <= END_PILE_FRACTION + 0.01,
            "{blown} of the frame blown in {adj:?}"
        );
    }

    /// A too-bright photo with dark detail. Cutting Exposure crushes the
    /// detail into a wall at black, so Shadows must lift it back.
    #[test]
    fn a_darkened_photo_lifts_its_shadows() {
        let mut photo = flat(0.02, 8);
        photo.extend(ramp(0.15, 0.18, 80));
        photo.extend(ramp(0.55, 0.97, 432));
        let adj = analyze(&photo, PixelFormat::Srgb8);
        assert!(adj.exposure < 0.0, "exposure {}", adj.exposure);
        assert!(adj.shadows > 0.0, "shadows {}", adj.shadows);
        let crushed = fraction_below_under(&photo, &adj, BOTTOM_LEVEL);
        assert!(
            crushed <= END_PILE_FRACTION + 0.01,
            "{crushed} of the frame crushed in {adj:?}"
        );
    }

    /// A dark scene with a sky that is already a wall of white. Highlights
    /// can't save it, so Exposure backs off instead of piling up more.
    #[test]
    fn exposure_never_leaves_the_top_worse_than_the_original() {
        let mut photo = ramp(0.0, 0.3, 700);
        photo.extend(flat(1.0, 300));
        let original = fraction_above_under(&photo, &Adjustments::default(), TOP_LEVEL);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        let after = fraction_above_under(&photo, &adj, TOP_LEVEL);
        assert!(
            after <= original + 0.01,
            "{after} blown against {original} before, in {adj:?}"
        );
    }

    #[test]
    fn highlights_only_go_down_shadows_only_go_up_and_the_rest_stay_off() {
        for (lo, hi) in [
            (0.0, 0.02),
            (0.98, 1.0),
            (0.0, 0.35),
            (0.7, 1.0),
            (0.0, 1.0),
        ] {
            let adj = analyze(&ramp(lo, hi, 256), PixelFormat::Srgb8);
            assert!(adj.highlights <= 0.0, "{lo}..{hi}: {adj:?}");
            assert!(adj.shadows >= 0.0, "{lo}..{hi}: {adj:?}");
            assert_eq!(
                (adj.whites, adj.blacks, adj.contrast),
                (0.0, 0.0, 0.0),
                "{lo}..{hi}"
            );
        }
    }

    /// Highlights and Shadows shift the median, and the loop's later passes
    /// bring it back.
    #[test]
    fn the_loop_settles_with_the_median_on_target() {
        let mut photo = ramp(0.0, 0.6, 700);
        photo.extend(flat(0.0, 100));
        photo.extend(ramp(0.9, 1.0, 200));
        let adj = analyze(&photo, PixelFormat::Srgb8);
        let median = percentile_under(&photo, &adj, 0.50);
        assert!(
            (median - TARGET_MEDIAN).abs() < MEDIAN_TOLERANCE,
            "median {median} in {adj:?}"
        );
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
