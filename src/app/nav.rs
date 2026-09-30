use super::bulk_delete::BulkDelete;
use super::*;
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::develop::{self};
use crate::groups::GroupId;
#[cfg(not(target_arch = "wasm32"))]
use crate::navigation::Playlist;
use crate::navigation::{self, flatten_visible_tree, visible_indices};
#[cfg(not(target_arch = "wasm32"))]
use crate::thumbnail::THUMB_PX;
use crate::ui::toolbar::ToolbarControl;

/// Where a photo, given by playlist index, sits in `visible`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Place {
    /// It has its own cell at this position: a single or a representative.
    Cell(usize),
    /// A group member without a cell of its own. Its group's cell is here.
    Hidden(usize),
    /// Filtered out, being trashed, or in a group whose cell is filtered out.
    Gone,
}

impl Place {
    /// The cell that shows the photo, its own or its group's.
    pub(super) fn cell(self) -> Option<usize> {
        match self {
            Place::Cell(p) | Place::Hidden(p) => Some(p),
            Place::Gone => None,
        }
    }
}

/// Answers [`Place`] for many photos against one `visible`, looking up each
/// group's cell once. Groups name their representative by file name, so that
/// lookup is a binary search of the playlist and worth sharing when a whole
/// selection lands on a few groups.
struct Placer<'a> {
    app: &'a App,
    group_cells: HashMap<&'a GroupId, Option<usize>>,
}

impl<'a> Placer<'a> {
    fn new(app: &'a App) -> Self {
        Placer {
            app,
            group_cells: HashMap::new(),
        }
    }

    fn place(&mut self, idx: usize) -> Place {
        let visible = &self.app.visible;
        if let Ok(p) = visible.binary_search(&idx) {
            return Place::Cell(p);
        }
        let (Some(pl), Some(groups)) = (self.app.playlist.as_ref(), self.app.catalog.groups())
        else {
            return Place::Gone;
        };
        let Some(id) = pl
            .entry(idx)
            .and_then(Path::file_name)
            .and_then(|n| groups.group_of(n))
        else {
            return Place::Gone;
        };
        let cell = *self.group_cells.entry(id).or_insert_with(|| {
            let rep = pl.index_of(groups.get(id)?.rep())?;
            visible.binary_search(&rep).ok()
        });
        cell.map_or(Place::Gone, Place::Hidden)
    }
}

