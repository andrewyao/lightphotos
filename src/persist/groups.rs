// SPDX-License-Identifier: MIT OR Apache-2.0

//! Saved photo groups for one folder. A group is two or more photos shown as
//! one, its cover: the representative when one was chosen, else the first
//! member. This module is the pure model: it holds no UI,
//! decode or filesystem state. Every mutation computes the sidecar writes it
//! implies as [`GroupWrite`]s, and [`Groups::apply`] is the one place those
//! writes change the index, so memory and disk go through the same values.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
// `std::time::SystemTime::now()` panics on wasm32; on native this is std.
use web_time::SystemTime;

use crate::hash::Fnv1a;
use crate::navigation::Slot;

/// A group's name, which is also its sidecar's file stem. The order of ids
/// is the order groups were created in, and it decides a photo two sidecars
/// both claim: the newest group keeps it. A minted id,
/// `g-<ms since the epoch, 12 hex digits>-<6 hex digits>`, orders by its
/// time. Any other id, such as the fixture script's `g-fixture00001` or a
/// hand-written one, counts as older than every minted id and orders by name
/// among its kind.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupId {
    minted_ms: Option<u64>,
    name: String,
}

impl GroupId {
    /// Only ASCII letters, digits, `-` and `_`, so an id joined into a path
    /// can never leave `groups/`.
    pub fn from_stem(stem: &OsStr) -> Option<GroupId> {
        let name = stem.to_str()?;
        let safe = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
        safe.then(|| GroupId {
            minted_ms: minted_ms(name),
            name: name.to_owned(),
        })
    }

    /// A new id that sorts after `newest`, the latest minted time in the
    /// folder, even when the clock reads the same millisecond or earlier.
    fn mint(
        group: &Group,
        at: SystemTime,
        newest: Option<u64>,
        taken: impl Fn(&GroupId) -> bool,
    ) -> GroupId {
        let now = at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let ms = newest
            .map_or(now, |n| now.max(n.saturating_add(1)))
            .min(MAX_MINTED_MS);
        for salt in 0u32.. {
            let mut h = Fnv1a::new();
            for m in &group.members {
                h.write(m.as_encoded_bytes());
                h.write(&[0]);
            }
            h.write(&salt.to_le_bytes());
            let id = GroupId {
                minted_ms: Some(ms),
                name: format!("g-{ms:012x}-{:06x}", h.finish() & 0xff_ffff),
            };
            if !taken(&id) {
                return id;
            }
        }
        unreachable!("a folder cannot hold 2^32 groups")
    }
}

/// The largest time 12 hex digits hold, some 8 900 years after 1970.
const MAX_MINTED_MS: u64 = 0xffff_ffff_ffff;

/// The time in a stem shaped like a minted id, `g-` then 12 and 6 lowercase
/// hex digits.
fn minted_ms(name: &str) -> Option<u64> {
    let (ms, hash) = name.strip_prefix("g-")?.split_once('-')?;
    let hex = |s: &str, len: usize| {
        s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    };
    if !(hex(ms, 12) && hex(hash, 6)) {
        return None;
    }
    u64::from_str_radix(ms, 16).ok()
}

impl std::fmt::Display for GroupId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

/// Two or more distinct photos, sorted by file name the way the folder
/// lists them, and the one chosen to stand for them, if any. [`Group::new`]
/// is the only constructor, so a group of one, a representative from outside
/// the group, or a member name a sidecar cannot hold cannot exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    members: Vec<OsString>,
    rep: Option<OsString>,
}

impl Group {
    /// `None` also for a member name that is not UTF-8, since the sidecar's
    /// JSON strings cannot hold it.
    pub fn new(members: Vec<OsString>, rep: Option<OsString>) -> Option<Group> {
        let members = sorted_members(members)?;
        rep.as_ref()
            .is_none_or(|r| members.contains(r))
            .then_some(Group { members, rep })
    }

