// SPDX-License-Identifier: GPL-3.0-or-later

//! Saved photo groups for one folder. A group is two or more photos shown as
//! one, its representative. This module is the pure model: it holds no UI,
//! decode or filesystem state. Every mutation computes the sidecar writes it
//! implies as [`GroupWrite`]s, and [`Groups::apply`] is the one place those
//! writes change the index, so memory and disk go through the same values.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::OsString;
use std::time::SystemTime;

use crate::hash::Fnv1a;

/// A group's name, which is also its sidecar's file stem, for example
/// `g-5f3a9c10e2`. Ordered as a string, which is what decides a photo two
/// sidecars both claim.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId(String);

impl GroupId {
    pub fn from_stem(stem: &std::ffi::OsStr) -> Option<GroupId> {
        let s = stem.to_str()?;
        (!s.is_empty()).then(|| GroupId(s.to_owned()))
    }

    #[allow(dead_code)] // only called from #[cfg(test)] today
    fn mint(group: &Group, at: SystemTime, taken: impl Fn(&GroupId) -> bool) -> GroupId {
        let nanos = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        for salt in 0u32.. {
            let mut h = Fnv1a::new();
            h.write(&nanos.to_le_bytes());
            for m in &group.members {
                h.write(m.as_encoded_bytes());
                h.write(&[0]);
            }
            h.write(&salt.to_le_bytes());
            let id = GroupId(format!("g-{:010x}", h.finish() & 0xff_ffff_ffff));
            if !taken(&id) {
                return id;
            }
        }
        unreachable!("a folder cannot hold 2^32 groups")
    }
}

impl std::fmt::Display for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Two or more distinct photos, by file name in folder order, and the one
/// that stands for them. [`Group::new`] is the only constructor, so a group
/// of one or a representative from outside the group cannot exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    members: Vec<OsString>,
    rep: OsString,
}

