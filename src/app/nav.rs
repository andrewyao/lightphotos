use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use crate::develop::{self};
#[cfg(not(target_arch = "wasm32"))]
use crate::navigation::Playlist;
use crate::navigation::{self, Cmp, flatten_visible_tree, visible_indices};
#[cfg(not(target_arch = "wasm32"))]
use crate::thumbnail::THUMB_PX;
use crate::ui;

impl App {
    /// Invalidate in-flight directory listings and any navigation deferred
    /// behind one. Runs on every tree action. Thumbnails are keyed to the
    /// handle maps instead, see [`App::invalidate_web_thumb_handles`].
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn supersede_web_pending_nav(&mut self) {
        self.web_nav_generation = self.web_nav_generation.wrapping_add(1);
        self.web_pending_nav = None;
    }

    /// Invalidate thumbnail work after a pick replaces the handle maps.
    /// `poll_web_thumbs` drops the old results on arrival. Clearing their keys
    /// lets the new pick request the same browser paths again.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn invalidate_web_thumb_handles(&mut self) {
        self.web_handle_generation = self.web_handle_generation.wrapping_add(1);
        self.web_thumb_inflight.clear();
        self.web_thumb_recovery_pending.clear();
        self.web_thumb_retries.clear();
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn defer_web_nav(&mut self, nav: crate::app::WebPendingNav) {
        self.web_pending_nav_generation = self.web_nav_generation;
        self.web_pending_nav = Some(nav);
    }

    /// Recompute `visible` from the filters, keeping the same photos selected.
    pub(super) fn recompute_visible(&mut self) {
        // Positions in `visible` shift when the filter changes, so remember the
        // selection as playlist indices.
        let sel_pl: Vec<usize> = self
            .selected
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .collect();
        let anchor_pl = self.anchor.and_then(|p| self.visible.get(p).copied());

        let Some(pl) = &self.playlist else {
            self.visible.clear();
            self.sel = None;
            self.selected.clear();
            self.anchor = None;
            return;
        };
        let ratings = &self.ratings;
        let entries = pl.entries();
        self.visible = visible_indices(entries, self.filter, |p| {
            ratings.get(p).copied().unwrap_or(0)
        });
        if self.eyes_filter {
            let keep: Vec<usize> = self
                .visible
                .iter()
                .copied()
                .filter(|&i| entries.get(i).is_some_and(|p| self.eyes_closed(p)))
                .collect();
            self.visible = keep;
        }
        if self.visible.is_empty() {
            self.sel = None;
        } else if let Some(s) = self.sel {
            if s >= self.visible.len() {
                self.sel = Some(self.visible.len() - 1);
            }
        }
        self.selected = remap_positions(&sel_pl, &self.visible);
        self.anchor = anchor_pl.and_then(|i| self.visible.iter().position(|&v| v == i));
    }

