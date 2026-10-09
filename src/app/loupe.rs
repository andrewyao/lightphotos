//! Loupe view state: zoom, pan and fit, the screen-to-texture coordinate
//! transforms, and the subject-selection overlay. It never decodes pixels. It
//! requests the full-resolution decode only once zoom outruns the preview.

use super::*;
use std::path::Path;

use crate::develop::Adjustments;
use crate::jobs::thumbnail::Origin;

impl App {
    /// Image size after rotation, with w and h swapped for 90° and 270°.
    fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.image_size();
        if self.current_rotation() % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// The loupe image area in physical pixels. While comparing, each side
    /// gets half the width. This must match the split the compare render uses.
    fn loupe_area(&self) -> (f32, f32) {
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
    pub(super) fn zoom(&self) -> f32 {
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
        self.ensure_full();
    }

    /// Whether the photo, at the current zoom or fit, spans more panel pixels
    /// than its preview has, so the full-resolution decode is worth fetching.
    /// At fit this is true only on a window larger than `PREVIEW_MAX` panel
    /// pixels.
    ///
    /// False until the wanted photo is on screen: right after a step the
    /// previous photo's zoom is still set, and the new photo fits only when
    /// its first image uploads.
    pub(super) fn full_wanted(&self) -> bool {
        if self.want.is_none() || self.shown.path() != self.want.as_deref() {
            return false;
        }
        self.outruns_preview_at(self.zoom()) && !self.preview_is_complete()
    }

    /// Whether the wanted photo's landed preview already holds every source
    /// pixel, so a full decode would only return the same pixels again. True
    /// for a photo no larger than its preview target.
    fn preview_is_complete(&self) -> bool {
        let (Some(want), Some(loader)) = (&self.want, &self.loader) else {
            return false;
        };
        let target = self.preview_px();
        loader.preview_origin(want, target) == Origin::Decoded
            && loader
                .get_preview(want, target)
                .is_some_and(|img| self.covers_source(img.width.max(img.height)))
    }

    /// Whether a decode with this longest side holds every pixel of the
    /// source. Native only: the web build learns `source_size` from the
    /// preview itself, so there the two always match.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn covers_source(&self, longest: u32) -> bool {
        self.source_size.is_some_and(|(w, h)| longest >= w.max(h))
    }

    #[cfg(target_arch = "wasm32")]
    pub(super) fn covers_source(&self, _longest: u32) -> bool {
        false
    }