impl Group {
    pub fn new(members: Vec<OsString>, rep: OsString) -> Option<Group> {
        let distinct: HashSet<&OsString> = members.iter().collect();
        let valid =
            members.len() >= 2 && distinct.len() == members.len() && distinct.contains(&rep);
        valid.then_some(Group { members, rep })
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // the browser saves no groups yet
    pub fn members(&self) -> &[OsString] {
        &self.members
    }

    #[cfg_attr(target_arch = "wasm32", allow(dead_code))] // the browser saves no groups yet
    pub fn rep(&self) -> &OsString {
        &self.rep
    }

    fn retain(&self, keep: impl Fn(&OsString) -> bool) -> Option<Group> {
        let members: Vec<OsString> = self.members.iter().filter(|m| keep(m)).cloned().collect();
        let rep = if keep(&self.rep) {
            self.rep.clone()
        } else {
            members.first()?.clone()
        };
        Group::new(members, rep)
    }

    #[allow(dead_code)] // only called from #[cfg(test)] today
    fn same_members(&self, other: &Group) -> bool {
        self.members.len() == other.members.len()
            && self.members.iter().all(|m| other.members.contains(m))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupWrite {
    Put(GroupId, Group),
    Delete(GroupId),
}

/// Every group in one folder. `of` indexes `by_id` by member, which is what
/// holds a photo to at most one group.
#[derive(Default, Debug)]
pub struct Groups {
    by_id: BTreeMap<GroupId, Group>,
    of: HashMap<OsString, GroupId>,
}

#[allow(dead_code)] // only called from #[cfg(test)] today
impl Groups {
    /// The groups a folder's sidecars describe, reconciled against the
    /// folder's `listing` of file names. A member whose file is gone is
    /// dropped, a photo two sidecars claim stays with the lower id, a group
    /// left under two members is ignored, and a missing representative moves
    /// to the first member. Nothing here writes, so opening a folder never
    /// changes it, and the next mutation of a repaired group rewrites its file.
    pub fn from_loaded(
        files: impl IntoIterator<Item = (GroupId, Group)>,
        listing: &HashSet<OsString>,
    ) -> Groups {
        let sorted: BTreeMap<GroupId, Group> = files.into_iter().collect();
        let mut groups = Groups::default();
        for (id, group) in sorted {
            let kept = group.retain(|m| listing.contains(m) && !groups.of.contains_key(m));
            if let Some(kept) = kept {
                groups.insert(id, kept);
            }
        }
        groups
    }

    pub fn get(&self, id: &GroupId) -> Option<&Group> {
        self.by_id.get(id)
    }

    pub fn group_of(&self, name: &std::ffi::OsStr) -> Option<&GroupId> {
        self.of.get(name)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&GroupId, &Group)> {
        self.by_id.iter()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Save `group`. Its members leave any group they were in, and a group
    /// that drops under two members is deleted. Creating a group whose
    /// members already form one only moves that group's representative, so
    /// repeating a create changes nothing.
    pub fn create(&self, group: Group, at: SystemTime) -> Vec<GroupWrite> {
        if let Some(id) = self.group_of(&group.rep) {
            if self.by_id[id].same_members(&group) {
                return self.set_rep(id, &group.rep);
            }
        }
        let mut writes = self.forget(&group.members);
        let id = GroupId::mint(&group, at, |id| self.by_id.contains_key(id));
        writes.push(GroupWrite::Put(id, group));
        writes
    }

    /// Make `rep` the representative of group `id`. Nothing for a photo
    /// outside the group or the current representative.
    pub fn set_rep(&self, id: &GroupId, rep: &OsString) -> Vec<GroupWrite> {
        let Some(group) = self.by_id.get(id) else {
            return Vec::new();
        };
        if group.rep == *rep {
            return Vec::new();
        }
        match Group::new(group.members.clone(), rep.clone()) {
            Some(g) => vec![GroupWrite::Put(id.clone(), g)],
            None => Vec::new(),
        }
    }

    pub fn dissolve(&self, id: &GroupId) -> Vec<GroupWrite> {
        if self.by_id.contains_key(id) {
            vec![GroupWrite::Delete(id.clone())]
        } else {
            Vec::new()
        }
    }

    /// Take `names` out of their groups, for photos that were trashed or
    /// absorbed into another group. A group losing its representative moves
    /// it to the first surviving member, and one left under two is deleted.
    pub fn forget(&self, names: &[OsString]) -> Vec<GroupWrite> {
        let gone: HashSet<&OsString> = names.iter().collect();
        let touched: BTreeSet<&GroupId> = names.iter().filter_map(|n| self.of.get(n)).collect();
        touched
            .into_iter()
            .map(|id| match self.by_id[id].retain(|m| !gone.contains(m)) {
                Some(g) => GroupWrite::Put(id.clone(), g),
                None => GroupWrite::Delete(id.clone()),
            })
            .collect()
    }

    /// Apply one write to the index. A `Put` whose members another group
    /// still claims takes them from it, so the index stays one group per
    /// photo even when the writes come from a stale snapshot.
    pub fn apply(&mut self, write: &GroupWrite) {
        match write {
            GroupWrite::Put(id, group) => {
                self.remove(id);
                for w in self.forget(&group.members) {
                    self.apply(&w);
                }
                self.insert(id.clone(), group.clone());
            }
            GroupWrite::Delete(id) => self.remove(id),
        }
    }

    fn insert(&mut self, id: GroupId, group: Group) {
        for m in &group.members {
            self.of.insert(m.clone(), id.clone());
        }
        self.by_id.insert(id, group);
    }

    fn remove(&mut self, id: &GroupId) {
        if let Some(old) = self.by_id.remove(id) {
            for m in &old.members {
                self.of.remove(m);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn names(ns: &[&str]) -> Vec<OsString> {
        ns.iter().map(OsString::from).collect()
    }

    fn group(ns: &[&str], rep: &str) -> Group {
        Group::new(names(ns), rep.into()).expect("a valid group")
    }

    fn id(s: &str) -> GroupId {
        GroupId(s.to_owned())
    }

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn listing(ns: &[&str]) -> HashSet<OsString> {
        names(ns).into_iter().collect()
    }

    fn applied(mut groups: Groups, writes: &[GroupWrite]) -> Groups {
        for w in writes {
            groups.apply(w);
        }
        groups
    }

    fn shape_ignoring_ids(groups: &Groups) -> Vec<(Vec<OsString>, OsString)> {
        let mut out: Vec<_> = groups
            .iter()
            .map(|(_, g)| {
                let mut m = g.members().to_vec();
                m.sort();
                (m, g.rep().clone())
            })
            .collect();
        out.sort();
        out
    }

    fn assert_indexed(groups: &Groups) {
        let members: usize = groups.iter().map(|(_, g)| g.members().len()).sum();
        assert_eq!(groups.of.len(), members, "every member is indexed once");
        for (id, g) in groups.iter() {
            for m in g.members() {
                assert_eq!(groups.group_of(m), Some(id));
            }
        }
    }

    #[test]
    fn a_group_needs_two_distinct_members_and_a_representative_among_them() {
        assert!(
            Group::new(names(&["a"]), "a".into()).is_none(),
            "one member"
        );
        assert!(
            Group::new(names(&["a", "a"]), "a".into()).is_none(),
            "a duplicate member"
        );
        assert!(
            Group::new(names(&["a", "b", "a"]), "a".into()).is_none(),
            "a duplicate among three"
        );
        assert!(
            Group::new(names(&["a", "b"]), "c".into()).is_none(),
            "a foreign representative"
        );
        assert!(Group::new(names(&["a", "b"]), "b".into()).is_some());
    }

    #[test]
    fn creating_over_two_groups_merges_them_and_deletes_the_emptied_files() {
        let loaded = Groups::from_loaded(
            [
                (id("g-1"), group(&["a", "b"], "a")),
                (id("g-2"), group(&["c", "d"], "d")),
            ],
            &listing(&["a", "b", "c", "d", "e"]),
        );
        let writes = loaded.create(group(&["a", "b", "c", "d", "e"], "c"), at(1));

        assert!(writes.contains(&GroupWrite::Delete(id("g-1"))));
        assert!(writes.contains(&GroupWrite::Delete(id("g-2"))));
        let puts: Vec<&Group> = writes
            .iter()
            .filter_map(|w| match w {
                GroupWrite::Put(_, g) => Some(g),
                GroupWrite::Delete(_) => None,
            })
            .collect();
        assert_eq!(puts, vec![&group(&["a", "b", "c", "d", "e"], "c")]);

        let after = applied(loaded, &writes);
        assert_eq!(after.len(), 1);
        assert_indexed(&after);
    }

    #[test]
    fn creating_over_part_of_a_group_shrinks_what_is_left() {
        let loaded = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b", "c"], "a"))],
            &listing(&["a", "b", "c", "d"]),
        );
        let writes = loaded.create(group(&["a", "d"], "d"), at(1));
        assert_eq!(
            writes[0],
            GroupWrite::Put(id("g-1"), group(&["b", "c"], "b"))
        );
        let after = applied(loaded, &writes);
        assert_eq!(
            shape_ignoring_ids(&after),
            vec![
                (names(&["a", "d"]), "d".into()),
                (names(&["b", "c"]), "b".into())
            ]
        );
        assert_indexed(&after);
    }

    #[test]
    fn forgetting_the_representative_promotes_the_first_survivor() {
        let groups = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b", "c"], "b"))],
            &listing(&["a", "b", "c"]),
        );
        assert_eq!(
            groups.forget(&names(&["b"])),
            vec![GroupWrite::Put(id("g-1"), group(&["a", "c"], "a"))]
        );
    }

    #[test]
    fn forgetting_down_to_one_member_dissolves_the_group() {
        let groups = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b", "c"], "a"))],
            &listing(&["a", "b", "c"]),
        );
        let writes = groups.forget(&names(&["a", "c"]));
        assert_eq!(writes, vec![GroupWrite::Delete(id("g-1"))]);
        let after = applied(groups, &writes);
        assert!(after.is_empty());
        assert_eq!(after.group_of("b".as_ref()), None);
    }

    #[test]
    fn two_sidecars_claiming_one_photo_resolve_to_the_lower_id() {
        let groups = Groups::from_loaded(
            [
                (id("g-b"), group(&["x", "c", "d"], "x")),
                (id("g-a"), group(&["a", "x"], "x")),
            ],
            &listing(&["a", "c", "d", "x"]),
        );
        assert_eq!(groups.group_of("x".as_ref()), Some(&id("g-a")));
        assert_eq!(groups.get(&id("g-a")), Some(&group(&["a", "x"], "x")));
        assert_eq!(
            groups.get(&id("g-b")),
            Some(&group(&["c", "d"], "c")),
            "the higher id loses the photo and its representative moves"
        );
        assert_indexed(&groups);
    }

    #[test]
    fn loading_drops_missing_files_and_ignores_groups_left_under_two() {
        let groups = Groups::from_loaded(
            [
                (id("g-1"), group(&["a", "b", "c"], "a")),
                (id("g-2"), group(&["d", "e"], "d")),
            ],
            &listing(&["b", "c", "e"]),
        );
        assert_eq!(groups.get(&id("g-1")), Some(&group(&["b", "c"], "b")));
        assert_eq!(groups.get(&id("g-2")), None);
        assert_eq!(groups.group_of("e".as_ref()), None);
        assert_indexed(&groups);
    }

    #[test]
    fn creating_the_same_group_twice_converges() {
        let base = || {
            Groups::from_loaded(
                [(id("g-1"), group(&["a", "b"], "a"))],
                &listing(&["a", "b", "c", "d"]),
            )
        };
        let wanted = group(&["a", "b", "c"], "c");

        let once = applied(base(), &base().create(wanted.clone(), at(1)));
        let second = once.create(wanted.clone(), at(2));
        assert_eq!(second, Vec::new(), "the second create has nothing to write");
        let twice = applied(once, &second);
        assert_eq!(
            shape_ignoring_ids(&twice),
            vec![(names(&["a", "b", "c"]), "c".into())]
        );

        let moved = twice.create(group(&["c", "b", "a"], "a"), at(3));
        assert!(
            matches!(moved.as_slice(), [GroupWrite::Put(_, g)] if g.rep() == "a"),
            "the same members with another representative only move it: {moved:?}"
        );
    }

    #[test]
    fn set_rep_refuses_a_photo_outside_the_group() {
        let groups = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b"], "a"))],
            &listing(&["a", "b", "c"]),
        );
        assert!(groups.set_rep(&id("g-1"), &"c".into()).is_empty());
        assert!(groups.set_rep(&id("g-1"), &"a".into()).is_empty());
        assert_eq!(
            groups.set_rep(&id("g-1"), &"b".into()),
            vec![GroupWrite::Put(id("g-1"), group(&["a", "b"], "b"))]
        );
    }

    #[test]
    fn dissolve_deletes_the_file_and_frees_the_members() {
        let groups = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b"], "a"))],
            &listing(&["a", "b"]),
        );
        let writes = groups.dissolve(&id("g-1"));
        assert_eq!(writes, vec![GroupWrite::Delete(id("g-1"))]);
        let after = applied(groups, &writes);
        assert_eq!(after.group_of("a".as_ref()), None);
        assert!(after.dissolve(&id("g-1")).is_empty());
    }

    #[test]
    fn a_put_from_a_stale_snapshot_takes_its_members_from_other_groups() {
        let groups = Groups::from_loaded(
            [(id("g-1"), group(&["a", "b", "c"], "a"))],
            &listing(&["a", "b", "c"]),
        );
        let after = applied(
            groups,
            &[GroupWrite::Put(id("g-2"), group(&["a", "b"], "b"))],
        );
        assert_eq!(after.get(&id("g-1")), None, "one photo left, so no group");
        assert_eq!(after.group_of("a".as_ref()), Some(&id("g-2")));
        assert_indexed(&after);
    }

    #[test]
    fn minted_ids_avoid_ids_already_taken() {
        let g = group(&["a", "b"], "a");
        let first = GroupId::mint(&g, at(7), |_| false);
        let name = first.to_string();
        assert!(name.starts_with("g-") && name.len() == 12, "{name}");
        let second = GroupId::mint(&g, at(7), |id| *id == first);
        assert_ne!(first, second);
    }
}