impl App {
    /// Invalidate in-flight directory listings and any navigation deferred
    /// behind one. Runs on every tree action. Thumbnails are keyed to the
    /// handle maps instead, see [`App::invalidate_web_thumb_handles`].
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn supersede_web_pending_nav(&mut self) {
        self.web_nav_generation = self.web_nav_generation.wrapping_add(1);
        self.web_pending_nav = None;
        self.web_session_restore = None;
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

    /// Recompute `visible` from the groups and the filters, keeping the same
    /// photos selected. A group is one cell, its representative's, and every
    /// group change must be followed by this call so no member shows alone.
    ///
    /// A selected photo that joined a group lands on its group's cell. The
    /// cursor does too, except in the Loupe, where it goes to `None` so the
    /// photo on screen stays put. A cursor whose photo left the view keeps
    /// its position, clamped, so it lands on a neighbor.
    #[hotpath::measure]
    pub(super) fn recompute_visible(&mut self) {
        // Positions in `visible` shift, so remember the selection as playlist
        // indices.
        let selected_pl: Vec<usize> = self
            .selected
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .collect();
        let anchor_pl = self.anchor.and_then(|p| self.visible.get(p).copied());
        let sel_pl = self.selected_index();

        let Some(pl) = &self.playlist else {
            self.visible.clear();
            self.sel = None;
            self.selected.clear();
            self.anchor = None;
            return;
        };
        let ratings = &self.ratings;
        let entries = pl.entries();
        let groups = self.catalog.groups();
        let cells = navigation::collapse_groups(entries, |n| groups.is_some_and(|g| g.hides(n)));
        self.visible = visible_indices(entries, cells, self.filter, |p| {
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
        // A running delete has trashed these but not yet dropped them from the
        // playlist, so hiding them here is what lets the grid shrink without
        // invalidating anything indexed by playlist position.
        if let Some(gone) = self.bulk_delete.as_ref().map(BulkDelete::gone) {
            if !gone.is_empty() {
                self.visible
                    .retain(|&i| entries.get(i).is_none_or(|p| !gone.contains(p)));
            }
        }
        let clamped = match self.visible.len() {
            0 => None,
            n => self.sel.map(|s| s.min(n - 1)),
        };
        let mut placer = Placer::new(self);
        let selected = selected_pl
            .iter()
            .filter_map(|&i| placer.place(i).cell())
            .collect();
        let anchor = anchor_pl.and_then(|i| placer.place(i).cell());
        let sel = match sel_pl.map(|i| placer.place(i)) {
            Some(Place::Cell(p)) => Some(p),
            Some(Place::Hidden(_)) if self.mode == ViewMode::Loupe => None,
            Some(Place::Hidden(p)) => Some(p),
            Some(Place::Gone) | None => clamped,
        };
        self.selected = selected;
        self.anchor = anchor;
        self.sel = sel;
        // The title carries the visible count.
        self.update_window_title();
    }

    /// See [`Place`].
    pub(super) fn place_of(&self, idx: usize) -> Place {
        Placer::new(self).place(idx)
    }

    /// The cell showing the Loupe's photo, its own or its group's.
    fn want_cell(&self) -> Option<usize> {
        let want = self.want.as_deref()?;
        let pl = self.playlist.as_ref()?;
        let idx = pl.index_of(want.file_name()?)?;
        (pl.entry(idx) == Some(want))
            .then(|| self.place_of(idx).cell())
            .flatten()
    }

    /// Playlist index of the current selection. `None` when nothing is selected.
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel?).copied()
    }

    /// Path of the photo that single-photo actions apply to. In the Loupe
    /// with `sel` empty (the filter hid every photo, including the open one),
    /// the photo on screen.
    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
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
        self.selected_cells()
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .filter_map(|i| pl.entry(i).map(|p| p.to_path_buf()))
            .collect()
    }

