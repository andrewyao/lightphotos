// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Develop panel's Optics section: Remove Chromatic Aberration decodes
//! and measures the photo on screen on its own thread, then stores the
//! scales as an edit.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use super::{App, CaState, StatusKind};
use crate::develop::{Adjustments, CaScale};
#[cfg(not(target_arch = "wasm32"))]
use crate::jobs::thumbnail::EmbeddedPreview;

/// The size the measurement decodes at. `chroma`'s search window is sized
/// for it: at full resolution the corner fringes run past `MAX_SHIFT`.
const MEASURE_PX: u32 = 2560;

type Outcome = (PathBuf, Result<CaScale, String>);

/// One photo waiting for its measurement.
#[derive(Debug, PartialEq)]
struct Measure {
    path: PathBuf,
    /// Asked for in the folder showing, so its result joins that folder's
    /// undo step and an undo there drops it. Cleared on a folder change,
    /// which clears the history; a photo carried over lands without undo,
    /// and nothing in the new folder stops it.
    here: bool,
}

pub(super) struct Optics {
    /// The photo being measured, so its checkbox shows on while it runs.
    pending: Option<Measure>,
    /// Photos waiting their turn. One is measured at a time, since each
    /// decode can take a large share of memory.
    queue: VecDeque<Measure>,
    /// Photos measured in this run, for the "n of m" status.
    done: usize,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
}

impl Optics {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            pending: None,
            queue: VecDeque::new(),
            done: 0,
            tx,
            rx,
        }
    }

    fn all(&self) -> impl Iterator<Item = &Measure> {
        self.pending.iter().chain(&self.queue)
    }

    fn waiting(&self, path: &Path) -> bool {
        self.all().any(|m| m.path == path)
    }

    fn is_running(&self) -> bool {
        self.pending.is_some() || !self.queue.is_empty()
    }

    /// Whether photos asked for in the folder showing are still waiting.
    fn running_here(&self) -> bool {
        self.all().any(|m| m.here)
    }

    /// Whether the folder showing's run keeps the open undo step open, so
    /// the whole run undoes as one step.
    pub(super) fn holds_undo_step(&self) -> bool {
        self.running_here()
    }

    /// The folder showing is about to change. Waiting photos carry on,
    /// because each result goes to its photo's own sidecar. Their undo step
    /// goes with the old folder's history.
    pub(super) fn leave_folder(&mut self) {
        for m in self.pending.iter_mut().chain(self.queue.iter_mut()) {
            m.here = false;
        }
    }
}

impl App {
    /// The Remove Chromatic Aberration checkbox over the photos the panel
    /// acts on: on once measured, and while waiting to be measured.
    pub(crate) fn remove_ca_state(&self) -> CaState {
        let (mut total, mut on) = (0, 0);
        for p in self.action_path_refs() {
            total += 1;
            on += usize::from(
                self.optics.waiting(p)
                    || self
                        .edits
                        .get(p)
                        .is_some_and(|a| a.chromatic_aberration.is_some()),
            );
        }
        match on {
            0 => CaState::Off,
            n if n == total => CaState::On,
            _ => CaState::Mixed,
        }
    }

    /// Whether any measurement is still going, this folder's or one
    /// carried over from another, so the frame loop keeps polling.
    pub(crate) fn remove_ca_pending(&self) -> bool {
        self.optics.is_running()
    }

    /// Whether this folder's photos are still being measured, which holds
    /// the Remove Chromatic Aberration control. A run carried over from
    /// another folder does not.
    pub(crate) fn remove_ca_pending_here(&self) -> bool {
        self.optics.running_here()
    }

    /// Turn Remove Chromatic Aberration on or off for every photo the panel
    /// acts on. Turning it on measures the photos that lack it, one by one.
    pub(super) fn set_remove_ca(&mut self, on: bool) {
        self.set_ca_for(&self.action_paths(), on);
    }

    /// Turn the correction on or off for `paths`. Turning it off also drops
    /// their queued measurements, so none lands later and turns it back on.
    pub(super) fn set_ca_for(&mut self, paths: &[PathBuf], on: bool) {
        if paths.is_empty() || self.refuse_if_read_only() {
            return;
        }
        if !on {
            self.optics.queue.retain(|m| !paths.contains(&m.path));
            if self
                .optics
                .pending
                .as_ref()
                .is_some_and(|m| paths.contains(&m.path))
            {
                self.optics.pending = None;
            }
            self.edit_each(paths, |adj| Adjustments {
                chromatic_aberration: None,
                ..adj
            });
            self.start_next_measure();
            return;
        }
        if !self.remove_ca_pending() {
            self.optics.done = 0;
        }
        for path in paths.iter().cloned() {
            let has = self
                .edits
                .get(&path)
                .is_some_and(|a| a.chromatic_aberration.is_some());
            // A photo still waiting from a run carried over from another
            // folder becomes this folder's again, so its result joins the
            // undo step and an undo here stops it.
            let waiting = self
                .optics
                .pending
                .iter_mut()
                .chain(self.optics.queue.iter_mut());
            if let Some(m) = waiting.into_iter().find(|m| m.path == path) {
                m.here = true;
            } else if !has {
                self.optics.queue.push_back(Measure { path, here: true });
            }
        }
        self.start_next_measure();
    }