    /// Like [`Group::new`], but a `rep` outside `members` is dropped
    /// instead of refusing the group.
    fn with_rep_if_member(members: Vec<OsString>, rep: Option<&OsString>) -> Option<Group> {
        let members = sorted_members(members)?;
        let rep = rep.filter(|r| members.contains(r)).cloned();
        Some(Group { members, rep })
    }

    pub fn members(&self) -> &[OsString] {
        &self.members
    }

    /// The representative, when one was chosen.
    pub fn rep(&self) -> Option<&OsString> {
        self.rep.as_ref()
    }

    /// The photo the collapsed stack shows: the representative, else the
    /// first member.
    pub fn cover(&self) -> &OsString {
        self.rep.as_ref().unwrap_or(&self.members[0])
    }

    fn retain(&self, keep: impl Fn(&OsString) -> bool) -> Option<Group> {
        let members = self.members.iter().filter(|m| keep(m)).cloned().collect();
        Group::with_rep_if_member(members, self.rep.as_ref())
    }
}

/// Two or more distinct UTF-8 names, sorted case-insensitively like the
/// folder listing, with the exact name breaking ties so twins sit together.
fn sorted_members(mut members: Vec<OsString>) -> Option<Vec<OsString>> {
    if members.len() < 2 || members.iter().any(|m| m.to_str().is_none()) {
        return None;
    }
    members.sort_by_cached_key(|m| (m.to_string_lossy().to_lowercase(), m.clone()));
    members.windows(2).all(|w| w[0] != w[1]).then_some(members)
}

/// A group as its sidecar spells it, before it is checked against the
/// folder. Any of it may be wrong: missing photos, duplicates, one member.
#[derive(Clone, Debug)]
pub struct SavedGroup {
    pub members: Vec<OsString>,
    pub rep: Option<OsString>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GroupWrite {
    Put(GroupId, Group),
    Delete(GroupId),
}

impl GroupWrite {
    fn id(&self) -> &GroupId {
        match self {
            GroupWrite::Put(id, _) | GroupWrite::Delete(id) => id,
        }
    }
}

/// Every group in one folder. `of` indexes `by_id` by member, which is what
/// holds a photo to at most one group.
#[derive(Default, Debug)]
pub struct Groups {
    by_id: BTreeMap<GroupId, Group>,
    of: HashMap<OsString, GroupId>,
    /// Groups whose sidecar disagrees with memory, because loading repaired
    /// them or a replayed write took their photos, with what the file should
    /// say. `None` means the file should go. [`Groups::take_repairs`] hands
    /// them to the next batch of writes.
    repairs: BTreeMap<GroupId, Option<Group>>,
}

impl Groups {
    /// The groups a folder's sidecars describe, reconciled against the
    /// folder's photos. A member that is not a photo in the folder is
    /// dropped, the newest id keeps a photo two sidecars claim, a group left
    /// under two members is dropped, and a missing representative is
    /// cleared. Nothing here writes, so opening a folder never
    /// changes it. Every group this changed is recorded for
    /// [`Groups::take_repairs`], so a dropped group cannot come back when
    /// its photos do.
    pub fn from_loaded(
        mut files: Vec<(GroupId, SavedGroup)>,
        is_photo: impl Fn(&OsStr) -> bool,
    ) -> Groups {
        files.sort_by(|a, b| b.0.cmp(&a.0));
        let mut groups = Groups::default();
        for (id, saved) in files {
            let mut seen = HashSet::new();
            let kept: Vec<OsString> = saved
                .members
                .iter()
                .filter(|m| is_photo(m) && !groups.of.contains_key(*m) && seen.insert(*m))
                .cloned()
                .collect();
            let intact = kept.len() == saved.members.len();
            let repaired = Group::with_rep_if_member(kept, saved.rep.as_ref());
            match &repaired {
                Some(g) if intact && g.rep == saved.rep => {}
                _ => {
                    groups.repairs.insert(id.clone(), repaired.clone());
                }
            }
            if let Some(group) = repaired {
                groups.insert(id, group);
            }
        }
        groups
    }

