// SPDX-License-Identifier: GPL-3.0-or-later

//! Groups photos that look alike, anywhere in the folder. `burst.rs` groups by
//! capture time instead. dHash finds candidates, then Vision feature prints
//! split off false matches.

use std::collections::HashMap;

use crate::burst::BurstMark;
use crate::phash::hamming;

/// Max differing bits (of 64) for two dHashes to be duplicate candidates.
pub const DEFAULT_MAX_DISTANCE: u32 = 8;

/// A frame's role in a duplicate group of 2+. Same shape as `BurstMark`, but
/// separate because a photo can be in a burst and a duplicate group at once.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DuplicateMark {
    Best,
    Sibling,
}

/// dHash duplicate candidates, as a group id per entry. Any pair within
/// `max_distance` bits joins one group, and joins chain: if a~b and b~c, all
/// three share a group even when a and c are far apart.
/// [`refine_by_feature_print`] splits chains later. Entries without a hash
/// are never grouped. Hashes are added as they arrive, so a batch of k new
/// hashes costs O(k·n) instead of regrouping all n photos.
#[derive(Default)]
pub struct HashGroups {
    hashes: Vec<Option<u64>>,
    /// `(entry, hash)` for every hashed entry, contiguous for the scan in `add`.
    hashed: Vec<(usize, u64)>,
    parent: Vec<usize>,
    ids: Vec<u32>,
    max_distance: u32,
}

impl HashGroups {
    pub fn new(len: usize, max_distance: u32) -> Self {
        Self {
            hashes: vec![None; len],
            hashed: Vec::new(),
            parent: (0..len).collect(),
            ids: (0..len as u32).collect(),
            max_distance,
        }
    }

    /// A group id per entry, numbered in order of each group's first entry.
    pub fn ids(&self) -> &[u32] {
        &self.ids
    }

    /// Adds `(entry, hash)` pairs. Returns false if an entry already had a
    /// different hash: groups can't be split, so the caller must rebuild.
    pub fn add(&mut self, new: impl IntoIterator<Item = (usize, u64)>) -> bool {
        for (i, hash) in new {
            match self.hashes[i] {
                Some(h) if h == hash => continue,
                Some(_) => return false,
                None => {}
            }
            for k in 0..self.hashed.len() {
                let (j, hj) = self.hashed[k];
                if hamming(hash, hj) <= self.max_distance {
                    self.union(i, j);
                }
            }
            self.hashes[i] = Some(hash);
            self.hashed.push((i, hash));
        }
        let mut id_of_root: HashMap<usize, u32> = HashMap::new();
        for i in 0..self.parent.len() {
            let next = id_of_root.len() as u32;
            self.ids[i] = *id_of_root.entry(self.find(i)).or_insert(next);
        }
        true
    }

    fn find(&mut self, mut x: usize) -> usize {
        let mut root = x;
        while self.parent[root] != root {
            root = self.parent[root];
        }
        while self.parent[x] != root {
            let next = self.parent[x];
            self.parent[x] = root;
            x = next;
        }
        root
    }

    fn union(&mut self, a: usize, b: usize) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }
}

/// Max Vision feature-print distance from the group's anchor to stay a
/// duplicate. Vision documents no distance scale, so this value is a guess
/// that has not been tuned on real photos.
pub const DEFAULT_MAX_FEATURE_DISTANCE: f32 = 0.5;