    /// Drops the folder showing's waiting and running measurements, for an
    /// undo that takes back their step. A result still on its way is
    /// dropped when it lands. Photos carried over from another folder have
    /// no step here to take back, so they keep going.
    pub(super) fn cancel_remove_ca(&mut self) {
        self.optics.queue.retain(|m| !m.here);
        if self.optics.pending.as_ref().is_some_and(|m| m.here) {
            self.optics.pending = None;
        }
        self.start_next_measure();
    }

    /// Start measuring the next queued photo unless one is under way. A photo
    /// that cannot start is reported and skipped.
    fn start_next_measure(&mut self) {
        while self.optics.pending.is_none() {
            let Some(next) = self.optics.queue.pop_front() else {
                return;
            };
            match self.start_measure(next.path.clone()) {
                Ok(()) => {
                    self.optics.pending = Some(next);
                    self.show_measure_progress();
                }
                Err(e) => self.set_status(
                    StatusKind::Error,
                    format!("{}: {e}", crate::i18n::t().remove_ca_failed),
                ),
            }
        }
    }

    fn show_measure_progress(&mut self) {
        let t = crate::i18n::t();
        let total = self.optics.done + 1 + self.optics.queue.len();
        let text = if total > 1 {
            (t.remove_ca_measuring_n)(self.optics.done + 1, total)
        } else {
            t.remove_ca_measuring.into()
        };
        self.set_status(StatusKind::Progress, text);
    }

