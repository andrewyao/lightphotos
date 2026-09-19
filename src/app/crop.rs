use super::*;

use crate::develop::Crop;

impl App {
    /// The crop rectangle currently being edited, if crop mode is active.
    pub(crate) fn crop_rect(&self) -> Option<Crop> {
        self.crop_edit.as_ref().map(|d| d.rect)
    }

    /// Enter crop mode, opening the loupe first if needed. The draft starts
    /// from the saved crop, and the GPU shows the full frame while editing.
    pub(super) fn enter_crop(&mut self) {
        if self.mode != ViewMode::Loupe {
            self.enter_loupe();
            if self.mode != ViewMode::Loupe {
                return; // nothing was selected
            }
        }
        if self.shown.path().is_none() {
            return;
        }
        let rect = self.current_adjustments().crop.unwrap_or(FULL_CROP);
        self.crop_edit = Some(CropDraft {
            rect,
            grab: None,
            aspect: 1.0,
        });
        self.push_crop_preview();
        self.fit_for_crop();
        self.request_redraw();
    }

    /// Push the current edits to the GPU with the crop removed, so the whole
    /// frame is visible under the crop overlay.
    pub(super) fn push_crop_preview(&mut self) {
        let mut adj = self.current_adjustments();
        adj.crop = None;
        let gpu = self.gpu_adjust(&adj);
        if let Some(r) = &mut self.renderer {
            r.set_adjustments(gpu);
        }
        self.request_redraw();
    }

    /// Commit the crop draft into the image's persisted adjustments (full-frame
    /// crops store as `None`), then leave crop mode.
    pub(super) fn commit_crop(&mut self) {
        let Some(draft) = self.crop_edit.take() else {
            return;
        };
        let r = draft.rect;
        let is_full = r.left <= MIN_CROP
            && r.top <= MIN_CROP
            && r.right >= 1.0 - MIN_CROP
            && r.bottom >= 1.0 - MIN_CROP;
        let mut adj = self.current_adjustments();
        adj.crop = if is_full { None } else { Some(r) };
        self.apply_adjustments(adj);
        self.request_redraw();
    }

    /// Leave crop mode without committing, restoring the previously-committed crop.
    pub(super) fn cancel_crop(&mut self) {
        if self.crop_edit.take().is_some() {
            self.push_adjustments();
            self.request_redraw();
        }
    }

    /// Begin resizing by `edge`. Captures the current pixel aspect ratio for
    /// Shift-locked drags.
    pub(super) fn crop_grab(&mut self, edge: CropEdge) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop_edit.as_mut() {
            let cw = (d.rect.right - d.rect.left) * w;
            let ch = (d.rect.bottom - d.rect.top) * h;
            d.aspect = if ch > 0.0 { cw / ch } else { 1.0 };
            d.grab = Some(CropGrab::Edge(edge));
        }
    }

    /// Begin moving the whole crop rectangle from texture coordinate `(u, v)`.
    pub(super) fn crop_grab_move(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = Some(CropGrab::Move {
                anchor: (u, v),
                rect0: d.rect,
            });
        }
    }

    /// Apply the active crop drag at texture coordinate `(u, v)`. A move keeps
    /// the size and stays in the frame. An edge drag with Shift also moves the
    /// perpendicular edges about their center to keep the grabbed aspect ratio.
    pub(super) fn crop_drag_to(&mut self, u: f32, v: f32) {
        let shift = self.modifiers.shift_key();
        let (w, h) = self.image_size();
        let Some(d) = self.crop_edit.as_mut() else {
            return;
        };
        let Some(grab) = d.grab else { return };
        let mut r = d.rect;
        match grab {
            CropGrab::Move { anchor, rect0 } => {
                let cw = rect0.right - rect0.left;
                let ch = rect0.bottom - rect0.top;
                let nl = (rect0.left + (u - anchor.0)).clamp(0.0, 1.0 - cw);
                let nt = (rect0.top + (v - anchor.1)).clamp(0.0, 1.0 - ch);
                r.left = nl;
                r.right = nl + cw;
                r.top = nt;
                r.bottom = nt + ch;
            }
            CropGrab::Edge(edge) => {
                match edge {
                    CropEdge::Left => r.left = u.clamp(0.0, r.right - MIN_CROP),
                    CropEdge::Right => r.right = u.clamp(r.left + MIN_CROP, 1.0),
                    CropEdge::Top => r.top = v.clamp(0.0, r.bottom - MIN_CROP),
                    CropEdge::Bottom => r.bottom = v.clamp(r.top + MIN_CROP, 1.0),
                }
                if shift && d.aspect > 0.0 && w > 0.0 && h > 0.0 {
                    match edge {
                        CropEdge::Left | CropEdge::Right => {
                            // Derive height from the locked ratio, keeping the vertical center.
                            let ch_norm =
                                (((r.right - r.left) * w) / d.aspect / h).clamp(MIN_CROP, 1.0);
                            let cy = (r.top + r.bottom) / 2.0;
                            r.top = (cy - ch_norm / 2.0).clamp(0.0, 1.0 - MIN_CROP);
                            r.bottom = (r.top + ch_norm).min(1.0);
                        }
                        CropEdge::Top | CropEdge::Bottom => {
                            let cw_norm =
                                (((r.bottom - r.top) * h) * d.aspect / w).clamp(MIN_CROP, 1.0);
                            let cx = (r.left + r.right) / 2.0;
                            r.left = (cx - cw_norm / 2.0).clamp(0.0, 1.0 - MIN_CROP);
                            r.right = (r.left + cw_norm).min(1.0);
                        }
                    }
                }
            }
        }
        d.rect = r;
        self.request_redraw();
    }

    pub(super) fn crop_release(&mut self) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = None;
        }
    }
}
