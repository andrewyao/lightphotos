use super::*;

use crate::develop::{Crop, Straighten, STRAIGHTEN_RANGE};

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
    /// The orientation of a box `w` by `h` as shown; square counts as wide.
    fn of(w: f32, h: f32) -> Self {
        if w >= h {
            CropOrientation::Horizontal
        } else {
            CropOrientation::Vertical
        }
    }

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

/// The furthest rect on the way from `from` to `to` that `turn` covers, so a
/// drag stops at the turned photo's edge. `from` must be covered. Every step
/// of the way keeps the ratio the two share.
fn furthest_covered(from: Crop, to: Crop, turn: &Straighten) -> Crop {
    let lerp = |t: f32| Crop {
        left: from.left + (to.left - from.left) * t,
        top: from.top + (to.top - from.top) * t,
        right: from.right + (to.right - from.right) * t,
        bottom: from.bottom + (to.bottom - from.bottom) * t,
    };
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if turn.covers(lerp(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    lerp(lo)
}

/// `r` if `turn` covers it. Otherwise `r` shrunk about its center until it
/// fits, or the largest box of its shape when its center is off the photo.
/// `w` and `h` are the photo's size in pixels.
fn fit_inside(r: Crop, turn: &Straighten, w: f32, h: f32) -> Crop {
    if turn.covers(r) {
        return r;
    }
    let (cu, cv) = ((r.left + r.right) / 2.0, (r.top + r.bottom) / 2.0);
    let center = Crop {
        left: cu,
        top: cv,
        right: cu,
        bottom: cv,
    };
    if turn.covers(center) {
        furthest_covered(center, r, turn)
    } else {
        turn.largest_crop((r.right - r.left) * w / ((r.bottom - r.top) * h))
    }
}

/// The turn, in degrees, that brings a line running `dx, dy` pixels to the
/// nearest of level or upright.
fn level_angle(dx: f32, dy: f32) -> f32 {
    let a = dy.atan2(dx).to_degrees();
    a - 90.0 * (a / 90.0).round()
}

#[derive(Default)]
pub(super) struct CropState {
    edit: Option<CropDraft>,
    /// Kept across crops, like Lightroom's overlay choice.
    overlay: CropOverlay,
}

impl App {
    /// Whether Crop & Transform is editing a draft.
    pub(super) fn cropping(&self) -> bool {
        self.crop.edit.is_some()
    }

    /// Drops the draft unsaved and goes back to the Develop page it came from.
    pub(super) fn abandon_crop_draft(&mut self) {
        if let Some(draft) = self.crop.edit.take() {
            self.develop_tab = draft.return_tab;
        }
    }

    /// Turns the draft's aspect with the photo after a 90° rotate. The texture
    /// rect stays on the same content, which now lies the other way on screen.
    pub(super) fn flip_crop_orientation(&mut self) {
        if let Some(d) = self.crop.edit.as_mut() {
            d.orientation = d.orientation.flipped();
        }
    }

    /// The crop rectangle currently being edited, if crop mode is active.
    pub(crate) fn crop_rect(&self) -> Option<Crop> {
        self.crop.edit.as_ref().map(|d| d.rect)
    }

    pub(crate) fn crop_aspect(&self) -> Option<CropAspect> {
        self.crop.edit.as_ref().map(|d| d.aspect)
    }

    pub(crate) fn crop_overlay(&self) -> CropOverlay {
        self.crop.overlay
    }

    pub(super) fn set_crop_overlay(&mut self, overlay: CropOverlay) {
        self.crop.overlay = overlay;
        self.request_redraw();
    }

    pub(crate) fn crop_orientation(&self) -> Option<CropOrientation> {
        self.crop.edit.as_ref().map(|d| d.orientation)
    }

    /// The draft's size in source pixels, width by height as shown on screen.
    pub(crate) fn crop_pixel_size(&self) -> Option<(u32, u32)> {
        let (w, h) = self.crop_display_px(self.crop_rect()?);
        Some((w.round() as u32, h.round() as u32))
    }

    /// The draft's straighten angle in degrees, if crop mode is active.
    pub(crate) fn crop_straighten(&self) -> Option<f32> {
        self.crop.edit.as_ref().map(|d| d.straighten)
    }

    pub(crate) fn straighten_tool(&self) -> StraightenTool {
        self.crop
            .edit
            .as_ref()
            .map_or(StraightenTool::Off, |d| d.straighten_tool)
    }

    /// The angle and box Enter would apply: the draft turned so the drawn
    /// line runs level or upright, and the draft's box shrunk about its
    /// center as far as the turned photo needs. `None` until a line is long
    /// enough to read.
    pub(crate) fn straighten_preview(&self) -> Option<(f32, Crop)> {
        let d = self.crop.edit.as_ref()?;
        let StraightenTool::Line { from, to } = d.straighten_tool else {
            return None;
        };
        let (w, h) = self.image_size();
        let (dx, dy) = ((to.0 - from.0) * w, (to.1 - from.1) * h);
        if dx.hypot(dy) < 0.01 * w.max(h) {
            return None;
        }
        let angle = (d.straighten + level_angle(dx, dy))
            .clamp(*STRAIGHTEN_RANGE.start(), *STRAIGHTEN_RANGE.end());
        let rect = fit_inside(d.rect, &Straighten::new(angle, w, h), w, h);
        Some((angle, rect))
    }

    /// The preview box's corners in the draft's canvas as shown now, in
    /// order around it. It is slanted by the turn Enter would add.
    pub(crate) fn straighten_outline(&self) -> Option<[(f32, f32); 4]> {
        let (angle, r) = self.straighten_preview()?;
        let (w, h) = self.image_size();
        let next = Straighten::new(angle, w, h);
        let shown = Straighten::new(self.crop_straighten()?, w, h);
        Some(
            [
                (r.left, r.top),
                (r.right, r.top),
                (r.right, r.bottom),
                (r.left, r.bottom),
            ]
            .map(|(u, v)| {
                let (su, sv) = next.to_source(u, v);
                shown.to_canvas(su, sv)
            }),
        )
    }

    pub(super) fn toggle_straighten_tool(&mut self) {
        if let Some(d) = self.crop.edit.as_mut() {
            d.grab = None;
            d.straighten_tool = match d.straighten_tool {
                StraightenTool::Off => StraightenTool::Ready,
                _ => StraightenTool::Off,
            };
        }
        self.request_redraw();
    }

    /// Start a new line at canvas coordinate `(u, v)`.
    pub(super) fn straighten_line_from(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop.edit.as_mut() {
            if d.straighten_tool != StraightenTool::Off {
                d.straighten_tool = StraightenTool::Line {
                    from: (u, v),
                    to: (u, v),
                };
            }
        }
        self.request_redraw();
    }

    pub(super) fn straighten_line_to(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop.edit.as_mut() {
            if let StraightenTool::Line { from, .. } = d.straighten_tool {
                d.straighten_tool = StraightenTool::Line { from, to: (u, v) };
            }
        }
        self.request_redraw();
    }

    /// Turn the draft by the drawn line and take the previewed box, then put
    /// the tool away. With no line, only the tool goes away.
    pub(super) fn apply_straighten(&mut self) {
        match self.straighten_preview() {
            Some((angle, _)) => self.straighten_draft(angle),
            None => {
                if let Some(d) = self.crop.edit.as_mut() {
                    d.straighten_tool = StraightenTool::Off;
                }
                self.request_redraw();
            }
        }
    }

    pub(super) fn reset_straighten(&mut self) {
        self.straighten_draft(0.0);
    }

    /// Set the draft's angle. The box stays where it is, and shrinks about
    /// its center only as far as the turned photo needs.
    fn straighten_draft(&mut self, angle: f32) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop.edit.as_mut() {
            d.straighten = angle;
            d.rect = fit_inside(d.rect, &Straighten::new(angle, w, h), w, h);
            d.straighten_tool = StraightenTool::Off;
            d.grab = None;
        }
        self.push_crop_preview();
    }

    /// Put the draft back to the full, level frame at the photo's own ratio.
    /// The 90-degree rotation stays.
    pub(super) fn reset_crop(&mut self) {
        let (img_w, img_h) = self.image_size();
        let (disp_w, disp_h) = self.crop_display_px(FULL_CROP);
        if let Some(d) = self.crop.edit.as_mut() {
            d.rect = FULL_CROP;
            d.straighten = 0.0;
            d.straighten_tool = StraightenTool::Off;
            d.grab = None;
            d.aspect = CropAspect::infer(disp_w, disp_h, img_w, img_h);
            d.orientation = CropOrientation::of(disp_w, disp_h);
        }
        self.push_crop_preview();
    }

    /// The saved adjustments with the draft's crop and angle in them.
    /// Full-frame crops store as `None`. A turned photo always keeps its box,
    /// since even a near-full one hides corners the turn leaves empty.
    fn draft_adjustments(&self, draft: &CropDraft) -> Adjustments {
        let r = draft.rect;
        let is_full = draft.straighten == 0.0
            && r.left <= MIN_CROP
            && r.top <= MIN_CROP
            && r.right >= 1.0 - MIN_CROP
            && r.bottom >= 1.0 - MIN_CROP;
        let mut adj = self.current_adjustments();
        adj.crop = if is_full { None } else { Some(r) };
        adj.straighten = draft.straighten;
        adj
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
    /// it already has, and the GPU shows the full frame while editing. A
    /// saved box the saved turn leaves corners in is shrunk to fit.
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
        let saved = self.current_adjustments().crop.unwrap_or(FULL_CROP);
        let (img_w, img_h) = self.image_size();
        let (disp_w, disp_h) = self.crop_display_px(saved);
        let straighten = self.current_adjustments().straighten;
        let rect = fit_inside(
            saved,
            &Straighten::new(straighten, img_w, img_h),
            img_w,
            img_h,
        );
        self.crop.edit = Some(CropDraft {
            rect,
            grab: None,
            straighten,
            straighten_tool: StraightenTool::Off,
            grab_aspect: 1.0,
            aspect: CropAspect::infer(disp_w, disp_h, img_w, img_h),
            orientation: CropOrientation::of(disp_w, disp_h),
            return_tab: self.develop_tab,
        });
        self.develop_tab = DevelopTab::Crop;
        self.develop_open = true;
        self.exports.close_form();
        self.push_crop_preview();
        self.fit_for_crop();
        self.request_redraw();
    }

    /// Push the current edits to the GPU with the crop removed and the
    /// draft's angle, so the whole turned frame shows under the overlay.
    fn push_crop_preview(&mut self) {
        let mut adj = self.current_adjustments();
        adj.crop = None;
        if let Some(d) = &self.crop.edit {
            adj.straighten = d.straighten;
        }
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

    /// Save the draft and leave crop mode, applying a line still drawn with
    /// the Straighten tool. Nothing saves before this.
    pub(super) fn commit_crop(&mut self) {
        if self.straighten_preview().is_some() {
            self.apply_straighten();
        }
        let Some(draft) = self.crop.edit.take() else {
            return;
        };
        self.develop_tab = draft.return_tab;
        let adj = self.draft_adjustments(&draft);
        self.apply_adjustments(adj);
        self.request_redraw();
    }

    /// Leave crop mode without saving, putting the saved view back.
    pub(super) fn cancel_crop(&mut self) {
        if let Some(draft) = self.crop.edit.take() {
            self.develop_tab = draft.return_tab;
            self.push_adjustments();
            self.request_redraw();
        }
    }

    pub(super) fn set_crop_aspect(&mut self, aspect: CropAspect) {
        if let Some(d) = self.crop.edit.as_mut() {
            d.aspect = aspect;
        }
        self.snap_crop_to_aspect();
    }

    pub(super) fn set_crop_orientation(&mut self, orientation: CropOrientation) {
        if let Some(d) = self.crop.edit.as_mut() {
            d.orientation = orientation;
        }
        self.snap_crop_to_aspect();
    }

    /// Reshape the draft to its fixed ratio, keeping its center and area.
    /// Custom keeps the rect as it is.
    fn snap_crop_to_aspect(&mut self) {
        let (w, h) = self.image_size();
        let quarter_turned = self.current_rotation() % 2 == 1;
        let Some(d) = self.crop.edit.as_mut() else {
            return;
        };
        if let Some(r) = d.aspect.w_over_h(d.orientation, w, h) {
            let texture_ratio = if quarter_turned { 1.0 / r } else { r };
            d.rect = reshaped(d.rect, texture_ratio, w, h);
            let turn = Straighten::new(d.straighten, w, h);
            if !turn.covers(d.rect) {
                d.rect = turn.largest_crop(texture_ratio);
            }
        }
        self.request_redraw();
    }

    /// Begin resizing by `edge`. Captures the current pixel aspect ratio for
    /// Shift-locked drags.
    pub(super) fn crop_grab(&mut self, edge: CropEdge) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop.edit.as_mut() {
            let cw = (d.rect.right - d.rect.left) * w;
            let ch = (d.rect.bottom - d.rect.top) * h;
            d.grab_aspect = if ch > 0.0 { cw / ch } else { 1.0 };
            d.grab = Some(CropGrab::Edge(edge));
        }
    }

    /// Begin moving the whole crop rectangle from texture coordinate `(u, v)`.
    pub(super) fn crop_grab_move(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop.edit.as_mut() {
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
        let Some(d) = self.crop.edit.as_mut() else {
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
        let turn = Straighten::new(d.straighten, w, h);
        d.rect = if turn.covers(r) {
            r
        } else {
            furthest_covered(d.rect, r, &turn)
        };
        self.request_redraw();
    }

    pub(super) fn crop_release(&mut self) {
        if let Some(d) = self.crop.edit.as_mut() {
            d.grab = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn leaving_crop_mode_restores_the_cropped_view() {
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

        // Leaving without touching a handle stores the same crop it started
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
        app.commit_crop();
        assert!(
            !app.touchup_active(),
            "touch-up must not return after a crop"
        );

        app.toggle_wb_picker();
        assert!(app.wb_picker_active());
        app.enter_crop();
        app.commit_crop();
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
        assert!(app.crop.edit.is_some(), "rotating stays in crop mode");
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
        assert!(app.crop.edit.is_none());
        assert_eq!(app.develop_tab(), DevelopTab::Sliders, "Enter goes back");
        let saved = app.current_adjustments().crop.expect("Enter commits");
        assert_rect(saved, (0.0, 0.125, 1.0, 0.875));

        app.set_develop_tab(DevelopTab::Masks);
        app.set_develop_tab(DevelopTab::Crop);
        assert!(app.crop.edit.is_some(), "the Crop tab enters crop mode");
        app.set_crop_aspect(CropAspect::Square);
        app.set_develop_tab(DevelopTab::Masks);
        assert!(app.crop.edit.is_none());
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
            app.current_adjustments()
                .crop
                .expect("Esc discards the edit"),
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

    /// Draw a line `deg` degrees off level, in source pixels, across the
    /// middle of the 4000x3000 photo.
    fn draw_line(app: &mut App, deg: f32) {
        let dy = 3200.0 * deg.to_radians().tan() / 3000.0;
        app.apply_ui_actions(vec![
            ui::UiAction::StraightenLineFrom(0.1, 0.5),
            ui::UiAction::StraightenLineTo(0.9, 0.5 + dy),
        ]);
    }

    #[test]
    fn enter_turns_the_photo_by_the_drawn_line_and_crops_inside_it() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R4x3);
        app.apply_ui_actions(vec![ui::UiAction::ToggleStraightenTool]);
        assert_eq!(app.straighten_tool(), StraightenTool::Ready);
        draw_line(&mut app, 5.0);

        let (angle, preview) = app.straighten_preview().expect("a drawn line previews");
        assert!((angle - 5.0).abs() < 1e-3, "{angle}");
        // The slanted box sits inside the photo as it shows now.
        let outline = app.straighten_outline().unwrap();
        // `covers` allows 1e-4 of slack at the edge.
        let inside = -1e-3..=1.0 + 1e-3;
        for (u, v) in outline {
            assert!(inside.contains(&u) && inside.contains(&v), "{u},{v}");
        }
        let (a, b) = (outline[0], outline[1]);
        let top = ((b.1 - a.1) * 3000.0)
            .atan2((b.0 - a.0) * 4000.0)
            .to_degrees();
        assert!(
            (top - 5.0).abs() < 1e-2,
            "the box's top edge follows the line: {top}"
        );
        assert_eq!(app.crop_rect().map(|r| r != preview), Some(true));

        // Applying a line turns the draft but saves nothing yet.
        app.apply_straighten();
        assert_eq!(app.straighten_tool(), StraightenTool::Off);
        assert_eq!(app.crop_straighten(), Some(angle));
        let r = app.crop_rect().unwrap();
        assert_eq!(r, preview);
        assert!((pixel_ratio(r) - 4.0 / 3.0).abs() < 1e-3, "{r:?}");
        assert_eq!(
            app.pushed_adj.unwrap().straighten,
            angle,
            "the GPU shows the turn"
        );
        assert_eq!(
            app.pushed_adj.unwrap().crop,
            None,
            "the full frame still shows under the overlay"
        );
        assert_eq!(app.current_adjustments().straighten, 0.0, "not saved yet");

        // A second line adds to the first, and Enter on it saves and leaves.
        app.toggle_straighten_tool();
        draw_line(&mut app, -2.0);
        app.handle_key(KeyCode::Enter);
        assert!(app.crop.edit.is_none(), "Enter saves and goes back");
        assert_eq!(app.develop_tab(), DevelopTab::Sliders);
        let saved = app.current_adjustments();
        assert!(
            (saved.straighten - 3.0).abs() < 1e-3,
            "{}",
            saved.straighten
        );
        assert!(saved.crop.is_some());
    }

    #[test]
    fn reset_returns_the_draft_to_the_full_level_frame() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R16x9);
        app.toggle_straighten_tool();
        draw_line(&mut app, 5.0);
        app.handle_key(KeyCode::Enter);
        app.enter_crop();

        app.apply_ui_actions(vec![ui::UiAction::ResetCrop]);
        assert_eq!(app.crop_straighten(), Some(0.0));
        assert_rect(app.crop_rect().unwrap(), (0.0, 0.0, 1.0, 1.0));
        assert_eq!(app.crop_aspect(), Some(CropAspect::Original));
        assert_eq!(app.pushed_adj.unwrap().straighten, 0.0);
        assert!(app.current_adjustments().crop.is_some(), "not saved yet");
        app.handle_key(KeyCode::Enter);
        let saved = app.current_adjustments();
        assert_eq!((saved.crop, saved.straighten), (None, 0.0));
    }

    #[test]
    fn nothing_saves_until_enter() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::Custom);
        app.crop_grab(CropEdge::Right);
        app.crop_drag_to(0.6, 0.5);
        app.crop_release();
        let dragged = app.crop_rect().unwrap();
        app.toggle_straighten_tool();
        draw_line(&mut app, 2.0);
        app.apply_straighten();
        assert_eq!(app.current_adjustments(), Adjustments::default());

        app.handle_key(KeyCode::Escape);
        assert!(app.crop.edit.is_none(), "Esc leaves crop mode");
        assert_eq!(app.current_adjustments(), Adjustments::default(), "unsaved");
        assert_eq!(app.pushed_adj.unwrap().crop, None);

        app.enter_crop();
        app.set_crop_aspect(CropAspect::Custom);
        app.crop_grab(CropEdge::Right);
        app.crop_drag_to(0.6, 0.5);
        app.crop_release();
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.current_adjustments().crop, Some(dragged));
        assert_eq!(app.pushed_adj.unwrap().crop, Some(dragged));
    }

    #[test]
    fn a_small_turn_keeps_its_box() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::R4x3);
        app.toggle_straighten_tool();
        draw_line(&mut app, 1.0);
        app.apply_straighten();
        let r = app.crop_rect().unwrap();
        assert!(r.left < MIN_CROP, "the box is within MIN_CROP: {r:?}");
        app.commit_crop();
        assert_eq!(
            app.current_adjustments().crop,
            Some(r),
            "a near-full box still hides the corners the turn leaves"
        );
    }

    #[test]
    fn a_saved_turn_without_a_box_gets_one_that_drags() {
        let photo = PathBuf::from(PHOTO);
        let mut app = photo_app(0, None);
        app.edits.insert(
            photo,
            Adjustments {
                straighten: 3.0,
                ..Default::default()
            },
        );
        app.enter_crop();
        let turn = Straighten::new(3.0, 4000.0, 3000.0);
        let r = app.crop_rect().unwrap();
        assert!(turn.covers(r), "{r:?}");

        app.crop_grab(CropEdge::Right);
        app.crop_drag_to(0.7, 0.5);
        assert!(app.crop_rect().unwrap().right < r.right, "the box drags");
    }

    #[test]
    fn leaving_crop_mode_applies_a_drawn_line() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.toggle_straighten_tool();
        draw_line(&mut app, 4.0);
        app.set_develop_tab(DevelopTab::Sliders);
        assert!(app.crop.edit.is_none());
        assert!((app.current_adjustments().straighten - 4.0).abs() < 1e-3);
    }

    #[test]
    fn straightening_keeps_a_placed_box_that_still_fits() {
        let placed = Crop {
            left: 0.4,
            top: 0.4,
            right: 0.6,
            bottom: 0.6,
        };
        let mut app = photo_app(0, Some(placed));
        app.enter_crop();
        app.toggle_straighten_tool();
        draw_line(&mut app, 3.0);
        app.apply_straighten();
        assert_eq!(app.crop_rect(), Some(placed), "a turn it fits leaves it be");
        app.reset_straighten();
        assert_eq!(app.crop_rect(), Some(placed), "so does Reset");

        // A box near the edge shrinks about its own center, not the photo's.
        let edge = Crop {
            left: 0.0,
            top: 0.0,
            right: 0.5,
            bottom: 0.5,
        };
        let mut app = photo_app(0, Some(edge));
        app.enter_crop();
        app.toggle_straighten_tool();
        draw_line(&mut app, 5.0);
        app.apply_straighten();
        let r = app.crop_rect().unwrap();
        assert!(Straighten::new(5.0, 4000.0, 3000.0).covers(r), "{r:?}");
        assert!(((r.left + r.right) / 2.0 - 0.25).abs() < 1e-3, "{r:?}");
        assert!(((r.top + r.bottom) / 2.0 - 0.25).abs() < 1e-3, "{r:?}");
    }

    #[test]
    fn a_steep_line_levels_to_upright() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.toggle_straighten_tool();
        draw_line(&mut app, 88.0);
        let (angle, _) = app.straighten_preview().unwrap();
        assert!((angle + 2.0).abs() < 1e-2, "{angle}");
    }

    #[test]
    fn escape_puts_the_tool_away_and_reset_levels_the_photo() {
        use winit::keyboard::KeyCode;
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.toggle_straighten_tool();
        draw_line(&mut app, 4.0);
        app.handle_key(KeyCode::Escape);
        assert_eq!(app.straighten_tool(), StraightenTool::Off);
        assert!(
            app.crop.edit.is_some(),
            "Esc on the tool stays in crop mode"
        );
        assert_eq!(app.crop_straighten(), Some(0.0), "Esc applies nothing");

        app.toggle_straighten_tool();
        draw_line(&mut app, 4.0);
        app.apply_straighten();
        let turned = app.crop_rect().unwrap();
        app.apply_ui_actions(vec![ui::UiAction::ResetStraighten]);
        assert_eq!(app.crop_straighten(), Some(0.0));
        assert_eq!(app.crop_rect(), Some(turned), "the box stays put");
    }

    #[test]
    fn a_drag_stops_at_the_turned_photos_edge() {
        let mut app = photo_app(0, None);
        app.enter_crop();
        app.set_crop_aspect(CropAspect::Custom);
        app.toggle_straighten_tool();
        draw_line(&mut app, 6.0);
        app.apply_straighten();
        let turn = Straighten::new(app.crop_straighten().unwrap(), 4000.0, 3000.0);

        app.crop_grab(CropEdge::Left);
        app.crop_drag_to(0.0, 0.5);
        let r = app.crop_rect().unwrap();
        assert!(turn.covers(r), "{r:?}");

        let before = app.crop_rect().unwrap();
        app.crop_grab_move(0.5, 0.5);
        app.crop_drag_to(0.0, 0.0);
        let r = app.crop_rect().unwrap();
        assert!(turn.covers(r), "{r:?}");
        let size = |r: Crop| (r.right - r.left, r.bottom - r.top);
        let (w0, h0) = size(before);
        let (w1, h1) = size(r);
        assert!(
            (w0 - w1).abs() < 1e-5 && (h0 - h1).abs() < 1e-5,
            "a move keeps the size"
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
