use super::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};


use crate::develop::Adjustments;
use crate::navigation::Cmp;
use crate::{trash, ui};

impl App {

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
            ui::BulkKind::Delete => format!("Move {n} photo(s) to the Trash?"),
        })
    }

    /// Open the confirm modal for `kind` (no-op when nothing is selected). Shared
    /// by the toolbar buttons and the Delete/Backspace key.
    pub(super) fn request_bulk(&mut self, kind: ui::BulkKind) {
        if self.selection_count() > 0 {
            self.pending_bulk = Some(kind);
            self.request_redraw();
        }
    }

    /// Run a confirmed bulk action against the current selection.
    pub(super) fn run_bulk(&mut self, kind: ui::BulkKind) {
        match kind {
            ui::BulkKind::Rate(stars) => self.apply_rating_to_selection(stars),
            ui::BulkKind::ApplySettings => self.apply_settings_to_selection(),
            ui::BulkKind::Export => self.export_selection(),
            ui::BulkKind::Delete => self.delete_selection(),
        }
    }

    /// Move every selected photo to the Trash, then drop it from the playlist,
    /// the in-memory maps, and the catalog, repairing the cursor + loupe.
    pub(super) fn delete_selection(&mut self) {
        let paths = self.selected_paths();
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
        if !trashed.is_empty() {
            let gone: HashSet<PathBuf> = trashed.iter().cloned().collect();
            if let Some(pl) = self.playlist.as_mut() {
                pl.remove_matching(|p| gone.contains(p));
            }
            for p in &trashed {
                self.ratings.remove(p);
                self.edits.remove(p);
                self.rotations.remove(p);
                self.catalog.remove(p);
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
        self.set_status(match last_err {
            None => format!("Moved {n} photo(s) to Trash"),
            Some(e) => format!("Trashed {n}/{total} \u{2014} last error: {e}"),
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
        } else {
            self.burst_marks.clear();
        }
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
    pub(super) fn set_filter(&mut self, filter: Option<(Cmp, u8)>) {
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
        // In Loupe, the shown image may have been filtered out; snap to selection.
        if self.mode == ViewMode::Loupe {
            self.load_selected();
        }
        self.request_redraw();
    }

    /// Change the toolbar comparator (≥ / = / ≤). If a star-level filter is
    /// already active, re-apply it with the new comparator so the view updates
    /// immediately.
    pub(super) fn set_filter_cmp(&mut self, cmp: Cmp) {
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
