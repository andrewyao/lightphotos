//! Undo and redo. Each step holds what every photo had before the step's
//! first change to it: its edit, its touch-ups and masks when the step
//! changed them, its rating, flag, color label and rotation when the step
//! changed those, and every group the step wrote, as it stood. One command is one step, and so is one slider drag or
//! one Auto Tone or Remove Chromatic Aberration batch: a step stays open
//! until the pointer is up at the end of a frame and no batch is running.
//!
//! A step is its own inverse in data: undoing it reads what those photos
//! hold now, puts the step's values back, and returns the values it read as
//! a step for the other stack. Undo and redo are the same operation.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::develop::{Adjustments, Mask, TouchUp};
use crate::persist::groups::{Group, GroupId};
use crate::sidecar::{ColorLabel, Flag};

/// Steps older than this are dropped.
const MAX_STEPS: usize = 100;

#[derive(Default)]
pub(crate) struct EditHistory {
    done: Vec<EditStep>,
    /// Undone steps, newest last. Any new step empties it.
    redo: Vec<EditStep>,
    open: Option<EditStep>,
    /// Nested `begin`s still running. A step cannot close while any are.
    depth: u32,
    /// A background batch (Auto Tone, Remove Chromatic Aberration) is still
    /// writing results, so its frames join one step.
    held: bool,
}

#[derive(Default)]
pub(crate) struct EditStep {
    pub(crate) before: Vec<Before>,
    /// Each path's index in `before`, so a step over thousands of photos
    /// finds a photo's entry without a scan.
    index: HashMap<PathBuf, usize>,
    /// The groups the step wrote, as each stood before it.
    pub(crate) groups: Vec<GroupBefore>,
    group_index: HashMap<GroupId, usize>,
}

pub(crate) struct GroupBefore {
    pub(crate) id: GroupId,
    /// `None` when the group did not exist.
    pub(crate) group: Option<Group>,
}

pub(crate) struct Before {
    pub(crate) path: PathBuf,
    /// `None` when the step left the edit alone.
    pub(crate) adj: Option<Adjustments>,
    /// `None` when the step left the touch-ups alone.
    pub(crate) touchups: Option<Vec<TouchUp>>,
    /// `None` when the step left the masks alone.
    pub(crate) masks: Option<Vec<Mask>>,
    pub(crate) marks: Marks,
}

/// The marks a step changed, each `None` when it left that mark alone.
#[derive(Clone, Copy, Default, PartialEq, Debug)]
pub(crate) struct Marks {
    /// 0 is unrated.
    pub(crate) rating: Option<u8>,
    pub(crate) flag: Option<Option<Flag>>,
    pub(crate) label: Option<Option<ColorLabel>>,
    /// Quarter turns clockwise, 0..4.
    pub(crate) rotation: Option<u8>,
}

impl Before {
    /// An entry for `path` that holds nothing yet.
    pub(crate) fn new(path: PathBuf) -> Self {
        Before {
            path,
            adj: None,
            touchups: None,
            masks: None,
            marks: Marks::default(),
        }
    }
}

impl EditStep {
    pub(crate) fn new(before: Vec<Before>, groups: Vec<GroupBefore>) -> Self {
        let index = before
            .iter()
            .enumerate()
            .map(|(i, b)| (b.path.clone(), i))
            .collect();
        let group_index = groups
            .iter()
            .enumerate()
            .map(|(i, g)| (g.id.clone(), i))
            .collect();
        EditStep {
            before,
            index,
            groups,
            group_index,
        }
    }

    fn is_empty(&self) -> bool {
        self.before.is_empty() && self.groups.is_empty()
    }
}

impl EditHistory {
    /// Holds the open step open until the matching `end`, so everything in
    /// between joins one step.
    pub(super) fn begin(&mut self) {
        self.depth = self.depth.saturating_add(1);
    }

    pub(super) fn end(&mut self) {
        self.depth = self.depth.saturating_sub(1);
    }

