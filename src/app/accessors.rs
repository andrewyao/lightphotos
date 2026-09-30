use super::*;
use std::path::{Path, PathBuf};

use crate::burst;
use crate::groups::{Group, GroupId};
use crate::navigation::Cmp;
use crate::thumbnail::THUMB_PX;

impl App {
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The group whose stack is the cell at `pos`. `None` for a single, and
    /// for a cell whose photo is not its group's representative, which a
    /// rebuilt view never shows.
    pub(crate) fn group_at(&self, pos: usize) -> Option<(&GroupId, &Group)> {
        let idx = *self.visible.get(pos)?;
        let name = self.playlist.as_ref()?.entry(idx)?.file_name()?;
        let groups = self.catalog.groups()?;
        let id = groups.group_of(name)?;
        let group = groups.get(id)?;
        (group.rep() == name).then_some((id, group))
    }

    /// Each selected stack's group once, in selection order.
    pub(crate) fn selected_groups(&self) -> Vec<(GroupId, Group)> {
        let mut seen = std::collections::HashSet::new();
        self.selected_cells()
            .into_iter()
            .filter_map(|p| self.group_at(p))
            .filter(|(id, _)| seen.insert(*id))
            .map(|(id, g)| (id.clone(), g.clone()))
            .collect()
    }

    pub(crate) fn selection_has_group(&self) -> bool {
        self.selected_cells()
            .into_iter()
            .any(|p| self.group_at(p).is_some())
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

    /// The saved looks, in the order rows render.
    pub(crate) fn presets(&self) -> &[crate::presets::Preset] {
        self.presets.presets()
    }

    /// The open name prompt's text, and whether it renames an existing preset.
    pub(crate) fn preset_name_edit(&self) -> Option<(String, bool)> {
        self.preset_name_edit
            .as_ref()
            .map(|(name, target)| (name.clone(), target.is_some()))
    }

    /// Name of the preset awaiting delete confirmation, if any.
    pub(crate) fn pending_preset_delete_name(&self) -> Option<String> {
        let Some(PendingConfirm::DeletePreset(id)) = self.pending_confirm else {
            return None;
        };
        self.presets.get(id).map(|p| p.name.clone())
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

    pub(crate) fn develop_tab(&self) -> DevelopTab {
        self.develop_tab
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

    pub(crate) fn show_settings(&self) -> bool {
        self.show_settings
    }

    pub(crate) fn autotone_centering(&self) -> crate::autotone::Centering {
        self.autotone_centering
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

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn grid_range(&self) -> (usize, usize) {
        self.grid_range
    }

    pub(crate) fn clear_grid_cells(&mut self) {
        self.grid_cell_rects.clear();
    }

    pub(crate) fn record_grid_cell(&mut self, pos: usize, rect: egui::Rect) {
        self.grid_cell_rects.push((pos, rect));
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn grid_cell_rect(&self, pos: usize) -> Option<egui::Rect> {
        self.grid_cell_rects
            .iter()
            .find(|(p, _)| *p == pos)
            .map(|(_, r)| *r)
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn selected_positions(&self) -> Vec<usize> {
        self.selected.iter().copied().collect()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn zoom_rel(&self) -> f32 {
        self.zoom_rel
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

    /// Color label of the visible cell at `pos`.
    pub(crate) fn label_at(&self, pos: usize) -> Option<crate::catalog::ColorLabel> {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .and_then(|p| self.catalog.label(p))
    }

    pub(crate) fn selected_label(&self) -> Option<crate::catalog::ColorLabel> {
        self.selected_path().and_then(|p| self.catalog.label(&p))
    }

    #[allow(dead_code)]
    pub(super) fn culling_score(&self, path: &Path) -> Option<f64> {
        burst::combined_score(
            self.sharpness.get(path).copied(),
            self.face_quality.get(path).and_then(|q| q.eye_state()),
        )
    }

    /// The face analysis for a path. `None` while pending, after a failure, or
    /// when never requested.
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

    pub(super) fn reset_eyes_filter(&mut self) {
        self.eyes_filter = false;
    }

    pub(crate) fn selected_rating(&self) -> u8 {
        self.selected_path()
            .map(|p| self.rating_of(&p))
            .unwrap_or(0)
    }

    /// The thumbnail texture and its size for the visible cell at `pos`, if
    /// uploaded.
    pub(crate) fn thumb_texture_for(&self, pos: usize) -> Option<(egui::TextureId, u32, u32)> {
        let idx = *self.visible.get(pos)?;
        let path = self.playlist.as_ref()?.entry(idx)?;
        self.thumb_texture_for_path(path)
    }

    /// Whether the thumbnail for the visible cell at `pos` failed to decode
    /// for good, as opposed to still loading.
    pub(crate) fn thumb_failed_at(&self, pos: usize) -> bool {
        let Some(path) = self
            .visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref()?.entry(i))
        else {
            return false;
        };
        self.loader
            .as_ref()
            .is_some_and(|l| l.thumb_failed(path, THUMB_PX))
    }

    pub(crate) fn thumb_texture_for_path(
        &self,
        path: &Path,
    ) -> Option<(egui::TextureId, u32, u32)> {
        let tex = self.thumb_tex.get(&(path.to_path_buf(), THUMB_PX))?;
        Some((tex.id, tex.width, tex.height))
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

    /// The grid tells a failed thumbnail from one still loading.
    #[test]
    fn a_failed_thumbnail_is_not_reported_as_loading() {
        let dir = std::env::temp_dir().join(format!("lp-thumb-failed-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for n in ["a.jpg", "b.jpg"] {
            std::fs::write(dir.join(n), []).unwrap();
        }
        let mut app = App::new(None);
        app.load_playlist(Playlist::from_dir(&dir), dir.clone());
        let mut loader =
            crate::loader::Loader::new(16384, crate::cache_limits::CacheLimits::PLATFORM);
        loader.mark_thumb_failed_external(dir.join("b.jpg"), THUMB_PX);
        app.loader = Some(loader);

        assert!(!app.thumb_failed_at(0), "a.jpg is still loading");
        assert!(app.thumb_failed_at(1));
        assert!(!app.thumb_failed_at(2), "past the end");
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
