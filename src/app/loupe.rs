//! Loupe view-state math: zoom/pan/fit, the crop/UV coordinate transforms,
//! and the subject-selection overlay.
//!
//! ## Pipeline position
//! - This is Pipeline 1's UI-thread half — it decides *what to ask for* and
//!   *how to show what comes back*, never decodes a pixel itself.
//! - `ensure_full_for_zoom` is the trigger for Pipeline 1's most expensive
//!   step: it's called after every zoom change and only then decides
//!   whether to call `Loader::request_full` (native) or
//!   `request_web_full` (`app/web.rs`, wasm32) — normal browsing at
//!   "fit to window" never reaches it.
//! - `fit_to_window`/`fit_for_crop`/`center`/`zoom_at` all end by calling
//!   `push_transform`, which uploads the new transform to `renderer.rs` —
//!   the same shared final stage every pipeline path converges on.

use super::*;
use std::path::Path;


use crate::develop::Adjustments;

impl App {

    /// On-screen footprint after rotation (w/h swapped for 90°/270°).
    pub(super) fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.image_size();
        if self.current_rotation() % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// The loupe image area in physical pixels: the whole surface unless a
    /// viewport was carved out by egui panels last frame. While comparing,
    /// each side only occupies half the width, so zoom/pan/fit math must
    /// target that half — this must match the equal-width split used to carve
    /// the actual GPU viewports (see the compare render call site).
    pub(super) fn loupe_area(&self) -> (f32, f32) {
        match self.loupe_viewport {
            Some((_, _, w, h)) => {
                if self.compare && self.mode == ViewMode::Loupe && w >= 2 && h > 0 {
                    ((w / 2).max(1) as f32, h.max(1) as f32)
                } else {
                    (w.max(1) as f32, h.max(1) as f32)
                }
            }
            None => self.win_size,
        }
    }

