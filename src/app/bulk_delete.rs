// SPDX-License-Identifier: MIT OR Apache-2.0

//! Deleting a selection, paced across frames.
//!
//! Two costs used to land in one frame: the trash call per photo, which is
//! filesystem metadata I/O, and `finish_delete`'s reconciliation, which is
//! O(playlist). This module moves the first off the UI thread and splits the
//! second by what it indexes.
//!
//! A photo's removal divides in two. The path-keyed half (ratings, edits,
//! rotations, the folder store record, the signal caches) is O(1) per photo and
//! runs as each result lands. The half that shifts playlist indices
//! (`playlist.remove_matching`, the selection) runs exactly once, at the end.
//! In between, trashed photos sit in `gone` and [`App::recompute_visible`]
//! filters them out, so the grid shrinks live while every structure indexed
//! by playlist position stays valid.

use super::*;
use std::collections::{HashMap, HashSet, VecDeque};

use crate::persist::groups::GroupId;
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
    held: HeldGroups,
    total: usize,
    errors: usize,
    last_err: Option<String>,

    done_tx: Sender<DeleteResult>,
    done_rx: Receiver<DeleteResult>,
    /// Dropped when the batch ends, which is what stops the worker thread.
    #[cfg(not(target_arch = "wasm32"))]
    submit: Option<Sender<PathBuf>>,

    /// wasm has no OS paths, so the folder the batch started in is captured
    /// here and its sidecar cleanup still lands there after a navigation.
    #[cfg(target_arch = "wasm32")]
    origin_dir: PathBuf,
    #[cfg(target_arch = "wasm32")]
    origin_handle: web_sys::FileSystemDirectoryHandle,
}

#[derive(Default)]
struct HeldGroups {
    slot_by_name: HashMap<std::ffi::OsString, usize>,
    held: Vec<Held>,
}

struct Held {
    pending: usize,
    trashed: Vec<PathBuf>,
}

impl HeldGroups {
    fn hold(&mut self, names: &[&std::ffi::OsStr]) {
        self.held.push(Held {
            pending: names.len(),
            trashed: Vec::new(),
        });
        for name in names {
            self.slot_by_name
                .insert(name.to_os_string(), self.held.len() - 1);
        }
    }

    fn release(&mut self, path: PathBuf, trashed: bool) -> Vec<PathBuf> {
        let slot = path
            .file_name()
            .and_then(|n| self.slot_by_name.get(n))
            .copied();
        let Some(held) = slot.and_then(|i| self.held.get_mut(i)) else {
            return if trashed { vec![path] } else { Vec::new() };
        };
        held.pending -= 1;
        if trashed {
            held.trashed.push(path);
        }
        if held.pending > 0 {
            return Vec::new();
        }
        std::mem::take(&mut held.trashed)
    }

    fn release_all(&mut self) -> Vec<PathBuf> {
        self.held
            .iter_mut()
            .flat_map(|h| std::mem::take(&mut h.trashed))
            .collect()
    }
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
                let result = crate::web::web_folder_store_fs::remove_file(&handle, &name).await;
                let _ = tx.send((path, result));
            });
        }
    }
}

impl App {
    pub(super) fn delete_selection(&mut self) {
        self.start_delete(self.selected_member_paths());
    }