    pub fn get(&self, id: &GroupId) -> Option<&Group> {
        self.by_id.get(id)
    }

    pub fn group_of(&self, name: &OsStr) -> Option<&GroupId> {
        self.of.get(name)
    }

    /// Where `name` sits among the stacks. `expanded` says which stacks
    /// the Grid shows member by member.
    pub fn slot(&self, name: &OsStr, expanded: impl Fn(&GroupId) -> bool) -> Slot<&GroupId> {
        let Some(stack) = self.of.get(name) else {
            return Slot::Single;
        };
        let expanded = expanded(stack);
        if self.by_id[stack].cover() == name {
            Slot::Cover { stack, expanded }
        } else {
            Slot::Member { stack, expanded }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&GroupId, &Group)> {
        self.by_id.iter()
    }

    #[cfg_attr(not(feature = "hotpath"), allow(dead_code))]
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Save `group`. Its members leave any group they were in, and a group
    /// that drops under two members is deleted. The new group's write comes
    /// first and its id is the newest, so a crash partway through still
    /// loads with the photos in the new group. Creating a group whose members
    /// already form one only rewrites that group's representative, so
    /// repeating a create changes nothing.
    pub fn create(&self, group: Group, at: SystemTime) -> Vec<GroupWrite> {
        if let Some(id) = self.group_of(&group.members[0]) {
            if self.by_id[id].members == group.members {
                if self.by_id[id].rep == group.rep {
                    return Vec::new();
                }
                return vec![GroupWrite::Put(id.clone(), group)];
            }
        }
        let newest = self
            .by_id
            .keys()
            .chain(self.repairs.keys())
            .filter_map(|id| id.minted_ms)
            .max();
        let id = GroupId::mint(&group, at, newest, |id| {
            self.by_id.contains_key(id) || self.repairs.contains_key(id)
        });
        let absorbed = self.forget(&group.members);
        let mut writes = vec![GroupWrite::Put(id, group)];
        writes.extend(absorbed);
        writes
    }

    /// Make `rep` the representative of group `id`. Nothing for a photo
    /// outside the group or the current representative.
    pub fn set_rep(&self, id: &GroupId, rep: &OsString) -> Vec<GroupWrite> {
        let Some(group) = self.by_id.get(id) else {
            return Vec::new();
        };
        if group.rep.as_ref() == Some(rep) {
            return Vec::new();
        }
        match Group::new(group.members.clone(), Some(rep.clone())) {
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
    /// absorbed into another group. A group losing its representative is
    /// left without one, and one left under two is deleted.
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

    /// The writes that bring every repaired group's sidecar in line with
    /// memory, handed out once.
    pub fn take_repairs(&mut self) -> Vec<GroupWrite> {
        std::mem::take(&mut self.repairs)
            .into_iter()
            .map(|(id, group)| match group {
                Some(g) => GroupWrite::Put(id, g),
                None => GroupWrite::Delete(id),
            })
            .collect()
    }

    /// Apply one write that is on its way to disk. A `Put` whose members
    /// another group still claims takes them from it and records that group
    /// as a repair, since its sidecar still names them. A batch that goes on
    /// to write that group clears the record; a write replayed onto an older
    /// snapshot leaves it for the next batch.
    pub fn apply(&mut self, write: &GroupWrite) {
        self.repairs.remove(write.id());
        match write {
            GroupWrite::Put(id, group) => {
                self.remove(id);
                for taken in self.forget(&group.members) {
                    self.apply(&taken);
                    let (id, repaired) = match taken {
                        GroupWrite::Put(id, g) => (id, Some(g)),
                        GroupWrite::Delete(id) => (id, None),
                    };
                    self.repairs.insert(id, repaired);
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
        Group::new(names(ns), Some(rep.into())).expect("a valid group")
    }

    /// A group with no representative chosen.
    fn bare(ns: &[&str]) -> Group {
        Group::new(names(ns), None).expect("a valid group")
    }

    fn id(s: &str) -> GroupId {
        GroupId::from_stem(s.as_ref()).expect("a valid id")
    }

    fn minted(ms: u64) -> GroupId {
        id(&format!("g-{ms:012x}-000000"))
    }

    fn at(ms: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_millis(ms)
    }

    fn saved(ns: &[&str], rep: &str) -> SavedGroup {
        SavedGroup {
            members: names(ns),
            rep: Some(rep.into()),
        }
    }

    fn load(files: Vec<(GroupId, SavedGroup)>, photos: &[&str]) -> Groups {
        let photos: HashSet<OsString> = names(photos).into_iter().collect();
        Groups::from_loaded(files, |n| photos.contains(n))
    }

    fn applied(mut groups: Groups, writes: &[GroupWrite]) -> Groups {
        for w in writes {
            groups.apply(w);
        }
        groups
    }

    fn shape_ignoring_ids(groups: &Groups) -> Vec<(Vec<OsString>, Option<OsString>)> {
        let mut out: Vec<_> = groups
            .iter()
            .map(|(_, g)| (g.members().to_vec(), g.rep().cloned()))
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

    #[cfg(unix)]
    #[test]
    fn grouping_a_name_a_sidecar_cannot_hold_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'a', 0xff]);
        assert_eq!(Group::new(vec![bad, "b".into()], None), None);
    }

    #[test]
    fn the_cover_is_the_representative_else_the_first_member() {
        assert_eq!(group(&["a", "b", "c"], "b").cover(), "b");
        assert_eq!(bare(&["c", "a", "b"]).cover(), "a");
        let groups = load(vec![(id("g-a"), saved(&["a", "b"], "b"))], &["a", "b"]);
        let cleared = applied(groups, &[GroupWrite::Put(id("g-a"), bare(&["a", "b"]))]);
        assert_eq!(
            cleared.slot("a".as_ref(), |_| false),
            Slot::Cover {
                stack: &id("g-a"),
                expanded: false
            }
        );
    }

    #[test]
    fn creating_a_stack_with_no_representative_twice_converges() {
        let groups = Groups::default();
        let writes = groups.create(bare(&["a", "b"]), at(1));
        let once = applied(groups, &writes);
        assert_eq!(once.create(bare(&["b", "a"]), at(2)), Vec::new());
        assert!(
            matches!(once.create(group(&["a", "b"], "b"), at(3)).as_slice(),
                [GroupWrite::Put(_, g)] if g.rep().is_some_and(|r| r == "b")),
            "choosing a representative for the same members rewrites only it"
        );
    }

    #[test]
    fn slot_tells_the_cover_from_the_other_members() {
        let groups = load(
            vec![(id("g-a"), saved(&["a", "b", "c"], "b"))],
            &["a", "b", "c", "d"],
        );
        let g = id("g-a");
        let slot = |n: &str, expanded: bool| groups.slot(n.as_ref(), |_| expanded);
        assert_eq!(
            slot("a", false),
            Slot::Member {
                stack: &g,
                expanded: false
            }
        );
        assert_eq!(
            slot("b", true),
            Slot::Cover {
                stack: &g,
                expanded: true
            }
        );
        assert_eq!(
            slot("c", true),
            Slot::Member {
                stack: &g,
                expanded: true
            }
        );
        assert_eq!(slot("d", false), Slot::Single, "a photo in no group");
    }

    #[test]
    fn a_group_needs_two_distinct_members_and_any_representative_among_them() {
        assert!(
            Group::new(names(&["a"]), Some("a".into())).is_none(),
            "one member"
        );
        assert!(
            Group::new(names(&["a", "a"]), Some("a".into())).is_none(),
            "a duplicate member"
        );
        assert!(
            Group::new(names(&["a", "b", "a"]), Some("a".into())).is_none(),
            "a duplicate among three"
        );
        assert!(
            Group::new(names(&["a", "b"]), Some("c".into())).is_none(),
            "a foreign representative"
        );
        assert!(Group::new(names(&["a", "b"]), Some("b".into())).is_some());
        assert!(
            Group::new(names(&["a", "b"]), None).is_some(),
            "no representative chosen"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_member_name_a_sidecar_cannot_hold_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'a', 0xff]);
        assert!(Group::new(vec![bad.clone(), "b".into()], None).is_none());
        assert!(Group::new(vec!["b".into(), bad.clone()], Some(bad)).is_none());
    }

    #[test]
    fn members_sort_by_name_the_way_the_folder_lists_them() {
        let g = group(&["IMG_3.JPG", "img_1.jpg", "IMG_2.JPG"], "IMG_3.JPG");
        assert_eq!(g.members(), names(&["img_1.jpg", "IMG_2.JPG", "IMG_3.JPG"]));
        assert_eq!(
            g,
            group(&["IMG_2.JPG", "IMG_3.JPG", "img_1.jpg"], "IMG_3.JPG")
        );
        assert!(
            Group::new(names(&["a", "A", "a"]), Some("a".into())).is_none(),
            "a duplicate that sorts apart from its twin"
        );
    }

    #[test]
    fn creating_over_two_groups_puts_the_new_group_first_then_deletes_the_old() {
        let loaded = load(
            vec![
                (id("g-1"), saved(&["a", "b"], "a")),
                (id("g-2"), saved(&["c", "d"], "d")),
            ],
            &["a", "b", "c", "d", "e"],
        );
        let writes = loaded.create(group(&["a", "b", "c", "d", "e"], "c"), at(1));

        assert!(
            matches!(&writes[0], GroupWrite::Put(_, g) if *g == group(&["a", "b", "c", "d", "e"], "c")),
            "the new group is written first: {writes:?}"
        );
        assert_eq!(
            writes[1..],
            [GroupWrite::Delete(id("g-1")), GroupWrite::Delete(id("g-2"))]
        );

        let after = applied(loaded, &writes);
        assert_eq!(after.len(), 1);
        assert_indexed(&after);
    }

    #[test]
    fn creating_over_part_of_a_group_shrinks_what_is_left() {
        let loaded = load(
            vec![(id("g-1"), saved(&["a", "b", "c"], "a"))],
            &["a", "b", "c", "d"],
        );
        let writes = loaded.create(group(&["a", "d"], "d"), at(1));
        assert_eq!(writes[1], GroupWrite::Put(id("g-1"), bare(&["b", "c"])));
        let after = applied(loaded, &writes);
        assert_eq!(
            shape_ignoring_ids(&after),
            vec![
                (names(&["a", "d"]), Some("d".into())),
                (names(&["b", "c"]), None)
            ]
        );
        assert_indexed(&after);
    }

    #[test]
    fn forgetting_the_representative_leaves_the_group_without_one() {
        let groups = load(
            vec![(id("g-1"), saved(&["a", "b", "c"], "b"))],
            &["a", "b", "c"],
        );
        assert_eq!(
            groups.forget(&names(&["b"])),
            vec![GroupWrite::Put(id("g-1"), bare(&["a", "c"]))]
        );
    }

    #[test]
    fn forgetting_down_to_one_member_dissolves_the_group() {
        let groups = load(
            vec![(id("g-1"), saved(&["a", "b", "c"], "a"))],
            &["a", "b", "c"],
        );
        let writes = groups.forget(&names(&["a", "c"]));
        assert_eq!(writes, vec![GroupWrite::Delete(id("g-1"))]);
        let after = applied(groups, &writes);
        assert!(after.is_empty());
        assert_eq!(after.group_of("b".as_ref()), None);
    }

    /// Only the first write of a merge reached the disk before a crash. The
    /// next load must side with the new group, the user's latest action.
    #[test]
    fn a_merge_cut_short_after_its_first_write_loads_as_the_new_group() {
        let old = minted(1_000);
        let photos = ["a", "b", "c", "d"];
        let before = load(vec![(old.clone(), saved(&["a", "b", "c"], "a"))], &photos);
        let writes = before.create(group(&["b", "c", "d"], "d"), at(2_000));
        let GroupWrite::Put(new, new_group) = &writes[0] else {
            panic!("the new group comes first: {writes:?}");
        };

        let reloaded = load(
            vec![
                (old.clone(), saved(&["a", "b", "c"], "a")),
                (new.clone(), saved(&["b", "c", "d"], "d")),
            ],
            &photos,
        );
        assert_eq!(reloaded.get(new), Some(new_group));
        assert_eq!(reloaded.get(&old), None, "the old group keeps only a");
        assert_indexed(&reloaded);
    }

    #[test]
    fn a_merge_in_the_same_millisecond_or_after_the_clock_steps_back_still_wins() {
        let photos = ["a", "b", "c", "d"];
        for clock in [1_000, 500] {
            let old = minted(1_000);
            let before = load(vec![(old.clone(), saved(&["a", "b", "c"], "a"))], &photos);
            let writes = before.create(group(&["b", "c", "d"], "d"), at(clock));
            let GroupWrite::Put(new, new_group) = &writes[0] else {
                panic!("the new group comes first: {writes:?}");
            };
            assert!(old < *new, "clock {clock}: {old} < {new}");

            let reloaded = load(
                vec![
                    (old.clone(), saved(&["a", "b", "c"], "a")),
                    (new.clone(), saved(&["b", "c", "d"], "d")),
                ],
                &photos,
            );
            assert_eq!(reloaded.get(new), Some(new_group), "clock {clock}");
        }
    }

    #[test]
    fn two_sidecars_claiming_one_photo_resolve_to_the_newest_id() {
        let (older, newer) = (minted(1_000), minted(2_000));
        let groups = load(
            vec![
                (newer.clone(), saved(&["a", "x"], "x")),
                (older.clone(), saved(&["x", "c", "d"], "x")),
            ],
            &["a", "c", "d", "x"],
        );
        assert_eq!(groups.group_of("x".as_ref()), Some(&newer));
        assert_eq!(groups.get(&newer), Some(&group(&["a", "x"], "x")));
        assert_eq!(
            groups.get(&older),
            Some(&bare(&["c", "d"])),
            "the older group loses the photo, which was its representative"
        );
        assert_indexed(&groups);
    }

    #[test]
    fn ids_order_by_creation_time_after_every_unminted_id() {
        let fixture = id("g-fixture00002");
        assert!(id("g-fixture00001") < fixture, "unminted ids order by name");
        assert!(id("zzz") < minted(0), "a minted id is newer than any other");
        assert!(minted(0xff) < minted(0x100));
        let early = GroupId::mint(&group(&["y", "z"], "y"), at(5), None, |_| false);
        let late = GroupId::mint(&group(&["a", "b"], "a"), at(6), None, |_| false);
        assert!(early < late, "{early} < {late}");
        assert_eq!(
            id(&late.to_string()),
            late,
            "a minted id reads back as minted"
        );

        let groups = load(
            vec![
                (id("g-fixture00001"), saved(&["a", "b"], "a")),
                (fixture.clone(), saved(&["b", "c"], "b")),
            ],
            &["a", "b", "c"],
        );
        assert_eq!(groups.group_of("b".as_ref()), Some(&fixture));
    }

    #[test]
    fn loading_drops_missing_files_and_ignores_groups_left_under_two() {
        let groups = load(
            vec![
                (id("g-1"), saved(&["a", "b", "c"], "a")),
                (id("g-2"), saved(&["d", "e"], "d")),
            ],
            &["b", "c", "e"],
        );
        assert_eq!(groups.get(&id("g-1")), Some(&bare(&["b", "c"])));
        assert_eq!(groups.get(&id("g-2")), None);
        assert_eq!(groups.group_of("e".as_ref()), None);
        assert_indexed(&groups);
    }

    #[test]
    fn loading_records_the_writes_that_would_repair_each_file() {
        let mut groups = load(
            vec![
                (id("g-1"), saved(&["a", "b", "c"], "a")),
                (id("g-2"), saved(&["d", "e"], "d")),
                (id("g-3"), saved(&["f"], "f")),
                (id("g-4"), saved(&["h", "g"], "g")),
                (id("g-5"), saved(&["i", "notes.txt"], "i")),
                (id("g-6"), saved(&["j", "k", "j"], "j")),
                (id("g-7"), saved(&["l", "m"], "gone")),
            ],
            &["b", "c", "e", "f", "g", "h", "i", "j", "k", "l", "m"],
        );
        assert_eq!(groups.get(&id("g-3")), None, "a one-member file is dropped");
        assert_eq!(
            groups.get(&id("g-5")),
            None,
            "a file that is not a photo cannot keep a group"
        );
        assert_eq!(groups.get(&id("g-6")), Some(&group(&["j", "k"], "j")));
        assert_indexed(&groups);

        assert_eq!(
            groups.take_repairs(),
            vec![
                GroupWrite::Put(id("g-1"), bare(&["b", "c"])),
                GroupWrite::Delete(id("g-2")),
                GroupWrite::Delete(id("g-3")),
                GroupWrite::Delete(id("g-5")),
                GroupWrite::Put(id("g-6"), group(&["j", "k"], "j")),
                GroupWrite::Put(id("g-7"), bare(&["l", "m"])),
            ],
            "g-4 only lists its members out of order, which needs no rewrite"
        );
        assert!(
            groups.take_repairs().is_empty(),
            "repairs are handed out once"
        );
    }

    #[test]
    fn the_older_of_two_claims_is_repaired_and_the_newer_is_not() {
        let (older, newer) = (minted(1_000), minted(2_000));
        let mut groups = load(
            vec![
                (older.clone(), saved(&["a", "b", "c"], "a")),
                (newer.clone(), saved(&["b", "c"], "b")),
            ],
            &["a", "b", "c"],
        );
        assert_eq!(groups.take_repairs(), vec![GroupWrite::Delete(older)]);
    }

    #[test]
    fn a_batch_that_writes_a_repaired_group_clears_its_repair() {
        let loaded = load(
            vec![
                (id("g-1"), saved(&["a", "b", "gone"], "a")),
                (id("g-2"), saved(&["c", "gone2"], "c")),
            ],
            &["a", "b", "c", "d"],
        );
        let writes = loaded.dissolve(&id("g-1"));
        let mut after = applied(loaded, &writes);
        assert_eq!(
            after.take_repairs(),
            vec![GroupWrite::Delete(id("g-2"))],
            "g-1's own write already brings its file in line"
        );
    }

    #[test]
    fn creating_the_same_group_twice_converges() {
        let base = || {
            load(
                vec![(id("g-1"), saved(&["a", "b"], "a"))],
                &["a", "b", "c", "d"],
            )
        };
        let wanted = group(&["a", "b", "c"], "c");

        let once = applied(base(), &base().create(wanted.clone(), at(1)));
        let second = once.create(wanted.clone(), at(2));
        assert_eq!(second, Vec::new(), "the second create has nothing to write");
        let twice = applied(once, &second);
        assert_eq!(
            shape_ignoring_ids(&twice),
            vec![(names(&["a", "b", "c"]), Some("c".into()))]
        );

        let moved = twice.create(group(&["c", "b", "a"], "a"), at(3));
        assert!(
            matches!(moved.as_slice(), [GroupWrite::Put(_, g)] if g.rep().is_some_and(|r| r == "a")),
            "the same members with another representative only move it: {moved:?}"
        );
    }

    #[test]
    fn set_rep_refuses_a_photo_outside_the_group() {
        let groups = load(vec![(id("g-1"), saved(&["a", "b"], "a"))], &["a", "b", "c"]);
        assert!(groups.set_rep(&id("g-1"), &"c".into()).is_empty());
        assert!(groups.set_rep(&id("g-1"), &"a".into()).is_empty());
        assert_eq!(
            groups.set_rep(&id("g-1"), &"b".into()),
            vec![GroupWrite::Put(id("g-1"), group(&["a", "b"], "b"))]
        );
    }

    #[test]
    fn dissolve_deletes_the_file_and_frees_the_members() {
        let groups = load(vec![(id("g-1"), saved(&["a", "b"], "a"))], &["a", "b"]);
        let writes = groups.dissolve(&id("g-1"));
        assert_eq!(writes, vec![GroupWrite::Delete(id("g-1"))]);
        let after = applied(groups, &writes);
        assert_eq!(after.group_of("a".as_ref()), None);
        assert!(after.dissolve(&id("g-1")).is_empty());
    }

    #[test]
    fn a_replayed_put_takes_members_from_other_groups_and_records_the_repair() {
        let groups = load(
            vec![(id("g-1"), saved(&["a", "b", "c"], "a"))],
            &["a", "b", "c"],
        );
        let mut after = applied(
            groups,
            &[GroupWrite::Put(id("g-2"), group(&["a", "b"], "b"))],
        );
        assert_eq!(after.get(&id("g-1")), None, "one photo left, so no group");
        assert_eq!(after.group_of("a".as_ref()), Some(&id("g-2")));
        assert_indexed(&after);
        assert_eq!(
            after.take_repairs(),
            vec![GroupWrite::Delete(id("g-1"))],
            "g-1's file still claims a and b, so the next batch deletes it"
        );
    }

    #[test]
    fn a_create_batch_leaves_no_repair_behind() {
        let loaded = load(
            vec![
                (id("g-1"), saved(&["a", "b"], "a")),
                (id("g-2"), saved(&["c", "d", "e"], "d")),
            ],
            &["a", "b", "c", "d", "e"],
        );
        let writes = loaded.create(group(&["a", "b", "c"], "c"), at(1));
        let mut after = applied(loaded, &writes);
        assert_eq!(after.take_repairs(), Vec::new());
        assert_indexed(&after);
    }

    #[test]
    fn a_sidecar_stem_that_could_leave_groups_is_not_an_id() {
        for bad in ["", "..", ".", "a/b", "../g-1", "g 1", "g.1", "g-\u{e9}"] {
            assert_eq!(GroupId::from_stem(bad.as_ref()), None, "{bad:?}");
        }
        for good in ["g-5f3a9c10e2", "g-fixture00001", "g-1", "My_Group"] {
            assert_eq!(
                GroupId::from_stem(good.as_ref()).map(|id| id.to_string()),
                Some(good.to_string())
            );
        }
        let minted = GroupId::mint(&group(&["a", "b"], "a"), at(3), None, |_| false);
        assert_eq!(
            GroupId::from_stem(minted.to_string().as_ref()),
            Some(minted)
        );
    }

    #[test]
    fn minted_ids_avoid_ids_already_taken() {
        let g = group(&["a", "b"], "a");
        let first = GroupId::mint(&g, at(7), None, |_| false);
        let name = first.to_string();
        assert!(
            name.starts_with("g-000000000007-") && name.len() == 21,
            "{name}"
        );
        let second = GroupId::mint(&g, at(7), None, |id| *id == first);
        assert_ne!(first, second);
    }
}