    /// Fit to the loupe area, centered: scales the image up or down so the whole
    /// image is as large as possible while staying fully on-screen ("contain").
    pub(super) fn fit_to_window(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        self.zoom = (ww / iw).min(wh / ih).clamp(MIN_ZOOM, MAX_ZOOM);
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Fit the *whole* image into the loupe area for crop mode: unlike
    /// `fit_to_window` this shrinks images larger than the viewport (no grow-only
    /// floor) and leaves a small margin, so the entire image — and thus all four
    /// crop edges and their handles — stay on-screen and grabbable.
    pub(super) fn fit_for_crop(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        // ~5% border each side so edge handles aren't flush against the viewport.
        const MARGIN: f32 = 0.9;
        self.zoom = ((ww / iw).min(wh / ih) * MARGIN).clamp(MIN_ZOOM, MAX_ZOOM);
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Reset to 100% (1 image pixel == 1 screen pixel), centered.
    pub(super) fn reset_100(&mut self) {
        self.zoom = 1.0;
        self.fitted = false;
        self.center();
        self.push_transform();
        self.ensure_full_for_zoom();
    }

    /// Fetch the full-resolution decode once the current zoom would magnify the
    /// screen-fit preview past its own pixels — i.e. the moment the preview
    /// stops being enough and softness would actually be visible. Below that
    /// threshold this does nothing, which is what keeps normal browsing off the
    /// expensive decode path entirely. `Loader::request_full` de-duplicates, so
    /// calling this on every zoom step is cheap.
    pub(super) fn ensure_full_for_zoom(&mut self) {
        if self.want.is_none() {
            return;
        }
        let (iw, ih) = self.image_size();
        if !zoom_outruns_preview(iw.max(ih), self.zoom, self.preview_px()) {
            return;
        }
        // wasm32: `loader.rs`'s own worker queue has no live workers there
        // (same reason `try_show`'s `request_preview` call is native-only —
        // see its own comment), so `loader.request_full` would silently do
        // nothing. `request_web_full` (`app/web.rs`) is the wasm-effective
        // equivalent, polled every frame from `main.rs` the same way
        // `request_web_preview` is.
        #[cfg(target_arch = "wasm32")]
        {
            self.request_web_full();
        }
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = self.want.clone() {
            if let Some(loader) = &mut self.loader {
                loader.request_full(path);
            }
        }
    }

    /// Rotate the current image 90° (clockwise if `cw`), remembering it per-image.
    pub(super) fn rotate(&mut self, cw: bool) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let step = (self.current_rotation() + if cw { 1 } else { 3 }) % 4;
        if step == 0 {
            self.rotations.remove(&path);
        } else {
            self.rotations.insert(path.clone(), step);
        }
        self.catalog.set_rotation(&path, step);
        if self.fitted {
            self.fit_to_window();
        } else {
            self.center();
            self.push_transform();
        }
    }

    pub(super) fn center(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        self.pan = ((ww - iw * self.zoom) / 2.0, (wh - ih * self.zoom) / 2.0);
    }

    /// Zoom by `factor`, keeping the image point under (cx, cy) fixed. `cx/cy`
    /// are in loupe-area-local pixels (origin at the viewport's top-left).
    pub(crate) fn zoom_at(&mut self, factor: f32, cx: f32, cy: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ipx = (cx - self.pan.0) / self.zoom;
        let ipy = (cy - self.pan.1) / self.zoom;
        self.pan.0 = cx - ipx * new_zoom;
        self.pan.1 = cy - ipy * new_zoom;
        self.zoom = new_zoom;

        // Once an axis fully fits in the viewport, keep the image centered on that
        // axis so the surrounding gap stays even (matches `center()`).
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        if iw * new_zoom <= ww {
            self.pan.0 = (ww - iw * new_zoom) / 2.0;
        }
        if ih * new_zoom <= wh {
            self.pan.1 = (wh - ih * new_zoom) / 2.0;
        }

        self.fitted = false;
        self.push_transform();
        self.ensure_full_for_zoom();
    }

    /// Cursor position relative to the loupe viewport's top-left, in physical px.
    /// While comparing, a cursor over the right half is folded back into the
    /// same `0..half` local space as the left half, matching `loupe_area()`,
    /// so zoom-at-cursor anchors correctly regardless of which side it's over.
    pub(crate) fn cursor_in_loupe(&self) -> (f32, f32) {
        // `self.cursor` is already physical pixels (winit `CursorMoved` reports a
        // `PhysicalPosition`), and `loupe_viewport` is physical too — so we just
        // subtract the viewport origin; no scale-factor conversion.
        let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
        match self.loupe_viewport {
            Some((x, y, w, h)) => {
                let mut lx = px - x as f32;
                let ly = py - y as f32;
                if self.compare && self.mode == ViewMode::Loupe && w >= 2 && h > 0 {
                    let half = (w / 2) as f32;
                    // Equal-sized viewports leave an odd spare pixel as a
                    // divider between the before and after images.
                    let right_start = half + (w % 2) as f32;
                    if lx >= right_start {
                        lx -= right_start;
                    }
                }
                (lx, ly)
            }
            None => (px, py),
        }
    }

    /// The `(scale, offset, rot)` the shader transform is currently built from —
    /// the values `push_transform` uploads. Shared so the crop overlay can map
    /// between screen points and texture UVs using the exact same geometry.
    /// `rot` is the row-major 2×2 `[m00, m01, m10, m11]` used by the shader.
    pub(super) fn loupe_transform(&self) -> ([f32; 2], [f32; 2], [f32; 4]) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let denom_x = self.zoom * iw;
        let denom_y = self.zoom * ih;
        let scale = [ww / denom_x, wh / denom_y];
        let offset = [-self.pan.0 / denom_x, -self.pan.1 / denom_y];
        (scale, offset, self.rot_matrix())
    }

