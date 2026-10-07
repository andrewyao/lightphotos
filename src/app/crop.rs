use super::*;

use crate::develop::Crop;

/// The crop's shape: the photo's own, free, or a fixed ratio.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CropAspect {
    Original,
    Custom,
    R4x3,
    R16x9,
    Square,
}

/// Which way a fixed ratio's long edge runs on screen.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CropOrientation {
    Horizontal,
    Vertical,
}

/// The composition guides drawn inside the crop box.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum CropOverlay {
    #[default]
    Thirds,
    Grid,
    Golden,
    Diagonal,
    Spiral,
    None,
}

impl CropOrientation {
    pub(super) fn flipped(self) -> Self {
        match self {
            CropOrientation::Horizontal => CropOrientation::Vertical,
            CropOrientation::Vertical => CropOrientation::Horizontal,
        }
    }
}

impl CropAspect {
    /// Long edge over short edge, or `None` for Custom. Original reads the
    /// photo's own pixel size.
    fn long_over_short(self, img_w: f32, img_h: f32) -> Option<f32> {
        match self {
            CropAspect::Original => Some(img_w.max(img_h) / img_w.min(img_h)),
            CropAspect::Custom => None,
            CropAspect::R4x3 => Some(4.0 / 3.0),
            CropAspect::R16x9 => Some(16.0 / 9.0),
            CropAspect::Square => Some(1.0),
        }
    }

    /// Display-space width over height, or `None` for Custom.
    fn w_over_h(self, orientation: CropOrientation, img_w: f32, img_h: f32) -> Option<f32> {
        let r = self.long_over_short(img_w, img_h)?;
        Some(match orientation {
            CropOrientation::Horizontal => r,
            CropOrientation::Vertical => 1.0 / r,
        })
    }

    /// The ratio a `crop_w x crop_h` pixel crop already has, so reopening a
    /// saved 16:9 crop shows 16:9 rather than Custom.
    fn infer(crop_w: f32, crop_h: f32, img_w: f32, img_h: f32) -> Self {
        const TOLERANCE: f32 = 0.005;
        let ratio = crop_w.max(crop_h) / crop_w.min(crop_h);
        [
            CropAspect::Original,
            CropAspect::R4x3,
            CropAspect::R16x9,
            CropAspect::Square,
        ]
        .into_iter()
        .find(|a| {
            a.long_over_short(img_w, img_h)
                .is_some_and(|r| (ratio / r - 1.0).abs() <= TOLERANCE)
        })
        .unwrap_or(CropAspect::Custom)
    }
}

/// A box of pixel ratio `w_over_h` with `r`'s center and area, in a
/// `w x h` pixel frame. It shrinks to fit the frame, then slides back inside
/// it, so an uncropped frame gives the largest centered box.
fn reshaped(r: Crop, w_over_h: f32, w: f32, h: f32) -> Crop {
    let area = (r.right - r.left) * w * (r.bottom - r.top) * h;
    let (bw, bh) = ((area * w_over_h).sqrt(), (area / w_over_h).sqrt());
    let fit = (w / bw).min(h / bh).min(1.0);
    let span = |lo: f32, hi: f32, len: f32| {
        let start = ((lo + hi - len) / 2.0).clamp(0.0, 1.0 - len);
        (start, start + len)
    };
    let (left, right) = span(r.left, r.right, bw * fit / w);
    let (top, bottom) = span(r.top, r.bottom, bh * fit / h);
    Crop {
        left,
        top,
        right,
        bottom,
    }
}

/// Resize the dragged span `lo..hi` (its `lo` end if `moving_lo`) and derive
/// the perpendicular span, `k` times as long, about its center. The dragged
/// end gives way first, so neither span leaves the frame and the ratio holds.
fn locked_spans(
    (lo, hi): (f32, f32),
    moving_lo: bool,
    (plo, phi): (f32, f32),
    k: f32,
) -> ((f32, f32), (f32, f32)) {
    let room = if moving_lo { hi } else { 1.0 - lo };
    let len = (hi - lo).clamp(MIN_CROP / k, 1.0 / k).min(room);
    let span = if moving_lo {
        (hi - len, hi)
    } else {
        (lo, lo + len)
    };
    let plen = len * k;
    let start = ((plo + phi - plen) / 2.0).min(1.0 - plen).max(0.0);
    (span, (start, start + plen))
}

