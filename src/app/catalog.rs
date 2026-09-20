use super::*;
use std::path::{Path, PathBuf};

use crate::develop::Adjustments;
use crate::duplicates::DuplicateMark;
use crate::navigation::Cmp;
use crate::ui;

impl App {
    /// Points the catalog at `dir` and reads its sidecars in the background.
    /// `switch_dir` clears the cache first, so the old folder's ratings never
    /// show for the new one. `poll_catalog_load` folds in the result.
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
                    let _ = tx.send((for_thread.clone(), token, loaded));
                    // Sweep after sending, because the UI waits on the catalog
                    // and nothing waits on the sweep.
                    crate::thumbnail::sweep_orphans(&for_thread);
                });
            match spawned {
                Ok(_) => self.catalog_load_pending = Some((dir, token)),
                Err(e) => {
                    // The catalog stays empty, so tell the user. Keep the load
                    // marked pending on purpose: export refuses to run while a
                    // load is pending, which stops it exporting without the
                    // user's edits. Switching folders again retries.
                    self.catalog
                        .note_persist_error(format!("could not load {}: {e}", dir.display()));
                    self.catalog_load_pending = Some((dir, token));
                }
            }
        }

        // wasm32 has no threads and File System Access is async, so the load
        // runs as a `spawn_local` task that sends on the same channel.
        //
        // Look the handle up by `dir` so the load and the thumbnail sweep can
        // never target a different folder. No handle means nothing to load.
        #[cfg(target_arch = "wasm32")]
        match self.web_dir_handles.get(&dir).cloned() {
            Some(handle) => {
                let for_task = dir.clone();
                // Photos still in this folder, with their handles. The sweep
                // deletes cached thumbnails for any other name.
                let live: std::collections::HashMap<
                    std::ffi::OsString,
                    web_sys::FileSystemFileHandle,
                > = self
                    .web_file_handles
                    .iter()
                    .filter(|(p, _)| p.parent() == Some(dir.as_path()))
                    .filter_map(|(p, h)| p.file_name().map(|n| (n.to_os_string(), h.clone())))
                    .collect();
                let reads = self.web_read_inflight.clone();
                wasm_bindgen_futures::spawn_local(async move {
                    let loaded = crate::web_catalog_fs::load_sidecars(&handle).await;
                    let _ = tx.send((for_task, token, loaded));
                    crate::web_thumb_cache::sweep_orphans(&handle, &live, reads).await;
                });
                self.catalog_load_pending = Some((dir, token));
            }
            None => {
                self.catalog_load_pending = None;
            }
        }
    }

    /// Applies finished catalog loads and refreshes the ratings and edits
    /// mirrors for the open folder. Returns true while a load is still pending.
    pub(crate) fn poll_catalog_load(&mut self) -> bool {
        while let Ok((dir, token, loaded)) = self.catalog_load_rx.try_recv() {
            self.catalog.apply_loaded(&dir, loaded);
            // Match the token too: after A, B, A navigation two loads for A can
            // be in flight, and only the latest clears pending.
            if self.catalog_load_pending.as_ref() == Some(&(dir.clone(), token)) {
                self.catalog_load_pending = None;
            }
            // Take and restore the playlist, because `reconcile_catalog_mirrors`
            // needs `&mut self` while reading it.
            if let Some(playlist) = self.playlist.take() {
                if playlist.dir() == dir.as_path() {
                    self.reconcile_catalog_mirrors(&playlist);
                    self.request_redraw();
                }
                self.playlist = Some(playlist);
            }
        }
        if self.catalog_load_pending.is_none() {
            if let Some(paths) = self.autotone_deferred.take() {
                self.enqueue_auto_tone(paths);
            }
        }
        self.catalog_load_pending.is_some()
    }

    /// Makes the app's ratings, edits, touchups, and rotations for `playlist`
    /// match the loaded catalog, removing values its sidecars no longer have.
    pub(super) fn reconcile_catalog_mirrors(&mut self, playlist: &Playlist) {
        // A drag in progress lives only in `edits`; write it before the
        // catalog overwrites it.
        self.save_edit();
        for p in playlist.entries() {
            match self.catalog.get(p) {
                Some(stars) => self.ratings.insert(p.clone(), stars),
                None => self.ratings.remove(p),
            };
            let adj = self.catalog.adjustments(p);
            if adj.is_identity() {
                self.edits.remove(p);
            } else {
                self.edits.insert(p.clone(), adj);
            }
            let touchups = self.catalog.touchups(p);
            if touchups.is_empty() {
                self.touchups.remove(p);
            } else {
                self.touchups.insert(p.clone(), touchups);
            }
            match self.catalog.rotation(p) {
                0 => self.rotations.remove(p),
                rot => self.rotations.insert(p.clone(), rot),
            };
        }
    }

    /// Rates the selected photo (0 clears). Under a filter the photo may drop
    /// out of view, and the loupe then follows the cursor to a neighbor.
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
        if self.filter.is_some() {
            let want_idx = self.selected_index();
            self.recompute_visible();
            if let Some(idx) = want_idx {
                if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
                    self.sel = Some(pos);
                }
            }
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    /// Labels the selected photo; `None` clears the label.
    pub(super) fn set_label(&mut self, label: Option<crate::catalog::ColorLabel>) {
        let Some(path) = self.selected_path() else {
            return;
        };
        self.catalog.set_label(&path, label);
        self.request_redraw();
    }

    /// In the loupe, loads the selected photo if a filter recompute moved the
    /// cursor off the one shown.
    pub(super) fn resync_loupe_selection(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        if self.want != self.selected_path() {
            self.load_selected();
            self.request_neighbors();
        }
    }

    /// The confirm-modal text for the pending bulk action, or `None` when no
    /// confirmation is open.
    pub(crate) fn pending_bulk_prompt(&self) -> Option<String> {
        let kind = self.pending_bulk?;
        let n = self.selection_count();
        let t = crate::i18n::t();
        Some(match kind {
            ui::BulkKind::Rate(0) => (t.confirm_clear_rating)(n),
            ui::BulkKind::Rate(s) => (t.confirm_rate)(&"\u{2605}".repeat(s as usize), n),
            ui::BulkKind::Export => (t.confirm_export)(n),
            ui::BulkKind::ApplySettings => (t.confirm_apply_settings)(n),
            ui::BulkKind::ApplyPreset(id) => {
                let name = self.presets.get(id).map(|p| p.name.clone());
                (t.confirm_apply_preset)(name.as_deref().unwrap_or_default(), n)
            }
            ui::BulkKind::AutoTone => (t.confirm_auto_tone)(n),
            ui::BulkKind::Delete => (t.confirm_delete)(n),
        })
    }

    fn bulk_available(&self) -> bool {
        self.selection_count() > 0
    }

    /// Opens the confirm modal for `kind` when something is selected.
    pub(super) fn request_bulk(&mut self, kind: ui::BulkKind) {
        if kind == ui::BulkKind::Delete && self.bulk_delete_running() {
            return;
        }
        if self.bulk_available() {
            self.pending_bulk = Some(kind);
            self.request_redraw();
        }
    }

    pub(super) fn run_bulk(&mut self, kind: ui::BulkKind) {
        match kind {
            ui::BulkKind::Rate(stars) => self.apply_rating_to_selection(stars),
            ui::BulkKind::ApplySettings => self.apply_settings_to_selection(),
            ui::BulkKind::ApplyPreset(id) => self.apply_preset_to_selection(id),
            ui::BulkKind::AutoTone => self.auto_tone_selection(),
            ui::BulkKind::Export => self.export_selection(),
            ui::BulkKind::Delete => self.delete_selection(),
        }
    }

    /// Copies the selected photo's tone settings (not crop) to the in-app
    /// clipboard. Does nothing unless exactly one photo is selected.
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
        self.set_status((crate::i18n::t().copied_settings_from)(&name));
        self.request_redraw();
    }

    /// Applies the copied tone settings to every selected photo.
    pub(super) fn apply_settings_to_selection(&mut self) {
        let Some((_, tone)) = self.copied_settings.clone() else {
            return;
        };
        let n = self.apply_tone_to(tone, &self.selected_paths());
        if n == 0 {
            return;
        }
        self.set_status((crate::i18n::t().applied_settings)(n));
    }

    /// Writes one look onto every path, keeping each photo's own crop, and
    /// returns how many were touched. An entry whose merge comes out identity
    /// is removed rather than stored, matching how the catalog stores edits.
    /// Grid and filmstrip thumbnails re-bake by themselves, because
    /// `edit_sig_for` hashes the live edits into the thumbnail cache key.
    /// Shared by the settings clipboard and by applying a preset.
    pub(super) fn apply_tone_to(&mut self, tone: Adjustments, paths: &[PathBuf]) -> usize {
        if paths.is_empty() {
            return 0;
        }
        for path in paths {
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
        if let Some(shown) = self.shown.path().map(Path::to_path_buf) {
            if paths.contains(&shown) {
                self.push_adjustments();
                self.hist_dirty = true;
            }
        }
        self.request_redraw();
        paths.len()
    }

    /// File name the copied settings came from, if any.
    pub(crate) fn copied_settings_name(&self) -> Option<String> {
        self.copied_settings.as_ref().map(|(p, _)| file_label(p))
    }

    pub(crate) fn has_copied_settings(&self) -> bool {
        self.copied_settings.is_some()
    }

    /// Applies `stars` (0 clears) to every selected photo.
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
        if self.filter.is_some() {
            self.recompute_visible();
            self.resync_loupe_selection();
        }
        let n = paths.len();
        self.set_status(if stars == 0 {
            (crate::i18n::t().cleared_rating)(n)
        } else {
            (crate::i18n::t().rated)(n, stars)
        });
        self.request_redraw();
    }

    /// Toggles best-of-burst badges. Ignored while a star filter is active,
    /// because bursts are runs of adjacent photos in the unfiltered folder.
    /// Turning off keeps the caches, so turning back on is instant.
    pub(super) fn toggle_bursts(&mut self) {
        if self.filter.is_some() {
            return;
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

    /// Toggles duplicate badges. Unlike bursts this works under a filter,
    /// because duplicate groups cover the whole folder regardless of order.
    /// Turning off keeps the caches, so turning back on is instant.
    pub(super) fn toggle_dupes(&mut self) {
        self.dupes_on = !self.dupes_on;
        if self.dupes_on {
            self.recompute_dup_marks();
            self.request_dup_thumbs();
            self.request_feature_prints();
            self.request_face_quality();
        } else {
            self.dup_index = Default::default();
            self.dup_marks.clear();
        }
        self.request_redraw();
    }

    /// Opens Survey Mode on the duplicate group of the visible cell at `pos`.
    /// Does nothing unless that group has at least two photos.
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

    pub(super) fn close_survey(&mut self) {
        self.survey_members.clear();
        self.survey_best = None;
        self.survey_focus = 0;
        self.mode = ViewMode::Grid;
        self.normalize_focus();
        self.request_redraw();
    }

    pub(super) fn set_survey_focus(&mut self, i: usize) {
        if i < self.survey_members.len() {
            self.survey_focus = i;
            self.request_redraw();
        }
    }

    /// Moves Survey Mode's focus by `delta`, wrapping at the ends.
    pub(super) fn survey_move_focus(&mut self, delta: i32) {
        let n = self.survey_members.len();
        if n < 2 {
            return;
        }
        let cur = self.survey_focus as i32;
        self.survey_focus = (cur + delta).rem_euclid(n as i32) as usize;
        self.request_redraw();
    }

    /// Rates the group's best photo 5 stars and every other member 1 star.
    /// "Best" is `survey_best`, which matches the grid's badge.
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
        self.set_status((crate::i18n::t().kept_best)(n.saturating_sub(1)));
        self.request_redraw();
    }

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

    /// Sets or clears the star filter. Does nothing in the Loupe, because a
    /// filter change there could hide the open photo from the selection
    /// cursor. This is the only mode check for both the toolbar and the
    /// `Shift+0`..`Shift+5` shortcuts.
    pub(super) fn set_filter(&mut self, filter: Option<(Cmp, u8)>) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        // Bursts need the unfiltered folder, so a filter turns them off.
        // Clearing the filter does not turn them back on.
        if filter.is_some() && self.bursts_on {
            self.bursts_on = false;
            self.burst_marks.clear();
        }
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

    /// Sets the filter comparator (≥, =, ≤) and reapplies any active star
    /// filter. Does nothing in the Loupe, like `set_filter`.
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

    /// 0 when unrated.
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

    /// Values cached in memory from an earlier visit must not outlive a
    /// sidecar that no longer has them, e.g. after another tool cleared it.
    #[test]
    fn reloading_a_folder_drops_values_its_sidecars_no_longer_have() {
        let dir = unique_tmp_dir();
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, b"").unwrap();

        let mut app = App::new(None);
        app.ratings.insert(photo.clone(), 4);
        app.rotations.insert(photo.clone(), 1);
        app.edits.insert(
            photo.clone(),
            Adjustments {
                exposure: 1.0,
                ..Default::default()
            },
        );
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_catalog_load() {
            assert!(
                std::time::Instant::now() < deadline,
                "catalog load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }

        assert_eq!(app.ratings.get(&photo), None);
        assert_eq!(app.rotations.get(&photo), None);
        assert_eq!(app.edits.get(&photo), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// Rating a photo below an active "≥N" filter drops it from `visible` and
    /// leaves `sel` as `None` in the Loupe. A later rating must still apply to
    /// the photo on screen (`want`).
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

        app.catalog.flush_blocking(std::time::Duration::from_secs(10));
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