    /// The display-UV → texture-UV rotation matrix for the current 90° step.
    pub(super) fn rot_matrix(&self) -> [f32; 4] {
        match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        }
    }

    /// Configure the renderer for the before/after compare view: the shared
    /// live zoom/pan transform (compare-aware via `loupe_area()`, so it
    /// targets the half-width area each side actually occupies), the primary
    /// adjustments = "before" (identity tone but the same crop), the
    /// secondary = "after" (the full edits).
    pub(super) fn push_compare(&mut self) {
        let after = self.current_adjustments();
        let before = Adjustments {
            crop: after.crop,
            ..Adjustments::default()
        };
        let (scale, offset, rot) = self.loupe_transform();
        let mut gpu_before = self.gpu_adjust(&before);
        gpu_before._pad0 = 0.0;
        let gpu_after = self.gpu_adjust(&after);
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
            r.set_adjustments(gpu_before);
            r.set_adjustments_b(gpu_after);
        }
    }

    /// Toggle the before/after compare view (Loupe only). Flipping it changes
    /// what `loupe_area()` returns (full width <-> half width) with no
    /// viewport-resize event to trigger the usual per-frame refit. Preserve
    /// the image point at the viewport center when the view is manually
    /// zoomed, so toggling compare does not discard the current pan.
    pub(super) fn toggle_compare(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        let old_width = self.loupe_area().0;
        self.compare = !self.compare;
        if self.fitted {
            if self.crop_edit.is_some() {
                self.fit_for_crop();
            } else {
                self.fit_to_window();
            }
        } else {
            let new_width = self.loupe_area().0;
            self.pan.0 += (new_width - old_width) / 2.0;
            self.push_transform();
        }
        if !self.compare {
            self.push_adjustments();
        }
        self.request_redraw();
    }

    /// Whether the before/after compare view is active.
    pub(crate) fn compare(&self) -> bool {
        self.compare
    }

    // ---- Subject-selection overlay ------------------------------------------

    /// Whether the subject-selection overlay is switched on.
    pub(crate) fn selection_on(&self) -> bool {
        self.selection_on
    }

    /// Whether the overlay highlights the background rather than the subject.
    pub(crate) fn selection_inverted(&self) -> bool {
        self.selection_invert
    }

    /// The mask for the photo currently on screen, if one has been computed.
    pub(crate) fn current_selection(&self) -> Option<&crate::segmentation::Mask> {
        let want = self.want.as_ref()?;
        self.current_selection
            .as_ref()
            .filter(|(path, _)| path == want)
            .map(|(_, mask)| mask)
    }

    /// Whether a mask is being computed for the photo on screen right now
    /// (so the UI can say "working" rather than "no subject found").
    pub(crate) fn selection_pending(&self) -> bool {
        self.selection_pending.is_some()
    }

    /// Flip the overlay on/off, kicking off the mask computation on the way on.
    pub(super) fn toggle_selection(&mut self) {
        self.selection_on = !self.selection_on;
        if self.selection_on {
            self.request_selection_mask();
        }
        self.sync_selection_overlay();
        self.request_redraw();
    }

    /// Flip the overlay between highlighting the subject and the background.
    /// Purely a display change — the same mask, read the other way round.
    pub(super) fn toggle_selection_invert(&mut self) {
        self.selection_invert = !self.selection_invert;
        self.sync_selection_overlay();
        self.request_redraw();
    }

    /// Push the current mask (or its absence) to the renderer.
    ///
    /// The mask goes up at Vision's own resolution: the shader samples it with
    /// the image's normalized UVs, so the GPU's bilinear filter does the
    /// stretching and there's nothing to keep in step with zoom or pan.
    pub(super) fn sync_selection_overlay(&mut self) {
        let want = self.want.clone();
        let mask = if self.selection_on {
            self.current_selection
                .as_ref()
                .filter(|(path, _)| Some(path) == want.as_ref())
                .map(|(_, mask)| mask)
        } else {
            None
        };
        let inverted = self.selection_invert;
        if let Some(r) = &mut self.renderer {
            r.set_selection_inverted(inverted);
            r.set_selection_mask(mask.map(|m| (m.alpha.as_slice(), m.width, m.height)));
        }
    }

    /// Drop a mask that no longer belongs to the photo on screen. Called when
    /// the Loupe moves to a different picture.
    pub(super) fn invalidate_selection(&mut self) {
        let stale = match (&self.current_selection, &self.want) {
            (Some((path, _)), Some(want)) => path != want,
            (Some(_), None) => true,
            _ => false,
        };
        if stale {
            self.current_selection = None;
            self.sync_selection_overlay();
        }
    }

    /// Start segmenting the photo on screen, unless it's already done or
    /// already running.
    ///
    /// One detached thread per request rather than a worker pool: this fires
    /// on a deliberate user action, for exactly one photo at a time, so there
    /// is no queue to schedule and nothing to keep warm between uses.
    pub(super) fn request_selection_mask(&mut self) {
        if !self.selection_on || self.selection_pending.is_some() {
            return;
        }
        let Some(want) = self.want.clone() else {
            return;
        };
        if self.current_selection().is_some() {
            return;
        }

        self.selection_pending = Some(want.clone());
        let tx = self.selection_tx.clone();
        // If the spawn fails, clear the pending marker so the next frame can
        // retry rather than the overlay hanging on "working" forever.
        if std::thread::Builder::new()
            .name("segmentation-worker".to_string())
            .spawn(move || {
                let result = crate::segmentation::segment(&want);
                let _ = tx.send((want, result));
            })
            .is_err()
        {
            self.selection_pending = None;
        }
    }

    /// Fold a finished segmentation into `current_selection`, discarding it if
    /// the Loupe has moved on to a different photo meanwhile.
    pub(crate) fn poll_selection_mask(&mut self) {
        while let Ok((path, result)) = self.selection_rx.try_recv() {
            if self.selection_pending.as_ref() == Some(&path) {
                self.selection_pending = None;
            }
            if self.want.as_ref() != Some(&path) {
                continue; // moved on; this mask is for a photo nobody is looking at
            }
            match result {
                Ok(mask) => self.current_selection = Some((path, mask)),
                // No subject found is a legitimate answer, not an error worth a
                // toast — the overlay simply has nothing to draw.
                Err(_) => self.current_selection = None,
            }
            self.sync_selection_overlay();
            self.request_redraw();
        }
    }

    /// Recompute the shader transform from the current view state.
    pub(crate) fn push_transform(&mut self) {
        let (scale, offset, rot) = self.loupe_transform();
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
        }
        self.request_redraw();
    }

    /// Map a normalized texture UV (crop space, 0..1) to a screen point inside
    /// the loupe rect `central` (egui logical px). Inverse of
    /// `loupe_screen_to_tex`; used to draw the crop rectangle/handles/mask.
    pub(crate) fn loupe_tex_to_screen(&self, central: egui::Rect, u: f32, v: f32) -> egui::Pos2 {
        let (scale, offset, rot) = self.loupe_transform();
        // Invert uv = R·(d − 0.5) + 0.5. R is a rotation, so R⁻¹ = Rᵀ.
        let (du, dv) = (u - 0.5, v - 0.5);
        let dx = rot[0] * du + rot[2] * dv + 0.5;
        let dy = rot[1] * du + rot[3] * dv + 0.5;
        // Invert d = base_uv · scale + offset.
        let bx = (dx - offset[0]) / scale[0];
        let by = (dy - offset[1]) / scale[1];
        egui::pos2(
            central.min.x + bx * central.width(),
            central.min.y + by * central.height(),
        )
    }

    /// Map a screen point inside the loupe rect `central` to a normalized texture
    /// UV (crop space, 0..1). Inverse of `loupe_tex_to_screen`; used to turn a
    /// crop-edge drag into a crop coordinate.
    pub(crate) fn loupe_screen_to_tex(&self, central: egui::Rect, p: egui::Pos2) -> (f32, f32) {
        let (scale, offset, rot) = self.loupe_transform();
        let bx = if central.width() > 0.0 {
            (p.x - central.min.x) / central.width()
        } else {
            0.0
        };
        let by = if central.height() > 0.0 {
            (p.y - central.min.y) / central.height()
        } else {
            0.0
        };
        let dx = bx * scale[0] + offset[0];
        let dy = by * scale[1] + offset[1];
        // uv = R·(d − 0.5) + 0.5, R row-major [m00, m01, m10, m11].
        let u = rot[0] * (dx - 0.5) + rot[1] * (dy - 0.5) + 0.5;
        let v = rot[2] * (dx - 0.5) + rot[3] * (dy - 0.5) + 0.5;
        (u, v)
    }
}