    /// The selected cells in order, or the cursor's cell alone when nothing
    /// is multi-selected.
    pub(super) fn selected_cells(&self) -> Vec<usize> {
        if self.selected.is_empty() {
            self.sel.into_iter().collect()
        } else {
            self.selected.iter().copied().collect()
        }
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
    fn extend_grid(&mut self, dx: isize, dy: isize) {
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
        // A hidden member open in the Loupe steps from its group's cell.
        let cur = self.sel.or_else(|| self.want_cell()).unwrap_or(0);
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
    fn focus_grid_first(&mut self) {
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

    pub(crate) fn left_tab(&self) -> LeftTab {
        self.left_tab
    }

    pub(super) fn set_left_tab(&mut self, tab: LeftTab) {
        if self.left_tab != tab {
            self.left_tab = tab;
            self.normalize_focus();
            self.request_redraw();
        }
    }

    /// The focused photo, when something on screen shows its metadata and it
    /// is not cached yet: the Loupe's info bar, or the Grid's Info tab.
    pub(super) fn metadata_to_read(&self) -> Option<PathBuf> {
        let shown = match self.mode {
            ViewMode::Loupe => true,
            ViewMode::Grid => self.left_tab == LeftTab::Info,
        };
        let path = self.selected_path().filter(|_| shown)?;
        (!self.exif_cache.contains_key(&path)).then_some(path)
    }

    pub(super) fn toggle_left_tab(&mut self) {
        self.set_left_tab(match self.left_tab {
            LeftTab::Folders => LeftTab::Info,
            LeftTab::Info => LeftTab::Folders,
        });
    }

    /// The Grid always has the left panel. The Loupe gives its width to the
    /// photo and shows the panel only for its Metadata tab, so `I` opens and
    /// closes it there.
    pub(crate) fn left_panel_visible(&self) -> bool {
        match self.mode {
            ViewMode::Grid => true,
            ViewMode::Loupe => self.left_tab == LeftTab::Info,
        }
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
            Region::Folders => self.left_panel_visible() && self.left_tab == LeftTab::Folders,
            Region::Develop => self.mode == ViewMode::Loupe && self.develop_visible(),
        }
    }

    /// After a mode switch, move focus off an unavailable region to the mode's
    /// main region (Grid, or Detail in the Loupe), at `Selected`.
    pub(super) fn normalize_focus(&mut self) {
        if !self.region_available(self.main_focus) {
            self.main_focus = match self.mode {
                ViewMode::Grid => Region::Grid,
                ViewMode::Loupe => Region::Detail,
            };
        }
        if self.region_available(self.focus) {
            return;
        }
        self.focus = match self.mode {
            ViewMode::Grid => Region::Grid,
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
                ViewMode::Grid => Region::Grid,
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
    fn visible_tree(&self) -> Vec<PathBuf> {
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
    fn folder_expand(&mut self) {
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
    fn folder_collapse(&mut self) {
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

    fn folder_enter(&mut self) {
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

    fn nav_left(&mut self) {
        match self.focus {
            Region::Folders => self.folder_collapse(),
            Region::Grid => self.move_grid(-1, 0),
            Region::Detail => self.step_loupe(false),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_adjust(-1),
            Region::Toolbar => {}
        }
    }

    fn nav_right(&mut self) {
        match self.focus {
            Region::Folders => self.folder_expand(),
            Region::Grid => self.move_grid(1, 0),
            Region::Detail => self.step_loupe(true),
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_adjust(1),
            Region::Toolbar => {}
        }
    }

    fn nav_up(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(-1),
            Region::Grid => self.move_grid(0, -1),
            Region::Detail => self.zoom_by(1.1),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_move(-1),
            Region::Toolbar => {}
        }
    }

    fn nav_down(&mut self) {
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
        // The cursor walks the sliders, so show them.
        self.set_develop_tab(DevelopTab::Sliders);
        let max = develop::SLIDERS.len() as isize - 1;
        self.develop_focus = (self.develop_focus as isize + delta).clamp(0, max) as usize;
        self.request_redraw();
    }

    /// How many controls the F6 cursor walks in the Grid toolbar,
    /// taken from the list the toolbar row is drawn from.
    pub(crate) const TOOLBAR_CONTROLS: usize = ToolbarControl::DRAWN;

    pub(super) fn toolbar_move(&mut self, delta: isize) {
        let n = Self::TOOLBAR_CONTROLS as isize;
        self.toolbar_focus = (self.toolbar_focus as isize + delta).rem_euclid(n) as usize;
        self.request_redraw();
    }

    /// Run the focused toolbar control's click action.
    fn activate_toolbar_focus(&mut self) {
        let Some(control) = ToolbarControl::drawn().nth(self.toolbar_focus) else {
            return;
        };
        let action = control.action(self);
        self.apply_ui_actions(vec![action]);
    }

    /// Put the keyboard cursor on toolbar control `idx`, for `ui::toolbar`'s
    /// cursor test.
    #[cfg(test)]
    pub(crate) fn focus_toolbar_control(&mut self, idx: usize) {
        self.set_focus(Region::Toolbar, FocusLevel::Entered);
        self.toolbar_focus = idx;
    }

    /// Nudge the focused Develop slider one step in direction `dir` (-1 or +1).
    fn develop_adjust(&mut self, dir: isize) {
        self.set_develop_tab(DevelopTab::Sliders);
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
pub(in crate::app) mod tests {
    use super::*;
    use crate::app::presets::tests::folder_app;
    use crate::groups::Group;
    use crate::navigation::Cmp;

    /// The playlist indices `visible` shows, for asserting a whole Grid.
    pub(in crate::app) fn cells(app: &App) -> Vec<usize> {
        app.visible.clone()
    }

    fn name(app: &App, idx: usize) -> std::ffi::OsString {
        let pl = app.playlist.as_ref().unwrap();
        pl.entry(idx).unwrap().file_name().unwrap().to_os_string()
    }

    /// Save a group of the photos at playlist indices `members`, straight
    /// through the catalog, then rebuild the view.
    pub(in crate::app) fn group_photos(app: &mut App, members: &[usize], rep: usize) {
        let names = members.iter().map(|&i| name(app, i)).collect();
        let group = Group::new(names, name(app, rep)).unwrap();
        let groups = app.catalog.groups().expect("the folder's groups loaded");
        let writes = groups.create(group, std::time::SystemTime::now());
        app.catalog.apply_group_writes(writes).unwrap();
        app.recompute_visible();
    }

    fn drain_catalog_load(app: &mut App) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.poll_catalog_load() {
            assert!(
                std::time::Instant::now() < deadline,
                "catalog load timed out"
            );
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// A folder whose `groups/` already holds a group of `members` with
    /// representative `rep`, written by a catalog of its own.
    fn folder_with_saved_group(tag: &str, n: usize, members: &[usize], rep: usize) -> PathBuf {
        let (mut app, dir, _) = folder_app(tag, n);
        group_photos(&mut app, members, rep);
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        dir
    }

    #[test]
    fn a_group_collapses_to_its_representative() {
        let (mut app, dir, _) = folder_app("nav-collapse", 6);
        group_photos(&mut app, &[1, 2, 3], 2);
        assert_eq!(cells(&app), vec![0, 2, 4, 5]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A 3+ filter hides a group whose representative is unrated even with a
    /// member at 5, and shows a group whose representative is 4 while its
    /// members are unrated.
    #[test]
    fn a_filter_judges_the_representative_alone() {
        let (mut app, dir, paths) = folder_app("nav-filter", 6);
        group_photos(&mut app, &[1, 2], 1);
        group_photos(&mut app, &[3, 4], 4);
        app.ratings.insert(paths[2].clone(), 5);
        app.ratings.insert(paths[4].clone(), 4);
        app.set_filter(Some((Cmp::Gte, 3)));
        assert_eq!(cells(&app), vec![4]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Selected members land on their group's cell, and the cursor follows
    /// its photo, not its position.
    #[test]
    fn selection_survives_a_collapse() {
        let (mut app, dir, _) = folder_app("nav-sel", 8);
        app.selected = BTreeSet::from([0, 1, 3, 5]);
        app.anchor = Some(3);
        app.sel = Some(5);
        group_photos(&mut app, &[1, 2, 3], 2);
        assert_eq!(cells(&app), vec![0, 2, 4, 5, 6, 7]);
        assert_eq!(app.selected, BTreeSet::from([0, 1, 3]));
        assert_eq!(app.anchor, Some(1), "the anchor lands on its group's cell");
        assert_eq!(app.sel, Some(3), "the cursor stays on photo 5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Groups load after the first paint. The Grid must collapse when they
    /// land, with the cursor still on the same photo.
    #[test]
    fn groups_landing_after_first_paint_collapse_the_grid() {
        let dir = folder_with_saved_group("nav-late", 6, &[1, 2, 3], 1);
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        assert_eq!(cells(&app), vec![0, 1, 2, 3, 4, 5], "no groups yet");
        app.select_single(4);
        drain_catalog_load(&mut app);
        assert_eq!(cells(&app), vec![0, 1, 4, 5]);
        assert_eq!(app.sel, Some(2), "the cursor stays on photo 4");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A photo opened from Finder that turns out to be a hidden member stays
    /// on screen when the groups land. Stepping then leaves its group's cell.
    #[test]
    fn the_loupe_keeps_a_hidden_member_on_screen_when_groups_load() {
        let dir = folder_with_saved_group("nav-hidden", 5, &[1, 2, 3], 1);
        let opened = dir.join("2.jpg");
        let mut app = App::new(None);
        app.open(opened.clone());
        drain_catalog_load(&mut app);
        assert_eq!(cells(&app), vec![0, 1, 4]);
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.sel, None);
        assert_eq!(app.want.as_deref(), Some(opened.as_path()));
        assert_eq!(app.selected_path().as_deref(), Some(opened.as_path()));

        app.step_loupe(true);
        assert_eq!(app.want, Some(dir.join("4.jpg")), "one step past the group");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A group costs one step in the Loupe.
    #[test]
    fn stepping_the_loupe_across_a_group_costs_one_step() {
        let (mut app, dir, paths) = folder_app("nav-step", 6);
        group_photos(&mut app, &[1, 2, 3], 1);
        app.select_single(0);
        app.enter_loupe();
        app.step_loupe(true);
        assert_eq!(app.want.as_ref(), Some(&paths[1]));
        app.step_loupe(true);
        assert_eq!(app.want.as_ref(), Some(&paths[4]));
        app.step_loupe(false);
        assert_eq!(app.want.as_ref(), Some(&paths[1]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A member shows through its group's cell only while the representative
    /// passes the filters.
    #[test]
    fn a_member_whose_representative_is_filtered_out_is_gone() {
        let (mut app, dir, paths) = folder_app("nav-gone", 4);
        group_photos(&mut app, &[1, 2], 1);
        assert_eq!(app.place_of(2), Place::Hidden(1));
        assert_eq!(app.place_of(1), Place::Cell(1));
        app.ratings.insert(paths[2].clone(), 5);
        app.set_filter(Some((Cmp::Gte, 3)));
        assert_eq!(app.place_of(2), Place::Gone);
        assert_eq!(app.place_of(1), Place::Gone);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Rating a photo out of the filter leaves the cursor on its neighbor.
    #[test]
    fn rating_a_photo_out_of_the_filter_moves_to_its_neighbor() {
        let (mut app, dir, paths) = folder_app("nav-rate", 4);
        for p in &paths {
            app.ratings.insert(p.clone(), 3);
        }
        app.set_filter(Some((Cmp::Gte, 3)));
        app.select_single(1);
        app.set_rating(1);
        assert_eq!(cells(&app), vec![0, 2, 3]);
        assert_eq!(app.selected_path().as_ref(), Some(&paths[2]));
        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A stack paints its member count inside its own cell in the Grid, and
    /// in the filmstrip too.
    #[test]
    fn a_group_cell_paints_its_member_count_inside_the_cell() {
        use crate::app::presets::tests::settled;
        let (mut app, dir, _) = folder_app("nav-pill", 8);
        group_photos(&mut app, &[1, 2, 3, 4, 5, 6], 1);
        let painted = settled(&mut app);
        let cell = app.grid_cell_rect(1).expect("the stack's cell is drawn");
        assert!(
            cell.contains(painted.pos_of("6")),
            "the count sits inside {cell:?}"
        );
        assert_eq!(painted.texts().iter().filter(|t| **t == "6").count(), 1);

        app.select_single(1);
        app.enter_loupe();
        assert!(settled(&mut app).has("6"), "the filmstrip shows the count");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Loupe names its folder, and the arrow beside the name goes back to
    /// the Grid.
    #[test]
    fn the_loupe_back_arrow_returns_to_the_grid() {
        use crate::app::presets::tests::{click, settled};
        use crate::ui::UiAction;

        let dir = std::env::temp_dir().join(format!("lp-nav-back-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), []).unwrap();
        let name = dir.file_name().unwrap().to_str().unwrap().to_owned();
        let mut app = App::new(None);
        app.open(dir.clone());
        app.sel = Some(0);
        app.enter_loupe();
        assert_eq!(app.mode, ViewMode::Loupe);

        let painted = settled(&mut app);
        assert!(painted.texts().contains(&name.as_str()));
        let (actions, _) = click(&mut app, painted.pos_of("\u{2190}"));
        assert_eq!(actions, vec![UiAction::EnterGrid]);
        app.apply_ui_actions(actions);
        assert_eq!(app.mode, ViewMode::Grid);
        let painted = settled(&mut app);
        assert!(painted.texts().contains(&name.as_str()));
        assert!(!painted.texts().contains(&"\u{2190}"));
    }

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
