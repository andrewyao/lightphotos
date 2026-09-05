use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};


use crate::develop::{self};
use crate::navigation::{self, flatten_visible_tree, visible_indices, Cmp};
#[cfg(not(target_arch = "wasm32"))]
use crate::navigation::Playlist;
use crate::ui;

impl App {

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn supersede_web_pending_nav(&mut self) {
        self.web_nav_generation = self.web_nav_generation.wrapping_add(1);
        self.web_pending_nav = None;
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn defer_web_nav(&mut self, nav: crate::app::WebPendingNav) {
        self.web_pending_nav_generation = self.web_nav_generation;
        self.web_pending_nav = Some(nav);
    }

    /// Recompute `visible` from the current filter + ratings, clamping `sel` and
    /// remapping the multi-selection so it survives re-filtering.
    pub(super) fn recompute_visible(&mut self) {
        // Snapshot the multi-selection + anchor as *playlist* indices before the
        // rebuild: positions within `visible` shift when the filter changes, but
        // playlist indices are stable, so we can restore the same photos after.
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
        // The blink filter narrows whatever the star filter left, rather than
        // replacing it — they answer different questions, so stacking them is
        // what a photographer would expect from two independent chips.
        if self.eyes_filter {
            let keep: Vec<usize> = self
                .visible
                .iter()
                .copied()
                .filter(|&i| entries.get(i).is_some_and(|p| self.eyes_closed(p)))
                .collect();
            self.visible = keep;
        }
        // Clamp the cursor to the new bounds; clear it if nothing is visible.
        if self.visible.is_empty() {
            self.sel = None;
        } else if let Some(s) = self.sel {
            if s >= self.visible.len() {
                self.sel = Some(self.visible.len() - 1);
            }
        }
        // Remap the multi-selection + anchor from playlist indices to their new
        // positions, dropping any photo the filter removed.
        self.selected = remap_positions(&sel_pl, &self.visible);
        self.anchor = anchor_pl.and_then(|i| self.visible.iter().position(|&v| v == i));
    }

    /// The playlist index of the current selection, if any. `None` when the
    /// grid has no active selection (browse-first state).
    pub(super) fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel?).copied()
    }

    /// The path of the current selection, if any. In Survey Mode this is the
    /// focused member (`survey_focus`) instead of the Grid/Loupe `sel` — so
    /// rating hotkeys "just work" against whichever screen is showing without
    /// Survey needing its own parallel rating path.
    ///
    /// In Loupe mode, falls back to `want` (the photo actually on screen)
    /// when `sel` is `None` — which happens whenever the active star filter
    /// matches nothing in the folder, including the photo currently open.
    /// Without this, a filter that knocks the open photo out of the Grid's
    /// filtered `visible` list orphans `sel` at `None` indefinitely (nothing
    /// in Loupe mode ever re-clicks a grid cell to reset it), silently
    /// breaking rating/export/etc. for the photo the user is actually
    /// looking at, even though it's still right there on screen. Doesn't
    /// touch the case where `sel` lands on a *different* (still-visible)
    /// photo after a filter/rating change — that's the intentional
    /// "advance to the next matching neighbor" behavior (see
    /// `resync_loupe_selection`), left alone.
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

    /// Paths of every photo in the multi-selection, in `visible` order. Falls
    /// back to the primary cell when the set is empty but a cell is active, so
    /// bulk operations always have at least the current photo to work on.
    ///
    /// In Loupe mode, further falls back to `want` (the photo actually on
    /// screen) when both the multi-selection and `sel` are empty — same
    /// reasoning as `selected_path`'s own fallback: `sel` can go stale while
    /// a photo is genuinely open, and bulk actions (Delete, Apply Settings,
    /// Export) should still act on it rather than silently seeing "nothing
    /// selected".
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

    /// Number of photos a bulk action would affect (the multi-selection, or the
    /// single primary cell when the set is empty). See `selected_paths`' doc
    /// comment for the Loupe/`want` fallback this mirrors.
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

    /// Collapse the multi-selection down to just the primary cell (or empty when
    /// nothing is active). Called after a plain arrow move / single click.
    pub(super) fn collapse_selection(&mut self) {
        self.selected = self.sel.into_iter().collect();
        self.anchor = self.sel;
    }

    /// Plain select: primary = `pos`, selection = `{pos}`.
    pub(super) fn select_single(&mut self, pos: usize) {
        self.sel = Some(pos);
        self.anchor = Some(pos);
        self.selected = BTreeSet::from([pos]);
    }

    /// Cmd-click: toggle `pos` in the multi-selection; the primary follows the
    /// clicked cell (or an adjacent survivor when the primary is deselected).
    pub(super) fn select_toggle(&mut self, pos: usize) {
        if self.selected.remove(&pos) {
            // Deselected the clicked cell: move the primary to another member.
            self.sel = self.selected.iter().next_back().copied();
        } else {
            self.selected.insert(pos);
            self.sel = Some(pos);
        }
        self.anchor = self.sel;
    }

    /// Shift-click / Shift-arrow: select the inclusive range from the anchor
    /// (or the primary, seeded on first use) to `pos`. The anchor stays put so
    /// the range can be re-dragged from the same origin.
    pub(super) fn select_range(&mut self, pos: usize) {
        if self.anchor.is_none() {
            self.anchor = self.sel.or(Some(pos));
        }
        let a = self.anchor.unwrap_or(pos);
        self.selected = range_set(a, pos);
        self.sel = Some(pos);
    }

    /// Cmd+A: select every visible cell.
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

    /// Shift+arrow in the grid: extend the range selection to the cell the
    /// arrow lands on, keeping the anchor fixed.
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

    /// True when the visible cell at `pos` is part of the multi-selection.
    pub(crate) fn is_selected(&self, pos: usize) -> bool {
        self.selected.contains(&pos)
    }

    /// In Loupe mode, make the selection the wanted image and request decode.
    /// Requests the screen-fit preview and (as an instant placeholder) the
    /// thumbnail, so the shown image updates immediately even before the preview
    /// decode finishes. Full resolution is deliberately *not* requested here —
    /// it costs seconds and hundreds of megabytes, and is only needed once the
    /// user zooms past what the preview holds (see `ensure_full_for_zoom`).
    pub(super) fn load_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        // Native only — see the matching comment in `app/thumbs.rs::try_show`:
        // `loader.rs`'s own worker queue is never serviced on wasm32, so
        // calling into it here only leaves a permanent (never-cleared)
        // in-flight marker behind; wasm32's Loupe/thumbnail needs are already
        // covered by `app/web.rs`'s `request_web_preview`/`request_web_thumbs`.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let px = self.thumb_px;
            let preview_px = self.preview_px();
            if let Some(loader) = &mut self.loader {
                loader.request_preview(path.clone(), preview_px);
                loader.request_thumb(path.clone(), px);
            }
        }
        // A different photo means the cached source dimensions no longer apply;
        // `on_exif_info` refills them (and re-fits) when the metadata read for
        // the new photo lands.
        if self.want.as_deref() != Some(path.as_path()) {
            self.source_size = self
                .exif_cache
                .get(&path)
                .and_then(|m| m.source_size);
        }
        self.want = Some(path);
        // The selection overlay belongs to one photo; stepping to the next
        // drops the old mask and (if the overlay is on) starts the new one.
        self.invalidate_selection();
        self.request_selection_mask();
        self.try_show();
    }

    /// Request screen-fit preview decodes of the loupe neighbors (prev/next in
    /// the visible list) so stepping feels instant. Previews only: prefetching
    /// full resolution for images the user hasn't even reached would swamp the
    /// decode pool and the memory budget for no benefit.
    ///
    /// Deliberately does nothing until the photo actually on screen has its own
    /// preview. Queue priority can't help here — the neighbors are the same tier
    /// as the current photo, so idle workers pick them up immediately and all
    /// three decode at once, competing for the same cores and memory bandwidth.
    /// Measured on 24MP RAW, that turned a ~300ms open into ~970ms, with the
    /// photo the user was *looking at* finishing last of the three. Prefetch is
    /// only worth anything once there's nothing more urgent to do.
    pub(crate) fn request_neighbors(&mut self) {
        // Guarded here rather than at each call site so the frame loop can call
        // this whenever a decode lands without caring what mode we're in — in
        // the grid, `sel` indexes grid cells while `want` is whatever the loupe
        // last showed, so "neighbors" would mean nothing.
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

    /// Move the loupe selection by ±1 within the visible list (wraps).
    /// Mouse-wheel scroll over the filmstrip steps through photos (like the
    /// Left/Right arrow keys) rather than just panning the strip: a plain
    /// vertical wheel doesn't pan a horizontal-only `ScrollArea` in egui by
    /// default, and stepping the selection is the more useful behavior for
    /// "scroll to browse" anyway. `delta` is the raw wheel delta for this
    /// frame (egui convention: positive = scroll up/left); it's accumulated
    /// across frames so small trackpad increments still add up to a step.
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

    /// Move the grid selection by (dx, dy) cells (clamped, row-aware). The first
    /// arrow press with no active selection lands on the first cell.
    pub(super) fn move_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        // First arrow press with no selection lands on the first cell.
        self.sel = Some(match self.sel {
            None => 0,
            Some(s) => navigation::grid_move(s, self.visible.len(), self.grid_cols, dx, dy),
        });
        self.collapse_selection();
        self.request_redraw();
    }

    /// Move keyboard focus into the Grid and put the cursor on its first image —
    /// the Lightroom-style "jump into this folder's photos, starting at the
    /// top". Used when Tab or Enter fires from the Folders region. Always
    /// resets to the first image (not just when nothing was selected), so a
    /// stale selection from a prior visit doesn't linger. Selection is skipped
    /// on an empty grid, but focus still moves so Escape can jump back to
    /// Folders.
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

    /// Enter Loupe on the current selection. A no-op in the grid when nothing is
    /// selected (the `selected_path` guard below).
    pub(super) fn enter_loupe(&mut self) {
        // No-op when nothing is selected; the guard also guarantees sel is Some.
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

    /// Switch to the Grid (thumbnail) view. No-op if already there.
    pub(super) fn enter_grid(&mut self) {
        if self.mode != ViewMode::Grid {
            self.mode = ViewMode::Grid;
            self.update_window_title();
            self.normalize_focus();
            self.request_redraw();
        }
    }

    // ---- Keyboard focus & panel visibility ----

    /// Whether the left folder-tree panel is currently shown.
    pub(crate) fn folders_visible(&self) -> bool {
        true
    }

    /// Whether the right Develop panel is currently shown (Loupe only).
    pub(crate) fn develop_visible(&self) -> bool {
        self.develop_open
    }

    /// Whether the bottom filmstrip is currently shown (Loupe only).
    pub(crate) fn filmstrip_visible(&self) -> bool {
        true
    }

    /// Whether the Loupe metadata panel is currently shown.
    pub(crate) fn metadata_panel_visible(&self) -> bool {
        true
    }

    /// Whether a region can receive keyboard focus right now. `Grid` (grid
    /// mode) and `Detail`/`Filmstrip` (loupe mode) are the content regions and
    /// always available in their mode. `Detail` and `Develop` are both always
    /// available in Loupe mode — the Develop panel stays visible throughout
    /// Loupe, so the two only differ in *where keyboard focus is* (the image
    /// vs. the slider list), never in what's on screen. Toolbar is always
    /// available.
    pub(super) fn region_available(&self, r: Region) -> bool {
        match r {
            Region::Toolbar => true,
            Region::Grid => self.mode == ViewMode::Grid,
            Region::Detail => self.mode == ViewMode::Loupe,
            Region::Filmstrip => self.mode == ViewMode::Loupe,
            Region::Folders => self.folders_visible(),
            Region::Develop => self.mode == ViewMode::Loupe && self.develop_visible(),
        }
    }

    /// Snap focus to a valid region when the current one isn't available (after a
    /// mode switch). Defaults to the mode's main content region — Grid, or
    /// Detail (the bare image) in Loupe — so focus lands on the image, not the
    /// Filmstrip chrome or the Develop slider list. An unavailable region can't
    /// stay "entered", so this also resets the level.
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

    /// F6: toggle between the current main-chain region and the two chrome
    /// regions, walking the fixed 3-slot ring `[main_focus, Toolbar,
    /// Filmstrip]` (reversed when `backward`), wrapping and skipping
    /// Filmstrip when it isn't available (i.e. not in Loupe mode). The main
    /// slot is always available — it's wherever `main_focus` last was. Always
    /// lands at `Selected` — F6 backs out of whatever was entered in the old
    /// region.
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

    /// F6 while a region is entered. Only Toolbar has a control cursor to
    /// move at this level — other regions' content navigation is already
    /// fully covered by arrows, so F6 there falls through to `cycle_region`
    /// like it would from `Selected`, instead of being swallowed.
    pub(super) fn cycle_control(&mut self, backward: bool) {
        if self.focus == Region::Toolbar {
            self.toolbar_move(if backward { -1 } else { 1 });
            return;
        }
        self.cycle_region(backward);
    }

    /// Hook run whenever focus changes region (or is re-entered at the same
    /// region). Resets the Develop/Toolbar cursor to the first slider/control
    /// (Folders needs no seeding — `folder_sel` doubles as its cursor and is
    /// always valid, since only interactive navigation changes it and that
    /// keeps ancestors expanded/visible as it goes). Also records `main_focus`
    /// whenever focus lands on a main-chain region, so F6/Escape can return to
    /// it from the chrome regions.
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

    /// The folder tree flattened to its currently-visible rows (DFS over expanded
    /// folders), top to bottom — the order folder arrow-nav moves through.
    pub(super) fn visible_tree(&self) -> Vec<PathBuf> {
        let Some(root) = self.folder_root.clone() else {
            return Vec::new();
        };
        let is_expanded = |p: &Path| self.expanded.contains(p);
        let children = |p: &Path| self.subdirs.get(p).cloned().unwrap_or_default();
        flatten_visible_tree(&root, &is_expanded, &children)
    }

    /// Load `dir`'s images into the grid without touching expansion state —
    /// the shared target of `folder_move` and `folder_collapse`'s
    /// select-parent branch. Native: synchronous `load_folder`. wasm: defer
    /// to `apply_web_load_folder` once `dir`'s listing is cached.
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

    /// Up/Down in the tree: move the selection by `delta` rows within the
    /// visible tree (clamped) and load the newly-selected folder, matching a
    /// standard single-select tree — there's no separate cursor to move
    /// without also loading.
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

    /// Right-arrow in the tree: expand the selected folder, or select+load its
    /// first child if already expanded. A no-op on a childless folder.
    pub(super) fn folder_expand(&mut self) {
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        self.supersede_web_pending_nav();
        self.ensure_subdirs(&cur);
        #[cfg(target_arch = "wasm32")]
        if !self.subdirs.contains_key(&cur) {
            // Listing just kicked off; the user's next right-arrow press
            // (after it lands and the triangle appears) will expand it.
            return;
        }
        if self.subdirs(&cur).is_empty() {
            return; // leaf
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

    /// Left-arrow in the tree: collapse the selected folder if open, else
    /// select+load its parent (stopping at the root).
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

    /// Enter in the tree: toggle the selected folder's expansion (when it has
    /// children) and switch to the Grid.
    pub(super) fn folder_enter(&mut self) {
        let Some(sel) = self.folder_sel.clone() else {
            return;
        };
        self.open_folder(sel);
    }

    /// Open a folder as one unit: toggle its expansion (when it has children)
    /// and load its images into the grid. Shared by the folder-row click and
    /// the Enter key so mouse and keyboard behave identically.
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
            // A pure container folder (subdirectories but no photos of its own,
            // e.g. a plain year folder) has nothing to show in the grid — loading
            // it anyway flashes an empty grid for a frame before the user drills
            // further. Skip straight to its first child instead, same place a
            // second `folder_expand` press on it would land.
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

    // ---- Focus-routed arrow / Enter dispatchers ----

    pub(super) fn nav_left(&mut self) {
        match self.focus {
            Region::Folders => self.folder_collapse(),
            Region::Grid => self.move_grid(-1, 0),
            // Reserved for a future pan feature — not bound in this pass.
            Region::Detail => {}
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_adjust(-1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_right(&mut self) {
        match self.focus {
            Region::Folders => self.folder_expand(),
            Region::Grid => self.move_grid(1, 0),
            Region::Detail => {}
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_adjust(1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_up(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(-1),
            Region::Grid => self.move_grid(0, -1),
            Region::Detail => {}
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_move(-1),
            Region::Toolbar => {}
        }
    }

    pub(super) fn nav_down(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(1),
            Region::Grid => self.move_grid(0, 1),
            Region::Detail => {}
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_move(1),
            Region::Toolbar => {}
        }
    }

    /// Enter: perform the focused region's content-level action. Walks the
    /// main chain one step deeper at a time — Folders -> Grid -> Detail ->
    /// Develop — never skipping a step. For Toolbar, which has no single
    /// always-right action besides "enter", the first Enter at `Selected`
    /// just enters (cursor to the first control); a second Enter activates
    /// the focused control. Filmstrip has no "activate" beyond what its
    /// arrows already do (live-swap the shown image), so Enter there just
    /// returns focus to the main region, same as Escape/F6 would.
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
            // Arrows already do everything inside Develop; nothing further
            // for Enter to do here.
            Region::Develop => {}
        }
    }

    /// Route an arrow key. With Shift held in the grid it extends the range
    /// selection; otherwise it's the normal focus-routed move. `(dx, dy)` maps to
    /// left/right/up/down. Arrows always act on the region's content regardless
    /// of focus level. Grid selection is already the region's content, so grid
    /// arrows do not add an extra focus level that would consume the next Escape.
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

    /// Move the Develop slider cursor by `delta` (clamped to 0..=7).
    pub(super) fn develop_move(&mut self, delta: isize) {
        let max = DEVELOP_SLIDERS as isize - 1;
        self.develop_focus = (self.develop_focus as isize + delta).clamp(0, max) as usize;
        self.request_redraw();
    }

    /// Number of keyboard-focusable controls in the Grid/Survey toolbar
    /// (`toolbar::grid_toolbar`; Phase 1: the stable set that always renders
    /// — see `toolbar_focus_sync` in `ui.rs`, which must stay in lockstep
    /// with this count and with `activate_toolbar_focus`'s index mapping.
    /// Rating-histogram bars and the selection-dependent bulk actions aren't
    /// included yet since their count varies frame to frame.
    const TOOLBAR_CONTROLS: usize = 16;
    /// Number of keyboard-focusable controls in the Loupe toolbar
    /// (`toolbar::loupe_toolbar`): Help + Grid + Loupe toggle, nothing else
    /// — everything Grid-only was deliberately dropped for Loupe, not
    /// merely disabled, so it isn't shown here either.
    const LOUPE_TOOLBAR_CONTROLS: usize = 3;

    /// The current mode's toolbar control count — a different, much smaller
    /// toolbar renders in Loupe than in Grid/Survey (see `toolbar.rs`), so
    /// the F6 keyboard-focus cycle must track whichever one is actually on
    /// screen.
    pub(super) fn toolbar_control_count(&self) -> usize {
        if self.mode == ViewMode::Loupe {
            Self::LOUPE_TOOLBAR_CONTROLS
        } else {
            Self::TOOLBAR_CONTROLS
        }
    }

    /// Move the Toolbar control cursor by `delta`, wrapping.
    pub(super) fn toolbar_move(&mut self, delta: isize) {
        let n = self.toolbar_control_count() as isize;
        if n == 0 {
            return;
        }
        self.toolbar_focus = (self.toolbar_focus as isize + delta).rem_euclid(n) as usize;
        self.request_redraw();
    }

    /// Activate the keyboard-focused Toolbar control — the same action its
    /// click handler would push, so keyboard and mouse converge on one path.
    /// Index order must match `toolbar_focus_sync`'s call sites in `ui.rs`,
    /// separately for whichever toolbar (`grid_toolbar`/`loupe_toolbar`) is
    /// actually rendering — see `toolbar_control_count`.
    pub(super) fn activate_toolbar_focus(&mut self) {
        if self.mode == ViewMode::Loupe {
            let action = match self.toolbar_focus {
                0 => ui::UiAction::ToggleHelp,
                1 => ui::UiAction::EnterGrid,
                2 => ui::UiAction::EnterLoupe,
                _ => return,
            };
            self.apply_ui_actions(vec![action]);
            return;
        }
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
            10 => ui::UiAction::ToggleBursts,
            11 => ui::UiAction::ToggleDupes,
            12 => ui::UiAction::ToggleEyesClosed,
            13 => ui::UiAction::ToggleHelp,
            14 => ui::UiAction::EnterGrid,
            15 => ui::UiAction::EnterLoupe,
            _ => return,
        };
        self.apply_ui_actions(vec![action]);
    }

    /// Nudge the focused Develop slider's value (`dir` = -1/+1) by one step and
    /// apply it. Tone fields step ±1 (200-unit span, integer display); exposure
    /// steps ±0.05 (10-stop span, two-decimal display).
    pub(super) fn develop_adjust(&mut self, dir: isize) {
        let mut adj = self.current_adjustments();
        let sign = dir as f32;
        let (field, range, step): (&mut f32, std::ops::RangeInclusive<f32>, f32) =
            match self.develop_focus {
                0 => (&mut adj.temp, develop::TONE_RANGE, 1.0),
                1 => (&mut adj.tint, develop::TONE_RANGE, 1.0),
                2 => (&mut adj.exposure, develop::EXPOSURE_RANGE, 0.05),
                3 => (&mut adj.contrast, develop::TONE_RANGE, 1.0),
                4 => (&mut adj.highlights, develop::TONE_RANGE, 1.0),
                5 => (&mut adj.shadows, develop::TONE_RANGE, 1.0),
                6 => (&mut adj.whites, develop::TONE_RANGE, 1.0),
                7 => (&mut adj.blacks, develop::TONE_RANGE, 1.0),
                8 => (&mut adj.vibrance, develop::TONE_RANGE, 1.0),
                9 => (&mut adj.saturation, develop::TONE_RANGE, 1.0),
                _ => (&mut adj.denoise, develop::DENOISE_RANGE, 1.0),
            };
        *field = (*field + sign * step).clamp(*range.start(), *range.end());
        self.apply_adjustments(adj);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toolbar_control_count_is_smaller_in_loupe_than_grid() {
        let mut app = App::new(None);
        app.mode = ViewMode::Grid;
        assert_eq!(app.toolbar_control_count(), 16);
        app.mode = ViewMode::Loupe;
        assert_eq!(
            app.toolbar_control_count(),
            3,
            "the Loupe toolbar only has Help + Grid + Loupe toggle"
        );
    }

    #[test]
    fn loupe_toolbar_focus_zero_toggles_help() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.toolbar_focus = 0;
        assert!(!app.show_help());
        app.activate_toolbar_focus();
        assert!(
            app.show_help(),
            "index 0 in the Loupe toolbar must toggle help, matching the '?' button"
        );
    }

    #[test]
    fn loupe_toolbar_focus_one_enters_grid() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.toolbar_focus = 1;
        app.activate_toolbar_focus();
        assert_eq!(
            app.mode(),
            ViewMode::Grid,
            "index 1 in the Loupe toolbar must switch to Grid, matching the 'G' button"
        );
    }

    /// `selected_path()` falls back to `want` when `sel` has gone stale
    /// (`None`) in Loupe mode (see its doc comment) — `selection_count`/
    /// `selected_paths` must agree, or bulk actions (Delete, Apply Settings,
    /// Export) silently see "nothing selected" for the photo actually open,
    /// even though rating/single-photo actions work fine.
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
