//! Auto Tone: choose Develop slider values from a photo's own histogram.
//!
//! The structure follows RapidRAW's `perform_auto_analysis` — downscale, build
//! a luma histogram, read percentiles and clipping fractions off it, then set
//! sliders — but none of its constants transfer. Its tone operators are
//! different from ours (its Blacks is a masked lift near black, ours is a
//! global linear remap; its sliders divide by five different scales, ours all
//! divide by 100), so a value copied across would mean something else entirely.
//!
//! Instead, the sliders that have a target worth hitting are *solved* against
//! our own pipeline rather than guessed:
//!
//! - **Exposure** is bisected so the median lands on mid-gray.
//! - **Blacks/Whites** are bisected against the output endpoint percentiles.
//! - **Contrast** is bisected to stretch what the endpoints could not, and
//!   stays at zero whenever they already reached the target width.
//!
//! The remaining three answer taste questions with no target to solve for, so
//! they keep RapidRAW's heuristic shape and are frankly guesses: Highlights and
//! Shadows key off how much of the frame is clipped or crushed, and Vibrance
//! off mean saturation. Expect to retune those by eye; they are marked below.
//!
//! Everything is evaluated through [`develop::apply_linear`] (or
//! `apply_raw_display` for RAW), never a reimplementation of the tone math, so
//! this module cannot drift out of sync with what the shader draws.

use crate::develop::{self, Adjustments, EXPOSURE_RANGE, TONE_RANGE};
use crate::image_decode::PixelFormat;

/// Number of histogram bins. Matches the 8-bit display domain the percentile
/// thresholds below are written in.
const BINS: usize = 256;

/// Where the median should land, in display space. Mid-gray.
const TARGET_MEDIAN: f32 = 0.5;
/// Where the 1st and 99th percentiles should land. Inset from true black and
/// white on purpose: aiming at exactly 0 and 1 would crush one percent of the
/// frame and blow another percent by definition, which is a stronger stretch
/// than any photo asks for.
const TARGET_BLACK: f32 = 0.01;
const TARGET_WHITE: f32 = 0.99;
/// Target spread between the 1st and 99th percentile, in display space. Below
/// this the image reads as flat and Contrast is brought in to help.
const TARGET_RANGE: f32 = 220.0 / 255.0;
/// Ceiling on the Contrast that Auto Tone will add. Contrast here is an assist
/// for what the endpoints could not stretch on their own, and the endpoints
/// clamp often enough that an uncapped solve would sit at +100 on any flat
/// photo. A capped assist is the honest version of what this slider is for.
const MAX_AUTO_CONTRAST: f32 = 50.0;
/// Coordinate-descent passes over the solved sliders. Each one moves the
/// others, so a single pass leaves the earlier ones stale; three is enough for
/// the values to settle well inside the 1e-3 that `edit_signature` quantizes to.
const REFINE_PASSES: u32 = 3;

/// A pixel at or above this is treated as a blown highlight.
const HIGHLIGHT_LEVEL: f32 = 240.0 / 255.0;
/// A pixel at or above this is treated as fully clipped.
const CLIPPED_LEVEL: f32 = 250.0 / 255.0;
/// A pixel at or below this is treated as crushed shadow.
const SHADOW_LEVEL: f32 = 32.0 / 255.0;
/// White point above which the photo is considered already hot, so Exposure is
/// not allowed to add any more light.
const HOT_WHITE_POINT: f32 = 245.0 / 255.0;
/// Fraction of blown pixels past which the photo counts as highlight-heavy.
const HIGHLIGHT_FRACTION: f32 = 0.02;
/// Fraction of fully-clipped pixels past which the photo counts as hot.
const CLIPPED_FRACTION: f32 = 0.005;
/// Fraction of crushed pixels past which Shadows starts lifting.
const SHADOW_FRACTION: f32 = 0.05;

/// Bisection steps for each solved slider. 24 halvings take a ±100 slider
/// below 1e-5, far past what `edit_signature` quantizes to.
const SOLVE_STEPS: u32 = 24;

/// Bisection steps used to seat each bin's representative colour on that bin's
/// own display level (see [`fit_to_level`]). Fewer than `SOLVE_STEPS` because
/// the factor being searched for is always just under 1.
const FIT_STEPS: u32 = 16;

