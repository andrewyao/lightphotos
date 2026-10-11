//! Develop undo. Each step holds the edit every photo had before the step's
//! first change to it, and its touch-ups when the step cleared them. One
//! command is one step, and so is one slider drag or one Auto Tone or Remove
//! Chromatic Aberration batch: a step stays open until the pointer is up at
//! the end of a frame and no batch is running.

use std::path::{Path, PathBuf};

use crate::develop::{Adjustments, TouchUp};

/// Steps older than this are dropped.
const MAX_STEPS: usize = 100;

#[derive(Default)]
pub(crate) struct EditHistory {
    done: Vec<EditStep>,
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
}

pub(crate) struct Before {
    pub(crate) path: PathBuf,
    pub(crate) adj: Adjustments,
    /// `None` when the step left the touch-ups alone.
    pub(crate) touchups: Option<Vec<TouchUp>>,
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
        self.entry(path, before);
    }

    /// As [`note`](Self::note), and records `touchups` as `path`'s spots
    /// ahead of a change to them, unless the open step already holds some.
    pub(super) fn note_touchups(&mut self, path: &Path, adj: Adjustments, touchups: &[TouchUp]) {
        self.entry(path, adj)
            .touchups
            .get_or_insert_with(|| touchups.to_vec());
    }

    fn entry(&mut self, path: &Path, adj: Adjustments) -> &mut Before {
        let step = self.open.get_or_insert_with(EditStep::default);
        let i = match step.before.iter().position(|b| b.path == path) {
            Some(i) => i,
            None => {
                step.before.push(Before {
                    path: path.to_path_buf(),
                    adj,
                    touchups: None,
                });
                step.before.len() - 1
            }
        };
        &mut step.before[i]
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
        if let Some(step) = self.open.take().filter(|s| !s.before.is_empty()) {
            if self.done.len() == MAX_STEPS {
                self.done.remove(0);
            }
            self.done.push(step);
        }
    }

    /// The last step. An open one is the last, even while a drag or a
    /// `begin` still holds it.
    pub(super) fn pop(&mut self) -> Option<EditStep> {
        self.close();
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
        let firsts: Vec<_> = step.before.iter().map(|b| (&*b.path, b.adj)).collect();
        assert_eq!(
            firsts,
            vec![(a, exposure(0.0)), (Path::new("/b.jpg"), exposure(1.0))]
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
        assert_eq!(h.pop().unwrap().before[0].adj, exposure(0.4));
        assert_eq!(h.pop().unwrap().before[0].adj, exposure(0.0), "one step");
        assert!(h.pop().is_none());
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
        assert_eq!(h.pop().unwrap().before.len(), 2, "one step");
        assert!(h.pop().is_none());
    }

    #[test]
    fn pop_takes_a_step_still_held_open() {
        let mut h = EditHistory::default();
        h.note(Path::new("/a.jpg"), exposure(0.0));
        h.seal();
        h.begin();
        h.note(Path::new("/a.jpg"), exposure(0.3));
        assert_eq!(h.pop().unwrap().before[0].adj, exposure(0.3));
        assert_eq!(h.pop().unwrap().before[0].adj, exposure(0.0));
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
            assert_ne!(step.before[0].adj, exposure(0.0), "the first step is gone");
        }
        assert_eq!(n, MAX_STEPS);
    }
}