/// Split dHash false matches out of `groups`. Each group's first member is
/// the anchor. A member farther than `max_distance` from the anchor moves to
/// a new singleton group. `distance` returning `None` (not computed yet)
/// leaves the member in place. Comparing to the anchor only costs one Vision
/// comparison per member instead of every pair.
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
            continue;
        }
        let a = *anchor_of.entry(g).or_insert(i);
        if a == i {
            continue;
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

/// Same rules as `burst::compute_marks`.
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

    fn group_by_hash(hashes: &[Option<u64>], max_distance: u32) -> Vec<u32> {
        let mut groups = HashGroups::new(hashes.len(), max_distance);
        groups.add(
            hashes
                .iter()
                .enumerate()
                .filter_map(|(i, h)| Some((i, (*h)?))),
        );
        groups.ids().to_vec()
    }

    /// Connected components by brute force, ids in first-member order.
    fn reference_groups(hashes: &[Option<u64>], max_distance: u32) -> Vec<u32> {
        let n = hashes.len();
        let mut id = vec![u32::MAX; n];
        let mut next = 0;
        for start in 0..n {
            if id[start] != u32::MAX {
                continue;
            }
            id[start] = next;
            let mut stack = vec![start];
            while let Some(i) = stack.pop() {
                let Some(hi) = hashes[i] else { continue };
                for j in 0..n {
                    if id[j] == u32::MAX
                        && hashes[j].is_some_and(|hj| hamming(hi, hj) <= max_distance)
                    {
                        id[j] = next;
                        stack.push(j);
                    }
                }
            }
            next += 1;
        }
        id
    }

    #[test]
    fn adding_hashes_in_any_order_and_batches_matches_a_full_grouping() {
        let mut seed: u64 = 0x2545_F491_4F6C_DD1D;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for _ in 0..50 {
            let n = 60;
            // Few base hashes plus a few flipped bits, so chains and near misses occur.
            let bases: Vec<u64> = (0..4).map(|_| rand()).collect();
            let hashes: Vec<Option<u64>> = (0..n)
                .map(|_| {
                    if rand() % 7 == 0 {
                        return None;
                    }
                    let mut h = bases[(rand() % 4) as usize];
                    for _ in 0..(rand() % 12) {
                        h ^= 1 << (rand() % 64);
                    }
                    Some(h)
                })
                .collect();
            let expected = reference_groups(&hashes, DEFAULT_MAX_DISTANCE);
            assert_eq!(group_by_hash(&hashes, DEFAULT_MAX_DISTANCE), expected);

            let mut order: Vec<usize> = (0..n).collect();
            for i in (1..n).rev() {
                order.swap(i, (rand() % (i as u64 + 1)) as usize);
            }
            let mut groups = HashGroups::new(n, DEFAULT_MAX_DISTANCE);
            for batch in order.chunks(1 + (rand() % 9) as usize) {
                assert!(groups.add(batch.iter().filter_map(|&i| Some((i, hashes[i]?)))));
            }
            assert_eq!(groups.ids(), expected.as_slice());
        }
    }

    #[test]
    fn a_changed_hash_asks_for_a_rebuild() {
        let mut groups = HashGroups::new(2, DEFAULT_MAX_DISTANCE);
        assert!(groups.add([(0, 0)]));
        assert!(groups.add([(0, 0)]), "the same hash again is fine");
        assert!(!groups.add([(0, u64::MAX)]));
    }

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
        assert_eq!(group_by_hash(&[Some(0), Some(u64::MAX)], 8), vec![0, 1]);
    }

    #[test]
    fn within_threshold_joins_above_does_not() {
        let a = 0u64;
        let close = 0b1111u64;
        let far = u64::MAX;
        assert_eq!(
            group_by_hash(&[Some(a), Some(close), Some(far)], 8),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn unknown_hash_is_never_grouped_with_anything() {
        let hashes = [Some(0u64), None, None, Some(0u64)];
        let groups = group_by_hash(&hashes, 8);
        assert_eq!(groups[0], groups[3]);
        assert_ne!(groups[1], groups[2]);
        assert_ne!(groups[1], groups[0]);
        assert_ne!(groups[2], groups[0]);
    }

    #[test]
    fn transitive_chain_merges_into_one_group() {
        // a~b and b~c are within 4 bits, a~c is 8 apart. Only chaining joins a and c.
        let a = 0b0000_0000u64;
        let b = 0b0000_1111u64;
        let c = 0b1111_1111u64;
        let groups = group_by_hash(&[Some(a), Some(b), Some(c)], 4);
        assert_eq!(groups[0], groups[1]);
        assert_eq!(groups[1], groups[2]);
    }

    #[test]
    fn refine_splits_off_a_false_positive_member() {
        let groups = [0u32, 0, 0, 1];
        let refined = refine_by_feature_print(&groups, 0.5, |a, i| {
            assert_eq!(a, 0, "anchor should always be the group's first index");
            match i {
                1 => Some(0.1),
                2 => Some(0.9),
                _ => None,
            }
        });
        assert_eq!(refined[0], 0);
        assert_eq!(refined[1], 0);
        assert_ne!(refined[2], 0);
        assert_ne!(refined[2], refined[3]);
        assert_eq!(refined[3], 1);
    }

    #[test]
    fn refine_leaves_unknown_distances_in_place() {
        let groups = [0u32, 0];
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
