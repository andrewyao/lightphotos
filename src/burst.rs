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