/// The histogram of one photo, in the display space the user actually sees.
///
/// `lin` is the piece that makes the solve cheap: for each display-luma bin it
/// keeps one representative *linear-light colour*, so a candidate
/// `Adjustments` can be evaluated by pushing 256 pixels through the real
/// pipeline instead of every pixel in the photo. Representatives are re-sorted by
/// their output luma for every candidate: channel-wise monotonic operators do
/// not preserve the relative display brightness of different colours.
///
/// The representative has to be a colour, not a scalar: bins are keyed by
/// *display* luma, and a neutral grey carrying only the bin's linear luma
/// does not land back on that display luma once a channel is saturated. Pure
/// red shows at 0.299, but a grey of its linear luma comes back at ~0.578 —
/// brighter than a mid-grey pixel that sorts above it. Standing in a grey
/// therefore left the solve chasing statistics the photo did not have, and
/// got worse the more saturated the photo was.
struct Histogram {
    /// Pixel count per display-luma bin.
    count: [f32; BINS],
    /// The bin's linear-light stand-in: its pixels' mean colour, scaled by
    /// [`fit_to_level`] to read back at the bin's own display luma. Black
    /// where the bin is empty, which no percentile can land on.
    lin: [[f32; 3]; BINS],
    total: f32,
    /// Mean saturation, `(max - min) / max`, over display-space pixels.
    mean_sat: f32,
    format: PixelFormat,
}

/// Rec.601 luma, matching the weighting the gamma-space vibrance block in
/// `develop.rs` uses. This runs on display-space values, so it is deliberately
/// not the Rec.709 set `filmic_exposure` uses on linear light.
fn luma(px: [f32; 3]) -> f32 {
    0.299 * px[0] + 0.587 * px[1] + 0.114 * px[2]
}

/// Push one linear-light pixel through the real tone pipeline and return the
/// display-space colour — the same transform `recompute_histogram` applies
/// before binning, so the analysis sees exactly what the Develop panel plots.
fn display(adj: &Adjustments, format: PixelFormat, px: [f32; 3]) -> [f32; 3] {
    match format {
        // `apply_linear` returns linear light; the panel gamma-encodes it.
        PixelFormat::Srgb8 => {
            let out = develop::apply_linear(adj, px);
            [
                out[0].max(0.0).powf(1.0 / 2.2),
                out[1].max(0.0).powf(1.0 / 2.2),
                out[2].max(0.0).powf(1.0 / 2.2),
            ]
        }
        // `apply_raw_display` already returns raw_shader.wgsl's display value.
        PixelFormat::LinearF16 => develop::apply_raw_display(adj, px),
    }
}

/// Display-space luma of one linear-light pixel under `adj` — the quantity
/// every bin is keyed by and every target is written in.
fn respond(adj: &Adjustments, format: PixelFormat, linear: [f32; 3]) -> f32 {
    luma(display(adj, format, linear))
}