    /// Records `before` as `path`'s edit ahead of a change, unless the open
    /// step already holds an earlier one. Opens a step if none is.
    pub(super) fn note(&mut self, path: &Path, before: Adjustments) {
        self.entry(path).adj.get_or_insert(before);
    }

    /// As [`note`](Self::note), and records `touchups` as `path`'s spots
    /// ahead of a change to them, unless the open step already holds some.
    pub(super) fn note_touchups(&mut self, path: &Path, adj: Adjustments, touchups: &[TouchUp]) {
        let entry = self.entry(path);
        entry.adj.get_or_insert(adj);
        entry.touchups.get_or_insert_with(|| touchups.to_vec());
    }

    /// As [`note`](Self::note), and records `masks` as `path`'s masks
    /// ahead of a change to them, unless the open step already holds some.
    pub(super) fn note_masks(&mut self, path: &Path, adj: Adjustments, masks: &[Mask]) {
        let entry = self.entry(path);
        entry.adj.get_or_insert(adj);
        entry.masks.get_or_insert_with(|| masks.to_vec());
    }

    /// Records `rating` (0 is unrated) as `path`'s ahead of a change to it.
    pub(super) fn note_rating(&mut self, path: &Path, rating: u8) {
        self.entry(path).marks.rating.get_or_insert(rating);
    }

    pub(super) fn note_flag(&mut self, path: &Path, flag: Option<Flag>) {
        self.entry(path).marks.flag.get_or_insert(flag);
    }

    pub(super) fn note_label(&mut self, path: &Path, label: Option<ColorLabel>) {
        self.entry(path).marks.label.get_or_insert(label);
    }

    /// Records `rotation` (quarter turns) as `path`'s ahead of a change to it.
    pub(super) fn note_rotation(&mut self, path: &Path, rotation: u8) {
        self.entry(path).marks.rotation.get_or_insert(rotation);
    }

    /// Records `group` (`None` for no such group) as group `id`'s state
    /// ahead of a write to it, unless the open step already holds one.
    pub(super) fn note_group(&mut self, id: &GroupId, group: Option<&Group>) {
        let step = self.open_step();
        if !step.group_index.contains_key(id) {
            step.group_index.insert(id.clone(), step.groups.len());
            step.groups.push(GroupBefore {
                id: id.clone(),
                group: group.cloned(),
            });
        }
    }

    /// The open step, which a first note opens. A new step ends the chance
    /// to redo.
    fn open_step(&mut self) -> &mut EditStep {
        if self.open.is_none() {
            self.redo.clear();
        }
        self.open.get_or_insert_with(EditStep::default)
    }

    /// The open step's entry for `path`.
    fn entry(&mut self, path: &Path) -> &mut Before {
        let step = self.open_step();
        let i = match step.index.get(path) {
            Some(&i) => i,
            None => {
                let i = step.before.len();
                step.before.push(Before::new(path.to_path_buf()));
                step.index.insert(path.to_path_buf(), i);
                i
            }
        };
        &mut step.before[i]
    }

    /// Apply `change` to every edit recorded for `path`, in the open step
    /// and the done ones: a change made outside the history stays through
    /// any undo.
    pub(super) fn apply_outside(
        &mut self,
        path: &Path,
        change: impl Fn(Adjustments) -> Adjustments,
    ) {
        let steps = (self.done.iter_mut())
            .chain(self.redo.iter_mut())
            .chain(self.open.iter_mut());
        for step in steps {
            if let Some(&i) = step.index.get(path) {
                if let Some(adj) = step.before.get_mut(i).and_then(|b| b.adj.as_mut()) {
                    *adj = change(*adj);
                }
            }
        }
    }

    /// Holds the open step open while `batch_running`, across frames.
    pub(super) fn hold(&mut self, batch_running: bool) {
        self.held = batch_running;
    }