    /// Decode and measure `path` off the UI thread, sending the outcome to
    /// `optics.tx`.
    ///
    /// Not the Loupe's cached image: that can be the camera's embedded JPEG,
    /// which the camera has already corrected and may frame differently from
    /// the sensor data the edit applies to.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_measure(&self, path: PathBuf) -> Result<(), String> {
        let tx = self.optics.tx.clone();
        std::thread::Builder::new()
            .name("chroma-measure".to_string())
            .spawn(move || {
                let scale = crate::jobs::thumbnail::decode_at_size(
                    &path,
                    MEASURE_PX,
                    EmbeddedPreview::Never,
                )
                .map(|img| crate::develop::chroma::measure(&img));
                let _ = tx.send((path, scale));
            })
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// The browser cannot open a file by path off the main thread, so this
    /// reads the bytes here and hands them to the decode pool, whose result
    /// `poll_remove_ca` forwards to `optics.tx`.
    #[cfg(target_arch = "wasm32")]
    fn start_measure(&self, path: PathBuf) -> Result<(), String> {
        let handle = self
            .web
            .file_handles()
            .get(&path)
            .cloned()
            .ok_or("the file is not open")?;
        let pool = self
            .loader
            .as_ref()
            .map(|l| l.web_decoder())
            .ok_or("no decode threads started")?;
        let tx = self.optics.tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::web::web_fs::read_bytes(&handle).await {
                Ok(bytes) => pool.submit_measure(crate::web::web_decode::WebMeasureJob {
                    is_raw: crate::decode::image_decode::is_raw_extension(&path),
                    path,
                    bytes: std::sync::Arc::new(bytes),
                    max_px: MEASURE_PX,
                }),
                Err(e) => {
                    let _ = tx.send((path, Err(e)));
                }
            }
        });
        Ok(())
    }

    fn store_measure(&mut self, path: &Path, scale: CaScale, here: bool) {
        let base = self.edits.get(path).copied().unwrap_or_default();
        let merged = Adjustments {
            chromatic_aberration: Some(scale),
            ..base
        };
        if here {
            self.put_edit(path, merged);
        } else {
            self.store_edit(path, merged);
            self.keep_through_undo(path, base, merged);
        }
        self.save_patch(
            path,
            crate::persist::folder_store::Patch::adjustments(base, merged),
        );
        #[cfg(target_arch = "wasm32")]
        if merged != base {
            crate::web::analytics::property("develop_edit_applied", "edit_kind", "adjustment");
        }
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist.invalidate();
        }
    }

    /// Store a finished measurement on its photo and start the next. A
    /// result for a photo whose box was cleared meanwhile is dropped. The
    /// photo's folder need not be the one showing: see `save_patch`.
    pub(crate) fn poll_remove_ca(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(loader) = self.loader.as_mut() {
            for outcome in loader.take_web_measures() {
                let _ = self.optics.tx.send(outcome);
            }
        }
        while let Ok((path, scale)) = self.optics.rx.try_recv() {
            let Some(done) = self.optics.pending.take_if(|m| m.path == path) else {
                continue;
            };
            self.optics.done += 1;
            match scale {
                Ok(scale) => {
                    self.store_measure(&path, scale, done.here);
                    if self.optics.queue.is_empty() {
                        self.set_status(
                            StatusKind::Success,
                            crate::i18n::t().remove_ca_applied.into(),
                        );
                    }
                }
                Err(e) => self.set_status(
                    StatusKind::Error,
                    format!("{}: {e}", crate::i18n::t().remove_ca_failed),
                ),
            }
            self.start_next_measure();
            self.request_redraw();
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Remove CA measures every photo that lacks it, one at a time, skipping
    /// a photo it cannot decode, and clears them all again.
    /// Pasting Optics off a photo without the correction drops queued
    /// measurements, so none lands later and undoes the paste.
    #[test]
    fn pasting_optics_off_cancels_queued_measurements() {
        use crate::develop::copy::{CopyItem, CopyItems};
        let (mut app, dir, paths) = crate::app::test_support::folder_app("ca-paste", 3);
        app.set_ca_for(&paths, true);
        assert!(app.remove_ca_pending());
        let optics = CopyItems::NONE.with(CopyItem::RemoveCa, true);
        app.apply_look_to(&Adjustments::default(), optics, &paths);
        assert!(!app.remove_ca_pending());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_remove_ca_run_undoes_in_one_step() {
        let (mut app, dir, paths) = crate::app::test_support::folder_app("ca-undo", 2);
        app.selected = (0..2).collect();
        app.sel = Some(0);
        for path in &paths {
            crate::decode::image_encode::encode_jpeg(
                path,
                64,
                48,
                &vec![128u8; 64 * 48 * 4],
                crate::decode::image_encode::JpegQuality::Export,
            )
            .unwrap();
        }
        app.set_remove_ca(true);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.remove_ca_pending() {
            assert!(std::time::Instant::now() < deadline, "measuring timed out");
            app.poll_remove_ca();
            app.save_edit_unless_dragging();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.save_edit_unless_dragging();
        assert!(paths.iter().all(|p| app
            .folder_store
            .adjustments(p)
            .chromatic_aberration
            .is_some()));

        assert_eq!(app.undo_edit(), 2, "both photos in one step");
        assert!(paths
            .iter()
            .all(|p| app.folder_store.adjustments(p).is_identity()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A run started in folder A finishes after the user opens folder B.
    /// The result lands in A's sidecar with the rest of its record, and B's
    /// same-named photo is not touched. An undo in B neither takes it back
    /// nor stops it.
    #[test]
    fn a_run_finishes_in_its_own_folder_after_a_folder_change() {
        use crate::app::test_support::{folder_of, load_folder};
        let (dir_a, a) = folder_of("ca-switch-a", &["a.jpg".into(), "b.jpg".into()]);
        for path in &a {
            crate::decode::image_encode::encode_jpeg(
                path,
                64,
                48,
                &vec![128u8; 64 * 48 * 4],
                crate::decode::image_encode::JpegQuality::Export,
            )
            .unwrap();
        }
        let mut app = App::new(None);
        load_folder(&mut app, &dir_a);
        app.folder_store.set(&a[0], 3);
        let (dir_b, twins) = folder_of("ca-switch-b", &["a.jpg".into()]);
        let twin = twins[0].clone();
        {
            let mut store_b = crate::persist::folder_store::FolderStore::with_dir(dir_b.clone());
            store_b.set(&twin, 4);
        }
        app.selected = (0..2).collect();
        app.sel = Some(0);
        app.set_remove_ca(true);
        assert!(app.remove_ca_pending());

        app.run(crate::command::Command::OpenFolder(
            crate::command::OpenFolderParams {
                path: dir_b.clone(),
            },
        ))
        .unwrap();
        crate::app::test_support::wait_for_folder_store(&mut app);
        assert!(
            app.remove_ca_pending(),
            "a folder change must not stop the run"
        );
        assert!(
            !app.remove_ca_pending_here(),
            "A's run does not hold B's Remove CA control"
        );
        // A run started in B queues behind A's. An undo in B takes back
        // B's and leaves A's going.
        app.selected = std::collections::BTreeSet::from([0]);
        app.sel = Some(0);
        app.set_remove_ca(true);
        assert!(app.remove_ca_pending_here());
        assert_eq!(app.undo_edit(), 0, "B's history holds none of A's run");
        assert!(app.remove_ca_pending(), "and an undo in B does not stop it");
        assert!(!app.optics.waiting(&twin), "but it drops B's own");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.remove_ca_pending() {
            assert!(std::time::Instant::now() < deadline, "measuring timed out");
            app.poll_remove_ca();
            app.save_edit_unless_dragging();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        for path in &a {
            let saved = crate::sidecar::read_record(path).unwrap();
            assert!(
                saved.adjustments.chromatic_aberration.is_some(),
                "{path:?} is corrected in its own sidecar"
            );
        }
        assert_eq!(
            crate::sidecar::read_record(&a[0]).unwrap().rating,
            Some(3),
            "and keeps the rest of its record"
        );
        let saved_twin = crate::sidecar::read_record(&twin).unwrap();
        assert!(
            saved_twin.adjustments.is_identity(),
            "B's same-named photo is not corrected"
        );
        assert_eq!(saved_twin.rating, Some(4));
        assert!(app.folder_store.adjustments(&twin).is_identity());
        assert_eq!(
            app.folder_store.take_error(),
            None,
            "no sidecar write failed"
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// Back in folder A while A's run still goes, asking for its photos
    /// again makes them this folder's: their results join the undo step,
    /// and an undo stops them.
    #[test]
    fn asking_again_in_its_own_folder_makes_a_carried_over_photo_undoable() {
        use crate::app::test_support::{folder_of, load_folder, temp_folder};
        let (dir_a, a) = folder_of("ca-again-a", &["a.jpg".into(), "b.jpg".into()]);
        let mut app = App::new(None);
        load_folder(&mut app, &dir_a);
        app.selected = (0..2).collect();
        app.sel = Some(0);
        app.set_remove_ca(true);
        let dir_b = temp_folder("ca-again-b");
        load_folder(&mut app, &dir_b);
        load_folder(&mut app, &dir_a);
        assert!(app.optics.waiting(&a[1]), "A's run is still going");
        assert!(!app.remove_ca_pending_here());

        app.selected = (0..2).collect();
        app.sel = Some(0);
        app.set_remove_ca(true);
        assert!(app.remove_ca_pending_here(), "A's photos are A's again");
        app.undo_edit();
        assert!(!app.remove_ca_pending(), "and an undo stops them");
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[test]
    fn remove_ca_measures_the_selection_in_turn() {
        let (mut app, dir, paths) = crate::app::test_support::folder_app("ca-many", 3);
        app.selected = (0..3).collect();
        app.sel = Some(0);
        let pixels = vec![128u8; 64 * 48 * 4];
        crate::decode::image_encode::encode_jpeg(
            &paths[0],
            64,
            48,
            &pixels,
            crate::decode::image_encode::JpegQuality::Export,
        )
        .unwrap();
        // paths[2] stays an empty file, which cannot be decoded.
        let scale = crate::develop::CaScale {
            red: 0.001,
            blue: -0.001,
        };
        app.edits.insert(
            paths[1].clone(),
            Adjustments {
                chromatic_aberration: Some(scale),
                ..Default::default()
            },
        );
        assert_eq!(app.remove_ca_state(), CaState::Mixed);

        app.set_remove_ca(true);
        assert_eq!(
            app.remove_ca_state(),
            CaState::On,
            "waiting photos count as on"
        );
        let here = |path: &PathBuf| Measure {
            path: path.clone(),
            here: true,
        };
        assert_eq!(app.optics.pending, Some(here(&paths[0])));
        assert_eq!(app.optics.queue, [here(&paths[2])], "one at a time");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.remove_ca_pending() {
            assert!(std::time::Instant::now() < deadline, "measuring timed out");
            app.poll_remove_ca();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ca = |app: &App, i: usize| {
            app.edits
                .get(&paths[i])
                .and_then(|a| a.chromatic_aberration)
        };
        assert!(ca(&app, 0).is_some(), "the JPEG was measured");
        assert_eq!(ca(&app, 1), Some(scale), "a measured photo is left alone");
        assert!(
            ca(&app, 2).is_none(),
            "the empty file failed and was skipped"
        );
        assert_eq!(app.remove_ca_state(), CaState::Mixed);

        app.selected = BTreeSet::from([0, 1]);
        assert_eq!(app.remove_ca_state(), CaState::On);
        app.set_remove_ca(false);
        assert!(ca(&app, 0).is_none() && ca(&app, 1).is_none());
        assert_eq!(app.remove_ca_state(), CaState::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
