use super::*;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::burst::{self, BurstMark};
use crate::duplicates::{self, DuplicateMark};
use crate::navigation::Cmp;
use crate::thumbnail::THUMB_PX;

impl App {
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
    }

    /// Whether a folder or file is open. `false` means the landing page shows.
    pub(crate) fn has_playlist(&self) -> bool {
        self.playlist.is_some()
    }

    /// Whether a folder picker is open. Always false on native, where the
    /// picker blocks the main thread. The web picker is async.
    pub(crate) fn folder_pick_pending(&self) -> bool {
        #[cfg(target_arch = "wasm32")]
        {
            self.web_folder_pending
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            false
        }
    }

    pub(crate) fn focus(&self) -> Region {
        self.focus
    }

    pub(crate) fn focus_level(&self) -> FocusLevel {
        self.focus_level
    }

    pub(crate) fn develop_focus(&self) -> usize {
        self.develop_focus
    }

    pub(crate) fn toolbar_focus(&self) -> usize {
        self.toolbar_focus
    }

    /// The root of the folder tree: the opened folder, or a file's parent.
    pub(crate) fn folder_root(&self) -> Option<PathBuf> {
        self.folder_root.clone()
    }

    /// The folder whose images are in the grid.
    pub(crate) fn folder_sel(&self) -> Option<PathBuf> {
        self.folder_sel.clone()
    }

    pub(crate) fn is_expanded(&self, dir: &Path) -> bool {
        self.expanded.contains(dir)
    }

    /// The cached subdirectories of `dir`. Empty if not cached yet.
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

    pub(crate) fn show_help(&self) -> bool {
        self.show_help
    }

    pub(crate) fn pending_quit(&self) -> bool {
        self.pending_quit
    }

    /// Longest-side size in pixels for the loupe's screen-fit preview decode.
    /// `win_size` is already in physical pixels, so don't scale it by the DPI
    /// factor again.
    pub(crate) fn preview_px(&self) -> u32 {
        preview_target_px(self.win_size.0.max(self.win_size.1))
    }

    /// Position of the selection within `visible`. `None` until the user
    /// clicks or presses an arrow key.
    pub(crate) fn sel(&self) -> Option<usize> {
        self.sel
    }

    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn set_grid_cols(&mut self, cols: usize) {
        self.grid_cols = cols.max(1);
    }

    /// The grid's on-screen cell range `[start, end)`. Thumbnails load only
    /// for these cells.
    pub(crate) fn set_visible_grid_range(&mut self, start: usize, end: usize) {
        self.grid_range = (start, end);
    }

    pub(crate) fn take_grid_scroll_reset(&mut self) -> bool {
        std::mem::take(&mut self.grid_scroll_reset)
    }

    /// The filmstrip's on-screen cell range `[start, end)`.
    pub(crate) fn set_visible_strip_range(&mut self, start: usize, end: usize) {
        self.strip_range = (start, end);
    }

    /// The filmstrip's on-screen cell range as of last frame.
    pub(crate) fn strip_range(&self) -> (usize, usize) {
        self.strip_range
    }

    /// Rating of the visible cell at `pos`. 0 when unrated or out of range.
    pub(crate) fn rating_at(&self, pos: usize) -> u8 {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .map(|p| self.rating_of(p))
            .unwrap_or(0)
    }

    /// The "which frame is better" score: sharpness with a blink penalty. Bursts
    /// and duplicates both use it, so they agree on which frame to keep.
    pub(super) fn culling_score(&self, path: &Path) -> Option<f64> {
        burst::combined_score(
            self.sharpness.get(path).copied(),
            self.face_quality.get(path).and_then(|q| q.eye_state()),
        )
    }

    /// The face analysis for a path. `None` while pending, after a failure, or
    /// when never requested. Only grouped photos are analyzed.
    pub(crate) fn face_quality_of(&self, path: &Path) -> Option<crate::facequality::FaceQuality> {
        self.face_quality.get(path).copied()
    }

    pub(super) fn eyes_closed(&self, path: &Path) -> bool {
        self.face_quality_of(path)
            .and_then(|q| q.eye_state())
            .is_some_and(|s| s == crate::facequality::EyeState::Closed)
    }

    /// Whether the visible cell at `pos` has a detected blink. `false` when out
    /// of range or not yet analyzed.
    pub(crate) fn eyes_closed_at(&self, pos: usize) -> bool {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .is_some_and(|p| self.eyes_closed(p))
    }

    pub(crate) fn eyes_filter_on(&self) -> bool {
        self.eyes_filter
    }

    pub(super) fn toggle_eyes_filter(&mut self) {
        self.eyes_filter = !self.eyes_filter;
        self.recompute_visible();
        self.request_redraw();
    }

    /// Rebuild `burst_marks` from cached capture times and scores. Clears them
    /// when bursts are off or no folder is open.
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
        // Wait until every capture time is read. `group_by_time` treats a run of
        // unknown times as one burst, which would dim the whole folder.
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
    /// photo is not in a burst, or `pos` is out of range.
    pub(crate) fn burst_mark_at(&self, pos: usize) -> Option<BurstMark> {
        let idx = *self.visible.get(pos)?;
        self.burst_marks.get(idx).copied().flatten()
    }

    pub(crate) fn bursts_on(&self) -> bool {
        self.bursts_on
    }

    /// Turn bursts off for a new folder. Path-keyed caches are kept for revisits.
    pub(super) fn reset_burst_state(&mut self) {
        self.bursts_on = false;
        self.burst_marks.clear();
    }

    /// Rebuild `dup_marks` from cached dHashes and scores. Unlike bursts, this
    /// can run on a partial scan: an unknown hash never joins a group.
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
        let hashes: Vec<Option<u64>> = pl
            .entries()
            .iter()
            .map(|p| self.phashes.get(p).copied())
            .collect();
        self.dup_groups = duplicates::group_by_hash(&hashes, duplicates::DEFAULT_MAX_DISTANCE);
        self.refine_dup_marks();
    }

    /// Re-applies feature-print splits and scores to the current hash groups.
    /// Linear, unlike the O(n²) regroup in `recompute_dup_marks`, so call this
    /// when only feature distances or scores changed.
    pub(super) fn refine_dup_marks(&mut self) {
        let Some(pl) = &self.playlist else { return };
        if !self.dupes_on {
            return;
        }
        let entries = pl.entries();
        let scores: Vec<Option<f64>> = entries.iter().map(|p| self.culling_score(p)).collect();
        let groups = &self.dup_groups;
        // Split off dHash false positives whose feature-print distance to the
        // group's anchor is too large. Members without a feature print stay.
        let refined = duplicates::refine_by_feature_print(
            groups,
            duplicates::DEFAULT_MAX_FEATURE_DISTANCE,
            |anchor, i| {
                self.feature_distances
                    .get(&(entries[anchor].clone(), entries[i].clone()))
                    .copied()
            },
        );
        self.dup_marks = duplicates::compute_marks(&refined, &scores);
        self.dup_refined = refined;
    }

    /// Duplicate mark for the visible cell at `pos`. `None` when dupes are off,
    /// the photo has no duplicates, or `pos` is out of range.
    pub(crate) fn dup_mark_at(&self, pos: usize) -> Option<DuplicateMark> {
        let idx = *self.visible.get(pos)?;
        self.dup_marks.get(idx).copied().flatten()
    }

    pub(crate) fn dupes_on(&self) -> bool {
        self.dupes_on
    }

    /// Turn duplicate grouping and the blink filter off for a new folder, so it
    /// never opens to a silently empty grid. Path-keyed caches are kept.
    pub(super) fn reset_dup_state(&mut self) {
        self.dupes_on = false;
        self.dup_groups.clear();
        self.dup_refined.clear();
        self.dup_marks.clear();
        self.eyes_filter = false;
    }

    /// The duplicate group shown in Survey. Empty outside Survey.
    pub(crate) fn survey_members(&self) -> &[PathBuf] {
        &self.survey_members
    }

    pub(crate) fn survey_best(&self) -> Option<&Path> {
        self.survey_best.as_deref()
    }

    /// Index into `survey_members()` that the rating keys apply to.
    pub(crate) fn survey_focus(&self) -> usize {
        self.survey_focus
    }

    /// Rating of `path`, 0 when unrated. Survey uses it for its member list.
    pub(crate) fn rating_of_path(&self, path: &Path) -> u8 {
        self.rating_of(path)
    }

    pub(crate) fn selected_rating(&self) -> u8 {
        self.selected_path()
            .map(|p| self.rating_of(&p))
            .unwrap_or(0)
    }

    /// The thumbnail texture and its size for the visible cell at `pos`, if
    /// uploaded.
    pub(crate) fn thumb_texture_for(&self, pos: usize) -> Option<(&egui::TextureHandle, u32, u32)> {
        let idx = *self.visible.get(pos)?;
        let path = self.playlist.as_ref()?.entry(idx)?;
        self.thumb_texture_for_path(path)
    }

    pub(crate) fn thumb_texture_for_path(
        &self,
        path: &Path,
    ) -> Option<(&egui::TextureHandle, u32, u32)> {
        let key = (path.to_path_buf(), THUMB_PX, self.edit_sig_for(path));
        let handle = self.thumb_tex.get(&key)?;
        let [w, h] = handle.size();
        Some((handle, w as u32, h as u32))
    }
}

