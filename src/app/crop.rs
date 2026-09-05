use super::*;

use crate::develop::Crop;

impl App {
    // ---- Crop mode ----

    /// The crop rectangle currently being edited, if crop mode is active.
    pub(crate) fn crop_rect(&self) -> Option<Crop> {
        self.crop_edit.as_ref().map(|d| d.rect)
    }

    /// Enter crop mode on the current image. Crop is a Loupe sub-mode: from the
    /// Grid this first opens the loupe. Seeds the draft from any existing crop,
    /// drops the mask on the GPU so the whole frame is visible, and fits it.
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
        // Show the full frame (identity crop) while framing; the overlay masks.
        self.push_crop_preview();
        self.fit_for_crop();
        self.request_redraw();
    }

    /// Push the current image's tone edits with the crop forced to full-frame,
    /// so the whole image is visible while the crop overlay is being edited.
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
        self.apply_adjustments(adj); // persists to catalog + pushes real crop to GPU
        self.request_redraw();
    }

    /// Leave crop mode without committing, restoring the previously-committed crop.
    pub(super) fn cancel_crop(&mut self) {
        if self.crop_edit.take().is_some() {
            self.push_adjustments(); // restore the committed crop on the GPU
            self.request_redraw();
        }
    }

    /// Begin resizing by `edge`: record it and capture the current pixel aspect
    /// ratio (for Shift-lock while dragging).
    pub(super) fn crop_grab(&mut self, edge: CropEdge) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop_edit.as_mut() {
            let cw = (d.rect.right - d.rect.left) * w;
            let ch = (d.rect.bottom - d.rect.top) * h;
            d.aspect = if ch > 0.0 { cw / ch } else { 1.0 };
            d.grab = Some(CropGrab::Edge(edge));
        }
    }

    /// Begin moving the whole crop rectangle: anchor the drag at texture
    /// coordinate `(u, v)` and remember the rectangle as it is now.
    pub(super) fn crop_grab_move(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = Some(CropGrab::Move {
                anchor: (u, v),
                rect0: d.rect,
            });
        }
    }

    /// Apply the active crop drag at texture coordinate `(u, v)`:
    /// - `Move`: translate the whole rectangle (size fixed), clamped to the frame.
    /// - `Edge`: move that edge; with Shift, the perpendicular edges co-move about
    ///   their center to preserve the pixel aspect ratio captured at grab.
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
                // Keep the size; translate by the pointer delta, clamped so the
                // rectangle stays inside the frame.
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
                            // Width just changed; set height from the locked ratio,
                            // centered on the current vertical center.
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

    /// Clear the active crop drag (released).
    pub(super) fn crop_release(&mut self) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = None;
        }
    }
}