/// Whether the current zoom magnifies the screen-fit preview past its own
/// pixels, i.e. whether softness would now be visible and the full-resolution
/// decode is worth its cost.
///
/// `source_longest` is the original's longest side in pixels and `zoom` is
/// source-pixels-to-screen-pixels, so their product is how many screen pixels
/// the image spans — compare that against how many pixels the preview actually
/// has.
fn zoom_outruns_preview(source_longest: f32, zoom: f32, preview_px: u32) -> bool {
    source_longest * zoom > preview_px as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    // A 24MP photo (6000x4000) in a 2560px preview.
    const SOURCE: f32 = 6000.0;
    const PREVIEW: u32 = 2560;

    #[test]
    fn browsing_at_fit_never_asks_for_the_expensive_decode() {
        // Fit in a 2560px-wide window is zoom ~0.43: the preview has more pixels
        // than the screen can show, so full resolution would be invisible.
        assert!(!zoom_outruns_preview(SOURCE, 2560.0 / SOURCE, PREVIEW));
        // Zoomed out further, even more so.
        assert!(!zoom_outruns_preview(SOURCE, 0.1, PREVIEW));
    }

    #[test]
    fn the_preview_is_ridden_right_up_to_its_own_resolution() {
        // Exactly at the preview's pixel count is still not worth a full decode.
        assert!(!zoom_outruns_preview(SOURCE, PREVIEW as f32 / SOURCE, PREVIEW));
        // A hair past it is.
        assert!(zoom_outruns_preview(SOURCE, (PREVIEW as f32 + 1.0) / SOURCE, PREVIEW));
    }

    #[test]
    fn hitting_one_to_one_on_a_big_photo_fetches_full_resolution() {
        // Alt+0 sets zoom to 1.0 — every source pixel on screen, which no
        // preview can satisfy for a photo larger than the preview target.
        assert!(zoom_outruns_preview(SOURCE, 1.0, PREVIEW));
    }

    #[test]
    fn a_photo_smaller_than_the_preview_never_needs_a_second_decode() {
        // The preview *is* the full image here (decode-at-size can't upscale),
        // so even 1:1 must not trigger a redundant full decode.
        assert!(!zoom_outruns_preview(1600.0, 1.0, PREVIEW));
    }
}