impl App {
    /// The crop rectangle currently being edited, if crop mode is active.
    pub(crate) fn crop_rect(&self) -> Option<Crop> {
        self.crop_edit.as_ref().map(|d| d.rect)
    }

    pub(crate) fn crop_aspect(&self) -> Option<CropAspect> {
        self.crop_edit.as_ref().map(|d| d.aspect)
    }

    pub(crate) fn crop_overlay(&self) -> CropOverlay {
        self.crop_overlay
    }

    pub(super) fn set_crop_overlay(&mut self, overlay: CropOverlay) {
        self.crop_overlay = overlay;
        self.request_redraw();
    }

    pub(crate) fn crop_orientation(&self) -> Option<CropOrientation> {
        self.crop_edit.as_ref().map(|d| d.orientation)
    }

    /// The draft's size in source pixels, width by height as shown on screen.
    pub(crate) fn crop_pixel_size(&self) -> Option<(u32, u32)> {
        let (w, h) = self.crop_display_px(self.crop_rect()?);
        Some((w.round() as u32, h.round() as u32))
    }

    fn crop_display_px(&self, r: Crop) -> (f32, f32) {
        let (w, h) = self.image_size();
        let (cw, ch) = ((r.right - r.left) * w, (r.bottom - r.top) * h);
        if self.current_rotation() % 2 == 1 {
            (ch, cw)
        } else {
            (cw, ch)
        }
    }

