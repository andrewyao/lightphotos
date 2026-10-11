//! Face analysis for grouped photos, and the eyes-closed filter it feeds.
//! Vision runs on `FacePool`'s workers. Results also go to the signal cache,
//! so a folder's next visit seeds them instead of running Vision again.

use super::*;
use std::path::{Path, PathBuf};

use crate::persist::signalcache::Signal;
use crate::scoring::facequality::{FacePool, FaceQuality};

pub(super) struct Faces {
    /// `None` until the window is created, and on targets that cannot spawn
    /// threads.
    pool: Option<FacePool>,
    quality: HashMap<PathBuf, FaceQuality>,
    pending: HashSet<PathBuf>,
    /// Analyses that failed for good (corrupt or unsupported files).
    failed: HashSet<PathBuf>,
    /// The grid was rebuilt since grouped photos were last submitted.
    unscanned: bool,
    /// Show only photos with a detected blink. Stacks with the star filter. A
    /// photo the face pass hasn't reached stays hidden.
    eyes_filter: bool,
}

impl Faces {
    pub(super) fn new() -> Self {
        Self {
            pool: None,
            quality: HashMap::new(),
            pending: HashSet::new(),
            failed: HashSet::new(),
            unscanned: false,
            eyes_filter: false,
        }
    }

    /// Asks for the grouped photos to be scanned on the next request.
    pub(super) fn mark_unscanned(&mut self) {
        self.unscanned = true;
    }

    /// A result the signal cache saved. One computed this session wins.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn seed(&mut self, path: PathBuf, quality: FaceQuality) {
        // Cached before faces had a frame, so Compare would have none to
        // start on: analyze it again.
        if quality.faces > 0 && quality.frame.is_none() {
            return;
        }
        self.quality.entry(path).or_insert(quality);
    }

    #[cfg(test)]
    pub(super) fn submitted(&self) -> usize {
        self.pending.len()
    }
}

impl App {
    /// Starts the analysis workers. The window's setup calls it, so a test's
    /// App has none.
    pub(crate) fn start_face_pool(&mut self) {
        self.faces.pool = FacePool::new();
    }

    /// Returns true while any analysis is outstanding.
    pub(crate) fn request_face_quality(&mut self) -> bool {
        // The folder's cached analyses are still loading, and each one found
        // there is a Vision pass saved.
        #[cfg(not(target_arch = "wasm32"))]
        if self.signal_load_rx.is_some() {
            return true;
        }
        if std::mem::take(&mut self.faces.unscanned) && self.faces.pool.is_some() {
            let to_submit = self.face_candidates();
            if let Some(pool) = &self.faces.pool {
                for p in to_submit {
                    self.faces.pending.insert(p.clone());
                    pool.submit(p);
                }
            }
        }
        !self.faces.pending.is_empty()
    }

    fn face_candidates(&self) -> Vec<PathBuf> {
        let (Some(pl), Some(groups)) = (self.playlist.as_ref(), self.catalog.groups()) else {
            return Vec::new();
        };
        groups
            .iter()
            .flat_map(|(_, g)| g.members())
            .map(|name| pl.dir().join(name))
            .filter(|p| {
                !self.faces.quality.contains_key(p)
                    && !self.faces.pending.contains(p)
                    && !self.faces.failed.contains(p)
            })
            .collect()
    }

    pub(crate) fn poll_face_quality(&mut self) {
        let outcomes = self
            .faces
            .pool
            .as_ref()
            .map(|p| p.poll())
            .unwrap_or_default();
        if outcomes.is_empty() {
            return;
        }
        let mut changed = false;
        for o in outcomes {
            self.faces.pending.remove(&o.path);
            match o.result {
                Ok(q) => {
                    self.signals.record(&o.path, Signal::Faces(q));
                    self.faces.quality.insert(o.path, q);
                    changed = true;
                }
                Err(_) => {
                    self.faces.failed.insert(o.path);
                }
            }
        }
        if changed {
            if self.eyes_filter_on() {
                self.recompute_visible();
            }
            self.request_redraw();
        }
    }

    /// The square around `path`'s faces: `Some(None)` when it has none or
    /// can't be analyzed, `None` while the analysis runs. Queues it when it
    /// was never asked for, such as a photo in no group.
    pub(super) fn face_frame(
        &mut self,
        path: &Path,
    ) -> Option<Option<crate::scoring::facequality::FaceFrame>> {
        if let Some(q) = self.faces.quality.get(path) {
            return Some(q.frame);
        }
        let Some(pool) = &self.faces.pool else {
            return Some(None);
        };
        if self.faces.failed.contains(path) {
            return Some(None);
        }
        if self.faces.pending.insert(path.to_path_buf()) {
            pool.submit(path.to_path_buf());
        }
        None
    }

    /// The face analysis for a path. `None` while pending, after a failure, or
    /// when never requested.
    pub(super) fn face_quality_of(
        &self,
        path: &Path,
    ) -> Option<crate::scoring::facequality::FaceQuality> {
        self.faces.quality.get(path).copied()
    }

    pub(super) fn eyes_closed(&self, path: &Path) -> bool {
        self.face_quality_of(path)
            .and_then(|q| q.eye_state())
            .is_some_and(|s| s == crate::scoring::facequality::EyeState::Closed)
    }

    pub(crate) fn eyes_filter_on(&self) -> bool {
        self.faces.eyes_filter
    }

    pub(super) fn toggle_eyes_filter(&mut self) {
        self.faces.eyes_filter = !self.faces.eyes_filter;
        self.recompute_visible();
        self.request_redraw();
    }

    pub(super) fn reset_eyes_filter(&mut self) {
        self.faces.eyes_filter = false;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn face_quality_candidates_are_every_grouped_photo() {
        use crate::app::nav::tests::group_photos;
        use crate::app::test_support::folder_app;
        let (mut app, dir, paths) = folder_app("faces-scope", 8);
        group_photos(&mut app, &[1, 2], 1);
        group_photos(&mut app, &[5, 6, 7], 7);
        app.faces.quality.insert(
            paths[6].clone(),
            crate::scoring::facequality::FaceQuality {
                faces: 0,
                min_eye_openness: None,
                frame: None,
            },
        );
        let mut got = app.face_candidates();
        got.sort();
        assert_eq!(
            got,
            vec![
                paths[1].clone(),
                paths[2].clone(),
                paths[5].clone(),
                paths[7].clone()
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cached_analysis_with_faces_but_no_frame_runs_again() {
        use crate::scoring::facequality::{FaceFrame, FaceQuality};
        let mut faces = super::Faces::new();
        let old = FaceQuality {
            faces: 2,
            min_eye_openness: None,
            frame: None,
        };
        faces.seed("old.jpg".into(), old);
        assert!(faces.quality.is_empty(), "no frame for Compare to start on");
        let none = FaceQuality { faces: 0, ..old };
        faces.seed("empty.jpg".into(), none);
        let framed = FaceQuality {
            frame: Some(FaceFrame {
                center: (0.5, 0.5),
                side: 0.2,
            }),
            ..old
        };
        faces.seed("new.jpg".into(), framed);
        assert_eq!(faces.quality.len(), 2);
    }

    #[test]
    fn grouped_photos_are_scanned_once_per_rebuild() {
        use crate::app::test_support::folder_app;
        let (mut app, dir, _) = folder_app("faces-once", 2);
        assert!(app.faces.unscanned, "a rebuild asks for a scan");
        app.request_face_quality();
        assert!(!app.faces.unscanned, "the scan is taken");
        app.recompute_visible();
        assert!(app.faces.unscanned);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
