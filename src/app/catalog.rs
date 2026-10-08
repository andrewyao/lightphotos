use super::*;
use std::path::{Path, PathBuf};

use crate::catalog::{Flag, GroupWriteRefused};
use crate::develop::Adjustments;
use crate::groups::{Group, GroupWrite};
use crate::navigation::{Cmp, FlagFilter};
use crate::ui;

pub(super) struct CatalogLoad {
    /// Directory and token of the background sidecar scan in flight, if any.
    /// Cleared only when the result with the matching token lands. A token is
    /// needed because fast A to B to A navigation runs two loads for A, and the
    /// first one landing must not clear the second.
    pending: Option<(PathBuf, u64)>,
    /// One per `request_catalog_load` call.
    token: u64,
    tx: Sender<CatalogLoadResult>,
    rx: Receiver<CatalogLoadResult>,
}

impl CatalogLoad {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            pending: None,
            token: 0,
            tx,
            rx,
        }
    }
}

impl App {
    /// Whether the folder's sidecars are still loading.
    pub(super) fn catalog_loading(&self) -> bool {
        self.catalog_load.pending.is_some()
    }

    /// Makes the App act as if `dir`'s sidecars were still loading.
    #[cfg(test)]
    pub(super) fn pretend_catalog_loading(&mut self, dir: &Path) {
        self.catalog_load.pending = Some((dir.to_path_buf(), 1));
    }

    /// Swaps in `playlist`'s folder's derived-signal cache and copies what it
    /// holds into the two maps the culling signals live in, so a folder
    /// visited before starts with its capture times and face analyses
    /// already known.
    ///
    /// Seeding those maps is the whole integration. `request_face_quality`
    /// skips any photo already in `face_quality`, so a warm folder submits no
    /// Vision work without it changing.
    ///
    /// On native the swap runs on its own thread. Checking an entry against
    /// its file is one `stat`, a folder of thousands of photos on a network
    /// volume takes seconds of them, and writing out the outgoing folder's
    /// cache can block on a stuck volume too. Until `poll_signal_load` lands
    /// the result, `signals` is a detached cache that keeps whatever the
    /// workers record in the meantime, and `request_face_quality` holds its
    /// Vision work so a warm folder is not analysed twice. A quit in that
    /// window loses the outgoing folder's unwritten signals, which costs a
    /// recompute and nothing else.
    pub(super) fn adopt_signal_cache(&mut self, playlist: &Playlist) {
        let dir = playlist.dir();
        // The maps are keyed by full path and never cleared, so a folder
        // already adopted this session has nothing new to seed.
        if self.signals.dir() == Some(dir) {
            return;
        }

        #[cfg(target_arch = "wasm32")]
        {
            // Nothing persists on wasm, so there is no file to read.
            self.signals = crate::signalcache::SignalCache::load(dir);
        }

        #[cfg(not(target_arch = "wasm32"))]
        {
            let old = std::mem::replace(
                &mut self.signals,
                crate::signalcache::SignalCache::detached(dir),
            );
            let (tx, rx) = std::sync::mpsc::channel();
            let dir = dir.to_path_buf();
            let entries = playlist.entries().to_vec();
            let spawned = std::thread::Builder::new()
                .name("signalcache-load".into())
                .spawn(move || {
                    // Before the load, so a folder left and re-entered reads
                    // what it just wrote.
                    drop(old);
                    let cache = crate::signalcache::SignalCache::load(&dir);
                    let seeds = entries
                        .into_iter()
                        .filter_map(|p| cache.get(&p).copied().map(|s| (p, s)))
                        .collect();
                    // A later folder switch drops the receiver, and this
                    // result with it.
                    let _ = tx.send((cache, seeds));
                });
            // Without a thread the detached cache stays, so this folder's
            // signals are computed but never persisted, like a read-only one.
            self.signal_load_rx = spawned.is_ok().then_some(rx);
        }
    }

