// SPDX-License-Identifier: MIT OR Apache-2.0

//! Best-of-burst derivation. Given a per-entry burst grouping and per-entry
//! sharpness scores, decide which frame of each burst is the "best" (sharpest)
//! and which are its siblings. Pure and total so it can be unit-tested without
//! any UI, filesystem, or decode state.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use crate::navigation::group_by_time;

/// Max gap between consecutive shots for them to count as one burst. Fixed at
/// the typical camera burst-mode cadence; no UI knob (YAGNI).
pub const BURST_GAP: Duration = Duration::from_secs(2);

/// How a single frame relates to its burst. Absent (`None` in the output)
/// means the frame is a singleton (its group has size 1) — never badged.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BurstMark {
    /// The sharpest known frame of a burst of 2+ frames.
    Best,
    /// A non-best member of a burst of 2+ frames.
    Sibling,
}

/// Strict "a is a better score than b": a real score beats no score; two real
/// scores compare numerically. Used so ties and unscored frames keep the
/// earliest member as the provisional winner.
fn score_gt(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x > y,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// Mark each entry given its burst `group_ids` and optional sharpness `scores`
/// (parallel to `group_ids`). Groups of size 1 → `None`. In a group of 2+, the
/// member with the strictly-highest known score is `Best`, the rest `Sibling`;
/// with all scores tied or unknown, the earliest member is the provisional
/// `Best`. Output is 1:1 with `group_ids`.
pub fn compute_marks(group_ids: &[u32], scores: &[Option<f64>]) -> Vec<Option<BurstMark>> {
    let score_at = |i: usize| scores.get(i).copied().flatten();

    // Group sizes.
    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for &g in group_ids {
        *sizes.entry(g).or_insert(0) += 1;
    }

    // Winner index per group: first member seen, replaced only on a strictly
    // greater score (so ties/None keep the earliest).
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

/// Fold a frame's eye state into its sharpness score, *before* marking.
///
/// [`compute_marks`] only knows how to pick the highest score, and keeping it
/// that way is deliberate — every new culling signal folds in here instead of
/// growing another parameter on the marking logic.
///
/// A blink is a hard demotion, not a tiebreak: sharpness is a variance, so it
/// is never negative, which means mapping a closed-eyed frame into `(-1, 0)`
/// puts it strictly below every open-eyed or unjudged frame no matter how much
/// sharper it is. Within that band the mapping stays monotonic in sharpness, so
/// a burst where *everyone* blinked still promotes its sharpest frame rather
/// than falling back on shot order.
///
/// Unknown eyes (no face found, profile shot, analysis still running) leave the
/// score untouched — never treated as closed. One asymmetry to know about: a
/// frame with a *known* blink still outranks a frame with no sharpness score at
/// all, because [`compute_marks`] ranks any score above none. That only shows up
/// mid-scan, and resolves as soon as the missing score lands.
pub fn combined_score(sharpness: Option<f64>, eyes: Option<crate::facequality::EyeState>) -> Option<f64> {
    let s = sharpness?;
    match eyes {
        Some(crate::facequality::EyeState::Closed) => Some(-1.0 / (1.0 + s.max(0.0))),
        _ => Some(s),
    }
}

/// Convenience: group `times` by `gap` (via `group_by_time`) then mark. This is
/// the entry point the app uses each time capture times or scores change.
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
        // The blinking frame is four times sharper and must still lose.
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
        // A frame nobody could judge must not be demoted below one that blinked.
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
        // The demotion relies on sharpness never being negative; a flat frame
        // scoring exactly 0.0 is the boundary case.
        let blink = combined_score(Some(0.0), Some(EyeState::Closed)).unwrap();
        let open = combined_score(Some(0.0), Some(EyeState::Open)).unwrap();
        assert!(blink < 0.0 && blink < open, "blink={blink} open={open}");
    }

    #[test]
    fn combined_scores_are_None_for_unscored_frames_regardless_of_eyes() {
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
        // group 0: idx 0,1 (best = 1); group 1: idx 2 (singleton);
        // group 2: idx 3,4 (best = 4, since idx3 is unscored).
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
        // 0s,1s = burst A; 10s,11s = burst B (9s jump splits).
        let times = [t(0), t(1), t(10), t(11)];
        // In A, idx1 sharper; in B, idx2 sharper.
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
