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

    /// The absolute source-pixel→screen-pixel ratio at which the whole image
    /// exactly fits the loupe area ("contain"). The anchor `zoom_rel` is measured
    /// against: `zoom_rel == 1.0` is fitted, `zoom() == zoom_rel * fit_scale()`.
    ///
    /// Depends only on the image's aspect ratio and the loupe area, never on
    /// which decode tier's pixel dimensions happen to be uploaded — that is what
    /// makes the loupe transform survive a preview→full swap without a jump.
    pub(super) fn fit_scale(&self) -> f32 {
        fit_scale_of(self.display_size(), self.loupe_area())
    }

    /// The current absolute zoom (source-pixel→screen-pixel ratio), derived from
    /// the fit-relative `zoom_rel` and the live `fit_scale()`. Every consumer
    /// that needs an absolute scale goes through here.
    pub(crate) fn zoom(&self) -> f32 {
        self.zoom_rel * self.fit_scale()
    }

    /// Fit to the loupe area, centered: scales the image up or down so the whole
    /// image is as large as possible while staying fully on-screen ("contain").
    pub(super) fn fit_to_window(&mut self) {
        self.zoom_rel = 1.0;
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Fit the *whole* image into the loupe area for crop mode: unlike
    /// `fit_to_window` this shrinks images larger than the viewport (no grow-only
    /// floor) and leaves a small margin, so the entire image — and thus all four
    /// crop edges and their handles — stay on-screen and grabbable.
    pub(super) fn fit_for_crop(&mut self) {
        // ~5% border each side so edge handles aren't flush against the viewport.
        const MARGIN: f32 = 0.9;
        self.zoom_rel = MARGIN;
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Reset to 100% (1 image pixel == 1 screen pixel), centered. `zoom_rel` is
    /// `1.0 / fit_scale()` so that `zoom()` lands on exactly `1.0` right now (the
    /// `fit_scale()` factors cancel). If `image_size()` later gains its true
    /// `source_size` the effective zoom drifts slightly — negligible in practice,
    /// since that metadata almost always lands before the user hits this.
    pub(super) fn reset_100(&mut self) {
        let fs = self.fit_scale();
        self.zoom_rel = if fs > 0.0 { 1.0 / fs } else { 1.0 };
        self.fitted = false;
        self.center();
        self.push_transform();
        self.ensure_full_for_zoom();
    }

    /// Whether the full-resolution decode is currently justified: a photo is
    /// wanted and the zoom has magnified the screen-fit preview past its own
    /// pixels. Both `ensure_full_for_zoom` (the one-shot trigger, fired on
    /// every zoom change) and `request_web_full` (`app/web.rs`, re-polled every
    /// frame from `main.rs` so retry/backoff eventually fires) gate on this, so
    /// normal fitted browsing never reaches the expensive decode path.
    pub(super) fn full_wanted_for_zoom(&self) -> bool {
        if self.want.is_none() {
            return false;
        }
        let (iw, ih) = self.image_size();
        zoom_outruns_preview(iw.max(ih), self.zoom(), self.preview_px())
    }

    /// Fetch the full-resolution decode once the current zoom would magnify the
    /// screen-fit preview past its own pixels — i.e. the moment the preview
    /// stops being enough and softness would actually be visible. Below that
    /// threshold this does nothing, which is what keeps normal browsing off the
    /// expensive decode path entirely. `Loader::request_full` de-duplicates, so
    /// calling this on every zoom step is cheap.
    pub(super) fn ensure_full_for_zoom(&mut self) {
        if !self.full_wanted_for_zoom() {
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
        let old_zoom = self.zoom();
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
            let fs = self.fit_scale();
            if fs > 0.0 {
                self.zoom_rel = old_zoom / fs;
            }
            self.center();
            self.push_transform();
        }
    }

    pub(super) fn center(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let z = self.zoom();
        self.pan = ((ww - iw * z) / 2.0, (wh - ih * z) / 2.0);
    }

    /// Zoom by `factor`, keeping the image point under (cx, cy) fixed. `cx/cy`
    /// are in loupe-area-local pixels (origin at the viewport's top-left).
    pub(crate) fn zoom_at(&mut self, factor: f32, cx: f32, cy: f32) {
        let cur_zoom = self.zoom();
        let new_zoom = bounded_zoom(cur_zoom, factor);
        let ipx = (cx - self.pan.0) / cur_zoom;
        let ipy = (cy - self.pan.1) / cur_zoom;
        self.pan.0 = cx - ipx * new_zoom;
        self.pan.1 = cy - ipy * new_zoom;
        let fs = self.fit_scale();
        if fs > 0.0 {
            self.zoom_rel = new_zoom / fs;
        }

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
        loupe_xform(
            self.display_size(),
            self.loupe_area(),
            self.zoom(),
            self.pan,
            self.rot_matrix(),
        )
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
        // Absolute zoom before the split changes `loupe_area()` (and thus
        // `fit_scale()`); restored below so a compare toggle never rescales a
        // manually-zoomed view.
        let keep_zoom = self.zoom();
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
            let fs = self.fit_scale();
            if fs > 0.0 {
                self.zoom_rel = keep_zoom / fs;
            }
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

/// The `(scale, offset, rot)` shader transform for a given loupe view state.
/// Pulled out as a free function so the dimension-invariance property (an
/// `image_size` change from one decode tier to the next must not move the
/// on-screen image) can be unit-tested without constructing an `App`.
///
/// `image_size` is the display-oriented source size, `area` the loupe viewport,
/// `zoom` the absolute source-pixel→screen-pixel ratio (`App::zoom`), `pan` the
/// image's top-left corner in screen pixels, `rot` the display-UV→texture-UV
/// matrix.
fn loupe_xform(
    image_size: (f32, f32),
    area: (f32, f32),
    zoom: f32,
    pan: (f32, f32),
    rot: [f32; 4],
) -> ([f32; 2], [f32; 2], [f32; 4]) {
    let (iw, ih) = image_size;
    let (ww, wh) = area;
    let denom_x = zoom * iw;
    let denom_y = zoom * ih;
    let scale = [ww / denom_x, wh / denom_y];
    let offset = [-pan.0 / denom_x, -pan.1 / denom_y];
    (scale, offset, rot)
}

/// The fit ("contain") scale for an image of `image_size` in a loupe `area` —
/// the free-function core of `App::fit_scale`, so `zoom_rel` conversions can be
/// checked in isolation.
fn fit_scale_of(image_size: (f32, f32), area: (f32, f32)) -> f32 {
    let (iw, ih) = image_size;
    let (ww, wh) = area;
    (ww / iw).min(wh / ih)
}

/// Apply explicit zoom bounds without snapping a contain-fit zoom into them.
/// A fit can be outside the range used for manual zooming, so while below the
/// minimum only zoom-in can move it toward the range, and while above the
/// maximum only zoom-out can do so.
fn bounded_zoom(cur_zoom: f32, factor: f32) -> f32 {
    let requested = cur_zoom * factor;
    if cur_zoom < MIN_ZOOM {
        requested.clamp(cur_zoom, MIN_ZOOM)
    } else if cur_zoom > MAX_ZOOM {
        requested.clamp(MAX_ZOOM, cur_zoom)
    } else {
        requested.clamp(MIN_ZOOM, MAX_ZOOM)
    }
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
        assert!(!zoom_outruns_preview(
            SOURCE,
            PREVIEW as f32 / SOURCE,
            PREVIEW
        ));
        // A hair past it is.
        assert!(zoom_outruns_preview(
            SOURCE,
            (PREVIEW as f32 + 1.0) / SOURCE,
            PREVIEW
        ));
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

    // ---- Fit-relative zoom / dimension invariance --------------------------

    const AREA: (f32, f32) = (2560.0, 1440.0);
    // Same 3:2 aspect, two decode tiers: the screen-fit preview and the full
    // source. `4000 * 2560 / 6000 = 1706.67` floors to 1707 — the ~0.02%
    // aspect drift a real `fit_within` decode leaves behind.
    const PREVIEW_DIMS: (f32, f32) = (2560.0, 1707.0);
    const SOURCE_DIMS: (f32, f32) = (6000.0, 4000.0);

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    /// The whole point of storing `zoom` fit-relative: a manual zoom + pan,
    /// re-evaluated after `image_size()` jumps from the preview's dimensions to
    /// the true source dimensions, must yield the same shader transform.
    #[test]
    fn the_transform_survives_a_preview_to_full_swap() {
        let rot = [1.0, 0.0, 0.0, 1.0];
        let zoom_rel = 3.0;
        let pan = (-812.0, -430.0);

        let z_preview = zoom_rel * fit_scale_of(PREVIEW_DIMS, AREA);
        let z_source = zoom_rel * fit_scale_of(SOURCE_DIMS, AREA);
        // Absolute zoom differs wildly between the two tiers…
        assert!((z_preview / z_source - SOURCE_DIMS.0 / PREVIEW_DIMS.0).abs() < 0.01);

        let before = loupe_xform(PREVIEW_DIMS, AREA, z_preview, pan, rot);
        let after = loupe_xform(SOURCE_DIMS, AREA, z_source, pan, rot);
        // …but the transform the shader sees does not (aspect drift only).
        assert!(
            close(before.0, after.0),
            "scale {:?} vs {:?}",
            before.0,
            after.0
        );
        assert!(
            close(before.1, after.1),
            "offset {:?} vs {:?}",
            before.1,
            after.1
        );
    }

    /// `reset_100` picks `zoom_rel = 1.0 / fit_scale()` so the effective zoom is
    /// exactly 1:1 regardless of window or image size.
    #[test]
    fn reset_100_lands_on_true_one_to_one() {
        for area in [(2560.0, 1440.0), (800.0, 600.0), (5000.0, 3000.0)] {
            for dims in [SOURCE_DIMS, PREVIEW_DIMS, (1200.0, 1600.0)] {
                let fs = fit_scale_of(dims, area);
                let zoom_rel = 1.0 / fs;
                assert!(
                    (zoom_rel * fs - 1.0).abs() < 1e-4,
                    "area {area:?} dims {dims:?}"
                );
            }
        }
    }

    /// Fitted (`zoom_rel == 1.0`) fills the fit-limiting axis exactly: the
    /// visible region spans the whole texture on that axis (`scale` == 1.0) and
    /// letterboxes the other (`scale` > 1.0, more than the texture visible).
    #[test]
    fn fitted_exactly_contains_the_image() {
        for dims in [SOURCE_DIMS, PREVIEW_DIMS, (1200.0, 1600.0)] {
            let fs = fit_scale_of(dims, AREA);
            let (scale, _, _) = loupe_xform(dims, AREA, fs, (0.0, 0.0), [1.0, 0.0, 0.0, 1.0]);
            let tight = scale[0].min(scale[1]);
            let loose = scale[0].max(scale[1]);
            assert!(
                (tight - 1.0).abs() < 1e-4,
                "dims {dims:?}: tight axis {tight}"
            );
            assert!(loose >= 1.0 - 1e-4, "dims {dims:?}: loose axis {loose}");
        }
    }

    #[test]
    fn fit_anchor_is_not_limited_by_explicit_zoom_bounds() {
        // Fit-relative zoom must preserve the contain scale even when fitting
        // naturally lands outside the range used by explicit zoom operations.
        assert_eq!(fit_scale_of((100_000.0, 100_000.0), (100.0, 100.0)), 0.001);
        assert_eq!(fit_scale_of((1.0, 1.0), (100.0, 100.0)), 100.0);
    }

    #[test]
    fn zoom_below_minimum_moves_only_toward_the_allowed_range() {
        assert_eq!(bounded_zoom(0.001, 2.0), 0.002);
        assert_eq!(bounded_zoom(0.001, 0.5), 0.001);
        assert_eq!(bounded_zoom(0.001, 100.0), MIN_ZOOM);
    }

    #[test]
    fn zoom_above_maximum_moves_only_toward_the_allowed_range() {
        assert_eq!(bounded_zoom(100.0, 1.1), 100.0);
        assert_eq!(bounded_zoom(100.0, 0.5), 64.0);
        assert_eq!(bounded_zoom(100.0, 2.0), 100.0);
    }
}