    /// `full_wanted` as if the photo were fitted, which is how a neighbor
    /// opens. Gates the neighbors' full-decode prefetch, so zooming into one
    /// photo doesn't decode its neighbors at full size. Native only: the web
    /// build keeps one full decode, so it never prefetches neighbors.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn full_wanted_at_fit(&self) -> bool {
        self.want.is_some() && self.outruns_preview_at(self.fit_scale())
    }

    fn outruns_preview_at(&self, zoom: f32) -> bool {
        let (iw, ih) = self.image_size();
        // Count panel pixels, not drawn ones: a scaled display mode shrinks
        // the drawn window onto fewer panel pixels.
        zoom_outruns_preview(iw.max(ih), zoom * self.panel_scale, self.preview_px())
    }

    /// `ensure_full`, but only once the current photo's preview has landed,
    /// so the preview reaches the screen first and the full decode doesn't
    /// compete with it. `try_show` calls this every frame, which covers a
    /// photo opening fitted, a window resize, and a re-fit when the source
    /// size arrives. The web build makes the same check in `request_web_full`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn ensure_full_after_preview(&mut self) {
        let (Some(want), Some(loader)) = (&self.want, &self.loader) else {
            return;
        };
        if loader.get_preview(want, self.preview_px()).is_none() {
            return;
        }
        self.ensure_full();
    }

    /// Request the full-resolution decode if `full_wanted`. Cheap to
    /// call every frame because `Loader::request_full` de-duplicates.
    fn ensure_full(&mut self) {
        if !self.full_wanted() {
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
        crate::web::analytics::property("develop_edit_applied", "edit_kind", "adjustment");
        self.flip_crop_orientation();
        if self.fitted {
            if self.cropping() {
                self.fit_for_crop();
            } else {
                self.fit_to_window();
            }
        } else {
            let fs = self.fit_scale();
            if fs > 0.0 {
                self.zoom_rel = old_zoom / fs;
            }
            self.center();
            self.push_transform();
        }
    }

    fn center(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let z = self.zoom();
        self.pan = ((ww - iw * z) / 2.0, (wh - ih * z) / 2.0);
    }

    /// Whether the image is larger than the loupe area on either axis, so a
    /// plain drag has something to pan.
    pub(crate) fn image_overflows(&self) -> bool {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let z = self.zoom();
        iw * z > ww + 0.5 || ih * z > wh + 0.5
    }

    /// Zoom by `factor`, keeping the image point under `(cx, cy)` fixed. The
    /// point is in pixels from the loupe area's top-left.
    pub(crate) fn zoom_at(&mut self, factor: f32, cx: f32, cy: f32) {
        let cur_zoom = self.zoom();
        let (lo, hi) = self.zoom_bounds();
        let new_zoom = bounded_zoom(cur_zoom, factor, lo, hi);
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

        // Zooming all the way out lands on the fit, which a resize keeps.
        self.fitted = new_zoom <= lo * 1.0001;
        self.push_transform();
        self.ensure_full();
    }

    /// The manual zoom's range: the fit up to `MAX_ZOOM`, or just the fit
    /// for a photo so small its fit is closer than that.
    pub(crate) fn zoom_bounds(&self) -> (f32, f32) {
        let fs = self.fit_scale();
        (fs, MAX_ZOOM.max(fs))
    }

    /// Zoom to `zoom` screen pixels per source pixel, within
    /// [`zoom_bounds`](Self::zoom_bounds), about the loupe area's center.
    pub(super) fn set_zoom(&mut self, zoom: f32) {
        let cur = self.zoom();
        if cur > 0.0 && zoom > 0.0 {
            self.zoom_by(zoom / cur);
        }
    }

    /// A scroll of `(dx, dy)` physical pixels over the loupe. Shift pans
    /// horizontally, Alt pans vertically, and plain or Shift+Alt zooms at the
    /// cursor, except under Touch Up, where the Loupe's overlay turns the
    /// plain wheel into brush size and Shift+wheel into feather.
    pub(crate) fn on_scroll(&mut self, dx: f32, dy: f32) {
        let shift = self.modifiers.shift_key();
        let alt = self.modifiers.alt_key();
        // macOS turns a Shift+wheel into horizontal scrolling.
        let s = if shift && dy == 0.0 { dx } else { dy };
        if s == 0.0 {
            return;
        }
        match (shift, alt) {
            // Shift+wheel over the zoom marker resizes it; scrolling
            // up shrinks it, which zooms the tiles in, as the wheel does.
            (true, false) if self.resize_compare_square((s * 0.0025).exp()) => {
                self.request_redraw();
            }
            (true, false) if self.touchup_active() => {}
            (true, false) => self.pan_by(s, 0.0),
            (false, true) => self.pan_by(0.0, s),
            _ if self.touchup_active() => {}
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
            self.ensure_full();
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
    fn loupe_transform(&self) -> ([f32; 2], [f32; 2], [f32; 4]) {
        loupe_xform(
            self.display_size(),
            self.loupe_area(),
            self.zoom(),
            self.pan,
            self.rot_matrix(),
        )
    }

    /// The display-UV to texture-UV rotation matrix for the current rotation.
    fn rot_matrix(&self) -> [f32; 4] {
        match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        }
    }

    /// Set up the renderer for before/after compare. "Before" keeps only the
    /// crop and straighten; "after" has all edits. Both share the zoom and pan.
    pub(super) fn push_compare(&mut self) {
        let after = self.current_adjustments();
        let before = Adjustments {
            crop: after.crop,
            straighten: after.straighten,
            ..Adjustments::default()
        };
        let (scale, offset, rot) = self.loupe_transform();
        let mut gpu_before = self.gpu_adjust(&before);
        gpu_before._pad0 = 0.0;
        let gpu_after = self.gpu_adjust(&after);
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
            r.set_adjustments(gpu_before, &before.curve);
            r.set_adjustments_b(gpu_after, &after.curve);
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
            if self.cropping() {
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

    /// Whether this build has a subject-segmentation backend. Vision ships
    /// only on macOS, so elsewhere the Loupe leaves the control out instead of
    /// offering one that can only ever report "No subject".
    pub(crate) const fn selection_supported() -> bool {
        cfg!(target_os = "macos")
    }

    pub(crate) fn selection_on(&self) -> bool {
        self.selection_on
    }

    /// Whether the overlay highlights the background rather than the subject.
    pub(crate) fn selection_inverted(&self) -> bool {
        self.selection_invert
    }

    /// The mask for the photo currently on screen, if one has been computed.
    pub(crate) fn current_selection(&self) -> Option<&crate::scoring::segmentation::Mask> {
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

    /// O shows or hides the overlay. Shift+O swaps subject and background,
    /// showing the overlay first if it is hidden.
    pub(super) fn selection_key(&mut self, shift: bool) {
        if !Self::selection_supported() {
            return;
        }
        if !shift {
            return self.toggle_selection();
        }
        if !self.selection_on {
            self.toggle_selection();
        }
        self.toggle_selection_invert();
    }

    /// Push the current mask, or its absence, to the renderer. The mask stays
    /// at Vision's resolution. The shader samples it by image UV, so it needs
    /// no update on zoom or pan.
    fn sync_selection_overlay(&mut self) {
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
                let result = crate::scoring::segmentation::segment(&want);
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

    /// Like `loupe_tex_to_screen`, for a point of the source photo, which
    /// touch-ups and the white balance picker work in. The texture UV is the
    /// straightened canvas, so the two differ by the photo's straighten.
    pub(crate) fn loupe_source_to_screen(&self, central: egui::Rect, u: f32, v: f32) -> egui::Pos2 {
        let (u, v) = self.shown_straighten().to_canvas(u, v);
        self.loupe_tex_to_screen(central, u, v)
    }

    /// Inverse of `loupe_source_to_screen`.
    pub(crate) fn loupe_screen_to_source(&self, central: egui::Rect, p: egui::Pos2) -> (f32, f32) {
        let (u, v) = self.loupe_screen_to_tex(central, p);
        self.shown_straighten().to_source(u, v)
    }

    fn shown_straighten(&self) -> crate::develop::Straighten {
        let (w, h) = self.image_size();
        crate::develop::Straighten::new(self.current_adjustments().straighten, w, h)
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

/// Clamp a manual zoom to `lo..=hi`. Crop's fit, with its margin, lies below
/// `lo`, so from outside the range only moves toward it are allowed.
fn bounded_zoom(cur_zoom: f32, factor: f32, lo: f32, hi: f32) -> f32 {
    let requested = cur_zoom * factor;
    if cur_zoom < lo {
        requested.clamp(cur_zoom, lo)
    } else if cur_zoom > hi {
        requested.clamp(hi, cur_zoom)
    } else {
        requested.clamp(lo, hi)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::thumbnail::Origin;

    // A 24MP photo (6000x4000) in a 2560px preview.
    const SOURCE: f32 = 6000.0;
    const PREVIEW: u32 = 2560;

    #[test]
    fn browsing_at_fit_on_a_normal_window_skips_the_expensive_decode() {
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

    /// A 6000x4000 photo, the middle of three, fitted in the Loupe on a
    /// `win` physical-pixel window, its preview landed.
    fn fitted_app(tag: &str, win: (f32, f32)) -> (App, Vec<PathBuf>) {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
        use crate::jobs::{cache_limits::CacheLimits, loader::Loader};
        let (mut app, _, paths) = crate::app::test_support::folder_app(tag, 3);
        app.mode = ViewMode::Loupe;
        app.sel = Some(1);
        app.win_size = win;
        app.want = Some(paths[1].clone());
        app.source_size = Some((6000, 4000));
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        let preview = std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: 4,
            height: 4,
            rgba: vec![0; 64],
            pixel_format: PixelFormat::Srgb8,
        }));
        loader.insert_preview_external(
            paths[1].clone(),
            app.preview_px(),
            preview,
            Origin::Decoded,
        );
        app.loader = Some(loader);
        app.shown = Shown::Preview(paths[1].clone(), app.preview_px(), 4);
        app.fit_to_window();
        (app, paths)
    }

    fn land_preview(app: &mut App, path: &Path) {
        let target = app.preview_px();
        let img = app
            .loader
            .as_ref()
            .and_then(|l| l.get_preview(&app.want.clone().unwrap(), target));
        app.loader.as_mut().unwrap().insert_preview_external(
            path.to_path_buf(),
            target,
            img.unwrap(),
            Origin::Decoded,
        );
    }

    /// `fitted_app`, but the photo is 1200x800 and its preview, which the
    /// 1536px target can't shrink, holds the whole of it.
    fn small_photo_app(tag: &str) -> (App, Vec<PathBuf>) {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};
        let (mut app, paths) = fitted_app(tag, (1440.0, 900.0));
        app.source_size = Some((1200, 800));
        let img = std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: 1200,
            height: 800,
            rgba: vec![0; 1200 * 800 * 4],
            pixel_format: PixelFormat::Srgb8,
        }));
        let target = app.preview_px();
        app.loader.as_mut().unwrap().insert_preview_external(
            paths[1].clone(),
            target,
            img,
            Origin::Decoded,
        );
        app.shown = Shown::Preview(paths[1].clone(), target, 1200);
        app.shown_origin = Origin::Decoded;
        app.fit_to_window();
        (app, paths)
    }

    #[test]
    fn a_small_photos_preview_shows_as_full_resolution() {
        let (app, _) = small_photo_app("loupe-small-tier");
        assert_eq!(app.shown_tier(), Some(ShownTier::Full));
    }

    #[test]
    fn zooming_into_a_small_photo_skips_the_second_decode() {
        let (mut app, paths) = small_photo_app("loupe-small-zoom");
        app.set_zoom(5.0);
        app.try_show();
        assert!(!full_queued(&app, &paths[1]));
    }

    #[test]
    fn a_large_photos_preview_stays_below_full_resolution() {
        let (mut app, paths) = fitted_app("loupe-large-tier", (1440.0, 900.0));
        app.shown = Shown::Preview(paths[1].clone(), app.preview_px(), 1536);
        app.shown_origin = Origin::Decoded;
        assert_eq!(app.shown_tier(), Some(ShownTier::Preview));
    }

    #[test]
    fn zooming_in_on_a_normal_window_prefetches_no_neighbor_full_decodes() {
        let (mut app, paths) = fitted_app("loupe-zoom-neighbors", (2880.0, 1800.0));
        app.reset_100();
        assert!(app.full_wanted(), "100% outruns the preview");
        let full = app
            .loader
            .as_ref()
            .unwrap()
            .get_preview(&paths[1], app.preview_px());
        app.loader.as_mut().unwrap().insert_full_external(
            paths[1].clone(),
            full.unwrap(),
            Origin::Decoded,
        );
        app.request_neighbors();
        assert!(!full_queued(&app, &paths[0]));
        assert!(!full_queued(&app, &paths[2]));
    }

    #[test]
    fn stepping_while_zoomed_does_not_fetch_the_next_photo_at_full() {
        let (mut app, paths) = fitted_app("loupe-zoom-step", (2880.0, 1800.0));
        app.reset_100();
        // Step: the next photo is wanted and its preview has landed, but the
        // previous photo, and its zoom, are still on screen.
        app.sel = Some(2);
        land_preview(&mut app, &paths[2]);
        app.want = Some(paths[2].clone());
        app.try_show();
        assert!(!full_queued(&app, &paths[2]));
    }

    fn full_queued(app: &App, path: &Path) -> bool {
        app.loader.as_ref().is_some_and(|l| l.full_inflight(path))
    }

    #[test]
    fn the_title_bars_name_the_decode_on_screen() {
        let (mut app, paths) = fitted_app("loupe-tier", (1440.0, 900.0));
        let p = paths[1].clone();
        let cases = [
            (Shown::Thumb(p.clone()), Origin::Decoded, ShownTier::Thumb),
            (
                Shown::Preview(p.clone(), 2048, 1616),
                Origin::Embedded,
                ShownTier::Embedded,
            ),
            (
                Shown::Preview(p.clone(), 2048, 2048),
                Origin::Decoded,
                ShownTier::Preview,
            ),
            (
                Shown::Full(p.clone()),
                Origin::Embedded,
                ShownTier::Embedded,
            ),
            (Shown::Full(p.clone()), Origin::Decoded, ShownTier::Full),
        ];
        for (shown, origin, tier) in cases {
            app.shown = shown;
            app.shown_origin = origin;
            assert_eq!(app.shown_tier(), Some(tier));
        }
        // Still showing the previous photo while the new one loads.
        app.want = Some(paths[2].clone());
        assert_eq!(app.shown_tier(), None);
    }

    #[test]
    fn a_large_window_fetches_full_resolution_without_a_zoom() {
        // A 6K display: the fitted photo spans ~5000px, more than the 4096 preview.
        let (mut app, paths) = fitted_app("loupe-full-large", (6016.0, 3384.0));
        assert!(app.full_wanted());
        app.try_show();
        assert!(full_queued(&app, &paths[1]));
    }

    #[test]
    fn a_scaled_4k_display_stays_on_the_preview_at_fit() {
        // "Looks like 3360x1890" on a 3840x2160 panel: the window is drawn at
        // 6720x3780, but only 3840 panel pixels show it.
        let (mut app, paths) = fitted_app("loupe-full-scaled", (6720.0, 3780.0));
        assert!(app.full_wanted(), "the drawn window outruns the preview");
        app.panel_scale = 3840.0 / 6720.0;
        assert!(!app.full_wanted());
        app.try_show();
        app.request_neighbors();
        assert!(paths.iter().all(|p| !full_queued(&app, p)));
    }

    #[test]
    fn a_normal_window_stays_on_the_preview_at_fit() {
        let (mut app, paths) = fitted_app("loupe-full-small", (2880.0, 1800.0));
        assert!(!app.full_wanted());
        app.try_show();
        assert!(!full_queued(&app, &paths[1]));
    }

    #[test]
    fn a_large_window_waits_for_the_preview_before_the_full_decode() {
        let (mut app, paths) = fitted_app("loupe-full-wait", (6016.0, 3384.0));
        app.want = Some(paths[0].clone());
        app.try_show();
        assert!(!full_queued(&app, &paths[0]), "no preview of 0 yet");
    }

    #[test]
    fn a_large_window_prefetches_the_neighbors_once_the_current_full_lands() {
        let (mut app, paths) = fitted_app("loupe-full-neighbors", (6016.0, 3384.0));
        app.request_neighbors();
        assert!(!full_queued(&app, &paths[0]), "current full not landed yet");
        let full = app
            .loader
            .as_ref()
            .unwrap()
            .get_preview(&paths[1], app.preview_px());
        app.loader.as_mut().unwrap().insert_full_external(
            paths[1].clone(),
            full.unwrap(),
            Origin::Decoded,
        );
        app.request_neighbors();
        assert!(full_queued(&app, &paths[0]));
        assert!(full_queued(&app, &paths[2]));
    }

    #[test]
    fn a_normal_window_prefetches_no_neighbor_full_decodes() {
        let (mut app, paths) = fitted_app("loupe-full-no-neighbors", (2880.0, 1800.0));
        let full = app
            .loader
            .as_ref()
            .unwrap()
            .get_preview(&paths[1], app.preview_px());
        app.loader.as_mut().unwrap().insert_full_external(
            paths[1].clone(),
            full.unwrap(),
            Origin::Decoded,
        );
        app.request_neighbors();
        assert!(!full_queued(&app, &paths[0]));
        assert!(!full_queued(&app, &paths[2]));
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
        assert_eq!(bounded_zoom(0.25, 2.0, 1.0, 64.0), 0.5);
        assert_eq!(bounded_zoom(0.25, 0.5, 1.0, 64.0), 0.25);
        assert_eq!(bounded_zoom(0.25, 100.0, 1.0, 64.0), 1.0);
    }

    #[test]
    fn zoom_above_maximum_moves_only_toward_the_allowed_range() {
        assert_eq!(bounded_zoom(100.0, 1.1, 1.0, 64.0), 100.0);
        assert_eq!(bounded_zoom(100.0, 0.5, 1.0, 64.0), 64.0);
        assert_eq!(bounded_zoom(100.0, 2.0, 1.0, 64.0), 100.0);
    }

    #[test]
    fn zooming_out_stops_at_the_fit() {
        assert_eq!(bounded_zoom(0.5, 0.1, 0.3, 64.0), 0.3);
        assert_eq!(bounded_zoom(0.3, 0.5, 0.3, 64.0), 0.3);
    }

    /// The control and the backend must appear on the same platforms, so this
    /// reads the platform off `segment` itself rather than restating the cfg.
    #[test]
    fn the_selection_control_is_offered_only_where_segmentation_runs() {
        let missing = std::env::temp_dir().join("lightphotos_selection_support_probe.jpg");
        let _ = std::fs::remove_file(&missing);
        let err = crate::scoring::segmentation::segment(&missing)
            .expect_err("a missing file has no subject mask");
        let has_backend = !err.contains("unsupported on this platform");
        assert_eq!(
            App::selection_supported(),
            has_backend,
            "segment said: {err}"
        );
    }
}
