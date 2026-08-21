// SPDX-License-Identifier: GPL-3.0-or-later

//! Content-similarity duplicate grouping. Unlike `burst.rs` (which groups
//! strictly consecutive shots within a short time gap), this groups any
//! frames whose dHash is within `max_distance` of each other, regardless of
//! position in the list — catching near-duplicates taken further apart or
//! re-imported from multiple cards/cameras. Pure and total, mirroring
//! `burst.rs`'s testability.

use std::collections::HashMap;

use crate::burst::BurstMark;
use crate::phash::hamming;

/// Default Hamming-distance threshold (out of 64 bits) for two dHashes to be
/// considered candidates for the same duplicate group. Fixed, no UI knob
/// initially — same rationale as `burst::BURST_GAP`.
pub const DEFAULT_MAX_DISTANCE: u32 = 8;

/// How a single frame relates to its duplicate group. Absent (`None`) means
/// the frame is a singleton (its group has size 1, or its hash is unknown) —
/// never badged. Structurally identical to `BurstMark`, but kept as a
/// distinct type: a photo can be in both a time-burst and a content-duplicate
/// group at once, and those are separate underlying computations, unified
/// only at the UI badge layer.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DuplicateMark {
    /// The best-scoring (sharpest) known frame of a duplicate group of 2+.
    Best,
    /// A non-best member of a duplicate group of 2+.
    Sibling,
}

/// Group entries by dHash similarity via union-find: any pair within
/// `max_distance` (Hamming) joins the same group, and grouping is transitive
/// (single-linkage) — a chain of near-duplicates merges into one group, left
/// for the feature-print refinement pass to split apart if needed. Entries
/// with `None` (hash not yet computed) are never grouped with anything,
/// including each other. Output is 1:1 with `hashes`, group ids assigned in
/// first-appearance order (like `navigation::group_by_time`).
pub fn group_by_hash(hashes: &[Option<u64>], max_distance: u32) -> Vec<u32> {
    let n = hashes.len();
    let mut parent: Vec<usize> = (0..n).collect();

    fn find(parent: &mut [usize], x: usize) -> usize {
        if parent[x] != x {
            parent[x] = find(parent, parent[x]);
        }
        parent[x]
    }

    fn union(parent: &mut [usize], a: usize, b: usize) {
        let (ra, rb) = (find(parent, a), find(parent, b));
        if ra != rb {
            parent[rb] = ra;
        }
    }

    for i in 0..n {
        let Some(hi) = hashes[i] else { continue };
        for j in (i + 1)..n {
            let Some(hj) = hashes[j] else { continue };
            if hamming(hi, hj) <= max_distance {
                union(&mut parent, i, j);
            }
        }
    }

    let mut next_id = 0u32;
    let mut id_of_root: HashMap<usize, u32> = HashMap::new();
    (0..n)
        .map(|i| {
            let root = find(&mut parent, i);
            *id_of_root.entry(root).or_insert_with(|| {
                let id = next_id;
                next_id += 1;
                id
            })
        })
        .collect()
}

/// Default feature-print distance threshold for the Vision refinement pass —
/// at or below this, a dHash-candidate member is confirmed as a real
/// duplicate of its group's anchor; above it, it's split off as a dHash false
/// positive (similar gradient pattern, not actually the same content).
/// Vision doesn't document an absolute distance scale, so this is a
/// placeholder starting point, not an empirically-tuned constant — expect to
/// retune once tested against real near-duplicate / similar-but-different
/// photo pairs (see the plan's verification step).
pub const DEFAULT_MAX_FEATURE_DISTANCE: f32 = 0.5;

/// Refine dHash `groups` using a feature-print distance oracle. Within each
/// group of 2+, the first (lowest-index) member is the anchor; any other
/// member whose `distance(anchor_idx, member_idx)` exceeds `max_distance` is
/// split off into its own new singleton group id — a dHash false positive.
/// `distance` returning `None` (feature print not computed for that member
/// yet) leaves the member in its dHash group unchanged, refinement pending.
///
/// Deliberately anchor-relative rather than all-pairs: this only ever needs
/// one Vision comparison per non-anchor member (bounded cost, matching the
/// dHash-candidate-subset design), not a full pairwise sweep within the
/// group. `distance` is caller-supplied so this module stays decoupled from
/// the Vision FFI in `featureprint.rs` — pure and testable with a synthetic
/// oracle.
pub fn refine_by_feature_print(
    groups: &[u32],
    max_distance: f32,
    mut distance: impl FnMut(usize, usize) -> Option<f32>,
) -> Vec<u32> {
    let n = groups.len();
    let mut sizes: HashMap<u32, usize> = HashMap::new();
    for &g in groups {
        *sizes.entry(g).or_insert(0) += 1;
    }
    let mut anchor_of: HashMap<u32, usize> = HashMap::new();
    let mut next_id = groups.iter().copied().max().map_or(0, |m| m + 1);
    let mut out = groups.to_vec();
    for i in 0..n {
        let g = groups[i];
        if sizes.get(&g).copied().unwrap_or(0) < 2 {
            continue; // singleton, nothing to refine
        }
        let a = *anchor_of.entry(g).or_insert(i);
        if a == i {
            continue; // this member is the anchor itself
        }
        if let Some(d) = distance(a, i) {
            if d > max_distance {
                out[i] = next_id;
                next_id += 1;
            }
        }
    }
    out
}

