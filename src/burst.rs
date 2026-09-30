// SPDX-License-Identifier: GPL-3.0-or-later

use std::time::Duration;

/// Max gap between consecutive shots in one burst.
pub const BURST_GAP: Duration = Duration::from_secs(2);

/// Strictly better: any score beats no score. Strictness keeps the earliest
/// frame as the winner on ties.
fn score_gt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Index of the best frame in `scores`: the highest score, with ties and
/// missing scores going to the earliest frame. `0` for an empty slice.
pub fn best_index(scores: &[Option<f64>]) -> usize {
    let mut best = 0;
    for (i, &s) in scores.iter().enumerate().skip(1) {
        if score_gt(s, scores[best]) {
            best = i;
        }
    }
    best
}

/// Sharpness is never negative, so a blink maps to `(-1, 0)`: below every
/// open or unknown frame, but still ordered by sharpness if everyone blinked.
/// Unknown eyes leave the score alone. A blinking frame still beats a frame
/// with no sharpness score yet, until that score arrives.
pub fn combined_score(
    sharpness: Option<f64>,
    eyes: Option<crate::facequality::EyeState>,
) -> Option<f64> {
    let s = sharpness?;
    match eyes {
        Some(crate::facequality::EyeState::Closed) => Some(-1.0 / (1.0 + s.max(0.0))),
        _ => Some(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::facequality::EyeState;

    #[test]
    fn a_blink_loses_to_a_blurrier_open_eyed_sibling() {
        let scores = [
            combined_score(Some(400.0), Some(EyeState::Closed)),
            combined_score(Some(100.0), Some(EyeState::Open)),
        ];
        assert_eq!(best_index(&scores), 1);
    }

    #[test]
    fn an_all_blinking_burst_still_prefers_its_sharpest_frame() {
        let scores = [
            combined_score(Some(10.0), Some(EyeState::Closed)),
            combined_score(Some(90.0), Some(EyeState::Closed)),
        ];
        assert_eq!(best_index(&scores), 1);
    }

    #[test]
    fn unknown_eyes_leave_the_score_untouched() {
        assert_eq!(combined_score(Some(12.5), None), Some(12.5));
        assert_eq!(combined_score(Some(12.5), Some(EyeState::Open)), Some(12.5));
        assert_eq!(combined_score(None, Some(EyeState::Open)), None);
        assert_eq!(combined_score(None, None), None);
        // Unknown eyes must rank above a blink.
        let scores = [
            combined_score(Some(1.0), None),
            combined_score(Some(500.0), Some(EyeState::Closed)),
        ];
        assert_eq!(best_index(&scores), 0);
    }

    #[test]
    fn a_blink_stays_below_zero_even_at_zero_sharpness() {
        // Boundary case: a flat frame scores exactly 0.0.
        let blink = combined_score(Some(0.0), Some(EyeState::Closed)).unwrap();
        let open = combined_score(Some(0.0), Some(EyeState::Open)).unwrap();
        assert!(blink < 0.0 && blink < open, "blink={blink} open={open}");
    }

    #[test]
    fn a_blink_with_no_sharpness_score_stays_unscored() {
        assert_eq!(combined_score(None, Some(EyeState::Closed)), None);
    }

    #[test]
    fn the_highest_score_wins() {
        assert_eq!(best_index(&[Some(1.0), Some(2.0)]), 1);
        assert_eq!(best_index(&[Some(1.0), Some(9.0), Some(3.0)]), 1);
    }

    #[test]
    fn an_all_unscored_burst_defaults_to_the_first_frame() {
        assert_eq!(best_index(&[None, None, None]), 0);
    }

    #[test]
    fn a_tie_keeps_the_earliest_frame() {
        assert_eq!(best_index(&[Some(5.0), Some(5.0)]), 0);
        assert_eq!(best_index(&[Some(1.0), Some(5.0), Some(5.0)]), 1);
    }

    #[test]
    fn a_scored_frame_beats_an_earlier_unscored_one() {
        assert_eq!(best_index(&[None, Some(1.0)]), 1);
        assert_eq!(best_index(&[None, None, Some(0.0), None]), 2);
    }

    #[test]
    fn an_unscored_frame_never_displaces_a_scored_one() {
        assert_eq!(best_index(&[Some(0.0), None]), 0);
    }

    #[test]
    fn an_empty_or_single_burst_picks_index_zero() {
        assert_eq!(best_index(&[]), 0);
        assert_eq!(best_index(&[None]), 0);
    }
}
