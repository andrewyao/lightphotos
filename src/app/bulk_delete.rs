// SPDX-License-Identifier: GPL-3.0-or-later

//! Deleting a selection, paced across frames.
//!
//! Two costs used to land in one frame: the trash call per photo, which is
//! filesystem metadata I/O, and `finish_delete`'s reconciliation, which is
//! O(playlist) and regroups duplicates. This module moves the first off the
//! UI thread and splits the second by what it indexes.
//!
//! A photo's removal divides in two. The path-keyed half (ratings, edits,
//! rotations, the catalog record, the perceptual caches) is O(1) per photo
//! and runs as each result lands. The half that shifts playlist indices
//! (`playlist.remove_matching`, the `feature_*` retains, the duplicate and
//! burst marks, the selection) runs exactly once, at the end. In between,
//! trashed photos sit in `gone` and [`App::recompute_visible`] filters them
//! out, so the grid shrinks live while every structure indexed by playlist
//! position stays valid.

use super::*;
use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

/// A finished trash call.
type DeleteResult = (PathBuf, Result<(), String>);

/// Browser deletions in flight at once.
///
/// wasm has no worker thread, so `poll_delete` is the scheduler and this is
/// the real concurrency cap. The old code detached one `spawn_local` per path
/// up front, so a 20 000-photo delete handed the browser 20 000 open file
/// operations.
#[cfg(target_arch = "wasm32")]
const DELETE_WINDOW: usize = 32;

/// One running bulk delete. At most one exists, which `Option<BulkDelete>`
/// on [`App`] makes unrepresentable to break: two batches would each hold a
/// `gone` set and `recompute_visible` could not answer from either alone.
pub(crate) struct BulkDelete {
    /// Captured at batch start, not yet handed to a worker.
    queue: VecDeque<PathBuf>,
    /// Submitted, no result yet.
    in_flight: HashSet<PathBuf>,
    /// Trashed and dropped from every path-keyed map, but still in the
    /// playlist. See the module docs.
    gone: HashSet<PathBuf>,
    /// Set when `gone` grew since the last `recompute_visible`.
    view_dirty: bool,
    total: usize,
    errors: usize,
    last_err: Option<String>,

    done_tx: Sender<DeleteResult>,
    done_rx: Receiver<DeleteResult>,
    /// Dropped when the batch ends, which is what stops the worker thread.
    #[cfg(not(target_arch = "wasm32"))]
    submit: Option<Sender<PathBuf>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<std::thread::JoinHandle<()>>,
    /// Set to stop the worker between trash calls, so abandoning a batch waits
    /// for one filesystem operation rather than for everything submitted.
    #[cfg(not(target_arch = "wasm32"))]
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,

    /// wasm has no OS paths, so the folder the batch started in is captured
    /// here and its sidecar cleanup still lands there after a navigation.
    #[cfg(target_arch = "wasm32")]
    origin_dir: PathBuf,
    #[cfg(target_arch = "wasm32")]
    origin_handle: web_sys::FileSystemDirectoryHandle,
}

impl BulkDelete {
    /// Trashed but not yet dropped from the playlist. `recompute_visible` is
    /// the only reader.
    pub(super) fn gone(&self) -> &HashSet<PathBuf> {
        &self.gone
    }

    fn done(&self) -> usize {
        self.gone.len() + self.errors
    }

    fn finished(&self) -> bool {
        self.queue.is_empty() && self.in_flight.is_empty()
    }

    fn take_result(&self) -> Option<DeleteResult> {
        self.done_rx.try_recv().ok()
    }

