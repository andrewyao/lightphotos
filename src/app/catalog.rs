use super::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};


use crate::develop::Adjustments;
use crate::duplicates::DuplicateMark;
use crate::navigation::Cmp;
use crate::ui;
#[cfg(not(target_arch = "wasm32"))]
use crate::trash;

impl App {

    /// Point the catalog at `dir` and kick off its sidecar scan on a
    /// one-shot background thread — same idiom as subject segmentation's
    /// `request_selection_mask` (`app/loupe.rs`), not `loader.rs`'s
    /// persistent pool: a directory switch is a single job, not a stream of
    /// same-shaped ones. `Catalog::switch_dir` runs synchronously first (it's
    /// just a clear, no I/O) so no stale cross-directory data is visible in
    /// the meantime; the slow part (`load_sidecars`, one read per sidecar
    /// file) happens on the spawned thread and is folded in later by
    /// `poll_catalog_load`.
    pub(super) fn request_catalog_load(&mut self, dir: &Path) {
        self.catalog.switch_dir(dir);
        self.catalog_load_token += 1;
        let token = self.catalog_load_token;
        let dir = dir.to_path_buf();
        let tx = self.catalog_load_tx.clone();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let for_thread = dir.clone();
            let spawned = std::thread::Builder::new()
                .name("catalog-load".into())
                .spawn(move || {
                    let loaded = crate::catalog::load_sidecars(&for_thread);
                    let _ = tx.send((for_thread, token, loaded));
                });
            match spawned {
                Ok(_) => self.catalog_load_pending = Some((dir, token)),
                Err(e) => {
                    // No thread means `load_sidecars` never runs for `dir` —
                    // `switch_dir` above already cleared the cache, so it would
                    // otherwise silently stay empty (all ratings/edits appearing
                    // lost) with no signal to the user. Report it through the
                    // same toast mechanism as a failed sidecar write, AND leave
                    // `catalog_load_pending` set to this (unrunnable) request —
                    // not cleared/unchanged. `export.rs`'s `catalog_load_pending
                    // .is_some()` guard exists specifically to refuse exporting
                    // against an unloaded catalog; if this left pending at
                    // whatever it was before (typically `None`, since directory
                    // switches aren't usually mid-load), that guard would see
                    // nothing pending and wave an export for `dir` straight
                    // through against a cache that will now never populate —
                    // worse than the toast alone. No result will ever arrive on
                    // `catalog_load_rx` for this token, so this state persists
                    // until the user switches directories again (a fresh
                    // `request_catalog_load` call, which may succeed).
                    self.catalog
                        .note_persist_error(format!("could not load {}: {e}", dir.display()));
                    self.catalog_load_pending = Some((dir, token));
                }
            }
        }