    /// Begin trashing `paths`. Refused while another delete or an export is
    /// running: the export reads bytes from files this batch would remove.
    pub(super) fn start_delete(&mut self, mut paths: Vec<PathBuf>) {
        let mut seen = HashSet::new();
        paths.retain(|p| seen.insert(p.clone()));
        if paths.is_empty() {
            return;
        }
        if self.bulk_delete.is_some() || self.export_running() {
            self.set_status(
                StatusKind::Error,
                crate::i18n::t().delete_in_progress.to_string(),
            );
            return;
        }
        let total = paths.len();
        let held = self.whole_groups(&paths);
        let (done_tx, done_rx) = channel();

        #[cfg(target_arch = "wasm32")]
        let (origin_dir, origin_handle) = {
            let dir = paths[0].parent().unwrap_or(Path::new("")).to_path_buf();
            let Some(handle) = self.web.dir_handle(&dir) else {
                self.set_status(
                    StatusKind::Error,
                    (crate::i18n::t().delete_no_handle)(&dir.display().to_string()),
                );
                return;
            };
            (dir, handle)
        };

        #[cfg(not(target_arch = "wasm32"))]
        let submit = spawn_trash_worker(done_tx.clone());

        self.bulk_delete = Some(BulkDelete {
            queue: paths.into(),
            in_flight: HashSet::new(),
            gone: HashSet::new(),
            view_dirty: false,
            held,
            total,
            errors: 0,
            last_err: None,
            done_tx,
            done_rx,
            #[cfg(not(target_arch = "wasm32"))]
            submit,
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
        let Some(d) = self.bulk_delete.as_mut() else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        let origin = (d.origin_dir.clone(), d.origin_handle.clone());

        let mut trashed: Vec<PathBuf> = Vec::new();
        let mut forget: Vec<PathBuf> = Vec::new();
        {
            d.admit();
            while let Some((path, result)) = d.take_result() {
                d.in_flight.remove(&path);
                let ok = match result {
                    Ok(()) => {
                        d.gone.insert(path.clone());
                        d.view_dirty = true;
                        trashed.push(path.clone());
                        true
                    }
                    Err(e) => {
                        eprintln!("[lightphotos] trash failed for {}: {e}", path.display());
                        d.errors += 1;
                        d.last_err = Some(e);
                        false
                    }
                };
                forget.extend(d.held.release(path, ok));
            }
            if d.finished() {
                forget.extend(d.held.release_all());
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
        #[cfg(not(target_arch = "wasm32"))]
        if !forget.is_empty() {
            self.folder_store.forget_photos(&forget);
        }
        // `forget_photo` already dropped each record through the origin
        // folder's handle, so only the groups are left. They are in memory
        // only while that folder is still open, which the folder store checks.
        #[cfg(target_arch = "wasm32")]
        if !forget.is_empty() {
            self.folder_store.forget_group_members(&forget);
        }

        let Some(d) = self.bulk_delete.as_mut() else {
            return;
        };
        if std::mem::take(&mut d.view_dirty) {
            self.recompute_visible();
            self.after_visible_shrank();
        }

        let Some(d) = self.bulk_delete.as_ref() else {
            return;
        };
        if d.finished() {
            if let Some(d) = self.bulk_delete.take() {
                self.prune_deleted(d);
            }
        } else {
            self.set_status(
                StatusKind::Progress,
                (crate::i18n::t().deleting)(d.done(), d.total),
            );
        }
        self.request_redraw();
    }

    fn whole_groups(&self, paths: &[PathBuf]) -> HeldGroups {
        let Some(groups) = self.folder_store.groups() else {
            return Default::default();
        };
        let mut queued: HashMap<&GroupId, Vec<&std::ffi::OsStr>> = HashMap::new();
        for name in paths.iter().filter_map(|p| p.file_name()) {
            if let Some(id) = groups.group_of(name) {
                queued.entry(id).or_default().push(name);
            }
        }
        let mut held = HeldGroups::default();
        for (id, names) in queued {
            if groups
                .get(id)
                .is_some_and(|g| g.members().len() == names.len())
            {
                held.hold(&names);
            }
        }
        held
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
        self.autotone.forget(path);
        self.rotations.remove(path);
        self.capture_times.remove(path);
        self.sort_meta.remove(path);
        self.touchups.remove(path);
        self.touchup_undo.remove(path);
        self.masks.remove(path);
        self.detections.remove(path);
        self.exif_cache.remove(path);
        self.thumb_aspect.remove(path);
        self.signals.forget(path);

        #[cfg(target_arch = "wasm32")]
        {
            if self.folder_store.is_active_dir(origin_dir) {
                self.folder_store.remove_with_handle(path, origin_handle);
            } else {
                self.folder_store
                    .delete_sidecar_with_handle(path, origin_handle);
            }
            self.web.forget_handles(path);
        }
    }

    /// The half of the reconciliation that shifts playlist indices, run once
    /// per batch.
    ///
    /// Everything here is O(playlist) or worse, and none of it is needed while
    /// photos are merely hidden. `remove_matching` is what invalidates the
    /// index space that `gone` exists to keep stable.
    fn prune_deleted(&mut self, d: BulkDelete) {
        let gone = d.gone;
        self.autotone.drop_deferred(&gone);
        // Empty when the batch's folder is no longer open. The folder showing
        // lost nothing, so its selection stays.
        let removed = match self.playlist.as_mut() {
            Some(pl) if !gone.is_empty() => pl.remove_matching(|p| gone.contains(p)),
            _ => Vec::new(),
        };
        if !removed.is_empty() {
            self.visible = self
                .visible
                .iter()
                .filter_map(|&i| crate::navigation::shift_index(i, &removed))
                .collect();
            self.strip_layout = None;
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
        match d.last_err {
            None => self.set_status(StatusKind::Success, (t.deleted)(n)),
            Some(e) => self.set_status(StatusKind::Error, (t.deleted_partial)(n, d.total, &e)),
        }
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
fn spawn_trash_worker(done_tx: Sender<DeleteResult>) -> Option<Sender<PathBuf>> {
    let (submit, rx) = channel::<PathBuf>();
    match std::thread::Builder::new()
        .name("trash".into())
        .spawn(move || {
            while let Ok(path) = rx.recv() {
                let result = crate::shell::trash::move_to_trash(&path);
                if done_tx.send((path, result)).is_err() {
                    return;
                }
            }
        }) {
        Ok(_) => Some(submit),
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
    use crate::app::test_support::{temp_folder, wait_for_folder_store_within};
    use crate::navigation::Playlist;
    use std::time::{Duration, Instant};

    /// An App over `names`, each an empty file in a fresh folder.
    fn app_with_photos(names: &[&str]) -> (App, PathBuf, Vec<PathBuf>) {
        let dir = temp_folder("bulk-delete-test");
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

    /// The count follows the sort and leads the Actions menu on every photo,
    /// rather than sitting alone at the window's right edge.
    #[test]
    fn the_count_sits_between_the_sort_and_actions() {
        use crate::app::test_support::{folder_app, settled_at};
        let t = crate::i18n::t();
        let (mut app, dir, _) = folder_app("toolbar-rows", 3);
        app.select_single(0);
        let painted = settled_at(&mut app, egui::vec2(1900.0, 800.0));
        let count = painted.pos_of(&(t.n_of_m_photos)(3, 3));
        let sort = painted.pos_of(t.sort_name);
        let actions = painted.pos_of(&format!("{} \u{23f7}", t.actions_menu));
        assert!(
            sort.x < count.x && count.x < actions.x && actions.x - count.x < 120.0,
            "the count sits between the sort and Actions: {sort:?} {count:?} {actions:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
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

    /// A trashed photo leaves nothing behind in the maps keyed by its path.
    #[test]
    fn a_trashed_photo_leaves_no_per_photo_state() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        let gone = paths[0].clone();
        app.touchups.insert(gone.clone(), Vec::new());
        app.touchup_undo.insert(gone.clone(), Vec::new());
        app.masks.insert(gone.clone(), Vec::new());
        app.detections.insert(gone.clone(), Default::default());
        app.exif_cache.insert(gone.clone(), Default::default());
        app.thumb_aspect.insert(gone.clone(), 1.5);
        app.start_delete(vec![gone.clone()]);
        drain(&mut app);

        assert!(!app.touchups.contains_key(&gone), "touchups");
        assert!(!app.touchup_undo.contains_key(&gone), "touchup_undo");
        assert!(!app.masks.contains_key(&gone), "masks");
        assert!(!app.detections.contains_key(&gone), "detections");
        assert!(!app.exif_cache.contains_key(&gone), "exif_cache");
        assert!(!app.thumb_aspect.contains_key(&gone), "thumb_aspect");
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

    #[test]
    fn the_cursor_lands_on_the_next_photo_after_a_trash() {
        for loupe in [false, true] {
            let (mut app, dir, paths) =
                app_with_photos(&["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]);
            app.select_single(1);
            if loupe {
                app.enter_loupe();
            }
            app.start_delete(vec![paths[1].clone()]);
            drain(&mut app);

            assert_eq!(app.selected_path(), Some(paths[2].clone()), "loupe={loupe}");
            if loupe {
                assert_eq!(app.want.as_ref(), Some(&paths[2]), "the Loupe shows c");
            }
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// The grid has to shrink while a long delete runs, or the user watches a
    /// toast climb over a grid full of photos that are already in the Trash.
    ///
    /// The test feeds the results one at a time in place of the trash worker,
    /// so the half-done state does not depend on how fast the OS trashes.
    #[test]
    fn trashed_photos_leave_the_grid_without_touching_the_playlist() {
        let names = ["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg"];
        let (mut app, dir, paths) = app_with_photos(&names);
        app.start_delete(paths.clone());

        // Take over the real worker's result channel. Results it already sent
        // were collected by the poll in `start_delete`; any it sends later go
        // to the dropped receiver.
        let (results, pending) = {
            let d = app.bulk_delete.as_mut().unwrap();
            d.submit = None;
            let (tx, rx) = channel();
            d.done_tx = tx.clone();
            d.done_rx = rx;
            let mut pending: Vec<PathBuf> = d.in_flight.iter().cloned().collect();
            pending.sort();
            (tx, pending)
        };
        assert!(
            pending.len() > 1,
            "the worker must not finish the batch inside `start_delete`"
        );

        let (last, before_last) = pending.split_last().unwrap();
        for (fed, path) in before_last.iter().enumerate() {
            results.send((path.clone(), Ok(()))).unwrap();
            app.poll_delete();
            assert!(app.bulk_delete.is_some(), "the batch is still running");
            assert_eq!(
                app.visible.len(),
                pending.len() - fed - 1,
                "a trashed photo must leave the grid before the batch ends"
            );
            assert_eq!(
                app.playlist.as_ref().unwrap().entries().len(),
                names.len(),
                "hiding a trashed photo must not edit the playlist"
            );
        }

        results.send((last.clone(), Ok(()))).unwrap();
        app.poll_delete();
        assert!(app.bulk_delete.is_none());
        assert!(app.playlist.as_ref().unwrap().entries().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A deferred Auto Tone batch names photos by path and outlives the
    /// folder store load. Deleting some of them must drop those names without
    /// abandoning the rest of the batch.
    #[test]
    fn a_delete_drops_its_photos_from_a_deferred_auto_tone_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.autotone.defer(paths.clone());

        app.start_delete(vec![paths[0].clone()]);
        drain(&mut app);

        assert_eq!(
            app.autotone.deferred(),
            Some(&paths[1..]),
            "the surviving photo must stay queued for Auto Tone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_every_deferred_auto_tone_target_clears_the_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.autotone.defer(paths.clone());

        app.start_delete(paths);
        drain(&mut app);

        assert!(app.autotone.deferred().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A photo can be trashed between its Auto Tone thumbnail being requested
    /// and landing. Toning it then would write a sidecar for a file that is
    /// already in the Trash.
    #[test]
    fn a_trashed_photo_leaves_the_auto_tone_batch() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.enqueue_auto_tone(vec![paths[0].clone()]);
        assert!(app.autotone.is_pending(&paths[0]));

        app.start_delete(vec![paths[0].clone()]);
        drain(&mut app);

        assert!(
            !app.autotone.is_pending(&paths[0]),
            "a trashed photo must stop being an Auto Tone target"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A delete started in one folder finishes after the user opens another.
    /// `FolderStore` keys its cache by file name, so cleanup for the old
    /// folder's `p00.jpg` must not drop the new folder's `p00.jpg` record, and
    /// the end-of-batch reconciliation must not touch the new folder's
    /// selection.
    #[test]
    fn a_delete_finishes_in_its_own_folder_after_a_folder_change() {
        let doomed: Vec<String> = (0..30).map(|i| format!("p{i:02}.jpg")).collect();
        let refs: Vec<&str> = doomed.iter().map(String::as_str).collect();
        let (mut app, old_dir, old_paths) = app_with_photos(&refs);
        {
            let mut old_store =
                crate::persist::folder_store::FolderStore::with_dir(old_dir.clone());
            for p in &old_paths {
                old_store.set(p, 2);
            }
        }
        let old_sidecars: Vec<PathBuf> = old_paths
            .iter()
            .map(|p| crate::sidecar::sidecar_path(p).unwrap())
            .collect();
        assert!(old_sidecars.iter().all(|s| s.exists()));

        let new_dir = temp_folder("bulk-delete-test");
        let survivor = new_dir.join(&doomed[0]);
        std::fs::write(&survivor, b"").unwrap();
        std::fs::write(new_dir.join("q.jpg"), b"").unwrap();
        crate::persist::folder_store::FolderStore::with_dir(new_dir.clone()).set(&survivor, 4);

        app.selected = (0..old_paths.len()).collect();
        app.delete_selection();
        assert!(
            app.bulk_delete.is_some(),
            "the batch must still be running for the folder change to matter"
        );

        app.load_playlist(Playlist::from_dir(&new_dir), new_dir.clone());
        assert!(
            app.bulk_delete.is_some(),
            "a folder change must not stop the batch"
        );
        wait_for_folder_store_within(&mut app, Duration::from_secs(20));
        app.select_single(1);
        app.select_toggle(0);
        drain(&mut app);

        assert!(
            old_paths.iter().all(|p| !p.exists()),
            "every photo of the old folder is trashed"
        );
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(
            old_sidecars.iter().all(|s| !s.exists()),
            "and loses its sidecar"
        );
        assert_eq!(
            app.status_text(),
            Some((crate::i18n::t().deleted)(old_paths.len()).as_str())
        );

        assert!(survivor.exists(), "the new folder's photo is untouched");
        assert_eq!(
            app.folder_store.get(&survivor),
            Some(4),
            "the new folder's same-named photo must keep its rating"
        );
        assert_eq!(app.ratings.get(&survivor), Some(&4));
        assert_eq!(
            crate::persist::folder_store::FolderStore::with_dir(new_dir.clone()).get(&survivor),
            Some(4),
            "and must keep it on disk"
        );
        assert_eq!(app.playlist.as_ref().unwrap().entries().len(), 2);
        assert_eq!(app.visible.len(), 2);
        assert_eq!(app.selected.len(), 2, "the new folder's selection stays");

        let _ = std::fs::remove_dir_all(&old_dir);
        let _ = std::fs::remove_dir_all(&new_dir);
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

    #[test]
    fn a_held_group_is_released_when_its_last_photo_lands() {
        let p = |n: &str| PathBuf::from("/photos").join(n);
        let names = ["b.jpg", "c.jpg", "d.jpg"].map(std::ffi::OsStr::new);
        let mut held = HeldGroups::default();
        held.hold(&names);
        assert_eq!(held.release(p("b.jpg"), true), Vec::<PathBuf>::new());
        assert_eq!(held.release(p("c.jpg"), false), Vec::<PathBuf>::new());
        assert_eq!(
            held.release(p("d.jpg"), true),
            [p("b.jpg"), p("d.jpg")],
            "the last result releases every trashed member, not the failed one"
        );
        assert_eq!(held.release_all(), Vec::<PathBuf>::new(), "nothing left");
        assert_eq!(held.release(p("e.jpg"), true), [p("e.jpg")], "unheld");
        assert_eq!(held.release(p("f.jpg"), false), Vec::<PathBuf>::new());

        let mut held = HeldGroups::default();
        held.hold(&names);
        assert_eq!(held.release(p("b.jpg"), true), Vec::<PathBuf>::new());
        assert_eq!(held.release(p("c.jpg"), true), Vec::<PathBuf>::new());
        assert_eq!(
            held.release_all(),
            [p("b.jpg"), p("c.jpg")],
            "a batch that ends with d pending hands back only what was trashed"
        );
        assert_eq!(held.release_all(), Vec::<PathBuf>::new(), "once");
    }

    fn grouped_app(names: &[&str]) -> (App, PathBuf, Vec<PathBuf>) {
        let (mut app, dir, paths) = app_with_photos(names);
        app.folder_store.open_dir(&dir);
        app.recompute_visible();
        (app, dir, paths)
    }

    #[test]
    #[ignore]
    fn delete_frame_bench() {
        use crate::app::nav::tests::group_photos;
        const N: usize = 2000;
        let names: Vec<String> = (0..N).map(|i| format!("IMG_{i:05}.JPG")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let time = |app: &App| {
            let mut runs: Vec<Duration> = (0..5)
                .map(|_| {
                    let t = Instant::now();
                    let paths = app.selected_member_paths();
                    let held = app.whole_groups(&paths);
                    std::hint::black_box((paths.len(), held.slot_by_name.len()));
                    t.elapsed()
                })
                .collect();
            runs.sort();
            runs
        };

        let (mut singles, dir_a, _) = grouped_app(&refs);
        singles.select_all();
        let base = time(&singles);
        let (mut stack, dir_b, _) = grouped_app(&refs);
        group_photos(&mut stack, &(0..N).collect::<Vec<_>>(), 0);
        stack.select_single(0);
        assert_eq!(stack.selected_member_paths().len(), N);
        let grouped = time(&stack);
        eprintln!("delete frame, {N} singles: {base:?}");
        eprintln!("delete frame, one {N}-photo stack: {grouped:?}");
        eprintln!("medians: singles {:?}, stack {:?}", base[2], grouped[2]);
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    /// Each group's members and the photo its collapsed stack shows.
    fn group_names(app: &App) -> Vec<(Vec<String>, String)> {
        let s = |n: &std::ffi::OsString| n.to_string_lossy().into_owned();
        app.folder_store
            .groups()
            .unwrap()
            .iter()
            .map(|(_, g)| (g.members().iter().map(s).collect(), s(g.cover())))
            .collect()
    }

    #[test]
    fn trashing_a_representative_in_the_loupe_clears_it_and_the_next_member_covers() {
        use crate::app::nav::tests::group_photos;
        let (mut app, dir, paths) = grouped_app(&["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg"]);
        group_photos(&mut app, &[1, 2, 3], 1);
        app.select_single(1);
        app.enter_loupe();
        assert_eq!(
            app.selected_member_paths(),
            vec![paths[1].clone()],
            "the Loupe's Delete takes the photo on screen, not its whole stack"
        );
        app.request_bulk(crate::ui::BulkKind::Delete);
        app.confirm_pending();
        drain(&mut app);

        assert!(!paths[1].exists());
        let promoted = (
            vec!["c.jpg".to_string(), "d.jpg".to_string()],
            "c.jpg".to_string(),
        );
        assert_eq!(group_names(&app), vec![promoted.clone()]);
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.want.as_ref(), Some(&paths[2]), "the Loupe shows c");
        let names = |app: &App| -> Vec<PathBuf> {
            let pl = app.playlist.as_ref().unwrap();
            app.visible
                .iter()
                .map(|&i| pl.entries()[i].clone())
                .collect()
        };
        assert_eq!(
            names(&app),
            vec![paths[0].clone(), paths[2].clone(), paths[4].clone()]
        );

        app.folder_store.flush_blocking(Duration::from_secs(10));
        let reloaded = crate::persist::folder_store::FolderStore::with_dir(dir.clone());
        let on_disk: Vec<_> = reloaded
            .groups()
            .unwrap()
            .iter()
            .map(|(_, g)| (g.members().len(), g.rep().cloned()))
            .collect();
        assert_eq!(on_disk, vec![(2, None)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_group_stays_visible_while_its_representative_is_trashed() {
        use crate::app::nav::tests::group_photos;
        let names: Vec<String> = (0..30).map(|i| format!("p{i:02}.jpg")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir, paths) = grouped_app(&refs);
        group_photos(&mut app, &[0, 1], 0);
        let rest: Vec<PathBuf> = paths[..1].iter().chain(&paths[2..]).cloned().collect();
        app.start_delete(rest);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            assert!(
                app.visible.contains(&0) || app.visible.contains(&1),
                "the group lost its cell mid-batch"
            );
            app.poll_delete();
        }
        assert_eq!(app.visible.len(), 1);
        assert!(
            app.folder_store.groups().unwrap().is_empty(),
            "p01 is alone"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn visible_names(app: &App) -> Vec<String> {
        let pl = app.playlist.as_ref().unwrap();
        app.visible
            .iter()
            .map(|&i| {
                pl.entries()[i]
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect()
    }

    fn stack_and_single() -> (App, PathBuf, Vec<PathBuf>) {
        use crate::app::nav::tests::group_photos;
        let (mut app, dir, paths) =
            grouped_app(&["a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg"]);
        group_photos(&mut app, &[1, 2, 3], 1);
        assert_eq!(visible_names(&app), ["a.jpg", "b.jpg", "e.jpg", "f.jpg"]);
        app.selected = BTreeSet::from([1, 2]);
        app.sel = Some(1);
        (app, dir, paths)
    }

    #[test]
    fn delete_on_a_stack_and_a_single_trashes_every_member_and_the_single() {
        let (mut app, dir, paths) = stack_and_single();
        app.request_bulk(crate::ui::BulkKind::Delete);
        app.confirm_pending();
        drain(&mut app);
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(exists, [true, false, false, false, false, true]);
        assert_eq!(visible_names(&app), ["a.jpg", "f.jpg"]);
        assert!(app.folder_store.groups().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_delete_confirm_counts_every_member_and_names_the_groups() {
        let t = crate::i18n::t();
        let (mut app, dir, _) = stack_and_single();
        app.request_bulk(crate::ui::BulkKind::Delete);
        let (_, prompt) = app.pending_bulk_prompt().unwrap();
        assert_eq!(prompt, (t.confirm_delete_groups)(4, 1));

        app.cancel_pending();
        app.selected = BTreeSet::from([0, 2]);
        app.request_bulk(crate::ui::BulkKind::Delete);
        let (_, prompt) = app.pending_bulk_prompt().unwrap();
        assert_eq!(prompt, (t.confirm_delete)(2), "no group, the old wording");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trashing_a_whole_group_removes_it_in_one_step() {
        use crate::app::nav::tests::group_photos;
        let names: Vec<String> = (0..40).map(|i| format!("p{i:02}.jpg")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let (mut app, dir, paths) = grouped_app(&refs);
        let members: Vec<usize> = (0..40).collect();
        group_photos(&mut app, &members, 0);
        app.folder_store.flush_blocking(Duration::from_secs(10));
        app.select_single(0);
        app.request_bulk(crate::ui::BulkKind::Delete);
        app.confirm_pending();
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            let sizes: Vec<usize> = app
                .folder_store
                .groups()
                .unwrap()
                .iter()
                .map(|(_, g)| g.members().len())
                .collect();
            assert!(
                sizes.is_empty() || sizes == [40],
                "the group shrank mid-batch: {sizes:?}"
            );
            app.poll_delete();
        }
        assert!(paths.iter().all(|p| !p.exists()));
        assert!(app.folder_store.groups().unwrap().is_empty());
        app.folder_store.flush_blocking(Duration::from_secs(10));
        let groups_dir = dir.join(crate::sidecar::SIDECAR_DIR).join("groups");
        let left = std::fs::read_dir(&groups_dir).map_or(0, |d| d.count());
        assert_eq!(left, 0, "the group's sidecar is deleted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_queued_twice_is_trashed_once_and_its_group_goes_in_one_step() {
        use crate::app::nav::tests::group_photos;
        let (mut app, dir, paths) = grouped_app(&["a.jpg", "b.jpg", "c.jpg", "d.jpg"]);
        group_photos(&mut app, &[1, 2, 3], 1);
        let queued = [1, 2, 2, 3].map(|i| paths[i].clone()).to_vec();
        app.start_delete(queued);
        let deadline = Instant::now() + Duration::from_secs(20);
        while app.bulk_delete.is_some() {
            assert!(Instant::now() < deadline, "the delete batch never finished");
            let sizes: Vec<usize> = app
                .folder_store
                .groups()
                .unwrap()
                .iter()
                .map(|(_, g)| g.members().len())
                .collect();
            assert!(
                sizes.is_empty() || sizes == [3],
                "the group shrank mid-batch: {sizes:?}"
            );
            app.poll_delete();
        }
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(exists, [true, false, false, false]);
        assert!(app.folder_store.groups().unwrap().is_empty());
        let (kind, msg, _) = app.status.clone().expect("a delete reports");
        assert_eq!(
            (kind, msg),
            (StatusKind::Success, (crate::i18n::t().deleted)(3)),
            "three photos trashed, no second call failing on c"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn delete_group_open() -> (App, PathBuf, Vec<PathBuf>) {
        use crate::app::nav::tests::group_photos;
        let names = [
            "a.jpg", "b.jpg", "c.jpg", "d.jpg", "e.jpg", "f.jpg", "g.jpg", "h.jpg",
        ];
        let (mut app, dir, paths) = grouped_app(&names);
        group_photos(&mut app, &[1, 2, 3, 4, 5, 6], 1);
        app.selected = BTreeSet::from([1, 2]);
        app.sel = Some(1);
        #[cfg(target_os = "macos")]
        assert!(app.menu_enabled(crate::shell::menu::MenuCommand::DeleteGroup));
        app.modifiers = ModifiersState::SHIFT;
        app.handle_key(winit::keyboard::KeyCode::Delete);
        app.modifiers = ModifiersState::empty();
        assert_eq!(app.pending_group_delete(), Some((6, 1)));
        (app, dir, paths)
    }

    use crate::app::test_support::settled;

    fn click_button(app: &mut App, label: &str) {
        use crate::app::test_support::click;
        let at = settled(app).pos_of(label);
        let (actions, _) = click(app, at);
        app.apply_ui_actions(actions);
    }

    #[test]
    fn remove_group_keeps_every_photo_as_a_single() {
        let (mut app, dir, paths) = delete_group_open();
        click_button(&mut app, crate::i18n::t().remove_group);
        assert!(!app.confirm_open());
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        assert!(app.folder_store.groups().unwrap().is_empty());
        assert_eq!(visible_names(&app).len(), 8);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_group_trashes_every_member_and_nothing_else() {
        let (mut app, dir, paths) = delete_group_open();
        click_button(&mut app, &(crate::i18n::t().trash_group_photos)(6));
        assert!(!app.confirm_open());
        drain(&mut app);
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(
            exists,
            [true, false, false, false, false, false, false, true],
            "the single h was selected too, and stays"
        );
        assert_eq!(visible_names(&app), ["a.jpg", "h.jpg"]);
        assert!(app.folder_store.groups().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_esc_and_enter_leave_the_group_and_its_photos_alone() {
        let t = crate::i18n::t();
        let (mut app, dir, paths) = delete_group_open();
        app.handle_key(winit::keyboard::KeyCode::Enter);
        assert!(app.confirm_open(), "Enter picks neither action");
        click_button(&mut app, t.cancel);
        assert!(!app.confirm_open(), "Cancel closes it");
        app.request_delete_group();
        app.handle_key(winit::keyboard::KeyCode::Escape);
        assert!(!app.confirm_open(), "Esc closes it");
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        assert_eq!(group_names(&app).len(), 1);
        assert_eq!(app.selection_count(), 2, "the selection stays");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn focused_button(app: &mut App, trash: &str) -> Option<(String, f32)> {
        let t = crate::i18n::t();
        let painted = settled(app);
        let cursor = crate::ui::theme::colors(&app.egui_ctx).cursor;
        let rings = painted.outlined(cursor);
        let focused: Vec<(String, f32)> = [t.cancel, t.remove_group, trash]
            .into_iter()
            .map(|label| (label, painted.pos_of(label)))
            .filter(|(_, at)| rings.iter().any(|r| r.contains(*at)))
            .map(|(label, at)| (label.to_string(), at.x))
            .collect();
        assert!(focused.len() <= 1, "one button at a time: {focused:?}");
        focused.into_iter().next()
    }

    fn tab(app: &mut App, back: bool) {
        app.modifiers = if back {
            ModifiersState::SHIFT
        } else {
            ModifiersState::empty()
        };
        app.handle_key(winit::keyboard::KeyCode::Tab);
        app.modifiers = ModifiersState::empty();
    }

    #[test]
    fn tab_walks_the_delete_group_buttons_left_to_right_and_wraps() {
        let trash = (crate::i18n::t().trash_group_photos)(6);
        let (mut app, dir, _) = delete_group_open();
        assert_eq!(focused_button(&mut app, &trash), None, "nothing at first");
        let mut forward = Vec::new();
        for _ in 0..3 {
            tab(&mut app, false);
            forward.push(focused_button(&mut app, &trash).expect("Tab focuses a button"));
        }
        let xs: Vec<f32> = forward.iter().map(|(_, x)| *x).collect();
        assert!(
            xs.windows(2).all(|w| w[0] < w[1]),
            "left to right: {forward:?}"
        );
        tab(&mut app, false);
        assert_eq!(focused_button(&mut app, &trash), Some(forward[0].clone()));

        let mut backward = Vec::new();
        for _ in 0..3 {
            tab(&mut app, true);
            backward.push(focused_button(&mut app, &trash).unwrap());
        }
        let expected: Vec<_> = [2, 1, 0].map(|i| forward[i].clone()).into();
        assert_eq!(backward, expected, "Shift+Tab wraps and walks back");
        assert!(app.bulk_delete.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn enter_on(label: &str) -> (App, PathBuf, Vec<PathBuf>) {
        let trash = (crate::i18n::t().trash_group_photos)(6);
        let (mut app, dir, paths) = delete_group_open();
        for _ in 0..3 {
            tab(&mut app, false);
            if focused_button(&mut app, &trash).is_some_and(|(l, _)| l == label) {
                app.handle_key(winit::keyboard::KeyCode::Enter);
                return (app, dir, paths);
            }
        }
        panic!("Tab never reached {label}");
    }

    #[test]
    fn enter_presses_the_focused_delete_group_button() {
        let t = crate::i18n::t();
        let (app, dir, paths) = enter_on(t.cancel);
        assert!(!app.confirm_open(), "Cancel closes it");
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        assert_eq!(group_names(&app).len(), 1, "the group stays");
        let _ = std::fs::remove_dir_all(&dir);

        let (app, dir, paths) = enter_on(t.remove_group);
        assert!(!app.confirm_open());
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        assert!(app.folder_store.groups().unwrap().is_empty());
        assert_eq!(visible_names(&app).len(), 8, "every member is a single");
        let _ = std::fs::remove_dir_all(&dir);

        let (mut app, dir, paths) = enter_on(&(t.trash_group_photos)(6));
        assert!(!app.confirm_open());
        drain(&mut app);
        let exists: Vec<bool> = paths.iter().map(|p| p.exists()).collect();
        assert_eq!(
            exists,
            [true, false, false, false, false, false, false, true]
        );
        assert!(app.folder_store.groups().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn delete_while_the_folder_loads_is_refused_and_trashes_nothing() {
        let (mut app, dir, paths) = app_with_photos(&["a.jpg", "b.jpg"]);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        assert!(app.folder_store_loading());
        app.select_single(0);
        app.request_bulk(crate::ui::BulkKind::Delete);
        assert!(app.pending_confirm.is_none(), "no confirm is offered");
        #[cfg(target_os = "macos")]
        assert!(!app.menu_enabled(crate::shell::menu::MenuCommand::MoveToTrash));
        app.confirm_pending();
        assert!(app.bulk_delete.is_none());
        assert!(paths.iter().all(|p| p.exists()));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
