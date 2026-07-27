use super::*;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};


use crate::develop::{self};
use crate::navigation::{self, flatten_visible_tree, visible_indices, Cmp, Playlist};
use crate::ui;

impl App {

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
        self.visible = visible_indices(pl.entries(), self.filter, |p| {
            ratings.get(p).copied().unwrap_or(0)
        });
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

    /// The path of the current selection, if any.
    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        let pl = self.playlist.as_ref()?;
        let idx = self.selected_index()?;
        pl.entry(idx).map(|p| p.to_path_buf())
    }

    /// Paths of every photo in the multi-selection, in `visible` order. Falls
    /// back to the primary cell when the set is empty but a cell is active, so
    /// bulk operations always have at least the current photo to work on.
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
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
    /// single primary cell when the set is empty).
    pub(crate) fn selection_count(&self) -> usize {
        if self.selected.is_empty() {
            usize::from(self.sel.is_some())
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
    /// Requests both the full image and (as an instant placeholder) the
    /// thumbnail, so the shown image updates immediately even before the full
    /// decode finishes.
    pub(super) fn load_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            return;
        };
        let px = self.thumb_px;
        if let Some(loader) = &mut self.loader {
            loader.request(path.clone());
            loader.request_thumb(path.clone(), px);
        }
        self.want = Some(path);
        self.try_show();
    }

    /// Request full-image decodes of the loupe neighbors (prev/next in the
    /// visible list) so stepping feels instant.
    pub(super) fn request_neighbors(&mut self) {
        if self.visible.len() <= 1 {
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
                loader.request(p);
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

    /// Up/Down in the tree: move the selection by `delta` rows within the
    /// visible tree (clamped) and load the newly-selected folder, matching a
    /// standard single-select tree — there's no separate cursor to move
    /// without also loading.
    pub(super) fn folder_move(&mut self, delta: isize) {
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
            self.load_folder(tree[next].clone());
        }
    }

    /// Right-arrow in the tree: expand the selected folder, or select+load its
    /// first child if already expanded. A no-op on a childless folder.
    pub(super) fn folder_expand(&mut self) {
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        self.ensure_subdirs(&cur);
        if self.subdirs(&cur).is_empty() {
            return; // leaf
        }
        if self.expanded.contains(&cur) {
            if let Some(first) = self.subdirs(&cur).first().cloned() {
                self.load_folder(first);
            }
        } else {
            self.expanded.insert(cur);
            self.request_redraw();
        }
    }

    /// Left-arrow in the tree: collapse the selected folder if open, else
    /// select+load its parent (stopping at the root).
    pub(super) fn folder_collapse(&mut self) {
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        if self.expanded.contains(&cur) {
            self.expanded.remove(&cur);
            self.request_redraw();
        } else if Some(cur.as_path()) != self.folder_root.as_deref() {
            if let Some(parent) = cur.parent() {
                self.load_folder(parent.to_path_buf());
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
        self.ensure_subdirs(&path);
        let subdirs = self.subdirs(&path).to_vec();
        let pure_container = !subdirs.is_empty() && Playlist::from_dir(&path).entries().is_empty();
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

    /// Number of keyboard-focusable Toolbar controls (Phase 1: the stable set
    /// that always renders — see `toolbar_focus_sync` in `ui.rs`, which must
    /// stay in lockstep with this count and with `activate_toolbar_focus`'s
    /// index mapping. Rating-histogram bars and the selection-dependent bulk
    /// actions aren't included yet since their count varies frame to frame.
    const TOOLBAR_CONTROLS: usize = 14;

    pub(super) fn toolbar_control_count(&self) -> usize {
        Self::TOOLBAR_CONTROLS
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
    /// Index order must match `toolbar_focus_sync`'s call sites in `ui.rs`.
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
            10 => ui::UiAction::ToggleBursts,
            11 => ui::UiAction::ToggleHelp,
            12 => ui::UiAction::EnterGrid,
            13 => ui::UiAction::EnterLoupe,
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