/// Mark each entry given its duplicate-group `group_ids` and optional
/// `scores`, reusing `burst::compute_marks`'s group-size/tie-breaking logic
/// (it's generic over how the group ids were derived) and remapping its
/// output to `DuplicateMark`.
pub fn compute_marks(group_ids: &[u32], scores: &[Option<f64>]) -> Vec<Option<DuplicateMark>> {
    crate::burst::compute_marks(group_ids, scores)
        .into_iter()
        .map(|m| {
            m.map(|bm| match bm {
                BurstMark::Best => DuplicateMark::Best,
                BurstMark::Sibling => DuplicateMark::Sibling,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_and_single_unknown() {
        assert_eq!(group_by_hash(&[], 8), Vec::<u32>::new());
        assert_eq!(group_by_hash(&[None], 8), vec![0]);
    }

    #[test]
    fn identical_hashes_join_one_group() {
        assert_eq!(
            group_by_hash(&[Some(0), Some(0), Some(0)], 8),
            vec![0, 0, 0]
        );
    }

    #[test]
    fn far_apart_hashes_stay_separate() {
        // All 64 bits differ.
        assert_eq!(group_by_hash(&[Some(0), Some(u64::MAX)], 8), vec![0, 1]);
    }

    #[test]
    fn within_threshold_joins_above_does_not() {
        let a = 0u64;
        let close = 0b1111u64; // 4 bits differ, within threshold 8
        let far = u64::MAX; // 64 bits differ
        assert_eq!(
            group_by_hash(&[Some(a), Some(close), Some(far)], 8),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn unknown_hash_is_never_grouped_with_anything() {
        // Two identical known hashes group together; the two `None`s each get
        // their own private singleton group, even though they sit adjacent
        // in the list.
        let hashes = [Some(0u64), None, None, Some(0u64)];
        let groups = group_by_hash(&hashes, 8);
        assert_eq!(groups[0], groups[3]); // the two knowns share a group
        assert_ne!(groups[1], groups[2]); // the two unknowns don't share one
        assert_ne!(groups[1], groups[0]);
        assert_ne!(groups[2], groups[0]);
    }

    #[test]
    fn transitive_chain_merges_into_one_group() {
        // a~b (distance 4), b~c (distance 4), but a~c (distance 8) is exactly
        // at the threshold too here, so pick values where a~c would exceed it
        // on its own to prove transitivity is what joins them.
        let a = 0b0000_0000u64;
        let b = 0b0000_1111u64; // 4 bits from a
        let c = 0b1111_1111u64; // 8 bits from a directly, 4 bits from b
        let groups = group_by_hash(&[Some(a), Some(b), Some(c)], 4);
        assert_eq!(groups[0], groups[1]);
        assert_eq!(groups[1], groups[2]);
    }

    #[test]
    fn refine_splits_off_a_false_positive_member() {
        // Group 0 has three members (indices 0,1,2); group 1 is a singleton.
        let groups = [0u32, 0, 0, 1];
        // Anchor is index 0. Index 1 is a real duplicate (distance 0.1, under
        // threshold); index 2 is a dHash false positive (distance 0.9, over).
        let refined = refine_by_feature_print(&groups, 0.5, |a, i| {
            assert_eq!(a, 0, "anchor should always be the group's first index");
            match i {
                1 => Some(0.1),
                2 => Some(0.9),
                _ => None,
            }
        });
        assert_eq!(refined[0], 0); // anchor unchanged
        assert_eq!(refined[1], 0); // stays with the anchor
        assert_ne!(refined[2], 0); // split off
        assert_ne!(refined[2], refined[3]); // and not accidentally merged elsewhere
        assert_eq!(refined[3], 1); // untouched singleton group
    }

    #[test]
    fn refine_leaves_unknown_distances_in_place() {
        let groups = [0u32, 0];
        // distance() returns None (feature print not computed yet).
        let refined = refine_by_feature_print(&groups, 0.5, |_a, _i| None);
        assert_eq!(refined, vec![0, 0]);
    }

    #[test]
    fn refine_ignores_singleton_groups() {
        let groups = [0u32, 1, 2];
        let refined = refine_by_feature_print(&groups, 0.5, |_a, _i| {
            panic!("distance() should never be called for singleton groups")
        });
        assert_eq!(refined, groups);
    }

    #[test]
    fn compute_marks_picks_best_and_singletons_are_unmarked() {
        assert_eq!(
            compute_marks(&[0, 0, 1], &[Some(1.0), Some(2.0), Some(9.0)]),
            vec![
                Some(DuplicateMark::Sibling),
                Some(DuplicateMark::Best),
                None
            ]
        );
    }
}
