use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;


use crate::burst::{self, BurstMark};
use crate::duplicates::{self, DuplicateMark};
use crate::navigation::Cmp;

impl App {

/// Accessors used by the egui UI module (`ui.rs`).
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The keyboard-focused region (lit panel, arrow-key target).
    pub(crate) fn focus(&self) -> Region {
        self.focus
    }

    /// The focus depth used by the UI to distinguish a selected region from an
    /// entered control within that region.
    pub(crate) fn focus_level(&self) -> FocusLevel {
        self.focus_level
    }

    /// The index of the keyboard-focused Develop slider (0..=7).
    pub(crate) fn develop_focus(&self) -> usize {
        self.develop_focus
    }

    /// The index of the keyboard-focused Toolbar control.
    pub(crate) fn toolbar_focus(&self) -> usize {
        self.toolbar_focus
    }

    /// The root of the folder tree (the opened folder, or a file's parent).
    pub(crate) fn folder_root(&self) -> Option<PathBuf> {
        self.folder_root.clone()
    }

    /// The folder whose images are currently in the grid (highlighted in tree).
    pub(crate) fn folder_sel(&self) -> Option<PathBuf> {
        self.folder_sel.clone()
    }

    /// Whether a tree folder is expanded.
    pub(crate) fn is_expanded(&self, dir: &Path) -> bool {
        self.expanded.contains(dir)
    }