    /// Closes the open step, unless a `begin` or a batch holds it.
    pub(super) fn seal(&mut self) {
        if self.depth == 0 && !self.held {
            self.close();
        }
    }

    fn close(&mut self) {
        if let Some(step) = self.open.take().filter(|s| !s.is_empty()) {
            if self.done.len() == MAX_STEPS {
                self.done.remove(0);
            }
            self.done.push(step);
        }
    }

    /// The last step. An open one is the last, even while a drag or a
    /// `begin` still holds it.
    pub(super) fn pop_undo(&mut self) -> Option<EditStep> {
        self.close();
        self.done.pop()
    }

    /// The step an undo most recently took back.
    pub(super) fn pop_redo(&mut self) -> Option<EditStep> {
        self.redo.pop()
    }

    /// Files `inverse`, what an undo read before it put a step back, so a
    /// redo can undo the undo. It does not end the chance to redo.
    pub(super) fn push_redo(&mut self, inverse: EditStep) {
        if !inverse.is_empty() {
            self.redo.push(inverse);
        }
    }

    /// Files `inverse` as a done step, what a redo read before it put a
    /// step back.
    pub(super) fn push_done(&mut self, inverse: EditStep) {
        if inverse.is_empty() {
            return;
        }
        if self.done.len() == MAX_STEPS {
            self.done.remove(0);
        }
        self.done.push(inverse);
    }

    /// For the menu bar's Undo item.
    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn can_undo(&self) -> bool {
        !self.done.is_empty() || self.open.is_some()
    }

    /// For the menu bar's Redo item.
    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// The photos leave the folder store's cache when the folder changes, so
    /// their steps go with them.
    pub(super) fn clear(&mut self) {
        self.done.clear();
        self.redo.clear();
        self.open = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exposure(v: f32) -> Adjustments {
        Adjustments {
            exposure: v,
            ..Default::default()
        }
    }

    #[test]
    fn a_step_keeps_each_photos_first_before() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.note(a, exposure(0.0));
        h.note(a, exposure(0.5));
        h.note(Path::new("/b.jpg"), exposure(1.0));
        let step = h.pop_undo().unwrap();
        let firsts: Vec<_> = step.before.iter().map(|b| (&*b.path, b.adj)).collect();
        assert_eq!(
            firsts,
            vec![
                (a, Some(exposure(0.0))),
                (Path::new("/b.jpg"), Some(exposure(1.0)))
            ]
        );
        assert!(!h.can_undo());
    }

    #[test]
    fn begin_holds_the_step_open_across_seals() {
        let mut h = EditHistory::default();
        h.begin();
        h.note(Path::new("/a.jpg"), exposure(0.0));
        h.seal();
        h.note(Path::new("/a.jpg"), exposure(0.2));
        h.end();
        h.seal();
        h.note(Path::new("/a.jpg"), exposure(0.4));
        h.seal();
        assert_eq!(h.pop_undo().unwrap().before[0].adj, Some(exposure(0.4)));
        assert_eq!(
            h.pop_undo().unwrap().before[0].adj,
            Some(exposure(0.0)),
            "one step"
        );
        assert!(h.pop_undo().is_none());
    }

    #[test]
    fn a_held_step_spans_seals_until_released() {
        let mut h = EditHistory::default();
        h.hold(true);
        h.note(Path::new("/a.jpg"), exposure(0.0));
        h.seal();
        h.note(Path::new("/b.jpg"), exposure(0.0));
        h.seal();
        h.hold(false);
        h.seal();
        assert_eq!(h.pop_undo().unwrap().before.len(), 2, "one step");
        assert!(h.pop_undo().is_none());
    }

    #[test]
    fn pop_takes_a_step_still_held_open() {
        let mut h = EditHistory::default();
        h.note(Path::new("/a.jpg"), exposure(0.0));
        h.seal();
        h.begin();
        h.note(Path::new("/a.jpg"), exposure(0.3));
        assert_eq!(h.pop_undo().unwrap().before[0].adj, Some(exposure(0.3)));
        assert_eq!(h.pop_undo().unwrap().before[0].adj, Some(exposure(0.0)));
    }

