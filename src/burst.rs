// SPDX-License-Identifier: GPL-3.0-or-later

//! Picks the best frame of each burst from per-frame burst groups and scores.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::navigation::group_by_time;

/// Max gap between consecutive shots in one burst.
pub const BURST_GAP: Duration = Duration::from_secs(2);

/// A frame's role in a burst of 2+ frames. Single frames get `None`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BurstMark {
    Best,
    Sibling,
}

/// Strictly better: any score beats no score. Strictness keeps the earliest
/// frame as the winner on ties.
fn score_gt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// One mark per entry. `scores` is parallel to `group_ids`. In a group of 2+,
/// the highest score is `Best` and the rest are `Sibling`. Ties and missing
/// scores go to the earliest frame.
pub fn compute_marks(group_ids: &[u32], scores: &[Option<f64>]) -> Vec<Option<BurstMark>> {
    let score_at = |i: usize| scores.get(i).copied().flatten();

    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for &g in group_ids {
        *sizes.entry(g).or_insert(0) += 1;
    }

    let mut best: HashMap<u32, usize> = HashMap::new();
    for (i, &g) in group_ids.iter().enumerate() {
        match best.get(&g).copied() {
            None => {
                best.insert(g, i);
            }
            Some(bi) => {
                if score_gt(score_at(i), score_at(bi)) {
                    best.insert(g, i);
                }
            }
        }
    }

    group_ids
        .iter()
        .enumerate()
        .map(|(i, &g)| {
            if sizes.get(&g).copied().unwrap_or(0) < 2 {
                None
            } else if best.get(&g) == Some(&i) {
                Some(BurstMark::Best)
            } else {
                Some(BurstMark::Sibling)
            }
        })
        .collect()
}

/// Fold eye state into the sharpness score before [`compute_marks`]. New
/// culling signals belong here, so marking only ever compares one number.
///
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

/// Group by capture time, then mark. The app calls this whenever times or
/// scores change.
pub fn marks_for(
    times: &[Option<SystemTime>],
    scores: &[Option<f64>],
    gap: Duration,
) -> Vec<Option<BurstMark>> {
    let groups = group_by_time(times, gap);
    compute_marks(&groups, scores)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::facequality::EyeState;

    #[test]
    fn a_blink_loses_to_a_blurrier_open_eyed_sibling() {
        let scores = vec![
            combined_score(Some(400.0), Some(EyeState::Closed)),
            combined_score(Some(100.0), Some(EyeState::Open)),
        ];
        assert_eq!(
            compute_marks(&[0, 0], &scores),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn an_all_blinking_burst_still_prefers_its_sharpest_frame() {
        let scores = vec![
            combined_score(Some(10.0), Some(EyeState::Closed)),
            combined_score(Some(90.0), Some(EyeState::Closed)),
        ];
        assert_eq!(
            compute_marks(&[0, 0], &scores),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn unknown_eyes_leave_the_score_untouched() {
        assert_eq!(combined_score(Some(12.5), None), Some(12.5));
        assert_eq!(combined_score(Some(12.5), Some(EyeState::Open)), Some(12.5));
        assert_eq!(combined_score(None, Some(EyeState::Open)), None);
        assert_eq!(combined_score(None, None), None);
        // Unknown eyes must rank above a blink.
        let scores = vec![
            combined_score(Some(1.0), None),
            combined_score(Some(500.0), Some(EyeState::Closed)),
        ];
        assert_eq!(
            compute_marks(&[0, 0], &scores),
            vec![Some(BurstMark::Best), Some(BurstMark::Sibling)]
        );
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
    fn singleton_group_is_unmarked() {
        assert_eq!(compute_marks(&[0], &[None]), vec![None]);
        assert_eq!(
            compute_marks(&[0, 1, 2], &[Some(1.0), Some(2.0), Some(3.0)]),
            vec![None, None, None]
        );
    }

    #[test]
    fn two_member_burst_picks_higher_score() {
        assert_eq!(
            compute_marks(&[0, 0], &[Some(1.0), Some(2.0)]),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn all_unscored_burst_defaults_to_first() {
        assert_eq!(
            compute_marks(&[0, 0, 0], &[None, None, None]),
            vec![
                Some(BurstMark::Best),
                Some(BurstMark::Sibling),
                Some(BurstMark::Sibling)
            ]
        );
    }

    #[test]
    fn tie_keeps_earliest_as_best() {
        assert_eq!(
            compute_marks(&[0, 0], &[Some(5.0), Some(5.0)]),
            vec![Some(BurstMark::Best), Some(BurstMark::Sibling)]
        );
    }

    #[test]
    fn scored_member_beats_unscored_even_if_later() {
        assert_eq!(
            compute_marks(&[0, 0], &[None, Some(1.0)]),
            vec![Some(BurstMark::Sibling), Some(BurstMark::Best)]
        );
    }

    #[test]
    fn mixed_groups_are_independent() {
        let groups = [0u32, 0, 1, 2, 2];
        let scores = [Some(1.0), Some(9.0), Some(3.0), None, Some(4.0)];
        assert_eq!(
            compute_marks(&groups, &scores),
            vec![
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
                None,
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
            ]
        );
    }

    #[test]
    fn marks_for_groups_by_gap_then_marks() {
        let t = |s: u64| Some(SystemTime::UNIX_EPOCH + Duration::from_secs(s));
        let times = [t(0), t(1), t(10), t(11)];
        let scores = [Some(1.0), Some(2.0), Some(8.0), Some(3.0)];
        assert_eq!(
            marks_for(&times, &scores, Duration::from_secs(3)),
            vec![
                Some(BurstMark::Sibling),
                Some(BurstMark::Best),
                Some(BurstMark::Best),
                Some(BurstMark::Sibling),
            ]
        );
    }
}