/// Scale `px` until the identity pipeline puts it back at display luma
/// `level`, leaving its hue alone.
///
/// A bin's mean linear colour carries the right hue but not quite the right
/// brightness. Display luma is gamma-encoded, and encoding is concave, so the
/// encoded mean sits *above* the mean of what was encoded — by as much as 0.08
/// on a bin that mixes a saturated pixel with a neutral one of the same
/// apparent brightness. Left uncorrected that is a systematic over-read of
/// every percentile, which stops Exposure short of its target. One scalar per
/// bin removes it, and the bisection is cheap because the answer is always
/// just under 1.
fn fit_to_level(format: PixelFormat, px: [f32; 3], level: f32) -> [f32; 3] {
    let identity = Adjustments::default();
    let scaled = |k: f32| [px[0] * k, px[1] * k, px[2] * k];
    let (mut lo, mut hi) = (0.0f32, 4.0f32);
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
    /// Bin a downscaled linear-light sample.
    fn build(samples: &[[f32; 3]], format: PixelFormat) -> Option<Histogram> {
        if samples.is_empty() {
            return None;
        }
        let identity = Adjustments::default();
        let mut count = [0f32; BINS];
        let mut lin_sum = [[0f32; 3]; BINS];
        let mut sat_sum = 0f32;

        for &px in samples {
            // Display-space colour, via the identity pipeline for this format.
            let shown = display(&identity, format, px);
            let v = luma(shown).clamp(0.0, 1.0);
            let bin = ((v * (BINS - 1) as f32).round() as usize).min(BINS - 1);
            count[bin] += 1.0;
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
                lin[i] = fit_to_level(format, mean, i as f32 / (BINS - 1) as f32);
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

    /// Display-space value of the pixel at cumulative fraction `p`, under `adj`.
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

/// Bisect `field` over `range` so that `measure` hits `target`.
///
/// `measure` must be non-decreasing in the field, which every tone operator
/// here is. Returns the low end of the range when even that overshoots, and
/// the high end when it cannot reach.
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

/// Analyze a downscaled linear-light RGB sample and return the adjustments Auto
/// Tone would apply. `format` says which display pipeline the sample feeds.
///
/// Returns identity for an empty or degenerate sample.
pub(crate) fn analyze(samples: &[[f32; 3]], format: PixelFormat) -> Adjustments {
    let Some(hist) = Histogram::build(samples, format) else {
        return Adjustments::default();
    };

    // Measured off the untouched photo, so the clipping guards below describe
    // what the camera actually recorded rather than what we just did to it.
    let highlight_fraction = hist.fraction_above(HIGHLIGHT_LEVEL);
    let clipped_fraction = hist.fraction_above(CLIPPED_LEVEL);
    let shadow_fraction = hist.fraction_below(SHADOW_LEVEL);
    let identity = Adjustments::default();
    let white_point = hist.percentile(0.99, &identity);

    // A photo that is already hot gets no more light, however dark its median.
    // Blowing a sky further is never the right answer, and this is the one
    // guard of RapidRAW's that transfers unchanged: it is a statement about the
    // photo, not about either app's tone curve.
    let hot = white_point > HOT_WHITE_POINT
        || highlight_fraction > HIGHLIGHT_FRACTION
        || clipped_fraction > CLIPPED_FRACTION;

    let mut adj = Adjustments::default();

    // Coordinate descent: solve each slider against the *finished* pipeline
    // with the others held where they currently sit, then go round again.
    // Ordering inside a pass follows the pipeline (exposure, endpoints,
    // contrast) so the first pass starts from a sensible place, but nothing
    // depends on that ordering being right — the repeats are what make it
    // converge, and each measurement is of the real output either way.
    let exposure_range = if hot {
        *EXPOSURE_RANGE.start()..=0.0
    } else {
        EXPOSURE_RANGE
    };
    for _ in 0..REFINE_PASSES {
        // Exposure: median to mid-gray.
        solve(
            &mut adj,
            exposure_range.clone(),
            TARGET_MEDIAN,
            |a| &mut a.exposure,
            |a| hist.percentile(0.50, a),
        );
        // Endpoints: 1st and 99th percentile onto the target black and white.
        // Both are monotonic in their slider near the end they control.
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
        // Contrast: stretch whatever spread the endpoints could not reach,
        // capped. Left at zero outright when they already got there, so a
        // well-exposed photo comes back with this slider untouched.
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
            // More contrast on a frame that is already blowing mostly buys
            // more blown pixels.
            if highlight_fraction > HIGHLIGHT_FRACTION {
                adj.contrast *= 0.5;
            }
        }
    }

    // 5. Taste, not measurement. These three keep RapidRAW's heuristic shape
    //    because there is no target to solve them against; the constants are
    //    eyeball values and should be treated as such.
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

/// Overlay the sliders Auto Tone owns onto `base`, leaving everything else —
/// crop, white balance, saturation, denoise — exactly as the user set it.
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

    /// A synthetic photo: `n` neutral samples whose display-space luma sweeps
    /// linearly from `lo` to `hi`. Values are returned in linear light, which
    /// is what `analyze` consumes.
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

    /// Display-space median of a sample under `adj`, the thing Auto Tone aims
    /// at. Mirrors what the Develop panel's histogram would show.
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
        // Already spans the full range with its median on mid-gray, so there
        // is nothing for the solved sliders to do.
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
        // And it actually lands near the target it was solving for.
        let median = median_under(&dark, &adj);
        assert!(
            (median - TARGET_MEDIAN).abs() < 0.1,
            "median after auto: {median}"
        );
    }

    #[test]
    fn an_overexposed_photo_is_never_brightened() {
        // Mostly blown: the hot guard must hold exposure at or below zero even
        // though the median sits above mid-gray and wants pulling down.
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
        // A foggy frame: nothing near black, nothing near white. Blacks should
        // crush down and Whites should push up.
        let fogged = ramp(0.35, 0.62, 512);
        let adj = analyze(&fogged, PixelFormat::Srgb8);
        assert!(adj.blacks < -20.0, "blacks {}", adj.blacks);
        assert!(adj.whites > 20.0, "whites {}", adj.whites);
    }

    /// The nth-percentile display value of a sample under `adj` — what the
    /// solver is actually aiming at, measured over real pixels rather than the
    /// binned stand-in the solver uses.
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
        // The whole point of solving rather than guessing: on a photo whose
        // endpoints are reachable inside the slider range, all three targets
        // should actually be hit, not merely approached.
        let photo = ramp(0.12, 0.78, 512);
        let adj = analyze(&photo, PixelFormat::Srgb8);
        let median = percentile_under(&photo, &adj, 0.50);
        let black = percentile_under(&photo, &adj, 0.01);
        let white = percentile_under(&photo, &adj, 0.99);
        assert!((median - TARGET_MEDIAN).abs() < 0.06, "median {median}");
        assert!((black - TARGET_BLACK).abs() < 0.06, "black point {black}");
        assert!((white - TARGET_WHITE).abs() < 0.06, "white point {white}");
    }

    /// A saturated photo: neutral pixels mixed with strongly coloured ones,
    /// both sweeping the same display range. Four hues in rotation so no
    /// single channel can accidentally agree with the luma weights.
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

    /// The invariant everything else rests on: bins are keyed by *display*
    /// luma, so a bin's stored representative has to come back out of the
    /// pipeline at that same luma. The grayscale tests cannot see this — for a
    /// neutral pixel any stand-in carrying its linear luma is already exact.
    /// A coloured one is not: pure red shows at 0.299 but a grey of its linear
    /// luma reads ~0.578, brighter than a mid-grey pixel that sorts below it.
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

    /// And the consequence of that invariant: the same targets the grayscale
    /// solve hits must be hit on a saturated photo too. Read off real pixels,
    /// not the binned stand-in, so a self-consistent-but-wrong histogram
    /// cannot pass. This used to leave the median at ~0.15 instead of 0.5.
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

    /// `saturated`'s counterpart in the linear camera-RGB domain that
    /// `raw/preview.rs` hands back, with no sRGB encode applied.
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

    /// The nth-percentile display value of a RAW sample under `adj`, measured
    /// over real pixels rather than the binned stand-in. `percentile_under`'s
    /// counterpart for the pipeline `raw_shader.wgsl` draws.
    fn raw_percentile_under(samples: &[[f32; 3]], adj: &Adjustments, p: f32) -> f32 {
        let mut out: Vec<f32> = samples
            .iter()
            .map(|&px| luma(develop::apply_raw_display(adj, px)))
            .collect();
        out.sort_by(|a, b| a.partial_cmp(b).unwrap());
        out[((out.len() as f32 * p) as usize).min(out.len() - 1)]
    }

    /// Sweep `field` across `range` and assert `measure` never goes backwards.
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

    /// `solve` bisects, which is only valid while each measure is
    /// non-decreasing in the slider it is solving. Every tone operator here is,
    /// but the RAW display pipeline is a different curve from the sRGB one —
    /// an sRGB encode, a brightening power, and a smoothstep contrast lift, all
    /// clamped — and nothing checked that it keeps the promise. Contrast is
    /// checked against the spread, because that is what the solver aims it at;
    /// an individual bin is deliberately *not* monotonic in contrast.
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

    /// The bin invariant again, on the RAW pipeline. `apply_raw_display` clamps
    /// its own output, so this also pins down that the clamp does not strand a
    /// bin's representative somewhere other than the level it stands for.
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

    /// And the whole solve end to end on the RAW pipeline, which until now had
    /// no coverage at all: it is dead code on native macOS (ImageIO hands back
    /// sRGB), but it is what the browser build's Loupe RAW tier runs on.
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
        // A photo already spanning the range needs no help, and a Contrast
        // nudge on top of a good stretch is exactly the over-cooking this
        // slider's cap exists to avoid.
        assert_eq!(
            analyze(&ramp(0.0, 1.0, 512), PixelFormat::Srgb8).contrast,
            0.0
        );
    }

    #[test]
    fn contrast_is_capped() {
        // Even a photo far too flat for the endpoints to rescue must not pin
        // Contrast at the slider maximum.
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