    #[test]
    fn the_oldest_steps_fall_off() {
        let mut h = EditHistory::default();
        for i in 0..=MAX_STEPS {
            h.note(Path::new("/a.jpg"), exposure(i as f32 / 1000.0));
            h.seal();
        }
        let mut n = 0;
        while let Some(step) = h.pop_undo() {
            n += 1;
            assert_ne!(
                step.before[0].adj,
                Some(exposure(0.0)),
                "the first step is gone"
            );
        }
        assert_eq!(n, MAX_STEPS);
    }

    #[test]
    fn a_marks_only_step_leaves_the_edit_alone() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.note_rating(a, 2);
        h.note_rating(a, 4);
        h.note_flag(a, None);
        let step = h.pop_undo().unwrap();
        let before = &step.before[0];
        assert_eq!(before.adj, None);
        assert_eq!(before.marks.rating, Some(2), "the first before wins");
        assert_eq!(before.marks.flag, Some(None));
        assert_eq!(before.marks.rotation, None);
    }

    #[test]
    fn an_edit_noted_after_a_mark_fills_the_edit() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.note_rating(a, 2);
        h.note(a, exposure(0.5));
        let step = h.pop_undo().unwrap();
        assert_eq!(step.before.len(), 1);
        assert_eq!(step.before[0].adj, Some(exposure(0.5)));
        assert_eq!(step.before[0].marks.rating, Some(2));
    }

    #[test]
    fn a_new_step_ends_the_chance_to_redo() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.note(a, exposure(0.0));
        h.seal();
        let undone = h.pop_undo().unwrap();
        h.push_redo(undone);
        assert!(h.can_redo());
        h.note_rating(a, 0);
        assert!(!h.can_redo());
        assert!(h.pop_redo().is_none());
    }

    #[test]
    fn redo_survives_until_a_new_step_and_returns_what_was_filed() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.note(a, exposure(0.0));
        h.seal();
        let step = h.pop_undo().unwrap();
        h.push_redo(EditStep::new(step.before, Vec::new()));
        let redone = h.pop_redo().unwrap();
        assert_eq!(redone.before[0].adj, Some(exposure(0.0)));
        h.push_done(redone);
        assert!(h.can_undo());
        assert!(!h.can_redo());
    }

    #[test]
    fn a_change_outside_the_history_reaches_the_redo_steps() {
        let mut h = EditHistory::default();
        let a = Path::new("/a.jpg");
        h.push_redo(EditStep::new(
            vec![Before {
                adj: Some(exposure(0.0)),
                ..Before::new(a.to_path_buf())
            }],
            Vec::new(),
        ));
        h.apply_outside(a, |adj| Adjustments {
            contrast: 9.0,
            ..adj
        });
        let step = h.pop_redo().unwrap();
        assert_eq!(step.before[0].adj.map(|adj| adj.contrast), Some(9.0));
    }

    #[test]
    fn clear_forgets_redo() {
        let mut h = EditHistory::default();
        h.push_redo(EditStep::new(
            vec![Before::new("/a.jpg".into())],
            Vec::new(),
        ));
        h.clear();
        assert!(!h.can_redo());
    }

    #[test]
    fn a_groups_first_state_in_a_step_wins_and_a_step_of_groups_alone_counts() {
        let mut h = EditHistory::default();
        let id = GroupId::from_stem("g-1".as_ref()).unwrap();
        let both = Group::new(vec!["a.jpg".into(), "b.jpg".into()], None).unwrap();
        h.note_group(&id, None);
        h.note_group(&id, Some(&both));
        let step = h.pop_undo().unwrap();
        assert_eq!(step.groups.len(), 1);
        assert_eq!(step.groups[0].group, None, "the first state wins");
        assert!(step.before.is_empty());
    }
}