/// Round a window's longest side (physical pixels) up to the preview decode
/// size, clamped to the tier's bounds. The result is part of the preview cache
/// key, so rounding stops a window resize from re-decoding on every pixel.
pub(crate) fn preview_target_px(longest_physical: f32) -> u32 {
    let longest = longest_physical.max(1.0) as u32;
    let quantized = longest.div_ceil(PREVIEW_QUANTUM) * PREVIEW_QUANTUM;
    quantized.clamp(PREVIEW_MIN, PREVIEW_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refining_after_a_feature_print_matches_a_full_recompute() {
        let dir = std::env::temp_dir().join(format!("lp-dup-refine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let names = ["a.jpg", "b.jpg", "c.jpg"];
        for n in names {
            std::fs::write(dir.join(n), []).unwrap();
        }
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let entries = app.playlist.as_ref().unwrap().entries().to_vec();
        for p in &entries {
            app.phashes.insert(p.clone(), 0);
        }
        app.dupes_on = true;
        app.recompute_dup_marks();
        assert_eq!(app.dup_refined, vec![0, 0, 0], "equal hashes form one group");

        app.feature_distances.insert(
            (entries[0].clone(), entries[2].clone()),
            duplicates::DEFAULT_MAX_FEATURE_DISTANCE + 1.0,
        );
        app.refine_dup_marks();
        let (refined, marks) = (app.dup_refined.clone(), app.dup_marks.clone());
        assert_ne!(refined[2], refined[0], "the far feature print splits off");

        app.recompute_dup_marks();
        assert_eq!(app.dup_refined, refined);
        assert_eq!(app.dup_marks, marks);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn small_windows_still_get_a_preview_worth_having() {
        assert_eq!(preview_target_px(1.0), PREVIEW_MIN);
        assert_eq!(preview_target_px(640.0), PREVIEW_MIN);
    }

    #[test]
    fn huge_displays_are_capped() {
        // A 6K display must not turn the "cheap" tier into a full decode.
        assert_eq!(preview_target_px(6016.0), PREVIEW_MAX);
        assert_eq!(preview_target_px(100_000.0), PREVIEW_MAX);
    }

    #[test]
    fn the_target_always_covers_the_window() {
        // A preview smaller than the window would be magnified by fit-zoom.
        for longest in [1100, 1400, 2048, 2049, 3000, 3584] {
            assert!(preview_target_px(longest as f32) >= longest.min(PREVIEW_MAX));
        }
    }

    #[test]
    fn dragging_a_window_edge_does_not_thrash_the_cache() {
        for longest in 1537..=2048 {
            assert_eq!(preview_target_px(longest as f32), 2048, "at {longest}");
        }
        assert_eq!(preview_target_px(2049.0), 2560);
    }

    #[test]
    fn a_preview_is_always_sharper_than_the_largest_thumbnail() {
        // A preview no larger than a thumbnail would add no detail when swapped in.
        assert!(PREVIEW_MIN > THUMB_PX);
    }
}