        // wasm32: no real OS threads and no synchronous File System Access
        // read at all — `web_catalog_fs::load_sidecars` is async, dispatched
        // via `spawn_local` instead of a background thread, landing on the
        // exact same `catalog_load_tx`/`token` protocol so `poll_catalog_load`
        // needs no platform branch of its own. The folder-pick handle
        // (`Catalog::set_wasm_dir_handle`, called just before `load_playlist`
        // triggers this) should always be set by the time this runs; if it
        // somehow isn't, there's nothing to scan — leave the (already-
        // cleared-by-`switch_dir`) cache empty rather than wait forever for
        // a result that will never arrive.
        #[cfg(target_arch = "wasm32")]
        match self.catalog.wasm_dir_handle() {
            Some(handle) => {
                let for_task = dir.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let loaded = crate::web_catalog_fs::load_sidecars(&handle).await;
                    let _ = tx.send((for_task, token, loaded));
                });
                self.catalog_load_pending = Some((dir, token));
            }
            None => {
                self.catalog_load_pending = None;
            }
        }
    }

    /// Drain finished catalog loads. For each: fold it into `Catalog` (which
    /// itself discards anything for a directory since switched away from —
    /// see `Catalog::apply_loaded`), and if it's for the *currently active*
    /// playlist's directory, re-run the ratings/edits/touchups/rotations
    /// mirror seed (the loop `seed_mirrors` also runs eagerly, empty, at
    /// request time) now that real data exists, and redraw. Returns whether
    /// a load is still outstanding, same convention as
    /// `request_working_thumbs`/`selection_pending`.
    pub(crate) fn poll_catalog_load(&mut self) -> bool {
        while let Ok((dir, token, loaded)) = self.catalog_load_rx.try_recv() {
            self.catalog.apply_loaded(&dir, loaded);
            // Compare the token, not just `dir`: a second load for the same
            // directory can be in flight (e.g. rapid A→B→A navigation) —
            // only the result matching the *latest* request for the
            // currently-pending directory should clear "still waiting". See
            // `catalog_load_pending`'s doc comment on `App`.
            if self.catalog_load_pending.as_ref() == Some(&(dir.clone(), token)) {
                self.catalog_load_pending = None;
            }
            // Taken and put back rather than borrowed: `reconcile_catalog_mirrors`
            // needs `&mut self` at the same time as the playlist it reads,
            // and `Playlist` isn't `Clone`.
            if let Some(playlist) = self.playlist.take() {
                if playlist.dir() == dir.as_path() {
                    self.reconcile_catalog_mirrors(&playlist);
                    self.request_redraw();
                }
                self.playlist = Some(playlist);
            }
        }
        self.catalog_load_pending.is_some()
    }

    /// Drain sidecar persist failures that landed asynchronously since the
    /// last poll (`catalog.rs`'s wasm32 `write_sidecar`/`delete_sidecar` are
    /// fire-and-forget `spawn_local` tasks — this is how a failure reaches
    /// `last_error`/the status toast at all). `main.rs`'s `about_to_wait`
    /// calls this every frame; a thin wrapper because `App::catalog` is
    /// private to this module.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn poll_catalog_persist_errors(&mut self) {
        self.catalog.poll_persist_errors();
    }

    /// Populate `self.ratings`/`edits`/`touchups`/`rotations` for every image
    /// in `playlist` from whatever the catalog currently has cached. Called
    /// once (against an as-yet-empty cache) from `seed_mirrors` at directory-
    /// switch time, and again from `poll_catalog_load` once the background
    /// scan actually lands.
    pub(super) fn reconcile_catalog_mirrors(&mut self, playlist: &Playlist) {
        for p in playlist.entries() {
            if let Some(stars) = self.catalog.get(p) {
                self.ratings.insert(p.clone(), stars);
            }
            let adj = self.catalog.adjustments(p);
            if !adj.is_identity() {
                self.edits.insert(p.clone(), adj);
            }
            let touchups = self.catalog.touchups(p);
            if !touchups.is_empty() {
                self.touchups.insert(p.clone(), touchups);
            }
            let rot = self.catalog.rotation(p);
            if rot != 0 {
                self.rotations.insert(p.clone(), rot);
            }
        }
    }

    /// Set the rating of the selected/shown image; recompute the view if the
    /// active filter drops it.
    pub(super) fn set_rating(&mut self, stars: u8) {
        let Some(path) = self.selected_path() else {
            return;
        };
        if stars == 0 {
            self.ratings.remove(&path);
        } else {
            self.ratings.insert(path.clone(), stars);
        }
        self.catalog.set(&path, stars);
        // A rating change can move the item in/out of a filtered view.
        if self.filter.is_some() {
            let want_idx = self.selected_index();
            self.recompute_visible();
            // Keep selection on the same playlist entry if still visible.
            if let Some(idx) = want_idx {
                if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
                    self.sel = Some(pos);
                }
            }
            // If the rated photo dropped out of the filtered view, the cursor
            // has moved to a neighbor — resync the loupe's main image to it.
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    /// After a filtered-view recompute, keep the loupe's shown image in sync with
    /// the selection: if the previously shown photo was filtered out, the cursor
    /// moved to a neighbor and the main image must follow it.
    pub(super) fn resync_loupe_selection(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        if self.want != self.selected_path() {
            self.load_selected();
            self.request_neighbors();
        }
    }

    /// Human-readable prompt for the pending bulk action, or `None` when no
    /// confirmation is open. Drives the confirm modal.
    pub(crate) fn pending_bulk_prompt(&self) -> Option<String> {
        let kind = self.pending_bulk?;
        let n = self.selection_count();
        Some(match kind {
            ui::BulkKind::Rate(0) => format!("Clear the rating on {n} photo(s)?"),
            ui::BulkKind::Rate(s) => {
                format!("Apply {} to {n} photo(s)?", "\u{2605}".repeat(s as usize))
            }
            ui::BulkKind::Export => format!("Export {n} photo(s) as JPG?"),
            ui::BulkKind::ApplySettings => {
                format!("Apply the copied settings to {n} photo(s)?")
            }
            #[cfg(not(target_arch = "wasm32"))]
            ui::BulkKind::Delete => format!("Move {n} photo(s) to the Trash?"),
            #[cfg(target_arch = "wasm32")]
            ui::BulkKind::Delete => {
                format!("Permanently delete {n} photo(s)? This cannot be undone.")
            }
        })
    }

    /// Whether there's anything for a bulk action to act on right now — the
    /// guard for `request_bulk`. Every `BulkKind` acts on the multi-selection.
    fn bulk_available(&self) -> bool {
        self.selection_count() > 0
    }

    /// Open the confirm modal for `kind` (no-op when there's nothing to act
    /// on). Shared by the toolbar buttons and the Delete/Backspace key.
    pub(super) fn request_bulk(&mut self, kind: ui::BulkKind) {
        if self.bulk_available() {
            self.pending_bulk = Some(kind);
            self.request_redraw();
        }
    }

    /// Run a confirmed bulk action.
    pub(super) fn run_bulk(&mut self, kind: ui::BulkKind) {
        match kind {
            ui::BulkKind::Rate(stars) => self.apply_rating_to_selection(stars),
            ui::BulkKind::ApplySettings => self.apply_settings_to_selection(),
            ui::BulkKind::Export => self.export_selection(),
            ui::BulkKind::Delete => self.delete_selection(),
        }
    }

    /// Remove every selected photo from disk, then drop successful removals from the playlist,
    /// the in-memory maps, and the catalog, repairing the cursor + loupe.
    pub(super) fn delete_selection(&mut self) {
        self.run_delete(self.selected_paths());
    }

    /// Move `paths` to the Trash (native) and prune all successfully handled state.
    #[cfg(not(target_arch = "wasm32"))]
    fn run_delete(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let total = paths.len();
        let mut trashed: Vec<PathBuf> = Vec::new();
        let mut last_err: Option<String> = None;
        for path in &paths {
            match trash::move_to_trash(path) {
                Ok(()) => trashed.push(path.clone()),
                Err(e) => {
                    eprintln!("[lightphotos] trash failed for {}: {e}", path.display());
                    last_err = Some(e);
                }
            }
        }
        self.finish_delete(trashed, total, last_err);
    }

    /// wasm32: File System Access has no trash — `remove_entry` is a
    /// permanent delete. Results are returned through `web_delete_rx`; the UI
    /// is pruned only for operations that actually succeed.
    #[cfg(target_arch = "wasm32")]
    fn run_delete(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            return;
        }
        let total = paths.len();
        self.web_delete_pending = Some((total, total, Vec::new(), None));
        for path in &paths {
            let tx = self.web_delete_tx.clone();
            let Some(name) = path.file_name().map(std::ffi::OsString::from) else {
                let _ = tx.send((path.clone(), Err("path has no file name".into())));
                continue;
            };
            let dir_key = path.parent().unwrap_or(Path::new("")).to_path_buf();
            let Some(dir) = self.web_dir_handles.get(&dir_key).cloned() else {
                let _ = tx.send((
                    path.clone(),
                    Err(format!("no directory handle for {}", path.display())),
                ));
                continue;
            };
            let path = path.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = crate::web_catalog_fs::remove_file(&dir, &name).await;
                let _ = tx.send((path, result));
            });
        }
    }

    /// Drain asynchronous browser deletion results and finish once every
    /// requested path has reported success or failure.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn poll_web_deletes(&mut self) {
        while let Ok((path, result)) = self.web_delete_rx.try_recv() {
            let Some((remaining, _total, removed, last_err)) = self.web_delete_pending.as_mut()
            else {
                continue;
            };
            *remaining -= 1;
            match result {
                Ok(()) => removed.push(path),
                Err(e) => {
                    web_sys::console::error_1(&format!("[web] delete failed: {e}").into());
                    *last_err = Some(e);
                }
            }
            if *remaining == 0 {
                let (_, total, removed, last_err) = self.web_delete_pending.take().unwrap();
                self.finish_delete(removed, total, last_err);
            }
        }
    }

    /// Prune every derived structure for the just-deleted paths and
    /// repair the view — shared by both `run_delete` arms.
    fn finish_delete(&mut self, trashed: Vec<PathBuf>, total: usize, last_err: Option<String>) {
        if !trashed.is_empty() {
            let gone: HashSet<PathBuf> = trashed.iter().cloned().collect();
            let survey_was_affected = self
                .survey_members
                .iter()
                .any(|p| gone.contains(p));
            if let Some(pl) = self.playlist.as_mut() {
                pl.remove_matching(|p| gone.contains(p));
            }
            for p in &trashed {
                self.ratings.remove(p);
                self.edits.remove(p);
                self.rotations.remove(p);
                self.catalog.remove(p);
                #[cfg(target_arch = "wasm32")]
                {
                    self.web_file_handles.remove(p);
                    self.web_dir_handles.remove(p);
                }
            }
            // Remove stale path- and pair-keyed duplicate state. Pending jobs
            // for deleted files must not keep the redraw loop alive forever.
            for p in &trashed {
                self.phashes.remove(p);
                self.sharpness.remove(p);
                self.capture_times.remove(p);
            }
            self.feature_distances
                .retain(|(anchor, member), _| {
                    !gone.contains(anchor) && !gone.contains(member)
                });
            self.feature_failed
                .retain(|(anchor, member)| {
                    !gone.contains(anchor) && !gone.contains(member)
                });
            self.feature_pending
                .retain(|(anchor, member)| {
                    !gone.contains(anchor) && !gone.contains(member)
                });

            // Playlist indices changed, so all derived duplicate vectors need
            // to be rebuilt against the new playlist.
            self.recompute_dup_marks();
            if survey_was_affected && self.mode == ViewMode::Survey {
                self.close_survey();
            }
            // Every index is now invalidated; rebuild the view. The cursor keeps
            // its position (clamped), landing on a neighbor of the deleted photos.
            self.selected.clear();
            self.anchor = None;
            self.recompute_visible();
            self.collapse_selection();
            if self.mode == ViewMode::Loupe {
                if self.visible.is_empty() {
                    // Nothing left to show — fall back to the grid.
                    self.mode = ViewMode::Grid;
                    self.normalize_focus();
                    self.update_window_title();
                } else {
                    self.load_selected();
                    self.request_neighbors();
                }
            }
        }
        let n = trashed.len();
        #[cfg(not(target_arch = "wasm32"))]
        let done = format!("Moved {n} photo(s) to Trash");
        // wasm32 `remove_entry` is a permanent delete, not a trash move — say so.
        #[cfg(target_arch = "wasm32")]
        let done = format!("Permanently deleted {n} photo(s)");
        #[cfg(not(target_arch = "wasm32"))]
        self.set_status(match last_err {
            None => done,
            Some(e) => format!("Moved {n}/{total} \u{2014} last error: {e}"),
        });
        #[cfg(target_arch = "wasm32")]
        self.set_status(match last_err {
            None => done,
            Some(e) => format!("Permanently deleted {n}/{total} \u{2014} last error: {e}"),
        });
        self.request_redraw();
    }

    /// Copy the primary photo's develop settings (tone only, no crop) to the
    /// in-app clipboard for pasting onto other photos. Copy is from a single
    /// photo, so it's a no-op unless exactly one is selected.
    pub(super) fn copy_settings(&mut self) {
        if self.selection_count() != 1 {
            return;
        }
        let Some(path) = self.selected_path() else {
            return;
        };
        let tone = self
            .edits
            .get(&path)
            .copied()
            .unwrap_or_default()
            .tone_only();
        let name = file_label(&path);
        self.copied_settings = Some((path, tone));
        self.set_status(format!("Copied settings from {name}"));
        self.request_redraw();
    }

    /// Apply the copied tone settings to every selected photo, preserving each
    /// photo's own crop (and rotation). Thumbnails re-bake automatically because
    /// their cache key includes the edit signature.
    pub(super) fn apply_settings_to_selection(&mut self) {
        let Some((_, tone)) = self.copied_settings.clone() else {
            return;
        };
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        for path in &paths {
            // Overwrite the tone fields; keep this photo's existing crop.
            let existing = self.edits.get(path).copied().unwrap_or_default();
            let merged = Adjustments {
                crop: existing.crop,
                ..tone
            };
            if merged.is_identity() {
                self.edits.remove(path);
            } else {
                self.edits.insert(path.clone(), merged);
            }
            self.catalog.set_adjustments(path, &merged);
        }
        // If the shown image was among them, push its new look to the GPU live.
        if let Some(shown) = self.shown.path().map(Path::to_path_buf) {
            if paths.contains(&shown) {
                self.push_adjustments();
                self.hist_dirty = true;
            }
        }
        self.set_status(format!("Applied settings to {} photo(s)", paths.len()));
        self.request_redraw();
    }

    /// Name of the file the copied settings came from, if any (for the toolbar).
    pub(crate) fn copied_settings_name(&self) -> Option<String> {
        self.copied_settings.as_ref().map(|(p, _)| file_label(p))
    }

    /// Whether develop settings are on the clipboard (enables bulk Apply Settings).
    pub(crate) fn has_copied_settings(&self) -> bool {
        self.copied_settings.is_some()
    }

    /// Apply `stars` (0 clears) to every photo in the multi-selection.
    pub(super) fn apply_rating_to_selection(&mut self, stars: u8) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        for path in &paths {
            if stars == 0 {
                self.ratings.remove(path);
            } else {
                self.ratings.insert(path.clone(), stars);
            }
            self.catalog.set(path, stars);
        }
        // Rated photos may move in/out of a filtered view; recompute + resync.
        if self.filter.is_some() {
            self.recompute_visible();
            self.resync_loupe_selection();
        }
        let n = paths.len();
        self.set_status(if stars == 0 {
            format!("Cleared rating on {n} photo(s)")
        } else {
            format!("Rated {n} photo(s) \u{2605}{stars}")
        });
        self.request_redraw();
    }

    /// Flip best-of-burst mode. Ignored while a star filter is active (bursts run
    /// only over the unfiltered folder). Turning on kicks off the background
    /// capture-time scan and rebuilds marks; turning off clears the badges but
    /// keeps the caches so re-enabling is instant.
    pub(super) fn toggle_bursts(&mut self) {
        if self.filter.is_some() {
            return; // mutually exclusive with the filter
        }
        self.bursts_on = !self.bursts_on;
        if self.bursts_on {
            self.request_capture_times();
            self.recompute_burst_marks();
            self.request_burst_thumbs();
            self.request_face_quality();
        } else {
            self.burst_marks.clear();
        }
        self.request_redraw();
    }

    /// Flip content-duplicate grouping mode. Independent of `bursts_on`/the star
    /// filter — dHash grouping is order-independent (union-find over the whole
    /// folder), so filtering doesn't break its correctness the way it would for
    /// time-adjacency bursts. Turning on kicks off the whole-folder background
    /// dHash scoring pass and rebuilds marks; turning off clears the badges but
    /// keeps the cache so re-enabling is instant.
    pub(super) fn toggle_dupes(&mut self) {
        self.dupes_on = !self.dupes_on;
        if self.dupes_on {
            self.recompute_dup_marks();
            self.request_dup_thumbs();
            self.request_feature_prints();
            self.request_face_quality();
        } else {
            self.dup_groups.clear();
            self.dup_marks.clear();
        }
        self.request_redraw();
    }

    /// Open Survey Mode on the duplicate group containing the visible cell at
    /// `pos` (a duplicate-badge click in the grid). No-op if the cell isn't
    /// currently in a duplicate group of 2+.
    pub(super) fn open_survey(&mut self, pos: usize) {
        let Some(pl) = &self.playlist else { return };
        let Some(&idx) = self.visible.get(pos) else {
            return;
        };
        let Some(&group) = self.dup_refined.get(idx) else {
            return;
        };
        let mut members = Vec::new();
        let mut best = None;
        for (i, &g) in self.dup_refined.iter().enumerate() {
            if g != group {
                continue;
            }
            let Some(p) = pl.entry(i) else { continue };
            if matches!(self.dup_marks.get(i), Some(Some(DuplicateMark::Best))) {
                best = Some(p.to_path_buf());
            }
            members.push(p.to_path_buf());
        }
        if members.len() < 2 {
            return;
        }
        self.survey_members = members;
        self.survey_best = best;
        self.survey_focus = 0;
        self.mode = ViewMode::Survey;
        self.normalize_focus();
        self.request_redraw();
    }

    /// Close Survey Mode, back to the Grid.
    pub(super) fn close_survey(&mut self) {
        self.survey_members.clear();
        self.survey_best = None;
        self.survey_focus = 0;
        self.mode = ViewMode::Grid;
        self.normalize_focus();
        self.request_redraw();
    }

    /// Set Survey Mode's focused member directly (a click on a member).
    pub(super) fn set_survey_focus(&mut self, i: usize) {
        if i < self.survey_members.len() {
            self.survey_focus = i;
            self.request_redraw();
        }
    }

    /// Move Survey Mode's focused member left/right (wrapping). No-op outside
    /// Survey Mode or with fewer than 2 members.
    pub(super) fn survey_move_focus(&mut self, delta: i32) {
        let n = self.survey_members.len();
        if n < 2 {
            return;
        }
        let cur = self.survey_focus as i32;
        self.survey_focus = (cur + delta).rem_euclid(n as i32) as usize;
        self.request_redraw();
    }

    /// One-click Survey Mode action (Aftershoot's "Spray Can" analog): rate
    /// the group's best member (from `dup_marks`, so it matches the grid
    /// badge) 5 stars, and every sibling 1 star — landing them in the
    /// existing reject range so "Delete all Rejects" can sweep them later.
    pub(super) fn keep_best_reject_rest(&mut self) {
        if self.survey_members.len() < 2 {
            return;
        }
        let Some(best) = self.survey_best.clone() else {
            return;
        };
        for path in self.survey_members.clone() {
            let stars = if path == best { 5 } else { 1 };
            self.ratings.insert(path.clone(), stars);
            self.catalog.set(&path, stars);
        }
        if self.filter.is_some() {
            self.recompute_visible();
        }
        let n = self.survey_members.len();
        self.set_status(format!(
            "Kept best, rated {} sibling(s) \u{2605}1",
            n.saturating_sub(1)
        ));
        self.request_redraw();
    }

    /// Enqueue background capture-time reads for every entry not yet cached.
    pub(super) fn request_capture_times(&mut self) {
        let Some(pl) = &self.playlist else { return };
        let paths: Vec<PathBuf> = pl
            .entries()
            .iter()
            .filter(|p| !self.capture_times.contains_key(*p))
            .cloned()
            .collect();
        if let Some(loader) = &mut self.loader {
            for p in paths {
                loader.request_meta(p);
            }
        }
    }

    /// Apply a new filter (or clear it) and recompute the visible view.
    /// No-op in Loupe mode: filtering is a Grid concept (which cells are
    /// visible) and letting it change while a specific photo is open used to
    /// be able to silently knock that photo out of the Grid's filtered
    /// selection cursor, breaking rating for the photo actually on screen.
    /// Both entry points (the Grid toolbar, hidden entirely in Loupe — see
    /// `ui::toolbar::loupe_toolbar` — and the `Shift+1-5`/`Shift+0` keyboard
    /// shortcut, which has no mode check of its own) go through here, so
    /// gating it in one place covers both.
    pub(super) fn set_filter(&mut self, filter: Option<(Cmp, u8)>) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        // Bursts run only over the unfiltered folder; applying a filter ends
        // burst mode. Clearing the filter (`None`) leaves bursts off — the user
        // re-enables with the toggle / `B`.
        if filter.is_some() && self.bursts_on {
            self.bursts_on = false;
            self.burst_marks.clear();
        }
        // Keep the selected entry across the recompute when possible.
        let want_idx = self.selected_index();
        self.filter = filter;
        self.recompute_visible();
        if let Some(idx) = want_idx {
            if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
                self.sel = Some(pos);
            }
        }
        self.request_redraw();
    }

    /// Change the toolbar comparator (≥ / = / ≤). If a star-level filter is
    /// already active, re-apply it with the new comparator so the view updates
    /// immediately. No-op in Loupe mode — same reasoning as `set_filter`,
    /// which this can also indirectly trigger.
    pub(super) fn set_filter_cmp(&mut self, cmp: Cmp) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        self.filter_cmp = cmp;
        if let Some((_, n)) = self.filter {
            if (1..=5).contains(&n) {
                self.set_filter(Some((cmp, n)));
            }
        }
        self.request_redraw();
    }

    pub(super) fn adjust_thumb_px(&mut self, grow: bool) {
        let next = if grow {
            self.thumb_px + THUMB_STEP
        } else {
            self.thumb_px.saturating_sub(THUMB_STEP)
        };
        self.thumb_px = next.clamp(THUMB_MIN, THUMB_MAX);
        self.request_redraw();
    }

    /// Rating of a given path (0 when unset).
    pub(super) fn rating_of(&self, path: &Path) -> u8 {
        self.ratings.get(path).copied().unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::Playlist;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-app-catalog-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Reproduces the reported bug. `set_filter` itself is now a no-op in
    /// Loupe (see its doc comment), so the filter can no longer change while
    /// a photo is open — but a filter set *before* entering Loupe (from the
    /// Grid) can still knock the open photo out of `visible` via
    /// `set_rating`'s own `recompute_visible()` call, e.g. rating a photo
    /// below an already-active "≥N stars" threshold. `sel` goes stale
    /// (`None`) either way, and nothing in Loupe mode ever resets it. This
    /// constructs that end state directly (rather than via a specific
    /// trigger, which could change) and checks a *subsequent* rating still
    /// applies — `self.want` is what's really being looked at, independent
    /// of the Grid's filtered cursor.
    #[test]
    fn rating_a_loupe_photo_applies_even_when_sel_has_gone_stale() {
        let dir = unique_tmp_dir();
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, b"").unwrap();

        let mut app = App::new(None);
        app.playlist = Some(Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;
        app.recompute_visible();
        app.want = Some(photo.clone());
        app.sel = None;

        app.set_rating(3);
        assert_eq!(
            app.ratings.get(&photo),
            Some(&3),
            "rating the photo the user is looking at must apply even when \
             `sel` has gone stale (None) while in Loupe"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn set_filter_is_a_no_op_in_loupe_mode() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.set_filter(Some((Cmp::Gte, 5)));
        assert_eq!(
            app.filter, None,
            "the filter must not change while a photo is open in the Loupe"
        );
    }

    #[test]
    fn set_filter_cmp_is_a_no_op_in_loupe_mode() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        let before = app.filter_cmp;
        app.set_filter_cmp(Cmp::Lte);
        assert_eq!(
            app.filter_cmp, before,
            "the filter comparator must not change while a photo is open in the Loupe"
        );
    }
}