    /// Playlist index of the current selection. `None` when nothing is selected.
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel?).copied()
    }

    /// Path of the photo that single-photo actions apply to. In Survey Mode,
    /// the focused member. In the Loupe with `sel` empty (the filter hid every
    /// photo, including the open one), the photo on screen.
    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        if self.mode == ViewMode::Survey {
            return self.survey_members.get(self.survey_focus).cloned();
        }
        if self.mode == ViewMode::Loupe && self.sel.is_none() {
            return self.want.clone();
        }
        let pl = self.playlist.as_ref()?;
        let idx = self.selected_index()?;
        pl.entry(idx).map(|p| p.to_path_buf())
    }

    /// Paths bulk actions apply to, in `visible` order. Falls back to `sel`,
    /// then in the Loupe to the photo on screen, like `selected_path`.
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        if self.mode == ViewMode::Loupe && self.selected.is_empty() && self.sel.is_none() {
            return self.want.clone().into_iter().collect();
        }
        let Some(pl) = self.playlist.as_ref() else {
            return Vec::new();
        };
        let positions: Vec<usize> = if self.selected.is_empty() {
            self.sel.into_iter().collect()
        } else {
            self.selected.iter().copied().collect()
        };
        positions
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .filter_map(|i| pl.entry(i).map(|p| p.to_path_buf()))
            .collect()
    }

    /// `selected_paths().len()` without building the list.
    pub(crate) fn selection_count(&self) -> usize {
        if self.selected.is_empty() {
            if self.sel.is_some() {
                1
            } else {
                usize::from(self.mode == ViewMode::Loupe && self.want.is_some())
            }
        } else {
            self.selected.len()
        }
    }

    /// Reduce the multi-selection to `sel` alone.
    pub(super) fn collapse_selection(&mut self) {
        self.selected = self.sel.into_iter().collect();
        self.anchor = self.sel;
    }

    pub(super) fn select_single(&mut self, pos: usize) {
        self.sel = Some(pos);
        self.anchor = Some(pos);
        self.selected = BTreeSet::from([pos]);
    }

    /// Cmd-click: toggle `pos`. `sel` moves to the clicked cell, or to the last
    /// remaining member when the cell is deselected.
    pub(super) fn select_toggle(&mut self, pos: usize) {
        if self.selected.remove(&pos) {
            self.sel = self.selected.iter().next_back().copied();
        } else {
            self.selected.insert(pos);
            self.sel = Some(pos);
        }
        self.anchor = self.sel;
    }

    /// Shift-click or Shift-arrow: select from the anchor to `pos`. The anchor
    /// stays put, so repeated Shift-clicks resize the same range.
    pub(super) fn select_range(&mut self, pos: usize) {
        if self.anchor.is_none() {
            self.anchor = self.sel.or(Some(pos));
        }
        let a = self.anchor.unwrap_or(pos);
        self.selected = range_set(a, pos);
        self.sel = Some(pos);
    }

    pub(super) fn select_all(&mut self) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        self.selected = (0..n).collect();
        if self.sel.is_none() {
            self.sel = Some(0);
        }
        self.anchor = self.sel;
    }

    /// Shift+arrow in the grid.
    pub(super) fn extend_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        let from = self.sel.unwrap_or(0);
        if self.anchor.is_none() {
            self.anchor = Some(from);
        }
        let pos = navigation::grid_move(from, self.visible.len(), self.grid_cols, dx, dy);
        self.select_range(pos);
        self.request_redraw();
    }

    pub(crate) fn is_selected(&self, pos: usize) -> bool {
        self.selected.contains(&pos)
    }

    /// Make the selection the loupe's wanted image. Requests the preview, with
    /// the thumbnail as an instant placeholder. Full resolution costs seconds
    /// and hundreds of MB, so it waits until the user zooms
    /// (`ensure_full_for_zoom`).
    pub(super) fn load_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        // On wasm32 `app/web.rs` decodes the preview and thumbnail instead.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let px = THUMB_PX;
            let preview_px = self.preview_px();
            if let Some(loader) = &mut self.loader {
                loader.request_preview(path.clone(), preview_px);
                loader.request_thumb(path.clone(), px);
            }
        }
        // A new photo needs its own source size. `on_exif_info` fills it and
        // re-fits if the metadata isn't cached yet.
        if self.want.as_deref() != Some(path.as_path()) {
            self.source_size = self.exif_cache.get(&path).and_then(|m| m.source_size);
        }
        self.want = Some(path);
        self.invalidate_selection();
        self.request_selection_mask();
        self.try_show();
    }

    /// Prefetch previews of the previous and next photos so stepping feels
    /// instant. Waits until the current photo's preview is ready: otherwise all
    /// three decode at once, and on 24MP RAW the open went from ~300ms to
    /// ~970ms with the current photo finishing last.
    pub(crate) fn request_neighbors(&mut self) {
        // The frame loop calls this in any mode. Outside the Loupe, neighbors
        // mean nothing.
        if self.mode != ViewMode::Loupe || self.visible.len() <= 1 {
            return;
        }
        let target = self.preview_px();
        let current_ready = match (&self.want, self.loader.as_ref()) {
            (Some(want), Some(loader)) => loader.get_preview(want, target).is_some(),
            _ => false,
        };
        if !current_ready {
            return;
        }
        let Some(cur) = self.sel else { return };
        let prev = (cur + self.visible.len() - 1) % self.visible.len();
        let next = (cur + 1) % self.visible.len();
        let paths: Vec<PathBuf> = [prev, next]
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .filter_map(|i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .map(|p| p.to_path_buf())
            .collect();
        if let Some(loader) = &mut self.loader {
            for p in paths {
                loader.prefetch_preview(p, target);
            }
        }
    }

    /// Wheel over the filmstrip steps through photos, since egui doesn't pan a
    /// horizontal `ScrollArea` with a vertical wheel. `delta` is this frame's
    /// wheel delta (positive is up or left). It accumulates across frames so
    /// small trackpad deltas still add up to a step.
    pub(super) fn scroll_filmstrip(&mut self, delta: f32) {
        const STEP_PX: f32 = 30.0;
        self.filmstrip_scroll_accum += delta;
        while self.filmstrip_scroll_accum >= STEP_PX {
            self.step_loupe(false);
            self.filmstrip_scroll_accum -= STEP_PX;
        }
        while self.filmstrip_scroll_accum <= -STEP_PX {
            self.step_loupe(true);
            self.filmstrip_scroll_accum += STEP_PX;
        }
    }

    pub(super) fn step_loupe(&mut self, forward: bool) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len();
        let cur = self.sel.unwrap_or(0);
        self.sel = Some(if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        });
        self.collapse_selection();
        self.load_selected();
        self.request_neighbors();
        self.request_redraw();
    }

    /// Move the grid selection by (dx, dy) cells. With no selection, the first
    /// press lands on the first cell.
    pub(super) fn move_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.sel = Some(match self.sel {
            None => 0,
            Some(s) => navigation::grid_move(s, self.visible.len(), self.grid_cols, dx, dy),
        });
        self.collapse_selection();
        self.request_redraw();
    }

    /// Enter from Folders: focus the Grid and select its first image. Focus
    /// moves even when the grid is empty, so Escape can return to Folders.
    pub(super) fn focus_grid_first(&mut self) {
        self.focus = Region::Grid;
        self.focus_level = FocusLevel::Selected;
        self.on_focus_changed();
        if !self.visible.is_empty() {
            self.sel = Some(0);
            self.collapse_selection();
        }
        self.request_redraw();
    }

    /// Open the Loupe on the selection. Does nothing when nothing is selected.
    pub(super) fn enter_loupe(&mut self) {
        if self.selected_path().is_none() {
            return;
        }
        self.mode = ViewMode::Loupe;
        self.develop_open = true;
        self.compare = false;
        self.load_selected();
        self.request_neighbors();
        self.normalize_focus();
        self.request_redraw();
    }

    pub(super) fn enter_grid(&mut self) {
        if self.mode != ViewMode::Grid {
            self.mode = ViewMode::Grid;
            self.update_window_title();
            self.normalize_focus();
            self.request_redraw();
        }
    }

    pub(crate) fn folders_visible(&self) -> bool {
        true
    }

    pub(crate) fn develop_visible(&self) -> bool {
        self.develop_open
    }

    /// Loupe only.
    pub(crate) fn filmstrip_visible(&self) -> bool {
        true
    }

    pub(crate) fn metadata_panel_visible(&self) -> bool {
        true
    }

    /// Whether region `r` can take keyboard focus in the current mode.
    pub(super) fn region_available(&self, r: Region) -> bool {
        match r {
            Region::Toolbar => self.mode != ViewMode::Loupe,
            Region::Grid => self.mode == ViewMode::Grid,
            Region::Detail => self.mode == ViewMode::Loupe,
            Region::Filmstrip => self.mode == ViewMode::Loupe,
            Region::Folders => self.folders_visible(),
            Region::Develop => self.mode == ViewMode::Loupe && self.develop_visible(),
        }
    }

    /// After a mode switch, move focus off an unavailable region to the mode's
    /// main region (Grid, or Detail in the Loupe), at `Selected`.
    pub(super) fn normalize_focus(&mut self) {
        if !self.region_available(self.main_focus) {
            self.main_focus = match self.mode {
                ViewMode::Grid | ViewMode::Survey => Region::Grid,
                ViewMode::Loupe => Region::Detail,
            };
        }
        if self.region_available(self.focus) {
            return;
        }
        self.focus = match self.mode {
            ViewMode::Grid | ViewMode::Survey => Region::Grid,
            ViewMode::Loupe => Region::Detail,
        };
        self.focus_level = FocusLevel::Selected;
        self.on_focus_changed();
    }

    /// F6: step around the ring `[main_focus, Toolbar, Filmstrip]`, skipping
    /// unavailable regions. Always lands at `Selected`.
    pub(super) fn cycle_region(&mut self, backward: bool) {
        let main = if self.region_available(self.main_focus) {
            self.main_focus
        } else {
            match self.mode {
                ViewMode::Grid | ViewMode::Survey => Region::Grid,
                ViewMode::Loupe => Region::Detail,
            }
        };
        let ring = [main, Region::Toolbar, Region::Filmstrip];
        let n = ring.len();
        let cur = ring.iter().position(|&r| r == self.focus).unwrap_or(0);
        let mut i = cur;
        for _ in 0..n {
            i = if backward {
                (i + n - 1) % n
            } else {
                (i + 1) % n
            };
            if i == 0 || self.region_available(ring[i]) {
                self.focus = ring[i];
                self.focus_level = FocusLevel::Selected;
                self.on_focus_changed();
                self.request_redraw();
                return;
            }
        }
    }

    /// F6 while a region is entered. Moves the Toolbar's control cursor;
    /// elsewhere it cycles regions as usual.
    pub(super) fn cycle_control(&mut self, backward: bool) {
        if self.focus == Region::Toolbar {
            self.toolbar_move(if backward { -1 } else { 1 });
            return;
        }
        self.cycle_region(backward);
    }

    /// Run on every focus change. Resets the Develop and Toolbar cursors to
    /// their first control, and records `main_focus` for a main-chain region.
    pub(super) fn on_focus_changed(&mut self) {
        if !CHROME_ORDER.contains(&self.focus) {
            self.main_focus = self.focus;
        }
        match self.focus {
            Region::Develop => self.develop_focus = 0,
            Region::Toolbar => self.toolbar_focus = 0,
            _ => {}
        }
    }

    pub(super) fn set_focus(&mut self, focus: Region, level: FocusLevel) {
        self.focus = focus;
        self.focus_level = level;
        self.on_focus_changed();
    }

    /// The tree's visible rows, top to bottom.
    pub(super) fn visible_tree(&self) -> Vec<PathBuf> {
        let Some(root) = self.folder_root.clone() else {
            return Vec::new();
        };
        let is_expanded = |p: &Path| self.expanded.contains(p);
        let children = |p: &Path| self.subdirs.get(p).cloned().unwrap_or_default();
        flatten_visible_tree(&root, &is_expanded, &children)
    }

    /// Load `dir` into the grid without changing expansion. On wasm32 this
    /// waits for `dir`'s listing first.
    fn nav_to_folder(&mut self, dir: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        self.load_folder(dir);
        #[cfg(target_arch = "wasm32")]
        {
            if !self.subdirs.contains_key(&dir) {
                self.defer_web_nav(crate::app::WebPendingNav::Load(dir.clone()));
                self.request_dir_listing(&dir);
                self.request_redraw();
                return;
            }
            self.apply_web_load_folder(dir);
        }
    }

    /// Up/Down in the tree: move `delta` rows and load that folder.
    pub(super) fn folder_move(&mut self, delta: isize) {
        #[cfg(target_arch = "wasm32")]
        self.supersede_web_pending_nav();
        let tree = self.visible_tree();
        if tree.is_empty() {
            return;
        }
        let cur = self
            .folder_sel
            .as_ref()
            .and_then(|c| tree.iter().position(|p| p == c))
            .unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, tree.len() as isize - 1) as usize;
        if self.folder_sel.as_deref() != Some(tree[next].as_path()) {
            self.nav_to_folder(tree[next].clone());
        }
    }

    /// Right arrow in the tree: expand the folder, or load its first child if
    /// already expanded.
    pub(super) fn folder_expand(&mut self) {
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        self.supersede_web_pending_nav();
        self.ensure_subdirs(&cur);
        #[cfg(target_arch = "wasm32")]
        if !self.subdirs.contains_key(&cur) {
            // The listing just started. The next press expands.
            return;
        }
        if self.subdirs(&cur).is_empty() {
            return;
        }
        if self.expanded.contains(&cur) {
            if let Some(first) = self.subdirs(&cur).first().cloned() {
                self.nav_to_folder(first);
            }
        } else {
            self.expanded.insert(cur);
            self.request_redraw();
        }
    }

    /// Left arrow in the tree: collapse the folder, or load its parent if
    /// already collapsed. Stops at the root.
    pub(super) fn folder_collapse(&mut self) {
        #[cfg(target_arch = "wasm32")]
        self.supersede_web_pending_nav();
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        if self.expanded.contains(&cur) {
            self.expanded.remove(&cur);
            self.request_redraw();
        } else if Some(cur.as_path()) != self.folder_root.as_deref() {
            if let Some(parent) = cur.parent() {
                self.nav_to_folder(parent.to_path_buf());
            }
        }
    }

    pub(super) fn folder_enter(&mut self) {
        let Some(sel) = self.folder_sel.clone() else {
            return;
        };
        self.open_folder(sel);
    }

    /// Folder-row click or Enter: toggle expansion and load the folder into
    /// the grid.
    pub(super) fn open_folder(&mut self, path: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.ensure_subdirs(&path);
            let subdirs = self.subdirs(&path).to_vec();
            let pure_container =
                !subdirs.is_empty() && Playlist::from_dir(&path).entries().is_empty();
            if !subdirs.is_empty() {
                if self.expanded.contains(&path) {
                    if !pure_container {
                        self.expanded.remove(&path);
                    }
                } else {
                    self.expanded.insert(path.clone());
                }
            }
            // A folder with subfolders but no photos (a year folder) would show
            // an empty grid, so load its first child instead. It stays expanded.
            let target = if pure_container {
                subdirs.into_iter().next().unwrap_or(path)
            } else {
                path
            };
            self.load_folder(target);
            self.mode = ViewMode::Grid;
            self.update_window_title();
            self.normalize_focus();
            self.request_redraw();
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.supersede_web_pending_nav();
            if !self.subdirs.contains_key(&path) {
                self.defer_web_nav(crate::app::WebPendingNav::Open(path.clone()));
                self.request_dir_listing(&path);
                self.request_redraw();
                return;
            }
            self.apply_web_open_folder(path);
        }
    }

    pub(super) fn nav_left(&mut self) {
        match self.focus {
            Region::Folders => self.folder_collapse(),
            Region::Grid => self.move_grid(-1, 0),
            Region::Detail => self.step_loupe(false),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_adjust(-1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_right(&mut self) {
        match self.focus {
            Region::Folders => self.folder_expand(),
            Region::Grid => self.move_grid(1, 0),
            Region::Detail => self.step_loupe(true),
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_adjust(1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_up(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(-1),
            Region::Grid => self.move_grid(0, -1),
            Region::Detail => self.zoom_by(1.1),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_move(-1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_down(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(1),
            Region::Grid => self.move_grid(0, 1),
            Region::Detail => self.zoom_by(1.0 / 1.1),
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_move(1),
            Region::Toolbar => {}
        }
    }

    /// Enter: go one step deeper in the main chain (Folders > Grid > Detail >
    /// Develop). In the Toolbar, the first Enter enters and the second
    /// activates the control. In the Filmstrip, it returns to the main region.
    pub(super) fn nav_enter(&mut self) {
        match self.focus {
            Region::Folders => {
                self.folder_enter();
                if self.mode == ViewMode::Grid {
                    self.focus_grid_first();
                }
            }
            Region::Grid => {
                self.enter_loupe();
                if self.mode == ViewMode::Loupe {
                    self.focus = Region::Detail;
                    self.focus_level = FocusLevel::Selected;
                    self.on_focus_changed();
                    self.request_redraw();
                }
            }
            Region::Detail => {
                self.focus = Region::Develop;
                self.focus_level = FocusLevel::Entered;
                self.on_focus_changed(); // seeds develop_focus = 0
                self.request_redraw();
            }
            Region::Filmstrip => {
                self.focus = self.main_focus;
                self.focus_level = FocusLevel::Selected;
                self.on_focus_changed();
                self.request_redraw();
            }
            Region::Toolbar => {
                if self.focus_level == FocusLevel::Selected {
                    self.focus_level = FocusLevel::Entered;
                    self.on_focus_changed(); // seeds toolbar_focus = 0
                    self.request_redraw();
                } else {
                    self.activate_toolbar_focus();
                }
            }
            Region::Develop => {}
        }
    }

    /// Route an arrow key to the focused region. Arrows enter every region but
    /// the Grid, so one Escape still leaves the Grid.
    pub(super) fn nav_arrow(&mut self, dx: isize, dy: isize, shift: bool) {
        if self.focus != Region::Grid {
            self.focus_level = FocusLevel::Entered;
        }
        if shift && self.focus == Region::Grid {
            self.extend_grid(dx, dy);
            return;
        }
        match (dx, dy) {
            (-1, 0) => self.nav_left(),
            (1, 0) => self.nav_right(),
            (0, -1) => self.nav_up(),
            _ => self.nav_down(),
        }
    }

    pub(super) fn develop_move(&mut self, delta: isize) {
        let max = develop::SLIDERS.len() as isize - 1;
        self.develop_focus = (self.develop_focus as isize + delta).clamp(0, max) as usize;
        self.request_redraw();
    }

    /// Keyboard-focusable controls in the Grid and Survey toolbar. Must match
    /// the `toolbar_focus_sync` calls in `ui/toolbar.rs` and the index mapping
    /// in `activate_toolbar_focus`. Bulk actions are excluded because their
    /// count varies with the selection.
    const TOOLBAR_CONTROLS: usize = if SHOW_GROUPING_TOOLS { 13 } else { 10 };

    pub(super) fn toolbar_move(&mut self, delta: isize) {
        let n = Self::TOOLBAR_CONTROLS as isize;
        self.toolbar_focus = (self.toolbar_focus as isize + delta).rem_euclid(n) as usize;
        self.request_redraw();
    }

    /// Run the focused toolbar control's click action. Index order must match
    /// the `toolbar_focus_sync` calls in `ui/toolbar.rs`.
    pub(super) fn activate_toolbar_focus(&mut self) {
        let action = match self.toolbar_focus {
            0 => ui::UiAction::SetFilter(None),
            1 => ui::UiAction::SetFilterCmp(Cmp::Gte),
            2 => ui::UiAction::SetFilterCmp(Cmp::Eq),
            3 => ui::UiAction::SetFilterCmp(Cmp::Lte),
            n @ 4..=8 => {
                let star = (n - 4 + 1) as u8;
                ui::UiAction::SetFilter(Some((self.filter_cmp, star)))
            }
            9 => {
                let unrated = matches!(self.filter, Some((Cmp::Eq, 0)));
                ui::UiAction::SetFilter(if unrated { None } else { Some((Cmp::Eq, 0)) })
            }
            10 if SHOW_GROUPING_TOOLS => ui::UiAction::ToggleBursts,
            11 if SHOW_GROUPING_TOOLS => ui::UiAction::ToggleDupes,
            12 if SHOW_GROUPING_TOOLS => ui::UiAction::ToggleEyesClosed,
            _ => return,
        };
        self.apply_ui_actions(vec![action]);
    }

    /// Nudge the focused Develop slider one step in direction `dir` (-1 or +1).
    pub(super) fn develop_adjust(&mut self, dir: isize) {
        let Some(slider) = develop::SLIDERS.get(self.develop_focus) else {
            return;
        };
        let mut adj = self.current_adjustments();
        let field = (slider.field)(&mut adj);
        *field =
            (*field + dir as f32 * slider.step).clamp(*slider.range.start(), *slider.range.end());
        self.apply_adjustments(adj);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_nudges_follow_the_develop_panel_order() {
        let mut app = App::new(None);
        app.shown = Shown::Preview(PathBuf::from("/nonexistent/a.jpg"), 1024, 1024);
        let order: [fn(&Adjustments) -> f32; 11] = [
            |a| a.temp,
            |a| a.tint,
            |a| a.exposure,
            |a| a.contrast,
            |a| a.highlights,
            |a| a.shadows,
            |a| a.whites,
            |a| a.blacks,
            |a| a.vibrance,
            |a| a.saturation,
            |a| a.denoise,
        ];
        for (i, read) in order.iter().enumerate() {
            app.develop_focus = i;
            let before = read(&app.current_adjustments());
            app.develop_adjust(1);
            let step = if i == 2 { 0.05 } else { 1.0 };
            assert!(
                (read(&app.current_adjustments()) - before - step).abs() < 1e-6,
                "slider {i}"
            );
        }
    }

    /// The Loupe has no toolbar, so F6 must not land on one there.
    #[test]
    fn f6_skips_the_toolbar_in_the_loupe() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.focus = Region::Detail;
        for _ in 0..4 {
            app.cycle_region(false);
            assert_ne!(app.focus, Region::Toolbar);
        }
        app.mode = ViewMode::Grid;
        app.focus = Region::Grid;
        app.cycle_region(false);
        assert_eq!(app.focus, Region::Toolbar);
    }

    /// With `sel` empty in the Loupe, bulk actions must still act on the open
    /// photo, matching `selected_path`.
    #[test]
    fn selection_count_and_paths_fall_back_to_want_when_sel_is_stale_in_loupe() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.sel = None;
        app.selected.clear();
        app.want = Some(std::path::PathBuf::from("/tmp/does-not-need-to-exist.jpg"));

        assert_eq!(
            app.selection_count(),
            1,
            "the open photo counts as one, even with `sel` stale"
        );
        assert_eq!(
            app.selected_paths(),
            vec![app.want.clone().unwrap()],
            "the open photo must be the one bulk actions act on"
        );
    }
}