    /// Hand the worker everything it will take right now.
    #[cfg(not(target_arch = "wasm32"))]
    fn admit(&mut self) {
        let Some(submit) = self.submit.as_ref() else {
            return;
        };
        while let Some(path) = self.queue.pop_front() {
            if submit.send(path.clone()).is_err() {
                let _ = self
                    .done_tx
                    .send((path, Err("the trash worker stopped".to_string())));
                continue;
            }
            self.in_flight.insert(path);
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn admit(&mut self) {
        while self.in_flight.len() < DELETE_WINDOW {
            let Some(path) = self.queue.pop_front() else {
                return;
            };
            let Some(name) = path.file_name().map(std::ffi::OsString::from) else {
                let _ = self
                    .done_tx
                    .send((path, Err("path has no file name".to_string())));
                continue;
            };
            let handle = self.origin_handle.clone();
            let tx = self.done_tx.clone();
            self.in_flight.insert(path.clone());
            wasm_bindgen_futures::spawn_local(async move {
                let result = crate::web_catalog_fs::remove_file(&handle, &name).await;
                let _ = tx.send((path, result));
            });
        }
    }
}

impl App {
    pub(super) fn delete_selection(&mut self) {
        self.start_delete(self.selected_paths());
    }

    /// Begin trashing `paths`. Refused while another delete or an export is
    /// running: the export reads bytes from files this batch would remove.
    fn start_delete(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        if self.bulk_delete.is_some() || self.export_progress.is_some() {
            self.set_status(crate::i18n::t().delete_in_progress.to_string());
            return;
        }
        let total = paths.len();
        let (done_tx, done_rx) = channel();

        #[cfg(target_arch = "wasm32")]
        let (origin_dir, origin_handle) = {
            let dir = paths[0].parent().unwrap_or(Path::new("")).to_path_buf();
            let Some(handle) = self.web_dir_handles.get(&dir).cloned() else {
                self.set_status((crate::i18n::t().delete_no_handle)(
                    &dir.display().to_string(),
                ));
                return;
            };
            (dir, handle)
        };

        #[cfg(not(target_arch = "wasm32"))]
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(not(target_arch = "wasm32"))]
        let (submit, worker) = match spawn_trash_worker(done_tx.clone(), stop.clone()) {
            Some((submit, worker)) => (Some(submit), Some(worker)),
            None => (None, None),
        };

        self.bulk_delete = Some(BulkDelete {
            queue: paths.into(),
            in_flight: HashSet::new(),
            gone: HashSet::new(),
            view_dirty: false,
            total,
            errors: 0,
            last_err: None,
            done_tx,
            done_rx,
            #[cfg(not(target_arch = "wasm32"))]
            submit,
            #[cfg(not(target_arch = "wasm32"))]
            worker,
            #[cfg(not(target_arch = "wasm32"))]
            stop,
            #[cfg(target_arch = "wasm32")]
            origin_dir,
            #[cfg(target_arch = "wasm32")]
            origin_handle,
        });
        self.selected.clear();
        self.anchor = None;
        self.poll_delete();
    }

    /// Advance a running delete by one frame: submit what the worker will
    /// take, forget the photos whose trash call landed, hide them from the
    /// grid, and reconcile the playlist once the batch is done.
    ///
    /// The UI-thread cost of a frame is one path-keyed removal per result
    /// plus at most one `recompute_visible`. It does not depend on how fast
    /// the trash calls run, which is why it needs no wall-clock budget.
    pub(crate) fn poll_delete(&mut self) {
        if self.bulk_delete.is_none() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        let origin = {
            let d = self.bulk_delete.as_ref().unwrap();
            (d.origin_dir.clone(), d.origin_handle.clone())
        };

        let mut trashed: Vec<PathBuf> = Vec::new();
        {
            let d = self.bulk_delete.as_mut().unwrap();
            d.admit();
            while let Some((path, result)) = d.take_result() {
                d.in_flight.remove(&path);
                match result {
                    Ok(()) => {
                        d.gone.insert(path.clone());
                        d.view_dirty = true;
                        trashed.push(path);
                    }
                    Err(e) => {
                        eprintln!("[lightphotos] trash failed for {}: {e}", path.display());
                        d.errors += 1;
                        d.last_err = Some(e);
                    }
                }
            }
        }
        for path in &trashed {
            self.forget_photo(
                path,
                #[cfg(target_arch = "wasm32")]
                &origin.0,
                #[cfg(target_arch = "wasm32")]
                &origin.1,
            );
        }

        let view_dirty = std::mem::take(&mut self.bulk_delete.as_mut().unwrap().view_dirty);
        if view_dirty {
            self.recompute_visible();
            self.after_visible_shrank();
        }

        if self.bulk_delete.as_ref().unwrap().finished() {
            let d = self.bulk_delete.take().unwrap();
            self.prune_deleted(d);
        } else {
            let d = self.bulk_delete.as_ref().unwrap();
            self.set_status((crate::i18n::t().deleting)(d.done(), d.total));
        }
        self.request_redraw();
    }

    /// Abandon a running delete, keeping what it already trashed. Called on
    /// every folder change, because the reconciliation below is about to lose
    /// the playlist it applies to. Trash calls already handed to a worker are
    /// not recalled; those files really are being deleted.
    pub(super) fn cancel_delete(&mut self) {
        let Some(d) = self.bulk_delete.as_mut() else {
            return;
        };
        d.queue.clear();
        // Native stops the worker between trash calls and joins it, so this
        // waits for one filesystem operation and every result it did send is
        // still collected below. wasm cannot join a `spawn_local`, so up to
        // `DELETE_WINDOW` results land after the batch is gone and their
        // photos keep their entries in the path-keyed maps. Those photos are
        // not in the folder the user just moved to, so nothing shows them.
        #[cfg(not(target_arch = "wasm32"))]
        {
            d.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            d.submit = None;
            if let Some(worker) = d.worker.take() {
                let _ = worker.join();
            }
        }
        d.in_flight.clear();
        self.poll_delete();
    }

    /// Whether a bulk delete is still running.
    pub(crate) fn bulk_delete_running(&self) -> bool {
        self.bulk_delete.is_some()
    }

    /// Drop one trashed photo from every map keyed by its path. O(1) per
    /// photo, so this runs as each result lands rather than at the end.
    fn forget_photo(
        &mut self,
        path: &Path,
        #[cfg(target_arch = "wasm32")] origin_dir: &Path,
        #[cfg(target_arch = "wasm32")] origin_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        self.ratings.remove(path);
        self.edits.remove(path);
        self.autotone_pending.remove(path);
        self.autotone_base.remove(path);
        self.rotations.remove(path);
        // Pending duplicate jobs for deleted files would keep the redraw loop
        // awake forever.
        self.phashes.remove(path);
        self.sharpness.remove(path);
        self.capture_times.remove(path);

        #[cfg(not(target_arch = "wasm32"))]
        self.catalog.remove(path);
        #[cfg(target_arch = "wasm32")]
        {
            if self.catalog.is_active_dir(origin_dir) {
                self.catalog.remove_with_handle(path, origin_handle);
            } else {
                self.catalog.delete_sidecar_with_handle(path, origin_handle);
            }
            self.web_file_handles.remove(path);
            self.web_dir_handles.remove(path);
        }
    }

    /// The half of the reconciliation that shifts playlist indices, run once
    /// per batch.
    ///
    /// Everything here is O(playlist) or worse, and none of it is needed while
    /// photos are merely hidden. `remove_matching` is what invalidates the
    /// index space that `gone` exists to keep stable, so the marks indexed by
    /// that space are rebuilt right after it.
    fn prune_deleted(&mut self, d: BulkDelete) {
        let gone = d.gone;
        if !gone.is_empty() {
            let survey_was_affected = self.survey_members.iter().any(|p| gone.contains(p));
            if let Some(pl) = self.playlist.as_mut() {
                pl.remove_matching(|p| gone.contains(p));
            }
            if let Some(deferred) = self.autotone_deferred.as_mut() {
                deferred.retain(|p| !gone.contains(p));
                if deferred.is_empty() {
                    self.autotone_deferred = None;
                }
            }
            self.feature_distances
                .retain(|(anchor, member), _| !gone.contains(anchor) && !gone.contains(member));
            self.feature_failed
                .retain(|(anchor, member)| !gone.contains(anchor) && !gone.contains(member));
            self.feature_pending
                .retain(|(anchor, member)| !gone.contains(anchor) && !gone.contains(member));

            self.recompute_dup_marks();
            self.recompute_burst_marks();
            if survey_was_affected && self.mode == ViewMode::Survey {
                self.close_survey();
            }
            // The cursor keeps its position (clamped), so it lands on a
            // neighbor of the deleted photos.
            self.selected.clear();
            self.anchor = None;
            self.recompute_visible();
            self.collapse_selection();
            self.after_visible_shrank();
        }
        let n = gone.len();
        if n == 0 && d.last_err.is_none() {
            return;
        }
        let t = crate::i18n::t();
        self.set_status(match d.last_err {
            None => (t.deleted)(n),
            Some(e) => (t.deleted_partial)(n, d.total, &e),
        });
    }

    /// Keep the Loupe on a photo that still exists after `visible` shrank, and
    /// fall back to the Grid when nothing is left to show.
    fn after_visible_shrank(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        if self.visible.is_empty() {
            self.mode = ViewMode::Grid;
            self.normalize_focus();
            self.update_window_title();
        } else {
            self.resync_loupe_selection();
        }
    }
}

/// One thread, because every move lands in the same Trash directory and
/// whether more threads overlap that latency or just contend on one inode is
/// unmeasured. It trashes in submission order and exits when the batch drops
/// its sender.
#[cfg(not(target_arch = "wasm32"))]
fn spawn_trash_worker(
    done_tx: Sender<DeleteResult>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Option<(Sender<PathBuf>, std::thread::JoinHandle<()>)> {
    let (submit, rx) = channel::<PathBuf>();
    match std::thread::Builder::new()
        .name("trash".into())
        .spawn(move || {
            while let Ok(path) = rx.recv() {
                if stop.load(std::sync::atomic::Ordering::Relaxed) {
                    return;
                }
                let result = crate::trash::move_to_trash(&path);
                if done_tx.send((path, result)).is_err() {
                    return;
                }
            }
        }) {
        Ok(worker) => Some((submit, worker)),
        Err(e) => {
            eprintln!("[lightphotos] could not start the trash worker: {e}");
            None
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::burst::BurstMark;
    use crate::navigation::Playlist;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Duration, Instant, SystemTime};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-bulk-delete-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An App over `names`, each an empty file in a fresh folder.
    fn app_with_photos(names: &[&str]) -> (App, PathBuf, Vec<PathBuf>) {
        let dir = unique_tmp_dir();
        let paths: Vec<PathBuf> = names
            .iter()
            .map(|n| {
                let p = dir.join(n);
                std::fs::write(&p, b"").unwrap();
                p
            })
            .collect();
        let mut app = App::new(None);
        app.playlist = Some(Playlist::from_dir(&dir));
        app.recompute_visible();
        (app, dir, paths)
    }

    /// Drive the batch to completion the way the frame loop does.
    fn drain(app: &mut App) -> usize {
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut polls = 0;
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            app.poll_delete();
            polls += 1;
            std::thread::sleep(Duration::from_millis(1));
        }
        polls
    }

    #[test]
    fn the_trash_call_does_not_happen_on_the_calling_thread() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg", "c.jpg"]);
        app.start_delete(paths.clone());

        // `start_delete` polls once, so some results may already be in, but
        // the batch must still be live rather than finished inline.
        assert!(
            app.bulk_delete.is_some(),
            "a delete must outlive the frame that started it"
        );
        drain(&mut app);
        assert!(paths.iter().all(|p| !p.exists()), "every photo is trashed");
        assert!(
            app.playlist.as_ref().unwrap().entries().is_empty(),
            "the playlist is reconciled once the batch ends"
        );
        assert!(app.visible.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The playlist must be edited once, not once per frame: editing it per
    /// chunk is O(n^2) and invalidates every playlist-indexed structure.
    #[test]
    fn the_playlist_is_edited_once_at_the_end_not_per_frame() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        app.start_delete(paths.clone());

        let mut frames_with_a_shorter_playlist = 0;
        while app.bulk_delete.is_some() {
            if app.playlist.as_ref().unwrap().entries().len() < 4 {
                frames_with_a_shorter_playlist += 1;
            }
            app.poll_delete();
            std::thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(
            frames_with_a_shorter_playlist, 0,
            "the playlist must stay intact until the batch ends"
        );
        assert!(app.playlist.as_ref().unwrap().entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The grid has to shrink while a long delete runs, or the user watches a
    /// toast climb over a grid full of photos that are already in the Trash.
    /// Hiding them must not touch the playlist, because `dup_marks` and
    /// `burst_marks` are indexed by playlist position.
    #[test]
    fn trashed_photos_leave_the_grid_without_touching_the_playlist() {
        let names = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg"];
        let (mut app, dir, paths) = app_with_photos(&names);
        app.start_delete(paths.clone());

        // No sleep: polling far faster than the trash worker runs is what
        // makes the half-done state observable rather than a coin flip.
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut saw_a_shrunken_grid = false;
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            assert_eq!(
                app.playlist.as_ref().unwrap().entries().len(),
                names.len(),
                "hiding a trashed photo must not edit the playlist"
            );
            saw_a_shrunken_grid |= app.visible.len() < names.len();
            app.poll_delete();
        }

        assert!(
            saw_a_shrunken_grid,
            "a trashed photo must leave the grid before the batch ends"
        );
        assert!(app.playlist.as_ref().unwrap().entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `burst_marks` is indexed by playlist position, and `remove_matching`
    /// shifts every index. Without a rebuild the survivor of a burst keeps the
    /// badge of the photo that was deleted ahead of it.
    #[test]
    fn burst_badges_are_rebuilt_after_a_delete_shifts_the_playlist() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg", "c.jpg"]);
        let (a, b, c) = (paths[0].clone(), paths[1].clone(), paths[2].clone());

        let t = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        app.capture_times.insert(a.clone(), Some(t));
        app.capture_times
            .insert(b.clone(), Some(t + Duration::from_millis(400)));
        app.capture_times
            .insert(c.clone(), Some(t + Duration::from_secs(3600)));
        app.sharpness.insert(a.clone(), 10.0);
        app.sharpness.insert(b.clone(), 90.0);
        app.sharpness.insert(c.clone(), 50.0);
        app.bursts_on = true;
        app.recompute_burst_marks();
        assert_eq!(app.burst_mark_at(0), Some(BurstMark::Sibling));
        assert_eq!(app.burst_mark_at(1), Some(BurstMark::Best));
        assert_eq!(app.burst_mark_at(2), None);

        app.start_delete(vec![a]);
        drain(&mut app);

        assert_eq!(app.playlist.as_ref().unwrap().entries(), &[b, c]);
        assert_eq!(
            app.burst_mark_at(0),
            None,
            "the survivor of a two-photo burst is no longer in a burst, so it \
             must not inherit the deleted photo's badge"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deferred Auto Tone batch names photos by path and outlives the
    /// catalog load. Deleting some of them must drop those names without
    /// abandoning the rest of the batch.
    #[test]
    fn a_delete_drops_its_photos_from_a_deferred_auto_tone_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.autotone_deferred = Some(paths.clone());

        app.start_delete(vec![paths[0].clone()]);
        drain(&mut app);

        assert_eq!(
            app.autotone_deferred,
            Some(vec![paths[1].clone()]),
            "the surviving photo must stay queued for Auto Tone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_every_deferred_auto_tone_target_clears_the_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.autotone_deferred = Some(paths.clone());

        app.start_delete(paths);
        drain(&mut app);

        assert!(app.autotone_deferred.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A photo can be trashed between its Auto Tone thumbnail being requested
    /// and landing. Toning it then would write a sidecar for a file that is
    /// already in the Trash.
    #[test]
    fn a_trashed_photo_leaves_the_auto_tone_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.autotone_pending.insert(paths[0].clone());
        app.autotone_base
            .insert(paths[0].clone(), Default::default());

        app.start_delete(vec![paths[0].clone()]);
        drain(&mut app);

        assert!(
            !app.autotone_pending.contains(&paths[0]),
            "a trashed photo must stop being an Auto Tone target"
        );
        assert!(!app.autotone_base.contains_key(&paths[0]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_delete_is_refused_while_one_is_running() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        app.start_delete(paths[..2].to_vec());
        assert!(
            app.bulk_delete.is_some(),
            "the first batch must still be running for this to test anything"
        );
        app.start_delete(paths[2..].to_vec());

        assert_eq!(
            app.status_text(),
            Some(crate::i18n::t().delete_in_progress),
            "the second batch is refused, not queued behind the first"
        );
        drain(&mut app);
        assert!(
            paths[2].exists() && paths[3].exists(),
            "the refused batch must not have trashed anything"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_trash_call_is_counted_and_reported() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg"]);
        let missing = dir.join("never-existed.jpg");
        app.start_delete(vec![paths[0].clone(), missing]);
        drain(&mut app);

        assert_eq!(app.playlist.as_ref().unwrap().entries().len(), 0);
        let status = app.status_text().unwrap_or_default().to_string();
        assert!(
            status.contains('1') && status.contains('2'),
            "a partial delete reports how many of how many landed, got: {status}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A folder change abandons the batch. It must stop the worker rather than
    /// wait for everything already submitted, or leaving a folder mid-delete
    /// would block for as long as the whole delete.
    #[test]
    fn a_folder_change_abandons_the_batch_and_keeps_what_it_trashed() {
        let names: Vec<String> = (0..40).map(|i| format!("p{i:02}.jpg")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir, paths) = app_with_photos(&refs);
        for p in &paths {
            app.ratings.insert(p.clone(), 3);
        }
        app.start_delete(paths.clone());
        app.cancel_delete();

        assert!(
            app.bulk_delete.is_none(),
            "a cancelled batch must not keep polling"
        );
        let survivors = paths.iter().filter(|p| p.exists()).count();
        assert!(
            survivors >= 30,
            "cancelling must stop the worker, not drain the queue; only \
             {survivors} of 40 photos survived"
        );
        for p in &paths {
            assert_eq!(
                p.exists(),
                app.ratings.contains_key(p),
                "a photo keeps its rating exactly while it is still on disk"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
