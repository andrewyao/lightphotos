//! Local adjustment masks on the shown photo: the one writer of the masks
//! mirror, the draw tool, and the selected mask. Every change goes through
//! the main undo history, so a drag is one step like a slider drag.

use super::*;

use crate::develop::{Mask, MAX_MASKS};

/// The shape the armed mask tool draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MaskKind {
    Linear,
    Radial,
}

impl App {
    pub(crate) fn current_masks(&self) -> &[Mask] {
        self.shown
            .path()
            .and_then(|p| self.masks.get(p))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The selected mask's index, while it still names one.
    pub(crate) fn selected_mask(&self) -> Option<usize> {
        self.mask_selected
            .filter(|&i| i < self.current_masks().len())
    }

    /// The shape the next drag on the photo draws, if the tool is armed.
    pub(crate) fn mask_tool(&self) -> Option<MaskKind> {
        match self.tool {
            LoupeTool::Mask(kind) => Some(kind),
            _ => None,
        }
    }

    /// Whether the Loupe shows the mask overlay: the Masks page is open and
    /// Touch Up and compare aren't using the photo.
    pub(crate) fn masks_shown(&self) -> bool {
        self.mode == ViewMode::Loupe
            && self.develop_open
            && self.develop_tab == DevelopTab::Masks
            && !self.touchup_active()
            && !self.compare
    }

    /// Arms the tool for `kind`, or puts it down when it is already armed.
    /// Arming opens the Masks page, where the new mask's sliders are.
    pub(super) fn toggle_mask_tool(&mut self, kind: MaskKind) {
        if self.mask_tool() == Some(kind) {
            self.tool = LoupeTool::None;
        } else {
            if self.mode != ViewMode::Loupe {
                self.enter_loupe();
            }
            if self.selected_path().is_none() {
                return;
            }
            self.develop_open = true;
            if self.develop_tab != DevelopTab::Masks {
                self.set_develop_tab(DevelopTab::Masks);
            }
            self.tool = LoupeTool::Mask(kind);
            self.touchup_selected = None;
        }
        self.request_redraw();
    }

    pub(super) fn select_mask(&mut self, i: Option<usize>) {
        self.mask_selected = i.filter(|&i| i < self.current_masks().len());
        self.request_redraw();
    }

    /// Adds `mask` to the shown photo and selects it. Returns its index, or
    /// `None` when the photo already holds [`MAX_MASKS`].
    pub(super) fn add_mask(&mut self, mask: Mask) -> Option<usize> {
        let path = self.shown.path().map(Path::to_path_buf)?;
        let mut all = self.current_masks().to_vec();
        if all.len() >= MAX_MASKS {
            self.set_status(StatusKind::Error, crate::i18n::t().mask_limit.into());
            return None;
        }
        all.push(mask);
        let i = all.len() - 1;
        self.put_masks(&path, all);
        self.mask_selected = Some(i);
        Some(i)
    }

    /// Replaces mask `i` of the shown photo.
    pub(super) fn set_mask(&mut self, i: usize, mask: Mask) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let mut all = self.current_masks().to_vec();
        let Some(slot) = all.get_mut(i) else {
            return;
        };
        if *slot == mask {
            return;
        }
        *slot = mask;
        self.put_masks(&path, all);
    }

    pub(super) fn delete_mask(&mut self, i: usize) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let mut all = self.current_masks().to_vec();
        if i >= all.len() {
            return;
        }
        all.remove(i);
        self.put_masks(&path, all);
        self.mask_selected = None;
    }

    /// The one writer of a photo's masks for an edit: records the masks it
    /// replaces for Undo, then stores the new ones.
    pub(super) fn put_masks(&mut self, path: &Path, masks: Vec<Mask>) {
        let before = self.masks.get(path).cloned().unwrap_or_default();
        if before == masks {
            return;
        }
        let adj = self.edits.get(path).copied().unwrap_or_default();
        self.history.note_masks(path, adj, &before);
        self.store_masks(path, masks);
        #[cfg(target_arch = "wasm32")]
        crate::web::analytics::property("develop_edit_applied", "edit_kind", "mask");
    }

    /// Stores `masks` as `path`'s in the mirror and the folder store, and
    /// redraws the photo if it is the shown one. No undo record.
    pub(super) fn store_masks(&mut self, path: &Path, masks: Vec<Mask>) {
        self.folder_store.set_masks(path, &masks);
        if masks.is_empty() {
            self.masks.remove(path);
        } else {
            self.masks.insert(path.to_path_buf(), masks);
        }
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist.invalidate();
        }
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;
    use crate::develop::MaskShape;

    fn radial() -> Mask {
        let mut m = Mask::radial([0.5, 0.5], [0.2, 0.1]);
        m.amounts.exposure = 1.0;
        m
    }

    #[test]
    fn adding_then_dragging_a_mask_undoes_in_two_steps() {
        let (mut app, dir, photo) = one_photo("mask-undo");
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;

        assert_eq!(app.add_mask(radial()), Some(0));
        app.save_edit_unless_dragging();
        press(&app, true);
        for step in 1..=5 {
            let mut m = radial();
            if let MaskShape::Radial { center, .. } = &mut m.shape {
                center[0] = 0.5 + step as f32 * 0.02;
            }
            app.set_mask(0, m);
            app.save_edit_unless_dragging();
        }
        press(&app, false);
        app.save_edit_unless_dragging();
        let MaskShape::Radial { center, .. } = app.current_masks()[0].shape else {
            panic!("a radial mask");
        };
        assert!((center[0] - 0.6).abs() < 1e-6);
        assert_eq!(app.pushed_masks, 1, "the Loupe got the mask count");

        app.modifiers = winit::keyboard::ModifiersState::SUPER;
        app.handle_key(KeyCode::KeyZ);
        assert_eq!(app.current_masks(), &[radial()], "Cmd+Z undid the drag");
        app.handle_key(KeyCode::KeyZ);
        assert!(app.current_masks().is_empty(), "then the add");
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(app.folder_store.masks(&photo).is_empty(), "and the sidecar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_photo_holds_at_most_max_masks() {
        let (mut app, dir, _) = one_photo("mask-limit");
        for _ in 0..MAX_MASKS {
            assert!(app.add_mask(radial()).is_some());
        }
        assert_eq!(app.add_mask(radial()), None);
        assert_eq!(app.current_masks().len(), MAX_MASKS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
