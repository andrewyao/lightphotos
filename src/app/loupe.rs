//! Loupe view state: zoom, pan and fit, the screen-to-texture coordinate
//! transforms, and the subject-selection overlay. It never decodes pixels. It
//! requests the full-resolution decode only once zoom outruns the preview.

use super::*;
use std::path::Path;

use crate::develop::Adjustments;

impl App {
    /// Image size after rotation, with w and h swapped for 90° and 270°.
    pub(super) fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.image_size();
        if self.current_rotation() % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// The loupe image area in physical pixels. While comparing, each side
    /// gets half the width. This must match the split the compare render uses.
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

    /// Screen pixels per source pixel at which the whole image fits the loupe
    /// area. `zoom_rel` is relative to this, so swapping the preview for the
    /// full decode doesn't make the image jump.
    pub(super) fn fit_scale(&self) -> f32 {
        fit_scale_of(self.display_size(), self.loupe_area())
    }

    /// The current zoom in screen pixels per source pixel.
    pub(crate) fn zoom(&self) -> f32 {
        self.zoom_rel * self.fit_scale()
    }

    /// Scale the image up or down so it just fits the loupe area, centered.
    pub(super) fn fit_to_window(&mut self) {
        self.zoom_rel = 1.0;
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Fit for crop mode: like `fit_to_window` but with a margin, so all four
    /// crop handles stay on screen and grabbable.
    pub(super) fn fit_for_crop(&mut self) {
        const MARGIN: f32 = 0.9;
        self.zoom_rel = MARGIN;
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Zoom to 100% (one image pixel per screen pixel), centered. If the true
    /// source size arrives later, the zoom drifts slightly off 100%.
    pub(super) fn reset_100(&mut self) {
        let fs = self.fit_scale();
        self.zoom_rel = if fs > 0.0 { 1.0 / fs } else { 1.0 };
        self.fitted = false;
        self.center();
        self.push_transform();
        self.ensure_full_for_zoom();
    }

    /// Whether the zoom has magnified the preview past its own pixels, so the
    /// full-resolution decode is worth fetching.
    pub(super) fn full_wanted_for_zoom(&self) -> bool {
        if self.want.is_none() {
            return false;
        }
        let (iw, ih) = self.image_size();
        zoom_outruns_preview(iw.max(ih), self.zoom(), self.preview_px())
    }

    /// Request the full-resolution decode if `full_wanted_for_zoom`. Cheap to
    /// call on every zoom step because `Loader::request_full` de-duplicates.
    pub(super) fn ensure_full_for_zoom(&mut self) {
        if !self.full_wanted_for_zoom() {
            return;
        }
        // The loader has no workers on wasm, so the web build decodes through
        // `request_web_full` instead.
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

    /// Rotate the shown image 90°, clockwise if `cw`, and save it.
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
        #[cfg(target_arch = "wasm32")]
        crate::analytics::property("develop_edit_applied", "edit_kind", "adjustment");
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

    /// Zoom by `factor`, keeping the image point under `(cx, cy)` fixed. The
    /// point is in pixels from the loupe area's top-left.
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

        // Center any axis that fits entirely in the viewport.
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

    /// A scroll of `(dx, dy)` physical pixels over the loupe. Shift pans
    /// horizontally, Alt pans vertically, and plain or Shift+Alt zooms at the
    /// cursor.
    pub(crate) fn on_scroll(&mut self, dx: f32, dy: f32) {
        let shift = self.modifiers.shift_key();
        let alt = self.modifiers.alt_key();
        // macOS turns a Shift+wheel into horizontal scrolling.
        let s = if shift && dy == 0.0 { dx } else { dy };
        if s == 0.0 {
            return;
        }
        match (shift, alt) {
            (true, false) => self.pan_by(s, 0.0),
            (false, true) => self.pan_by(0.0, s),
            _ => {
                let (cx, cy) = self.cursor_in_loupe();
                self.zoom_at((s * 0.0025).exp(), cx, cy);
            }
        }
    }

    fn pan_by(&mut self, dx: f32, dy: f32) {
        self.pan.0 += dx;
        self.pan.1 += dy;
        self.fitted = false;
        self.push_transform();
    }

    /// Zoom by `factor` about the center of the loupe area.
    pub(super) fn zoom_by(&mut self, factor: f32) {
        let (w, h) = self.loupe_area();
        self.zoom_at(factor, w / 2.0, h / 2.0);
    }

    /// Step to the next of fit, 2x fit and 100% that is larger than the
    /// current zoom, wrapping back to fit. A stage that wouldn't change the
    /// zoom is skipped, so every press does something.
    pub(super) fn cycle_zoom(&mut self) {
        let fs = self.fit_scale();
        let cur = self.zoom() * 1.001;
        if fs > cur {
            self.fit_to_window();
        } else if 2.0 * fs > cur {
            self.zoom_rel = 2.0;
            self.fitted = false;
            self.center();
            self.push_transform();
            self.ensure_full_for_zoom();
        } else if 1.0 > cur {
            self.reset_100();
        } else {
            self.fit_to_window();
        }
    }

    /// Cursor position from the loupe viewport's top-left, in physical pixels.
    /// While comparing, the right half maps onto the same space as the left.
    pub(crate) fn cursor_in_loupe(&self) -> (f32, f32) {
        // Both values are already physical pixels, so no DPI scaling.
        let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
        match self.loupe_viewport {
            Some((x, y, w, h)) => {
                let mut lx = px - x as f32;
                let ly = py - y as f32;
                if self.compare && self.mode == ViewMode::Loupe && w >= 2 && h > 0 {
                    let half = (w / 2) as f32;
                    // An odd width leaves one spare pixel as the divider.
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

    /// The `(scale, offset, rot)` shader transform that `push_transform`
    /// uploads. `rot` is a row-major 2×2 `[m00, m01, m10, m11]`.
    pub(super) fn loupe_transform(&self) -> ([f32; 2], [f32; 2], [f32; 4]) {
        loupe_xform(
            self.display_size(),
            self.loupe_area(),
            self.zoom(),
            self.pan,
            self.rot_matrix(),
        )
    }

    /// The display-UV to texture-UV rotation matrix for the current rotation.
    pub(super) fn rot_matrix(&self) -> [f32; 4] {
        match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        }
    }

    /// Set up the renderer for before/after compare. "Before" keeps only the
    /// crop; "after" has all edits. Both share the zoom and pan.
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

    /// Toggle before/after compare (Loupe only). This halves or doubles the
    /// loupe width with no resize event, so refit here. A manual zoom keeps
    /// its scale and the image point at the center.
    pub(super) fn toggle_compare(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        let old_width = self.loupe_area().0;
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

    /// Whether a mask is being computed, so the UI can say "working" instead of
    /// "no subject found".
    pub(crate) fn selection_pending(&self) -> bool {
        self.selection_pending.is_some()
    }

    pub(super) fn toggle_selection(&mut self) {
        self.selection_on = !self.selection_on;
        if self.selection_on {
            self.request_selection_mask();
        }
        self.sync_selection_overlay();
        self.request_redraw();
    }

    pub(super) fn toggle_selection_invert(&mut self) {
        self.selection_invert = !self.selection_invert;
        self.sync_selection_overlay();
        self.request_redraw();
    }

    /// Push the current mask, or its absence, to the renderer. The mask stays
    /// at Vision's resolution. The shader samples it by image UV, so it needs
    /// no update on zoom or pan.
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

    /// Drop a mask that belongs to a photo no longer on screen.
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

    /// Start segmenting the photo on screen on its own thread, unless it's done
    /// or running. It runs for one photo per user action, so no pool is needed.
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
        // If the spawn fails, clear pending so the overlay doesn't show
        // "working" forever.
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

    /// Store a finished mask, unless the Loupe has moved to another photo.
    pub(crate) fn poll_selection_mask(&mut self) {
        while let Ok((path, result)) = self.selection_rx.try_recv() {
            if self.selection_pending.as_ref() == Some(&path) {
                self.selection_pending = None;
            }
            if self.want.as_ref() != Some(&path) {
                continue;
            }
            match result {
                Ok(mask) => self.current_selection = Some((path, mask)),
                // "No subject found" is a normal result, so show no error.
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

    /// Map a texture UV (0..1) to a point in the loupe rect `central`, in egui
    /// logical pixels. Inverse of `loupe_screen_to_tex`.
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

    /// Map a point in the loupe rect `central` to a texture UV (0..1). Inverse
    /// of `loupe_tex_to_screen`.
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

/// Whether the image, at `zoom` screen pixels per source pixel, spans more
/// screen pixels than the preview has. Then the preview looks soft.
fn zoom_outruns_preview(source_longest: f32, zoom: f32, preview_px: u32) -> bool {
    source_longest * zoom > preview_px as f32
}

/// The `(scale, offset, rot)` shader transform for a loupe view. `image_size`
/// is the rotated source size, `zoom` is `App::zoom`, and `pan` is the image's
/// top-left corner in screen pixels. A free function so tests need no `App`.
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

fn fit_scale_of(image_size: (f32, f32), area: (f32, f32)) -> f32 {
    let (iw, ih) = image_size;
    let (ww, wh) = area;
    (ww / iw).min(wh / ih)
}

/// Clamp a manual zoom to `MIN_ZOOM..=MAX_ZOOM`. A fit zoom can lie outside
/// that range, so from there only moves toward the range are allowed.
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
        // Fit in a 2560px window is zoom ~0.43, and the preview covers it.
        assert!(!zoom_outruns_preview(SOURCE, 2560.0 / SOURCE, PREVIEW));
        assert!(!zoom_outruns_preview(SOURCE, 0.1, PREVIEW));
    }

    #[test]
    fn the_preview_is_ridden_right_up_to_its_own_resolution() {
        assert!(!zoom_outruns_preview(
            SOURCE,
            PREVIEW as f32 / SOURCE,
            PREVIEW
        ));
        assert!(zoom_outruns_preview(
            SOURCE,
            (PREVIEW as f32 + 1.0) / SOURCE,
            PREVIEW
        ));
    }

    #[test]
    fn hitting_one_to_one_on_a_big_photo_fetches_full_resolution() {
        // Alt+0 sets zoom to 1.0, which no preview smaller than the source covers.
        assert!(zoom_outruns_preview(SOURCE, 1.0, PREVIEW));
    }

    #[test]
    fn a_photo_smaller_than_the_preview_never_needs_a_second_decode() {
        // The preview is already the full image, since decoding never upscales.
        assert!(!zoom_outruns_preview(1600.0, 1.0, PREVIEW));
    }

    const AREA: (f32, f32) = (2560.0, 1440.0);
    // The preview and full sizes of one 3:2 photo. Rounding 1706.67 to 1707
    // adds the small aspect drift a real preview decode has.
    const PREVIEW_DIMS: (f32, f32) = (2560.0, 1707.0);
    const SOURCE_DIMS: (f32, f32) = (6000.0, 4000.0);

    fn close(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    /// A manual zoom and pan must give the same shader transform after the
    /// image size jumps from the preview's to the source's.
    #[test]
    fn the_transform_survives_a_preview_to_full_swap() {
        let rot = [1.0, 0.0, 0.0, 1.0];
        let zoom_rel = 3.0;
        let pan = (-812.0, -430.0);

        let z_preview = zoom_rel * fit_scale_of(PREVIEW_DIMS, AREA);
        let z_source = zoom_rel * fit_scale_of(SOURCE_DIMS, AREA);
        assert!((z_preview / z_source - SOURCE_DIMS.0 / PREVIEW_DIMS.0).abs() < 0.01);

        let before = loupe_xform(PREVIEW_DIMS, AREA, z_preview, pan, rot);
        let after = loupe_xform(SOURCE_DIMS, AREA, z_source, pan, rot);
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

    /// At fit, the tighter axis shows exactly the whole texture (`scale` 1.0)
    /// and the other axis is letterboxed (`scale` >= 1.0).
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