    /// Installs the loaded signal cache and seeds the maps from it. Returns
    /// true while the load is still in flight.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn poll_signal_load(&mut self) -> bool {
        let Some(rx) = &self.signal_load_rx else {
            return false;
        };
        let (mut cache, seeds) = match rx.try_recv() {
            Ok(loaded) => loaded,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.signal_load_rx = None;
                return false;
            }
        };
        self.signal_load_rx = None;
        let recorded =
            std::mem::replace(&mut self.signals, crate::signalcache::SignalCache::empty());
        cache.absorb(recorded);
        self.signals = cache;

        // A value computed during the load is at least as fresh as the file's.
        for (p, s) in seeds {
            if let Some(capture) = s.capture {
                self.capture_times
                    .entry(p.clone())
                    .or_insert(capture.to_system_time());
            }
            if let Some(q) = s.faces {
                self.faces.seed(p, q);
            }
        }
        if self.eyes_filter_on() || self.grid_sort == GridSort::Time {
            self.recompute_visible();
        }
        self.request_redraw();
        false
    }

    /// Points the catalog at `dir` and reads its sidecars in the background.
    /// `switch_dir` clears the cache first, so the old folder's ratings never
    /// show for the new one. `poll_catalog_load` folds in the result.
    pub(super) fn request_catalog_load(&mut self, dir: &Path) {
        let mark = self.catalog.switch_dir(dir);
        self.catalog_load.token += 1;
        let token = self.catalog_load.token;
        let dir = dir.to_path_buf();
        let tx = self.catalog_load.tx.clone();

        #[cfg(not(target_arch = "wasm32"))]
        {
            let for_thread = dir.clone();
            let spawned = std::thread::Builder::new()
                .name("catalog-load".into())
                .spawn(move || {
                    let loaded = crate::catalog::load_sidecars(&for_thread);
                    let _ = tx.send((for_thread.clone(), token, mark, loaded));
                    // Sweep after sending, because the UI waits on the catalog
                    // and nothing waits on the sweep.
                    crate::thumbnail::sweep_orphans(&for_thread);
                });
            match spawned {
                Ok(_) => self.catalog_load.pending = Some((dir, token)),
                Err(e) => {
                    // The catalog stays empty, so tell the user. Keep the load
                    // marked pending on purpose: export refuses to run while a
                    // load is pending, which stops it exporting without the
                    // user's edits. Switching folders again retries.
                    self.catalog
                        .note_persist_error(format!("could not load {}: {e}", dir.display()));
                    self.catalog.abandon_load();
                    self.catalog_load.pending = Some((dir, token));
                }
            }
        }

        // wasm32 has no threads and File System Access is async, so the load
        // runs as a `spawn_local` task that sends on the same channel.
        //
        // Look the handle up by `dir` so the load and the thumbnail sweep can
        // never target a different folder. No handle means nothing to load.
        #[cfg(target_arch = "wasm32")]
        match self.web.dir_handle(&dir) {
            Some(handle) => {
                let for_task = dir.clone();
                // Photos still in this folder, with their handles. The sweep
                // deletes cached thumbnails for any other name.
                let live: std::collections::HashMap<
                    std::ffi::OsString,
                    web_sys::FileSystemFileHandle,
                > = self
                    .web
                    .file_handles()
                    .iter()
                    .filter(|(p, _)| p.parent() == Some(dir.as_path()))
                    .filter_map(|(p, h)| p.file_name().map(|n| (n.to_os_string(), h.clone())))
                    .collect();
                let reads = self.web.read_inflight();
                wasm_bindgen_futures::spawn_local(async move {
                    let loaded = crate::web_catalog_fs::load_sidecars(&handle).await;
                    let _ = tx.send((for_task, token, mark, loaded));
                    crate::web_thumb_cache::sweep_orphans(&handle, &live, reads).await;
                });
                self.catalog_load.pending = Some((dir, token));
            }
            None => {
                self.catalog.abandon_load();
                self.catalog_load.pending = None;
            }
        }
    }

    /// Applies finished catalog loads and refreshes the ratings and edits
    /// mirrors for the open folder. Returns true while a load is still pending.
    pub(crate) fn poll_catalog_load(&mut self) -> bool {
        while let Ok((dir, token, mark, loaded)) = self.catalog_load.rx.try_recv() {
            let photos = match &self.playlist {
                Some(p) if p.dir() == dir.as_path() => p.entries(),
                _ => &[],
            };
            self.catalog.apply_loaded(&dir, mark, loaded, photos);
            // Match the token too: after A, B, A navigation two loads for A can
            // be in flight, and only the latest clears pending.
            if self.catalog_load.pending.as_ref() == Some(&(dir.clone(), token)) {
                self.catalog_load.pending = None;
            }
            // Take and restore the playlist, because `reconcile_catalog_mirrors`
            // needs `&mut self` while reading it.
            if let Some(playlist) = self.playlist.take() {
                let here = playlist.dir() == dir.as_path();
                if here {
                    self.reconcile_catalog_mirrors(&playlist);
                }
                self.playlist = Some(playlist);
                if here {
                    self.recompute_visible();
                    self.resync_loupe_selection();
                    self.request_redraw();
                }
            }
        }
        if self.catalog_load.pending.is_none() {
            if let Some(paths) = self.autotone.take_deferred() {
                self.enqueue_auto_tone(paths);
            }
        }
        self.catalog_load.pending.is_some()
    }

    /// Makes the app's ratings, edits, touchups, and rotations for `playlist`
    /// match the loaded catalog, removing values its sidecars no longer have.
    fn reconcile_catalog_mirrors(&mut self, playlist: &Playlist) {
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

    /// Whether a rating or flag goes to `action_paths` rather than the one
    /// selected photo: a multi-selection, or any Compare picks.
    fn marks_action_paths(&self) -> bool {
        self.action_count() > 1 || !self.group_picks().is_empty()
    }

    /// Rates the selected photo, or every photo `action_paths` names when
    /// `marks_action_paths`, with no confirmation (0 clears, above
    /// `MAX_RATING` is ignored). Under a filter the photo may drop out of
    /// view, and the loupe then follows the cursor to a neighbor.
    pub(super) fn set_rating(&mut self, stars: u8) {
        if self.marks_action_paths() {
            if stars <= crate::catalog::MAX_RATING {
                self.apply_rating_to_selection(stars);
            }
        } else if let Some(path) = self.selected_path() {
            self.set_rating_of(path, stars);
        }
    }

    /// Rates `path`, as `set_rating` does the selected photo.
    pub(super) fn set_rating_of(&mut self, path: PathBuf, stars: u8) {
        if stars > crate::catalog::MAX_RATING {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if self.rating_of(&path) != stars {
            crate::analytics::event("photo_rated");
        }
        if stars == 0 {
            self.ratings.remove(&path);
        } else {
            self.ratings.insert(path.clone(), stars);
        }
        self.catalog.set(&path, stars);
        if self.filter.is_some() {
            self.recompute_visible();
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    /// Flags the selected photo, or the photos `set_rating` would rate, with
    /// no confirmation (`None` clears). Under a flag filter the photo may
    /// drop out of view, as under a star filter.
    pub(super) fn set_flag(&mut self, flag: Option<Flag>) {
        if self.marks_action_paths() {
            self.apply_flag_to_selection(flag);
        } else if let Some(path) = self.selected_path() {
            self.set_flag_of(path, flag);
        }
    }

    /// Flags `path`, as `set_flag` does the selected photo.
    pub(super) fn set_flag_of(&mut self, path: PathBuf, flag: Option<Flag>) {
        self.catalog.set_flag(&path, flag);
        self.after_flag_change();
    }

    fn after_flag_change(&mut self) {
        if self.flag_filter != FlagFilter::All {
            self.recompute_visible();
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    pub(super) fn flag_of(&self, path: &Path) -> Option<Flag> {
        self.catalog.flag(path)
    }

    pub(super) fn apply_group_writes(&mut self, writes: Vec<GroupWrite>) -> bool {
        match self.catalog.apply_group_writes(writes) {
            Ok(()) => {
                self.recompute_visible();
                self.resync_loupe_selection();
                self.request_redraw();
                true
            }
            Err(e) => {
                eprintln!("[lightphotos] group change refused: {e}");
                let t = crate::i18n::t();
                let msg = match e {
                    GroupWriteRefused::NoFolder | GroupWriteRefused::LoadPending => {
                        t.group_refused_loading
                    }
                };
                self.set_status(StatusKind::Error, msg.to_string());
                self.request_redraw();
                false
            }
        }
    }

    /// Make the photo at `path` its group's representative and move the
    /// cursor to it, so the Loupe shows it and the grid cell takes its
    /// thumbnail. Nothing for a photo in no group.
    pub(super) fn set_group_rep(&mut self, path: &std::path::Path) {
        let Some(name) = path.file_name() else {
            return;
        };
        let Some(groups) = self.catalog.groups() else {
            return;
        };
        let Some(id) = groups.group_of(name) else {
            return;
        };
        let writes = groups.set_rep(id, &name.to_os_string());
        if writes.is_empty() || !self.apply_group_writes(writes) {
            return;
        }
        let cell = self
            .playlist
            .as_ref()
            .and_then(|pl| pl.index_of(name))
            .and_then(|i| self.place_of(i).cell());
        if let Some(pos) = cell {
            self.select_single(pos);
            self.resync_loupe_selection();
        }
    }

    fn cell_name(&self, pos: usize) -> Option<std::ffi::OsString> {
        let idx = *self.visible.get(pos)?;
        let path = self.playlist.as_ref()?.entry(idx)?;
        path.file_name().map(std::ffi::OsStr::to_os_string)
    }

    /// Groups two or more selected single photos. A selection holding a
    /// group does nothing; ungroup it first.
    pub(super) fn group_selected(&mut self) {
        if !self.group_available() {
            return;
        }
        let cells = self.selected_cells();
        let t = crate::i18n::t();
        let Some(groups) = self.catalog.groups() else {
            self.set_status(StatusKind::Error, t.group_refused_loading.to_string());
            self.request_redraw();
            return;
        };
        // The representative is the first photo with the highest score, or
        // the cursor's when none is scored.
        let scored = cells
            .iter()
            .filter_map(|&p| Some((p, self.score_at(p)?.0.value)))
            .fold(None, |best: Option<(usize, u8)>, (p, v)| match best {
                Some((_, b)) if b >= v => best,
                _ => Some((p, v)),
            });
        let primary = scored
            .map(|(p, _)| p)
            .or_else(|| self.sel.filter(|s| cells.contains(s)))
            .unwrap_or(cells[0]);
        let members = cells.iter().filter_map(|&p| self.cell_name(p)).collect();
        let merged = self
            .cell_name(primary)
            .and_then(|rep| Group::new(members, rep));
        let Some(group) = merged else {
            self.set_status(StatusKind::Error, t.group_name_unsaveable.to_string());
            self.request_redraw();
            return;
        };
        let writes = groups.create(group, web_time::SystemTime::now());
        self.apply_group_writes(writes);
    }

    pub(super) fn ungroup_selected(&mut self) {
        let cells = self.selected_cells();
        let cursor_stack = self
            .sel
            .filter(|s| cells.contains(s) && self.group_at(*s).is_some())
            .or_else(|| cells.iter().copied().find(|&p| self.group_at(p).is_some()));
        let cursor_rep = cursor_stack
            .and_then(|p| self.group_at(p))
            .map(|(_, g)| g.rep().to_os_string());
        let targets = self.selected_groups();
        let Some(groups) = self.catalog.groups() else {
            return;
        };
        let writes: Vec<GroupWrite> = targets
            .iter()
            .flat_map(|(id, _)| groups.dissolve(id))
            .collect();
        if targets.is_empty() || !self.apply_group_writes(writes) {
            return;
        }
        let Some(pl) = self.playlist.as_ref() else {
            return;
        };
        let place =
            |name: &std::ffi::OsStr| pl.index_of(name).and_then(|i| self.place_of(i).cell());
        let freed: Vec<usize> = targets
            .iter()
            .flat_map(|(_, g)| g.members())
            .filter_map(|m| place(m))
            .collect();
        let cursor = cursor_rep.as_deref().and_then(place);
        self.selected.extend(freed);
        if cursor.is_some() {
            self.sel = cursor;
            self.anchor = cursor;
        }
        self.request_redraw();
    }

    /// Labels the selected photo; `None` clears the label.
    /// No key sets a color label yet; kept for the label UI still to come.
    #[allow(dead_code)]
    fn set_label(&mut self, label: Option<crate::catalog::ColorLabel>) {
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

    /// The pending bulk action and its confirm-modal text, or `None` when no
    /// bulk confirmation is open. Trashing the Compare pane's picks reads as
    /// a bulk Delete of them.
    pub(crate) fn pending_bulk_prompt(&self) -> Option<(ui::BulkKind, String)> {
        let t = crate::i18n::t();
        let kind = match self.pending_confirm {
            Some(PendingConfirm::Bulk(kind)) => kind,
            Some(PendingConfirm::DeletePicks) => {
                let prompt = (t.confirm_delete)(self.group_picks().len());
                return Some((ui::BulkKind::Delete, prompt));
            }
            _ => return None,
        };
        let n = self.action_count();
        let prompt = match kind {
            ui::BulkKind::ApplySettings => (t.confirm_apply_settings)(n),
            ui::BulkKind::ApplyPreset(id) => {
                let name = self.presets.get(id).map(|p| p.name.clone());
                (t.confirm_apply_preset)(name.as_deref().unwrap_or_default(), n)
            }
            ui::BulkKind::AutoTone => (t.confirm_auto_tone)(n),
            ui::BulkKind::AutoToneAll => (t.confirm_auto_tone)(self.all_member_paths().len()),
            ui::BulkKind::Delete => match self.selected_groups().len() {
                0 => (t.confirm_delete)(self.selected_member_paths().len()),
                groups => (t.confirm_delete_groups)(self.selected_member_paths().len(), groups),
            },
        };
        Some((kind, prompt))
    }

    pub(super) fn confirm_open(&self) -> bool {
        self.pending_confirm.is_some()
    }

    pub(super) fn confirm_pending(&mut self) {
        if matches!(
            self.pending_confirm,
            Some(PendingConfirm::DeleteGroup { focus: None })
        ) {
            return;
        }
        match self.pending_confirm.take() {
            Some(PendingConfirm::Bulk(kind)) => self.run_bulk(kind),
            Some(PendingConfirm::DeletePreset(id)) => self.delete_preset(id),
            Some(PendingConfirm::DeletePicks) => self.delete_group_picks(),
            Some(PendingConfirm::DeleteGroup { focus }) => match focus {
                None | Some(ui::Role::Cancel) => {}
                Some(ui::Role::Primary) => self.remove_selected_groups(),
                Some(ui::Role::Danger) => self.trash_selected_groups(),
            },
            None => {}
        }
        self.request_redraw();
    }

    pub(super) fn cancel_pending(&mut self) {
        self.pending_confirm = None;
        self.request_redraw();
    }

    pub(crate) fn delete_available(&self) -> bool {
        !self.bulk_delete_running() && self.catalog_load.pending.is_none()
    }

    fn bulk_available(&self) -> bool {
        self.action_count() > 0
    }

    /// Opens the confirm modal for `kind` when something is selected.
    pub(super) fn request_bulk(&mut self, kind: ui::BulkKind) {
        if kind == ui::BulkKind::Delete && !self.delete_available() {
            return;
        }
        let available = match kind {
            ui::BulkKind::AutoToneAll => !self.visible.is_empty(),
            _ => self.bulk_available(),
        };
        if available {
            self.pending_confirm = Some(PendingConfirm::Bulk(kind));
            self.request_redraw();
        }
    }

    pub(crate) fn request_delete_group(&mut self) {
        if self.delete_available() && self.selection_has_group() {
            self.pending_confirm = Some(PendingConfirm::DeleteGroup { focus: None });
            self.request_redraw();
        }
    }

    pub(crate) fn pending_group_delete(&self) -> Option<(usize, usize)> {
        if !matches!(
            self.pending_confirm,
            Some(PendingConfirm::DeleteGroup { .. })
        ) {
            return None;
        }
        let groups = self.selected_groups();
        let photos = groups
            .iter()
            .map(|(_, g)| self.present_member_paths(g).len())
            .sum();
        Some((photos, groups.len()))
    }

    pub(crate) fn group_delete_focus(&self) -> Option<ui::Role> {
        match self.pending_confirm {
            Some(PendingConfirm::DeleteGroup { focus }) => focus,
            _ => None,
        }
    }

    pub(super) fn step_group_delete_focus(&mut self, back: bool) {
        let Some(PendingConfirm::DeleteGroup { focus }) = &mut self.pending_confirm else {
            return;
        };
        let order = ui::delete_group_tab_order();
        let n = order.len();
        let next = match focus.and_then(|f| order.iter().position(|&r| r == f)) {
            None if back => n - 1,
            None => 0,
            Some(i) if back => (i + n - 1) % n,
            Some(i) => (i + 1) % n,
        };
        *focus = Some(order[next]);
        self.request_redraw();
    }

    pub(super) fn remove_selected_groups(&mut self) {
        self.cancel_pending();
        self.ungroup_selected();
    }

    pub(super) fn trash_selected_groups(&mut self) {
        self.cancel_pending();
        let paths = self
            .selected_groups()
            .iter()
            .flat_map(|(_, g)| self.present_member_paths(g))
            .collect();
        self.start_delete(paths);
    }

    pub(super) fn run_bulk(&mut self, kind: ui::BulkKind) {
        match kind {
            ui::BulkKind::ApplySettings => self.apply_settings_to_selection(),
            ui::BulkKind::ApplyPreset(id) => self.apply_preset_to_selection(id),
            ui::BulkKind::AutoTone => self.auto_tone_selection(),
            ui::BulkKind::AutoToneAll => self.auto_tone_all(),
            ui::BulkKind::Delete => self.delete_selection(),
        }
    }

    /// Copies the selected photo's tone settings (not crop) to the in-app
    /// clipboard. Does nothing unless exactly one photo is selected.
    pub(super) fn copy_settings(&mut self) {
        let [path] = &self.action_paths()[..] else {
            return;
        };
        let path = path.clone();
        let tone = self
            .edits
            .get(&path)
            .copied()
            .unwrap_or_default()
            .tone_only();
        let name = file_label(&path);
        self.copied_settings = Some((path, tone));
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().copied_settings_from)(&name),
        );
        self.request_redraw();
    }

    /// Applies the copied tone settings to every selected photo.
    pub(super) fn apply_settings_to_selection(&mut self) {
        let Some((_, tone)) = self.copied_settings.clone() else {
            return;
        };
        let n = self.apply_tone_to(tone, &self.action_paths());
        if n == 0 {
            return;
        }
        self.set_status(StatusKind::Success, (crate::i18n::t().applied_settings)(n));
    }

    /// Writes one look onto every path, keeping each photo's own crop and
    /// straighten, and returns how many were touched. An entry whose merge comes out identity
    /// is removed rather than stored, matching how the catalog stores edits.
    /// Grid and filmstrip thumbnails re-bake by themselves, because
    /// `edit_sig_for` hashes the live edits into the thumbnail cache key.
    /// Shared by the settings clipboard and by applying a preset.
    pub(super) fn apply_tone_to(&mut self, tone: Adjustments, paths: &[PathBuf]) -> usize {
        if paths.is_empty() {
            return 0;
        }
        #[cfg(target_arch = "wasm32")]
        let mut changed = false;
        for path in paths {
            let existing = self.edits.get(path).copied().unwrap_or_default();
            let merged = Adjustments {
                crop: existing.crop,
                straighten: existing.straighten,
                ..tone
            };
            #[cfg(target_arch = "wasm32")]
            {
                changed |= merged != existing;
            }
            if merged.is_identity() {
                self.edits.remove(path);
            } else {
                self.edits.insert(path.clone(), merged);
            }
            self.catalog.set_adjustments(path, &merged);
        }
        #[cfg(target_arch = "wasm32")]
        if changed {
            crate::analytics::property("develop_edit_applied", "edit_kind", "adjustment");
        }
        if let Some(shown) = self.shown.path().map(Path::to_path_buf) {
            if paths.contains(&shown) {
                self.push_adjustments();
                self.hist.invalidate();
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
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if paths.iter().any(|p| self.rating_of(p) != stars) {
            crate::analytics::event("photo_rated");
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
        self.set_status(
            StatusKind::Success,
            if stars == 0 {
                (crate::i18n::t().cleared_rating)(n)
            } else {
                (crate::i18n::t().rated)(n, stars)
            },
        );
        self.request_redraw();
    }

    /// Applies `flag` (`None` clears) to every selected photo.
    pub(super) fn apply_flag_to_selection(&mut self, flag: Option<Flag>) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        for path in &paths {
            self.catalog.set_flag(path, flag);
        }
        self.after_flag_change();
        let n = paths.len();
        self.set_status(
            StatusKind::Success,
            (crate::i18n::t().flagged)(n, flag_name(flag)),
        );
    }

    /// Sets the grid's flag filter. Does nothing in the Loupe, as
    /// `set_filter` does not.
    pub(super) fn set_flag_filter(&mut self, filter: FlagFilter) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        self.flag_filter = filter;
        self.recompute_visible();
        self.request_redraw();
    }

    /// Sets or clears the star filter. Does nothing in the Loupe, because a
    /// filter change there could hide the open photo from the selection
    /// cursor. This is the only mode check for both the toolbar and the
    /// `Shift+0`..`Shift+5` shortcuts.
    pub(super) fn set_filter(&mut self, filter: Option<(Cmp, u8)>) {
        if self.mode == ViewMode::Loupe {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        if self.filter != filter {
            crate::analytics::property(
                "filter_used",
                "filter_kind",
                if filter.is_some() { "rating" } else { "clear" },
            );
        }
        self.filter = filter;
        self.recompute_visible();
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

/// A flag state's name as the UI shows it, `None` being Unflagged.
pub(crate) fn flag_name(flag: Option<Flag>) -> &'static str {
    let t = crate::i18n::t();
    match flag {
        Some(Flag::Pick) => t.flag_picked,
        Some(Flag::Reject) => t.flag_rejected,
        None => t.flag_unflagged,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::{load_folder, temp_folder};
    use crate::navigation::Playlist;

    #[test]
    fn choosing_a_representative_moves_the_cell_the_loupe_and_the_sidecar() {
        use crate::app::nav::tests::{cells, group_photos};
        let (mut app, dir, paths) = crate::app::test_support::folder_app("set-rep", 6);
        // Photo 2 sits between the members, so the group's cell moves.
        group_photos(&mut app, &[1, 3], 1);
        app.select_single(1);
        app.enter_loupe();

        app.set_group_rep(&paths[3]);

        assert_eq!(
            cells(&app),
            vec![0, 2, 3, 4, 5],
            "the cell shows the new rep"
        );
        assert_eq!(app.sel, Some(2), "the cursor follows the group's cell");
        assert_eq!(app.want.as_ref(), Some(&paths[3]), "the Loupe shows it");
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        let reloaded = crate::catalog::Catalog::with_dir(dir.clone());
        let reps: Vec<_> = reloaded
            .groups()
            .unwrap()
            .iter()
            .map(|(_, g)| g.rep().clone())
            .collect();
        assert_eq!(reps, vec![paths[3].file_name().unwrap().to_os_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Values cached in memory from an earlier visit must not outlive a
    /// sidecar that no longer has them, e.g. after another tool cleared it.
    #[test]
    fn reloading_a_folder_drops_values_its_sidecars_no_longer_have() {
        let dir = temp_folder("app-catalog-test");
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
        load_folder(&mut app, &dir);

        assert_eq!(app.ratings.get(&photo), None);
        assert_eq!(app.rotations.get(&photo), None);
        assert_eq!(app.edits.get(&photo), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Drive the signal cache load to completion the way the frame loop does.
    #[cfg(not(target_arch = "wasm32"))]
    fn finish_signal_load(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_signal_load() {
            assert!(
                std::time::Instant::now() < deadline,
                "signal load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The load runs off the UI thread, so a folder's cached signals arrive a
    /// few frames after it opens. They must still be seeded, what workers
    /// record in the meantime must not be lost, and no Vision work may start
    /// before the cache has had its say.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_folder_opened_while_its_signals_load_keeps_both_old_and_new() {
        use crate::facequality::FaceQuality;
        use crate::signalcache::{Signal, SignalCache};

        let dir = temp_folder("app-catalog-test");
        let other = temp_folder("app-catalog-test");
        let a = dir.join("a.jpg");
        let b = dir.join("b.jpg");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        let blink = FaceQuality {
            faces: 1,
            min_eye_openness: Some(0.1),
        };
        {
            let mut cache = SignalCache::load(&dir);
            cache.record(&a, Signal::Faces(blink));
            cache.flush_blocking(std::time::Duration::from_secs(5));
        }

        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        assert!(
            app.request_face_quality() && app.faces.submitted() == 0,
            "Vision work waits for the cache and submits nothing"
        );
        let two_faces = FaceQuality {
            faces: 2,
            min_eye_openness: None,
        };
        app.signals.record(&b, Signal::Faces(two_faces));

        finish_signal_load(&mut app);
        assert_eq!(app.face_quality_of(&a), Some(blink), "seeded from disk");
        assert_eq!(
            app.signals.get(&b).and_then(|s| s.faces),
            Some(two_faces),
            "recorded during the load"
        );

        app.load_playlist(Playlist::from_dir(&other), other.clone());
        finish_signal_load(&mut app);
        assert_eq!(
            SignalCache::load(&dir).get(&b).and_then(|s| s.faces),
            Some(two_faces),
            "leaving the folder writes what was recorded during its load"
        );

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// Rating a photo below an active "≥N" filter drops it from `visible` and
    /// leaves `sel` as `None` in the Loupe. A later rating must still apply to
    /// the photo on screen (`want`).
    #[test]
    fn rating_a_loupe_photo_applies_even_when_sel_has_gone_stale() {
        let dir = temp_folder("app-catalog-test");
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

        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The selection's coverage of Unflagged, Picked and Rejected.
    fn coverage(app: &App) -> [FlagCoverage; 3] {
        [None, Some(Flag::Pick), Some(Flag::Reject)].map(|f| app.selection_flag_coverage(f))
    }

    #[test]
    fn flags_apply_to_one_photo_or_the_selection_and_report_mixed_coverage() {
        let (mut app, dir, paths) = crate::app::test_support::folder_app("flag-sel", 3);
        app.selected = (0..3).collect();
        app.sel = Some(0);
        app.apply_ui_actions(vec![ui::UiAction::SetFlag(Some(Flag::Pick))]);
        assert!(!app.confirm_open(), "a multi-photo flag does not ask");
        assert!(paths.iter().all(|p| app.flag_of(p) == Some(Flag::Pick)));
        assert_eq!(
            coverage(&app),
            [FlagCoverage::None, FlagCoverage::All, FlagCoverage::None]
        );

        app.select_single(1);
        app.apply_ui_actions(vec![ui::UiAction::SetFlag(Some(Flag::Reject))]);
        assert_eq!(
            app.flag_of(&paths[1]),
            Some(Flag::Reject),
            "Reject replaces Pick"
        );
        assert_eq!(
            app.flag_of(&paths[0]),
            Some(Flag::Pick),
            "the others keep theirs"
        );
        assert_eq!(
            coverage(&app),
            [FlagCoverage::None, FlagCoverage::None, FlagCoverage::All]
        );

        app.selected = (0..3).collect();
        assert_eq!(
            coverage(&app),
            [FlagCoverage::None, FlagCoverage::Some, FlagCoverage::Some]
        );

        app.select_single(1);
        app.apply_ui_actions(vec![ui::UiAction::SetFlag(None)]);
        assert_eq!(app.flag_of(&paths[1]), None);
        app.selected = (0..3).collect();
        assert_eq!(
            coverage(&app),
            [FlagCoverage::Some, FlagCoverage::Some, FlagCoverage::None],
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_grid_keeps_rejects_in_view_by_default_and_the_flag_filter_narrows_the_star_filter() {
        let (mut app, dir, paths) = crate::app::test_support::folder_app("flag-filter", 4);
        app.catalog.set_flag(&paths[0], Some(Flag::Pick));
        app.catalog.set_flag(&paths[1], Some(Flag::Pick));
        app.set_rating_of(paths[1].clone(), 3);

        app.select_single(2);
        app.apply_ui_actions(vec![ui::UiAction::SetFlag(Some(Flag::Reject))]);
        assert_eq!(app.flag_filter(), FlagFilter::All);
        assert_eq!(
            app.visible_len(),
            4,
            "a reject stays, dimmed, as in Lightroom"
        );

        let shown = |app: &mut App, f| {
            app.apply_ui_actions(vec![ui::UiAction::SetFlagFilter(f)]);
            app.visible_len()
        };
        assert_eq!(shown(&mut app, FlagFilter::All), 4);
        assert_eq!(shown(&mut app, FlagFilter::Rejected), 1);
        assert_eq!(shown(&mut app, FlagFilter::Unflagged), 1);
        assert_eq!(shown(&mut app, FlagFilter::Picked), 2);
        app.set_filter(Some((Cmp::Gte, 3)));
        assert_eq!(app.visible_len(), 1, "both filters apply");
        let _ = std::fs::remove_dir_all(&dir);
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
