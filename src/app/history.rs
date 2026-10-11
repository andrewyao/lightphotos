//! Develop undo. Each step holds the edit every photo had before the step's
//! first change to it. One command is one step, and so is one slider drag:
//! a step stays open until the pointer is up at the end of a frame.

use std::path::{Path, PathBuf};

use crate::develop::Adjustments;

/// Steps older than this are dropped.
const MAX_STEPS: usize = 100;

#[derive(Default)]
pub(crate) struct EditHistory {
    done: Vec<EditStep>,
    open: Option<EditStep>,
    /// Nested `begin`s still running. A step cannot close while any are.
    depth: u32,
}

#[derive(Default)]
pub(crate) struct EditStep {
    pub(crate) before: Vec<(PathBuf, Adjustments)>,
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
        let step = self.open.get_or_insert_with(EditStep::default);
        if !step.before.iter().any(|(p, _)| p == path) {
            step.before.push((path.to_path_buf(), before));
        }
    }

    /// Closes the open step, unless a `begin` holds it.
    pub(super) fn seal(&mut self) {
        if self.depth > 0 {
            return;
        }
        if let Some(step) = self.open.take().filter(|s| !s.before.is_empty()) {
            if self.done.len() == MAX_STEPS {
                self.done.remove(0);
            }
            self.done.push(step);
        }
    }

    /// The last step, sealing an open one first.
    pub(super) fn pop(&mut self) -> Option<EditStep> {
        self.seal();
        self.done.pop()
    }

    /// For the menu bar's Undo item.
    #[cfg(any(test, target_os = "macos"))]
    pub(crate) fn can_undo(&self) -> bool {
        !self.done.is_empty() || self.open.is_some()
    }

    /// The photos leave the catalog's cache when the folder changes, so
    /// their steps go with them.
    pub(super) fn clear(&mut self) {
        self.done.clear();
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
        let step = h.pop().unwrap();
        assert_eq!(
            step.before,
            vec![
                (a.to_path_buf(), exposure(0.0)),
                (PathBuf::from("/b.jpg"), exposure(1.0))
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
        assert_eq!(h.pop().unwrap().before[0].1, exposure(0.4));
        assert_eq!(h.pop().unwrap().before[0].1, exposure(0.0), "one step");
        assert!(h.pop().is_none());
    }

    #[test]
    fn the_oldest_steps_fall_off() {
        let mut h = EditHistory::default();
        for i in 0..=MAX_STEPS {
            h.note(Path::new("/a.jpg"), exposure(i as f32 / 1000.0));
            h.seal();
        }
        let mut n = 0;
        while let Some(step) = h.pop() {
            n += 1;
            assert_ne!(step.before[0].1, exposure(0.0), "the first step is gone");
        }
        assert_eq!(n, MAX_STEPS);
    }
}