    /// Enter crop mode on the Develop panel's Crop tab, opening the loupe
    /// first if needed. The draft starts from the saved crop, with the ratio
    /// it already has, and the GPU shows the full frame while editing.
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
        self.tool = LoupeTool::None;
        let rect = self.current_adjustments().crop.unwrap_or(FULL_CROP);
        let (img_w, img_h) = self.image_size();
        let (disp_w, disp_h) = self.crop_display_px(rect);
        self.crop_edit = Some(CropDraft {
            rect,
            grab: None,
            grab_aspect: 1.0,
            aspect: CropAspect::infer(disp_w, disp_h, img_w, img_h),
            orientation: if disp_w >= disp_h {
                CropOrientation::Horizontal
            } else {
                CropOrientation::Vertical
            },
            return_tab: self.develop_tab,
        });
        self.develop_tab = DevelopTab::Crop;
        self.develop_open = true;
        self.export_form_open = false;
        self.push_crop_preview();
        self.fit_for_crop();
        self.request_redraw();
    }

    /// Push the current edits to the GPU with the crop removed, so the whole
    /// frame is visible under the crop overlay.
    fn push_crop_preview(&mut self) {
        let mut adj = self.current_adjustments();
        adj.crop = None;
        #[cfg(test)]
        {
            self.pushed_adj = Some(adj);
        }
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
        self.develop_tab = draft.return_tab;
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
        if let Some(draft) = self.crop_edit.take() {
            self.develop_tab = draft.return_tab;
            self.push_adjustments();
            self.request_redraw();
        }
    }

    pub(super) fn set_crop_aspect(&mut self, aspect: CropAspect) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.aspect = aspect;
        }
        self.snap_crop_to_aspect();
    }

    pub(super) fn set_crop_orientation(&mut self, orientation: CropOrientation) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.orientation = orientation;
        }
        self.snap_crop_to_aspect();
    }

    /// Reshape the draft to its fixed ratio, keeping its center and area.
    /// Custom keeps the rect as it is.
    fn snap_crop_to_aspect(&mut self) {
        let (w, h) = self.image_size();
        let quarter_turned = self.current_rotation() % 2 == 1;
        let Some(d) = self.crop_edit.as_mut() else {
            return;
        };
        if let Some(r) = d.aspect.w_over_h(d.orientation, w, h) {
            let texture_ratio = if quarter_turned { 1.0 / r } else { r };
            d.rect = reshaped(d.rect, texture_ratio, w, h);
        }
        self.request_redraw();
    }

    /// Begin resizing by `edge`. Captures the current pixel aspect ratio for
    /// Shift-locked drags.
    pub(super) fn crop_grab(&mut self, edge: CropEdge) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop_edit.as_mut() {
            let cw = (d.rect.right - d.rect.left) * w;
            let ch = (d.rect.bottom - d.rect.top) * h;
            d.grab_aspect = if ch > 0.0 { cw / ch } else { 1.0 };
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
    /// the size and stays in the frame. An edge drag under a fixed ratio, or
    /// with Shift under Custom, also moves the perpendicular edges about their
    /// center to keep the ratio.
    pub(super) fn crop_drag_to(&mut self, u: f32, v: f32) {
        let shift = self.modifiers.shift_key();
        let (w, h) = self.image_size();
        let quarter_turned = self.current_rotation() % 2 == 1;
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
                // The texture-space pixel w/h to hold, if any.
                let lock = match d.aspect.w_over_h(d.orientation, w, h) {
                    Some(ratio) if quarter_turned => Some(1.0 / ratio),
                    Some(ratio) => Some(ratio),
                    None => (shift && d.grab_aspect > 0.0).then_some(d.grab_aspect),
                };
                if let Some(a) = lock {
                    match edge {
                        CropEdge::Left | CropEdge::Right => {
                            let ((l, rt), (t, b)) = locked_spans(
                                (r.left, r.right),
                                edge == CropEdge::Left,
                                (r.top, r.bottom),
                                w / (a * h),
                            );
                            (r.left, r.right, r.top, r.bottom) = (l, rt, t, b);
                        }
                        CropEdge::Top | CropEdge::Bottom => {
                            let ((t, b), (l, rt)) = locked_spans(
                                (r.top, r.bottom),
                                edge == CropEdge::Top,
                                (r.left, r.right),
                                a * h / w,
                            );
                            (r.left, r.right, r.top, r.bottom) = (l, rt, t, b);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committing_an_untouched_crop_restores_the_cropped_view() {
        let photo = PathBuf::from("/photos/a.jpg");
        let crop = Crop {
            left: 0.1,
            top: 0.1,
            right: 0.9,
            bottom: 0.9,
        };
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(photo.clone(), 1024, 1024);
        app.edits.insert(
            photo,
            Adjustments {
                crop: Some(crop),
                ..Default::default()
            },
        );

        // Crop mode shows the whole frame under the overlay.
        app.enter_crop();
        assert_eq!(app.pushed_adj.unwrap().crop, None);

        // Committing without touching a handle stores the same crop it started
        // from, so nothing changes -- but the GPU still holds the full frame
        // and has to be put back, or the loupe keeps showing it uncropped.
        app.commit_crop();
        assert_eq!(app.current_adjustments().crop, Some(crop));
        assert_eq!(app.pushed_adj.unwrap().crop, Some(crop));
    }

    #[test]
    fn cropping_turns_off_the_other_loupe_tools() {
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(PathBuf::from("/photos/a.jpg"), 1024, 1024);

        app.apply_ui_actions(vec![ui::UiAction::ToggleTouchUp]);
        assert!(app.touchup_active());
        app.enter_crop();
        app.cancel_crop();
        assert!(
            !app.touchup_active(),
            "touch-up must not return after a crop"
        );

        app.toggle_wb_picker();
        assert!(app.wb_picker_active());
        app.enter_crop();
        app.cancel_crop();
        assert!(
            !app.wb_picker_active(),
            "the picker must not return after a crop"
        );
    }

    const PHOTO: &str = "/photos/wide.jpg";

    /// A 4000x3000 photo in the Loupe, turned `rot` quarter turns, with
    /// `saved` as its committed crop.
    fn photo_app(rot: u8, saved: Option<Crop>) -> App {
        let photo = PathBuf::from(PHOTO);
        let mut app = App::new(None);
        app.mode = ViewMode::Loupe;
        app.shown = Shown::Preview(photo.clone(), 4000, 3000);
        app.source_size = Some((4000, 3000));
        app.loupe_viewport = Some((0, 0, 800, 600));
        if rot != 0 {
            app.rotations.insert(photo.clone(), rot);
        }
        if saved.is_some() {
            app.edits.insert(
                photo,
                Adjustments {
                    crop: saved,
                    ..Default::default()
                },
            );
        }
        app
    }

    fn assert_rect(got: Crop, want: (f32, f32, f32, f32)) {
        let got_t = (got.left, got.top, got.right, got.bottom);
        let close = [
            (got_t.0, want.0),
            (got_t.1, want.1),
            (got_t.2, want.2),
            (got_t.3, want.3),
        ]
        .iter()
        .all(|(a, b)| (a - b).abs() < 1e-4);
        assert!(close, "crop {got_t:?} != {want:?}");
    }

    fn in_frame(r: Crop) -> bool {
        let eps = 1e-5;
        r.left >= -eps && r.top >= -eps && r.right <= 1.0 + eps && r.bottom <= 1.0 + eps
    }

    fn pixel_ratio(r: Crop) -> f32 {
        ((r.right - r.left) * 4000.0) / ((r.bottom - r.top) * 3000.0)
    }

    #[test]
    fn an_uncropped_photo_snaps_to_the_largest_centered_box() {
        let snapped = |aspect, orientation| {
            let mut app = photo_app(0, None);
            app.enter_crop();
            app.set_crop_aspect(aspect);
            app.set_crop_orientation(orientation);
            (app.crop_rect().unwrap(), app.crop_pixel_size().unwrap())
        };
        use CropAspect::*;
        use CropOrientation::*;

        let (r, px) = snapped(R16x9, Horizontal);
        assert_rect(r, (0.0, 0.125, 1.0, 0.875));
        assert_eq!(px, (4000, 2250));

        let (r, px) = snapped(R4x3, Vertical);
        assert_rect(r, (0.21875, 0.0, 0.78125, 1.0));
        assert_eq!(px, (2250, 3000));

        let (r, _) = snapped(Original, Horizontal);
        assert_rect(r, (0.0, 0.0, 1.0, 1.0));

        let (r, px) = snapped(Square, Horizontal);
        assert_rect(r, (0.125, 0.0, 0.875, 1.0));
        assert_eq!(px, (3000, 3000));

        let (r, _) = snapped(Custom, Horizontal);
        assert_rect(r, (0.0, 0.0, 1.0, 1.0));
    }

    #[test]
    fn a_new_ratio_keeps_an_existing_crops_center_and_size() {
        // 1600x1200 px, centered at (1200, 600) px.
        let saved = Crop {
            left: 0.1,
            top: 0.0,
            right: 0.5,
            bottom: 0.4,
        };
        let mut app = photo_app(0, Some(saved));
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R16x9);
        let r = app.crop_rect().unwrap();
        assert!((pixel_ratio(r) - 16.0 / 9.0).abs() < 1e-3, "{r:?}");
        let center = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
        assert!(
            (center.0 - 0.3).abs() < 1e-4 && (center.1 - 0.2).abs() < 1e-4,
            "the center stays put: {center:?}"
        );
        let area = (r.right - r.left) * 4000.0 * (r.bottom - r.top) * 3000.0;
        assert!(
            (area / (1600.0 * 1200.0) - 1.0).abs() < 1e-3,
            "the size stays put: {area}"
        );

        // A crop in the corner turned tall would cross the top edge, so it
        // slides down rather than shrink, and keeps its size.
        app.set_crop_orientation(CropOrientation::Vertical);
        let r = app.crop_rect().unwrap();
        assert!(in_frame(r), "{r:?}");
        assert!((pixel_ratio(r) - 9.0 / 16.0).abs() < 1e-3, "{r:?}");
        let area = (r.right - r.left) * 4000.0 * (r.bottom - r.top) * 3000.0;
        assert!((area / (1600.0 * 1200.0) - 1.0).abs() < 1e-3, "{area}");
        assert!(r.top.abs() < 1e-5, "slid to the top edge: {r:?}");

        // Too big for the frame at the new shape, it shrinks to fit.
        let mut app = photo_app(
            0,
            Some(Crop {
                left: 0.05,
                top: 0.05,
                right: 0.95,
                bottom: 0.95,
            }),
        );
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R16x9);
        app.set_crop_orientation(CropOrientation::Vertical);
        let r = app.crop_rect().unwrap();
        assert!(in_frame(r), "{r:?}");
        assert_eq!(app.crop_pixel_size().map(|(_, h)| h), Some(3000));
    }

    /// The box is fitted on screen and mapped back to texture space, so the
    /// mapping has to be the renderer's own at every rotation.
    #[test]
    fn a_ratio_is_the_shape_on_screen_at_every_rotation() {
        let central = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(800.0, 600.0));
        for rot in 0..4u8 {
            let mut app = photo_app(rot, None);
            app.enter_crop();
            app.set_crop_orientation(CropOrientation::Horizontal);
            app.set_crop_aspect(CropAspect::R16x9);
            let r = app.crop_rect().unwrap();
            let corners = [
                app.loupe_tex_to_screen(central, r.left, r.top),
                app.loupe_tex_to_screen(central, r.right, r.bottom),
            ];
            let w = (corners[0].x - corners[1].x).abs();
            let h = (corners[0].y - corners[1].y).abs();
            assert!(
                (w / h - 16.0 / 9.0).abs() < 1e-3,
                "rotation {rot}: {w}x{h} on screen is not 16:9"
            );

            let frame = [
                app.loupe_tex_to_screen(central, 0.0, 0.0),
                app.loupe_tex_to_screen(central, 1.0, 1.0),
            ];
            let frame_w = (frame[0].x - frame[1].x).abs();
            let mid = |p: [egui::Pos2; 2]| (p[0] + p[1].to_vec2()) / 2.0;
            assert!(
                (w - frame_w).abs() < 0.5,
                "rotation {rot}: the box spans the photo's width on screen"
            );
            assert!(
                (mid(corners) - mid(frame)).length() < 0.5,
                "rotation {rot}: the box is centered on screen"
            );
        }
    }

    #[test]
    fn a_fixed_ratio_holds_through_edge_drags_without_shift() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R4x3);

        app.crop_grab(CropEdge::Left);
        app.crop_drag_to(0.5, 0.5);
        app.crop_release();
        let r = app.crop_rect().unwrap();
        assert!((pixel_ratio(r) - 4.0 / 3.0).abs() < 1e-3, "{r:?}");
        assert_rect(r, (0.5, 0.25, 1.0, 0.75));

        // Growing the height from here would center the width past the right
        // edge, so the box has to slide back in rather than squash.
        app.crop_grab(CropEdge::Top);
        app.crop_drag_to(0.5, 0.0);
        app.crop_release();
        let r = app.crop_rect().unwrap();
        assert!(in_frame(r), "{r:?} left the frame");
        assert!((pixel_ratio(r) - 4.0 / 3.0).abs() < 1e-3, "{r:?}");
        assert_rect(r, (0.25, 0.0, 1.0, 0.75));

        // 16:9 at full width cannot grow taller at all.
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R16x9);
        app.crop_grab(CropEdge::Top);
        app.crop_drag_to(0.5, 0.0);
        let r = app.crop_rect().unwrap();
        assert!(in_frame(r), "{r:?} left the frame");
        assert!((pixel_ratio(r) - 16.0 / 9.0).abs() < 1e-3, "{r:?}");
        assert_eq!(app.crop_pixel_size(), Some((4000, 2250)));
    }

    #[test]
    fn custom_drags_freely_and_shift_keeps_the_grabbed_shape() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::Custom);

        app.crop_grab(CropEdge::Right);
        app.crop_drag_to(0.6, 0.5);
        app.crop_release();
        assert_rect(app.crop_rect().unwrap(), (0.0, 0.0, 0.6, 1.0));

        let before = pixel_ratio(app.crop_rect().unwrap());
        app.modifiers = winit::keyboard::ModifiersState::SHIFT;
        app.crop_grab(CropEdge::Right);
        app.crop_drag_to(0.3, 0.5);
        let r = app.crop_rect().unwrap();
        assert!((pixel_ratio(r) - before).abs() < 1e-3, "{r:?}");
        assert!(in_frame(r));
    }

    #[test]
    fn brackets_rotate_the_crop_and_flip_its_orientation() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R16x9);
        let rect = app.crop_rect().unwrap();

        app.handle_key(KeyCode::BracketRight);
        assert_eq!(app.current_rotation(), 1, "] turns clockwise");
        assert_eq!(
            app.crop_rect(),
            Some(rect),
            "the box stays on the same content"
        );
        assert_eq!(app.crop_orientation(), Some(CropOrientation::Vertical));
        assert_eq!(app.crop_pixel_size(), Some((2250, 4000)));
        assert!(
            (app.zoom_rel - 0.9).abs() < 1e-6,
            "the refit keeps crop mode's margin, so the handles stay on screen"
        );

        app.handle_key(KeyCode::BracketLeft);
        app.handle_key(KeyCode::BracketLeft);
        assert_eq!(app.current_rotation(), 3, "[ turns anti-clockwise");
        assert_eq!(app.crop_rect(), Some(rect));
        assert_eq!(app.crop_orientation(), Some(CropOrientation::Vertical));
        assert!(app.crop_edit.is_some(), "rotating stays in crop mode");
    }

    #[test]
    fn the_crop_tab_is_crop_mode() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.develop_open = false;
        assert_eq!(app.develop_tab(), DevelopTab::Sliders);

        app.handle_key(KeyCode::KeyC);
        assert_eq!(app.develop_tab(), DevelopTab::Crop);
        assert!(app.develop_visible(), "C opens the Develop panel");
        app.set_crop_aspect(CropAspect::R16x9);
        app.handle_key(KeyCode::Enter);
        assert!(app.crop_edit.is_none());
        assert_eq!(app.develop_tab(), DevelopTab::Sliders, "Enter goes back");
        let saved = app.current_adjustments().crop.expect("Enter commits");
        assert_rect(saved, (0.0, 0.125, 1.0, 0.875));

        app.set_develop_tab(DevelopTab::Masks);
        app.set_develop_tab(DevelopTab::Crop);
        assert!(app.crop_edit.is_some(), "the Crop tab enters crop mode");
        app.set_crop_aspect(CropAspect::Square);
        app.set_develop_tab(DevelopTab::Masks);
        assert!(app.crop_edit.is_none());
        assert_eq!(app.develop_tab(), DevelopTab::Masks);
        assert_rect(
            app.current_adjustments()
                .crop
                .expect("leaving the tab commits"),
            (0.125, 0.0, 0.875, 1.0),
        );

        app.handle_key(KeyCode::KeyC);
        app.set_crop_aspect(CropAspect::R16x9);
        app.handle_key(KeyCode::Escape);
        assert_eq!(app.develop_tab(), DevelopTab::Masks, "Esc goes back");
        assert_rect(
            app.current_adjustments().crop.unwrap(),
            (0.125, 0.0, 0.875, 1.0),
        );

        app.handle_key(KeyCode::KeyC);
        app.teardown_loupe_state();
        assert_eq!(
            app.develop_tab(),
            DevelopTab::Masks,
            "navigating away leaves no Crop tab without a crop"
        );
    }

    #[test]
    fn reopening_a_saved_crop_selects_its_ratio() {
        let cases = [
            (
                (0.0, 0.125, 1.0, 0.875),
                CropAspect::R16x9,
                CropOrientation::Horizontal,
            ),
            (
                (0.21875, 0.0, 0.78125, 1.0),
                CropAspect::Original,
                CropOrientation::Vertical,
            ),
            (
                (0.125, 0.0, 0.875, 1.0),
                CropAspect::Square,
                CropOrientation::Horizontal,
            ),
            (
                (0.1, 0.1, 0.9, 0.5),
                CropAspect::Custom,
                CropOrientation::Horizontal,
            ),
        ];
        for ((left, top, right, bottom), aspect, orientation) in cases {
            let saved = Crop {
                left,
                top,
                right,
                bottom,
            };
            let mut app = photo_app(0, Some(saved));
            app.enter_crop();
            assert_eq!(app.crop_aspect(), Some(aspect), "{saved:?}");
            assert_eq!(app.crop_orientation(), Some(orientation), "{saved:?}");
        }
        let mut app = photo_app(0, None);
        app.enter_crop();
        assert_eq!(app.crop_aspect(), Some(CropAspect::Original));
    }
}