    /// The cached immediate subdirectories of `dir` (empty slice if uncached).
    pub(crate) fn subdirs(&self, dir: &Path) -> &[PathBuf] {
        self.subdirs.get(dir).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub(crate) fn filter(&self) -> Option<(Cmp, u8)> {
        self.filter
    }

    /// The comparator the toolbar will apply to the next star-level click.
    pub(crate) fn filter_cmp(&self) -> Cmp {
        self.filter_cmp
    }

    /// Count of photos in the current folder at each rating 0..=5 (index =
    /// stars). Computed over the whole playlist, ignoring the active filter, so
    /// the toolbar histogram shows the folder's true distribution.
    pub(crate) fn rating_counts(&self) -> [usize; 6] {
        let mut counts = [0usize; 6];
        if let Some(pl) = &self.playlist {
            for p in pl.entries() {
                counts[self.rating_of(p).min(5) as usize] += 1;
            }
        }
        counts
    }

    /// Whether the shortcut-help overlay is showing.
    pub(crate) fn show_help(&self) -> bool {
        self.show_help
    }

    pub(crate) fn pending_quit(&self) -> bool {
        self.pending_quit
    }

    pub(crate) fn thumb_px(&self) -> u32 {
        self.thumb_px
    }

    /// Position of the current selection within `visible`, or `None` in the
    /// grid's browse-first state (before any click/arrow).
    pub(crate) fn sel(&self) -> Option<usize> {
        self.sel
    }

    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn set_grid_cols(&mut self, cols: usize) {
        self.grid_cols = cols.max(1);
    }

    /// The grid reports which cell range `[start, end)` is scrolled into view so
    /// thumbnail loading can be virtualized to just those cells.
    pub(crate) fn set_visible_grid_range(&mut self, start: usize, end: usize) {
        self.grid_range = (start, end);
    }

    pub(crate) fn take_grid_scroll_reset(&mut self) -> bool {
        std::mem::take(&mut self.grid_scroll_reset)
    }

    /// The filmstrip reports which cell range `[start, end)` is scrolled into view
    /// so thumbnail loading is virtualized to just those cells (horizontal
    /// equivalent of `set_visible_grid_range`).
    pub(crate) fn set_visible_strip_range(&mut self, start: usize, end: usize) {
        self.strip_range = (start, end);
    }

    /// The filmstrip's cell range `[start, end)` scrolled into view as of last
    /// frame — used to tell whether the current selection is near enough to
    /// the visible edge to warrant scrolling.
    pub(crate) fn strip_range(&self) -> (usize, usize) {
        self.strip_range
    }

    /// Rating of the visible cell at `pos` (0 when unset/out of range).
    pub(crate) fn rating_at(&self, pos: usize) -> u8 {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .map(|p| self.rating_of(p))
            .unwrap_or(0)
    }

    /// The single "which frame is better" number, shared by burst and duplicate
    /// picking: sharpness with a blink penalty folded in (see
    /// [`burst::combined_score`]). Both groupings answer the same question, so
    /// neither should have its own idea of what makes a frame the keeper.
    pub(super) fn culling_score(&self, path: &Path) -> Option<f64> {
        burst::combined_score(
            self.sharpness.get(path).copied(),
            self.face_quality.get(path).and_then(|q| q.eye_state()),
        )
    }

    /// The face signal for a path, once analyzed. `None` while the analysis is
    /// still pending, failed, or was never requested (the pass only covers
    /// grouped photos).
    pub(crate) fn face_quality_of(&self, path: &Path) -> Option<crate::facequality::FaceQuality> {
        self.face_quality.get(path).copied()
    }

    /// Whether the face pass found a blink in this path's photo.
    pub(super) fn eyes_closed(&self, path: &Path) -> bool {
        self.face_quality_of(path)
            .and_then(|q| q.eye_state())
            .is_some_and(|s| s == crate::facequality::EyeState::Closed)
    }

    /// Whether the visible cell at `pos` has a detected blink (drives the grid
    /// badge). `false` when out of range or not yet analyzed.
    pub(crate) fn eyes_closed_at(&self, pos: usize) -> bool {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .is_some_and(|p| self.eyes_closed(p))
    }

    /// Whether the "eyes closed" filter is on (for the toolbar toggle state).
    pub(crate) fn eyes_filter_on(&self) -> bool {
        self.eyes_filter
    }

    /// Flip the "eyes closed" filter and re-narrow the grid.
    pub(super) fn toggle_eyes_filter(&mut self) {
        self.eyes_filter = !self.eyes_filter;
        self.recompute_visible();
        self.request_redraw();
    }

    /// Rebuild `burst_marks` from the cached capture times + sharpness over the
    /// current playlist entries. Clears the marks when bursts are off or there
    /// is no playlist. Cheap: O(entries).
    pub(super) fn recompute_burst_marks(&mut self) {
        let Some(pl) = &self.playlist else {
            self.burst_marks.clear();
            return;
        };
        if !self.bursts_on {
            self.burst_marks.clear();
            return;
        }
        let entries = pl.entries();
        // Grouping is only valid once every entry's capture time has been read.
        // Until then, unread entries collapse into one giant "burst"
        // (group_by_time treats a run of unknowns as one group), which would dim
        // the whole folder to a single frame. Paint nothing until the scan is done.
        if entries.iter().any(|p| !self.capture_times.contains_key(p)) {
            self.burst_marks.clear();
            return;
        }
        let times: Vec<Option<SystemTime>> = entries
            .iter()
            .map(|p| self.capture_times.get(p).copied().flatten())
            .collect();
        let scores: Vec<Option<f64>> = entries.iter().map(|p| self.culling_score(p)).collect();
        self.burst_marks = burst::marks_for(&times, &scores, burst::BURST_GAP);
    }

    /// Burst mark for the visible cell at `pos`. `None` when bursts are off, the
    /// cell is a singleton, or `pos` is out of range. `burst_marks` is indexed by
    /// playlist entry index, so we map the visible position through `visible`.
    pub(crate) fn burst_mark_at(&self, pos: usize) -> Option<BurstMark> {
        let idx = *self.visible.get(pos)?;
        self.burst_marks.get(idx).copied().flatten()
    }

    /// Whether burst mode is currently on (for the toolbar toggle state).
    pub(crate) fn bursts_on(&self) -> bool {
        self.bursts_on
    }

    /// Reset transient burst view state on a folder change. Keeps the path-keyed
    /// caches (harmless across folders; helps on revisit) but drops the toggle
    /// and derived marks so a new folder starts plain.
    pub(super) fn reset_burst_state(&mut self) {
        self.bursts_on = false;
        self.burst_marks.clear();
    }

    /// Rebuild `dup_marks` from the cached dHashes + sharpness over the current
    /// playlist entries. Clears the marks when dupes are off or there is no
    /// playlist. Unlike `recompute_burst_marks`, this doesn't need to wait for a
    /// full scan first: `duplicates::group_by_hash` treats an unknown hash as
    /// its own private singleton (never merged), so a partial scan just means
    /// fewer groups are found yet, not a false single giant group.
    pub(super) fn recompute_dup_marks(&mut self) {
        let Some(pl) = &self.playlist else {
            self.dup_groups.clear();
            self.dup_marks.clear();
            return;
        };
        if !self.dupes_on {
            self.dup_groups.clear();
            self.dup_marks.clear();
            return;
        }
        let entries = pl.entries();
        let hashes: Vec<Option<u64>> = entries.iter().map(|p| self.phashes.get(p).copied()).collect();
        let scores: Vec<Option<f64>> = entries.iter().map(|p| self.culling_score(p)).collect();
        let groups = duplicates::group_by_hash(&hashes, duplicates::DEFAULT_MAX_DISTANCE);
        // Second tier: split off any dHash false positive whose feature-print
        // distance to its group's anchor exceeds the threshold. Members
        // without a feature print yet stay in their dHash group unchanged.
        let refined = duplicates::refine_by_feature_print(
            &groups,
            duplicates::DEFAULT_MAX_FEATURE_DISTANCE,
            |anchor, i| {
                self.feature_distances
                    .get(&(entries[anchor].clone(), entries[i].clone()))
                    .copied()
            },
        );
        self.dup_marks = duplicates::compute_marks(&refined, &scores);
        self.dup_groups = groups;
        self.dup_refined = refined;
    }

    /// Duplicate mark for the visible cell at `pos`. `None` when dupes are off,
    /// the cell is a singleton, or `pos` is out of range. `dup_marks` is indexed
    /// by playlist entry index, so we map the visible position through `visible`.
    pub(crate) fn dup_mark_at(&self, pos: usize) -> Option<DuplicateMark> {
        let idx = *self.visible.get(pos)?;
        self.dup_marks.get(idx).copied().flatten()
    }

    /// Whether duplicate-grouping mode is currently on (for the toolbar toggle state).
    pub(crate) fn dupes_on(&self) -> bool {
        self.dupes_on
    }

    /// Reset transient duplicate-grouping view state on a folder change. Keeps
    /// the path-keyed `phashes` cache (harmless across folders) but drops the
    /// toggle and derived marks so a new folder starts plain.
    pub(super) fn reset_dup_state(&mut self) {
        self.dupes_on = false;
        self.dup_groups.clear();
        self.dup_refined.clear();
        self.dup_marks.clear();
        // Same rationale for the blink filter: the `face_quality` cache is
        // path-keyed and worth keeping, but a new folder starts unfiltered
        // rather than silently showing an empty grid.
        self.eyes_filter = false;
    }

    /// Paths of the duplicate group currently under review in Survey Mode
    /// (empty outside `ViewMode::Survey`).
    pub(crate) fn survey_members(&self) -> &[PathBuf] {
        &self.survey_members
    }

    /// The Survey group's best member, if any (see `open_survey`).
    pub(crate) fn survey_best(&self) -> Option<&Path> {
        self.survey_best.as_deref()
    }

    /// Index into `survey_members()` that rating hotkeys/arrow-keys apply to.
    pub(crate) fn survey_focus(&self) -> usize {
        self.survey_focus
    }

    /// Rating of `path` (0 when unset). Path-keyed counterpart to
    /// `rating_at(pos)`, for Survey Mode's arbitrary (non-visible-position)
    /// member list.
    pub(crate) fn rating_of_path(&self, path: &Path) -> u8 {
        self.rating_of(path)
    }

    /// Rating of the current selection (0 when unset).
    pub(crate) fn selected_rating(&self) -> u8 {
        self.selected_path()
            .map(|p| self.rating_of(&p))
            .unwrap_or(0)
    }

    /// The egui texture + source dimensions for the visible cell at `pos`, if
    /// its thumbnail has been uploaded this frame.
    pub(crate) fn thumb_texture_for(&self, pos: usize) -> Option<(&egui::TextureHandle, u32, u32)> {
        let idx = *self.visible.get(pos)?;
        let path = self.playlist.as_ref()?.entry(idx)?;
        self.thumb_texture_for_path(path)
    }

    /// Path-keyed counterpart to `thumb_texture_for(pos)`, for Survey Mode's
    /// arbitrary (non-visible-position) member list.
    pub(crate) fn thumb_texture_for_path(&self, path: &Path) -> Option<(&egui::TextureHandle, u32, u32)> {
        let key = (path.to_path_buf(), self.thumb_px, self.edit_sig_for(path));
        let handle = self.thumb_tex.get(&key)?;
        let [w, h] = handle.size();
        Some((handle, w as u32, h as u32))
    }
}
