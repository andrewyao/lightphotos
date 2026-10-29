//! The Loupe's full-screen view: the window goes full screen and only the
//! photo is drawn, as Lightroom Classic's Full Screen Preview (F). Entering
//! hides the side panel and the export form, puts the Loupe's tools and the
//! group Compare away, and leaving brings the panel back; before/after
//! Compare (Y) stays and shows both photos full screen. With the panels
//! hidden, any key, menu or click that opens one changes the chrome, and
//! `sync_fullscreen` ends the view when the chrome leaves the snapshot taken
//! on entry.
//!
//! A slide show is the same view with a timer that steps the Loupe through
//! the Grid's photos, as Lightroom's Impromptu Slideshow. It ends with the
//! view, and one started from the Grid goes back to it. It shows only each
//! photo's crop. By default the crop fills the screen and pans across what
//! overflows, in a random direction for each photo: left to right or right
//! to left when wider than the screen, top to bottom or bottom to top when
//! taller. A photo with faces starts at the edge farthest from the box
//! around them instead, and moves and zooms in on it as one smooth motion,
//! ending on the box.

use std::time::Duration;

use super::slideshow::{clock_seed, xorshift, Deck};
use super::*;
use crate::develop::Crop;

/// The full-screen view, while it is on.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct Shown {
    chrome: LoupeChrome,
    os: Os,
    /// The panels hidden on entering, given back on leaving.
    panels: Panels,
    /// The focus before entering, given back on leaving if its region is
    /// still on screen.
    focus: Focus,
    slideshow: Option<Slideshow>,
}

/// How long one photo takes to fade into the next.
const FADE: Duration = Duration::from_millis(800);

/// How often to look again while a photo's faces are being found.
const FACE_WAIT: Duration = Duration::from_millis(50);

/// The opacity of the photo fading out, `elapsed` into the fade; `None`
/// once it is gone. Smoothstep, so the fade starts and ends gently.
fn fade_opacity(elapsed: Duration) -> Option<f32> {
    let t = elapsed.as_secs_f32() / FADE.as_secs_f32();
    (t < 1.0).then_some(1.0 - t * t * (3.0 - 2.0 * t))
}

/// The most the view zooms in on faces, as a multiple of the zoom that
/// fills the screen.
const FACE_ZOOM: f32 = 1.5;

/// How much of the screen's tighter dimension the faces' box fills when the
/// view is as far in as it goes.
const BOX_FILL: f32 = 1.0;

/// The zoom and pan, as `App::zoom` and `App::pan`, that show the `crop`
/// (left, top, right, bottom, 0 to 1 of the photo as shown) of a photo of
/// `image` size in `area`. When `fill`, the crop fills the area, `t` of the
/// way (0 to 1) through its time; otherwise it fits, centered. A photo
/// without a `focus`, the box around its faces in the same units as `crop`,
/// pans from its left or top edge to its right or bottom one, or the other
/// way when `reverse`. With a `focus`, the view starts at the edge farthest
/// from it and moves and zooms in on it as one motion.
/// `None` for an empty photo, crop or area.
fn slide_view(
    image: (f32, f32),
    crop: [f32; 4],
    area: (f32, f32),
    fill: bool,
    t: f32,
    focus: Option<[f32; 4]>,
    reverse: bool,
) -> Option<(f32, (f32, f32))> {
    let ((iw, ih), (aw, ah)) = (image, area);
    let [l, top, r, b] = crop.map(|c| c.clamp(0.0, 1.0));
    let (cw, ch) = ((r - l) * iw, (b - top) * ih);
    // Also false for NaN, which a hostile sidecar's crop could give.
    if !(cw > 0.0 && ch > 0.0 && aw > 0.0 && ah > 0.0) {
        return None;
    }
    let faced = focus
        .filter(|_| fill)
        .and_then(|focus| face_view(image, [l, top, r, b], area, t, focus));
    if faced.is_some() {
        return faced;
    }
    let (zoom, along) = if fill {
        let along = ease_in_out(t);
        (
            (aw / cw).max(ah / ch),
            if reverse { 1.0 - along } else { along },
        )
    } else {
        ((aw / cw).min(ah / ch), 0.5)
    };
    // One axis fits exactly. Filling, the other overflows and pans;
    // fitting, it leaves room, split evenly.
    let x = -l * iw * zoom + (aw - cw * zoom) * along;
    let y = -top * ih * zoom + (ah - ch * zoom) * along;
    Some((zoom, (x, y)))
}

/// `slide_view` for a photo with faces, `t` through its time. The view
/// fills the screen with the crop and starts at the edge farthest from the
/// faces' box `focus`. It ends centered on the box, zoomed in to 1.5 times
/// the fill zoom or until the box fills the screen, whichever is less. One
/// easing carries the view from one to the other, so the pan and the zoom
/// are a single motion. Each edge of the view moves in a straight line, so
/// the box comes into view once and stays in it. `None` when no part of the
/// box is in the crop, or for numbers that are not finite.
fn face_view(
    (iw, ih): (f32, f32),
    crop: [f32; 4],
    (aw, ah): (f32, f32),
    t: f32,
    focus: [f32; 4],
) -> Option<(f32, (f32, f32))> {
    if !focus.iter().all(|v| v.is_finite()) {
        return None;
    }
    // Everything below is in the photo's pixels as shown.
    let (cx, cy) = ((crop[0] * iw, crop[2] * iw), (crop[1] * ih, crop[3] * ih));
    let bx = ((focus[0] * iw).max(cx.0), (focus[2] * iw).min(cx.1));
    let by = ((focus[1] * ih).max(cy.0), (focus[3] * ih).min(cy.1));
    // False for NaN too.
    if !(bx.1 > bx.0 && by.1 > by.0) {
        return None;
    }
    let (bw, bh) = (bx.1 - bx.0, by.1 - by.0);
    let z0 = (aw / (cx.1 - cx.0)).max(ah / (cy.1 - cy.0));
    let zoom_in = (FACE_ZOOM * z0).min(BOX_FILL * (aw / bw).min(ah / bh));
    let z1 = zoom_in.max(z0);
    let start = (pan_start(cx, bx, aw / z0), pan_start(cy, by, ah / z0));
    // The box's center, or as near as the crop lets the view get.
    let end = (
        keep_in_view((bx.0 + bx.1) / 2.0, cx, bx, aw / z1),
        keep_in_view((by.0 + by.1) / 2.0, cy, by, ah / z1),
    );
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    // Eased on the square root of the time, so it still starts gently but
    // reaches the box earlier and spends more of the photo's time on it.
    let e = ease_in_out(t.sqrt());
    let lerp = |a: f32, b: f32| a + (b - a) * e;
    // The view's size, 1 / zoom, is what moves in a straight line.
    let zoom = 1.0 / lerp(1.0 / z0, 1.0 / z1);
    let (x, y) = (lerp(start.0, end.0), lerp(start.1, end.1));
    let pan = (aw / 2.0 - x * zoom, ah / 2.0 - y * zoom);
    (zoom.is_finite() && pan.0.is_finite() && pan.1.is_finite()).then_some((zoom, pan))
}

/// On one axis, the center of a view `view` long that starts a face photo:
/// at the end of the crop farthest from the face. `crop` and `face` are
/// (start, end) ranges.
fn pan_start(crop: (f32, f32), face: (f32, f32), view: f32) -> f32 {
    let (lo, hi) = (crop.0 + view / 2.0, crop.1 - view / 2.0);
    if lo > hi {
        return (crop.0 + crop.1) / 2.0;
    }
    if (face.0 + face.1) / 2.0 >= (lo + hi) / 2.0 {
        lo
    } else {
        hi
    }
}

/// `center` of a view `view` long, moved the least that keeps the view in
/// `crop` and holding `face`, or at `face`'s center, in the crop, when the
/// face is longer than the view.
fn keep_in_view(center: f32, crop: (f32, f32), face: (f32, f32), view: f32) -> f32 {
    let (in_lo, in_hi) = (crop.0 + view / 2.0, crop.1 - view / 2.0);
    if in_lo > in_hi {
        return (crop.0 + crop.1) / 2.0;
    }
    let (lo, hi) = (
        (face.1 - view / 2.0).max(in_lo),
        (face.0 + view / 2.0).min(in_hi),
    );
    if lo <= hi {
        center.max(lo).min(hi)
    } else {
        ((face.0 + face.1) / 2.0).max(in_lo).min(in_hi)
    }
}

/// Cubic ease-in-out of `t`, 0 to 1: the pan rests at each end and moves
/// fastest, three times the even pace, halfway.
fn ease_in_out(t: f32) -> f32 {
    let t = if t.is_finite() {
        t.clamp(0.0, 1.0)
    } else {
        0.0
    };
    if t < 0.5 {
        4.0 * t * t * t
    } else {
        1.0 - (2.0 - 2.0 * t).powi(3) / 2.0
    }
}

/// `crop`, which is in texture UV, as the photo shows after `rot`, the
/// display-to-texture rotation `loupe_transform` uses.
fn shown_crop(crop: Crop, rot: [f32; 4]) -> [f32; 4] {
    shown_rect([crop.left, crop.top, crop.right, crop.bottom], rot)
}

/// `rect` (left, top, right, bottom) in texture UV, as the photo shows after
/// `rot`.
fn shown_rect(rect: [f32; 4], rot: [f32; 4]) -> [f32; 4] {
    // Invert uv = R·(d − 0.5) + 0.5. R is a rotation, so R⁻¹ = Rᵀ.
    let shown = |u: f32, v: f32| {
        let (du, dv) = (u - 0.5, v - 0.5);
        (
            rot[0] * du + rot[2] * dv + 0.5,
            rot[1] * du + rot[3] * dv + 0.5,
        )
    };
    let [left, top, right, bottom] = rect;
    let (a, b) = (shown(left, top), shown(right, bottom));
    [a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1)]
}

/// The slide show's clock. `sel` is the photo the clock times, so a photo
/// the user steps to gets its full time too.
#[derive(Clone, PartialEq, Debug)]
struct Slideshow {
    sel: Option<usize>,
    next_at: Instant,
    /// The Grid's panels and focus, for a show started from the Grid.
    grid: Option<(Panels, Focus)>,
    /// The last photo, drawn by the renderer, and when it began to fade.
    fade: Option<(egui::TextureId, Instant)>,
    /// The photos still to show, in a random show.
    deck: Option<Deck>,
    /// How far through its time the photo is, 0 to 1, as of the last tick.
    progress: f32,
    /// A dialog was open at the last tick; the photo gets its time again after.
    held: bool,
    /// Whether a photo without faces pans right to left or bottom to top
    /// instead. Rolled as each photo's time starts.
    reverse: bool,
    /// xorshift64 state for `reverse`.
    rng: u64,
    /// A test's choice of `reverse`, which `roll` keeps.
    #[cfg(test)]
    pin: Option<bool>,
}

impl Slideshow {
    /// Picks which way a photo without faces pans, as its time starts.
    fn roll(&mut self) {
        self.reverse = (xorshift(&mut self.rng) >> 32) & 1 == 1;
        #[cfg(test)]
        if let Some(pin) = self.pin {
            self.reverse = pin;
        }
    }
}

/// How the view relates to the window's OS full screen.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Os {
    /// No window, or a hidden one: a `--drive` run, which must not take
    /// over the desktop.
    Untouched,
    /// Entering asked the OS for full screen, which may take a moment or be
    /// refused. The view shows only the photo either way. `granted` once the
    /// window reports it.
    Asked { granted: bool },
    /// The window was already full screen through the green button, and
    /// stays so when the view ends.
    Already,
}

/// Who ends the view, which decides whether the window is taken out of OS
/// full screen.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Leave {
    /// The app: F, Esc, the title's button, or opening a panel or tool or
    /// leaving the Loupe. The window leaves OS full screen if entering put
    /// it there.
    App,
    /// The OS already took the window out (the green button, or the
    /// browser's own Esc, which never reaches the page).
    Os,
}

#[derive(Clone, Copy, PartialEq, Debug)]
struct Focus {
    region: Region,
    level: FocusLevel,
    main: Region,
    develop: usize,
}

/// What the Loupe shows around the photo. A change from the entry snapshot
/// means the user opened something the full-screen view would hide.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct LoupeChrome {
    rail: Option<RailItem>,
    tool: LoupeTool,
    cropping: bool,
}

/// The side panel and export form, which the view hides.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Panels {
    develop_open: bool,
    side_page: SidePage,
    export_form: bool,
}

impl Panels {
    const HIDDEN: Panels = Panels {
        develop_open: false,
        side_page: SidePage::Develop,
        export_form: false,
    };
}

impl App {
    /// Whether the Loupe shows only the photo.
    pub(crate) fn fullscreen_shown(&self) -> bool {
        self.fullscreen.is_some() && self.mode == ViewMode::Loupe
    }

    /// Whether F can show the view. Crop's draft only saves when the user
    /// says so, and the view would hide it, so Crop keeps the Loupe.
    pub(crate) fn fullscreen_available(&self) -> bool {
        self.mode == ViewMode::Loupe && !self.cropping()
    }

    /// F, and the button after the Loupe title's signal bars.
    pub(super) fn toggle_fullscreen(&mut self) {
        if self.fullscreen_shown() {
            self.exit_fullscreen(Leave::App);
            return;
        }
        if !self.fullscreen_available() {
            return;
        }
        let focus = self.focus_now();
        let panels = self.panels();
        self.save_export_edits();
        self.set_panels(Panels::HIDDEN);
        self.put_down_loupe_tool();
        if self.group_view() == GroupView::Compare {
            self.set_group_view(GroupView::Edit);
        }
        let os = match &self.window {
            Some(w) if w.is_visible() == Some(false) => Os::Untouched,
            Some(w) if w.fullscreen().is_some() => Os::Already,
            Some(w) => {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
                Os::Asked { granted: false }
            }
            None => Os::Untouched,
        };
        self.fullscreen = Some(Shown {
            chrome: self.loupe_chrome(),
            os,
            panels,
            focus,
            slideshow: None,
        });
        self.normalize_focus();
        self.request_redraw();
    }

    /// Shows the view and steps through the Grid's photos from the one
    /// selected, in grid order, back to the first after the last, or at
    /// random. With nothing selected it starts on the first photo, or a
    /// random one.
    pub(super) fn start_slideshow(&mut self) {
        let settings = self.slideshow_settings;
        let mut deck = settings.random.then(Deck::new);
        let grid = (self.mode == ViewMode::Grid).then(|| (self.panels(), self.focus_now()));
        if grid.is_some() {
            if self.sel.is_none() && !self.visible.is_empty() {
                let n = self.visible.len();
                self.sel = Some(deck.as_mut().and_then(|d| d.take(None, n)).unwrap_or(0));
                self.collapse_selection();
            }
            self.enter_loupe();
        }
        if !self.fullscreen_shown() {
            self.toggle_fullscreen();
        }
        let sel = self.sel;
        if let Some(shown) = &mut self.fullscreen {
            let mut show = Slideshow {
                sel,
                next_at: Instant::now() + settings.slide(),
                grid,
                fade: None,
                deck,
                progress: 0.0,
                held: false,
                reverse: false,
                // xorshift never leaves zero.
                rng: clock_seed() | 1,
                #[cfg(test)]
                pin: None,
            };
            show.roll();
            shown.slideshow = Some(show);
        }
        self.push_transform();
        self.request_neighbors();
    }

    pub(crate) fn slideshow_playing(&self) -> bool {
        self.slideshow().is_some()
    }

    fn slideshow(&self) -> Option<&Slideshow> {
        self.fullscreen
            .as_ref()
            .filter(|_| self.fullscreen_shown())?
            .slideshow
            .as_ref()
    }

    /// The zoom and pan that show only the photo's crop: filling the
    /// screen and panned as far as its time has run, or fitted when Zoom to
    /// Fill is off. Before/after Compare keeps the Loupe's own.
    pub(super) fn slide_view(&self) -> Option<(f32, (f32, f32))> {
        let show = self.slideshow()?;
        if self.compare {
            return None;
        }
        let crop = self
            .current_adjustments()
            .crop
            .map_or([0.0, 0.0, 1.0, 1.0], |c| shown_crop(c, self.rot_matrix()));
        let fill = self.slideshow_settings.fill;
        let focus = self.slide_focus();
        slide_view(
            self.display_size(),
            crop,
            self.loupe_area(),
            fill,
            show.progress,
            focus,
            show.reverse,
        )
    }

    /// The photo a random show takes next, to load ahead.
    pub(super) fn slideshow_next(&self) -> Option<usize> {
        self.slideshow()?
            .deck
            .as_ref()?
            .peek(self.sel, self.visible.len())
    }

    /// Makes the show pan photos without faces one way, now and for every
    /// photo after.
    #[cfg(test)]
    pub(super) fn pin_slide_direction(&mut self, reverse: bool) {
        if let Some(show) = self
            .fullscreen
            .as_mut()
            .and_then(|shown| shown.slideshow.as_mut())
        {
            show.pin = Some(reverse);
            show.roll();
        }
    }

    /// The box around the shown photo's faces, in the photo as shown (0 to 1).
    fn slide_focus(&self) -> Option<[f32; 4]> {
        let b = self
            .shown
            .path()
            .and_then(|p| self.face_quality_of(p))
            .and_then(|q| q.bounds)?;
        Some(shown_rect(
            [b.left, b.top, b.right, b.bottom],
            self.rot_matrix(),
        ))
    }

    /// Where the faces' box sits on screen now, (left, top, right, bottom) in
    /// physical pixels from the window's top-left, while a show that fills
    /// the screen has faces for the photo.
    #[cfg(test)]
    pub(super) fn slide_face_box(&self) -> Option<[f32; 4]> {
        if !self.slideshow_settings.fill {
            return None;
        }
        let focus = self.slide_focus()?;
        let (zoom, pan) = self.slide_view()?;
        let (iw, ih) = self.display_size();
        let (ox, oy) = self
            .loupe_viewport
            .map_or((0.0, 0.0), |(x, y, _, _)| (x as f32, y as f32));
        Some([
            ox + pan.0 + focus[0] * iw * zoom,
            oy + pan.1 + focus[1] * ih * zoom,
            ox + pan.0 + focus[2] * iw * zoom,
            oy + pan.1 + focus[3] * ih * zoom,
        ])
    }

    /// The photo the show takes after this one, if it is known yet.
    fn slideshow_next_pos(&self) -> Option<usize> {
        let n = self.visible.len();
        match &self.slideshow()?.deck {
            Some(deck) => deck.peek(self.sel, n),
            None => (n > 1).then(|| self.sel.map_or(0, |s| (s + 1) % n)),
        }
    }

    /// Asks for the face analysis of the photo after this one, so its
    /// faces are known when it shows, and says whether the analysis of the
    /// photo shown is still running. A show that doesn't fill has no use
    /// for faces.
    fn slide_faces_pending(&mut self) -> bool {
        if !self.slideshow_settings.fill || self.slideshow().is_none() {
            return false;
        }
        if let Some(next) = self.slideshow_next_pos().and_then(|p| self.cell_path(p)) {
            self.face_analysis(&next);
        }
        let shown = self.shown.path().map(Path::to_path_buf);
        shown.is_some_and(|path| self.face_analysis(&path).is_none())
    }

    /// Steps the slide show when its photo's time is up, and returns when
    /// to call again.
    pub(crate) fn tick_slideshow(&mut self, now: Instant) -> Option<Instant> {
        if !self.fullscreen_shown() {
            return None;
        }
        let (sel, n) = (self.sel, self.visible.len());
        let slide = self.slideshow_settings.slide();
        let dialog = self.confirm_open() || self.preset_name_edit.is_some();
        let faces_pending = self.slide_faces_pending();
        let show = self.fullscreen.as_mut()?.slideshow.as_mut()?;
        // These act on the photo shown when they are answered.
        if dialog {
            show.held = true;
            return None;
        }
        if show.held {
            show.held = false;
            show.next_at = now + slide;
            show.progress = 0.0;
        }
        let fade_over = show
            .fade
            .is_some_and(|(_, at)| fade_opacity(now.saturating_duration_since(at)).is_none());
        // A photo the user steps to shows at once, for its full time.
        let stepped = show.sel != sel;
        if fade_over || stepped {
            show.fade = None;
            if let Some(r) = &mut self.renderer {
                r.free_snapshot();
            }
        }
        // The photo's pan depends on where its faces are, so its time waits
        // for them.
        if faces_pending {
            show.next_at = now + slide;
            show.progress = 0.0;
            return Some(now + FACE_WAIT);
        }
        if !stepped && now < show.next_at {
            let left = show.next_at.saturating_duration_since(now);
            show.progress = 1.0 - left.as_secs_f32() / slide.as_secs_f32();
            let next_at = show.next_at;
            // A fitted photo stays still.
            if self.slideshow_settings.fill && self.slide_view().is_some() {
                self.push_transform();
            }
            return Some(next_at);
        }
        show.next_at = now + slide;
        show.progress = 0.0;
        show.roll();
        let next = match &mut show.deck {
            _ if stepped => None,
            Some(deck) => deck.take(sel, n),
            None => (n > 1).then(|| sel.map_or(0, |s| (s + 1) % n)),
        };
        if let Some(pos) = next {
            show.fade = self
                .renderer
                .as_mut()
                .and_then(|r| r.snapshot_photo())
                .map(|id| (id, now));
            self.show_in_loupe(pos);
        }
        let show = self.fullscreen.as_mut()?.slideshow.as_mut()?;
        show.sel = self.sel;
        let next_at = show.next_at;
        self.push_transform();
        Some(next_at)
    }

    /// The last photo and its opacity, while it fades into the one shown.
    pub(crate) fn slide_fade(&self) -> Option<(egui::TextureId, f32)> {
        if !self.fullscreen_shown() {
            return None;
        }
        let (id, at) = self.slideshow()?.fade?;
        Some((
            id,
            fade_opacity(Instant::now().saturating_duration_since(at))?,
        ))
    }

    fn free_slide_fade(&mut self) {
        if let Some(r) = &mut self.renderer {
            r.free_snapshot();
        }
    }

    pub(super) fn exit_fullscreen(&mut self, leave: Leave) {
        let Some(shown) = self.fullscreen.take() else {
            return;
        };
        self.free_slide_fade();
        // A show that filled the screen gives the photo back its own zoom.
        self.push_transform();
        if leave != Leave::Os && matches!(shown.os, Os::Asked { .. }) {
            if let Some(w) = &self.window {
                w.set_fullscreen(None);
            }
        }
        // A show from the Grid goes back to it, unless the user opened
        // something on the Loupe or left it.
        let grid = shown
            .slideshow
            .as_ref()
            .and_then(|s| s.grid)
            .filter(|_| self.mode == ViewMode::Loupe && self.loupe_chrome() == shown.chrome);
        self.restore_panels(shown.panels);
        let focus = match grid {
            Some((panels, focus)) => {
                self.enter_grid();
                self.set_panels(panels);
                focus
            }
            None => shown.focus,
        };
        // Set directly: `on_focus_changed` would reset the Develop cursor.
        if self.region_available(focus.region) {
            self.focus = focus.region;
            self.focus_level = focus.level;
            self.main_focus = focus.main;
            self.develop_focus = focus.develop;
        }
        self.normalize_focus();
        self.request_redraw();
    }

    /// Gives back what the view hid, under whatever was opened from it, as
    /// opening that outside the view would have left it. Develop, Info and
    /// Presets take the side panel. The export form and Compare reset the
    /// side page and close each other, but leave Develop open beneath.
    fn restore_panels(&mut self, hidden: Panels) {
        if self.develop_open || self.side_page != SidePage::Develop {
            return;
        }
        self.develop_open = hidden.develop_open;
        if !self.export_form_open() && self.group_view() != GroupView::Compare {
            self.side_page = hidden.side_page;
            self.exports.form_open = hidden.export_form;
        }
    }

    fn focus_now(&self) -> Focus {
        Focus {
            region: self.focus,
            level: self.focus_level,
            main: self.main_focus,
            develop: self.develop_focus,
        }
    }

    fn panels(&self) -> Panels {
        Panels {
            develop_open: self.develop_open,
            side_page: self.side_page,
            export_form: self.export_form_open(),
        }
    }

    fn set_panels(&mut self, panels: Panels) {
        self.develop_open = panels.develop_open;
        self.side_page = panels.side_page;
        self.exports.form_open = panels.export_form;
    }

    fn loupe_chrome(&self) -> LoupeChrome {
        LoupeChrome {
            rail: self.rail_lit(),
            tool: self.tool,
            cropping: self.cropping(),
        }
    }

    /// Ends the view when the Loupe was left or something was opened on it,
    /// and follows the window in and out of OS full screen.
    pub(crate) fn sync_fullscreen(&mut self) {
        let Some(shown) = &self.fullscreen else {
            return;
        };
        if self.mode != ViewMode::Loupe || self.loupe_chrome() != shown.chrome {
            self.exit_fullscreen(Leave::App);
            return;
        }
        let window_full = self
            .window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_some());
        match (shown.os, window_full) {
            (Os::Asked { granted: false }, true) => {
                if let Some(shown) = &mut self.fullscreen {
                    shown.os = Os::Asked { granted: true };
                }
            }
            (Os::Asked { granted: true } | Os::Already, false) => {
                self.exit_fullscreen(Leave::Os);
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WHOLE: [f32; 4] = [0.0, 0.0, 1.0, 1.0];

    /// A photo without faces, panning the usual way.
    fn slide_view(
        image: (f32, f32),
        crop: [f32; 4],
        area: (f32, f32),
        fill: bool,
        t: f32,
    ) -> Option<(f32, (f32, f32))> {
        super::slide_view(image, crop, area, fill, t, None, false)
    }

    #[test]
    fn a_wide_photo_fills_the_height_and_pans_from_its_left_edge_to_its_right() {
        let at = |t| slide_view((3000.0, 1000.0), WHOLE, (1600.0, 900.0), true, t);
        assert_eq!(at(0.0), Some((0.9, (0.0, 0.0))));
        assert_eq!(at(0.5), Some((0.9, (-550.0, 0.0))));
        // The right edge meets the screen's: 2700 px wide less 1100 px.
        assert_eq!(at(1.0), Some((0.9, (-1100.0, 0.0))));
        assert_eq!(at(7.0), at(1.0));
    }

    #[test]
    fn a_tall_photo_fills_the_width_and_pans_from_its_top_edge_to_its_bottom() {
        let at = |t| slide_view((1000.0, 1500.0), WHOLE, (1600.0, 900.0), true, t);
        assert_eq!(at(0.0), Some((1.6, (0.0, 0.0))));
        assert_eq!(at(1.0), Some((1.6, (0.0, -1500.0))));
        assert_eq!(at(f32::NAN), at(0.0));
    }

    #[test]
    fn the_pan_starts_and_ends_gently() {
        let y =
            |t| slide_view((1000.0, 1500.0), WHOLE, (1600.0, 900.0), true, t).map(|(_, p)| -p.1);
        let (first, last) = (y(0.1).unwrap_or(0.0), y(0.9).unwrap_or(0.0));
        // Linear would cover 150 px in each tenth, smoothstep 42.
        assert!(first < 10.0 && 1500.0 - last < 10.0, "{first} {last}");
        let mid = y(0.55).unwrap_or(0.0) - y(0.45).unwrap_or(0.0);
        assert!(mid > 400.0, "three times the even pace halfway: {mid}");
    }

    #[test]
    fn a_photo_the_screens_shape_fills_it_without_panning() {
        assert_eq!(
            slide_view((3200.0, 1800.0), WHOLE, (1600.0, 900.0), true, 0.5),
            Some((0.5, (0.0, 0.0)))
        );
        assert_eq!(
            slide_view((0.0, 1800.0), WHOLE, (1600.0, 900.0), true, 0.5),
            None
        );
        assert_eq!(
            slide_view((3200.0, 1800.0), WHOLE, (1600.0, 0.0), true, 0.5),
            None
        );
    }

    #[test]
    fn a_cropped_photo_fills_the_screen_with_its_crop() {
        // The middle half of a 1600x900 photo, 800x450: twice the zoom,
        // and its top-left corner at the screen's.
        let crop = [0.25, 0.25, 0.75, 0.75];
        let at = |t| slide_view((1600.0, 900.0), crop, (1600.0, 900.0), true, t);
        assert_eq!(at(0.0), Some((2.0, (-800.0, -450.0))));
        assert_eq!(at(1.0), at(0.0), "the crop is the screen's shape");
        let nan = [f32::NAN, 0.0, 1.0, 1.0];
        assert_eq!(
            slide_view((1600.0, 900.0), nan, (1600.0, 900.0), true, 0.0),
            None
        );
        assert_eq!(
            slide_view(
                (1600.0, 900.0),
                [0.5, 0.0, 0.5, 1.0],
                (1600.0, 900.0),
                true,
                0.0
            ),
            None
        );
    }

    #[test]
    fn without_fill_the_crop_fits_centered() {
        // The left half of a 1600x900 photo, 800x900, in a 1600x900 area:
        // full height, with 400 px either side.
        let crop = [0.0, 0.0, 0.5, 1.0];
        let at = |t| slide_view((1600.0, 900.0), crop, (1600.0, 900.0), false, t);
        assert_eq!(at(0.0), Some((1.0, (400.0, 0.0))));
        assert_eq!(at(1.0), at(0.0), "a fitted photo stays still");
        let right = [0.5, 0.0, 1.0, 1.0];
        let view = slide_view((1600.0, 900.0), right, (1600.0, 900.0), false, 0.0);
        assert_eq!(view, Some((1.0, (-400.0, 0.0))));
    }

    #[test]
    fn a_crop_turns_with_the_photo() {
        let crop = Crop {
            left: 0.0,
            top: 0.0,
            right: 0.5,
            bottom: 0.25,
        };
        assert_eq!(
            shown_crop(crop, [1.0, 0.0, 0.0, 1.0]),
            [0.0, 0.0, 0.5, 0.25]
        );
        // A quarter turn clockwise: the texture's top-left lands top-right.
        assert_eq!(
            shown_crop(crop, [0.0, 1.0, -1.0, 0.0]),
            [0.75, 0.0, 1.0, 0.5]
        );
    }

    #[test]
    fn a_photo_without_faces_pans_the_other_way_when_reversed() {
        let at = |t| {
            super::slide_view(
                (3000.0, 1000.0),
                WHOLE,
                (1600.0, 900.0),
                true,
                t,
                None,
                true,
            )
        };
        assert_eq!(at(0.0), Some((0.9, (-1100.0, 0.0))), "the right edge first");
        assert_eq!(at(0.5), Some((0.9, (-550.0, 0.0))));
        assert_eq!(at(1.0), Some((0.9, (0.0, 0.0))));
        let tall = |t| {
            super::slide_view(
                (1000.0, 1500.0),
                WHOLE,
                (1600.0, 900.0),
                true,
                t,
                None,
                true,
            )
        };
        assert_eq!(tall(0.0), Some((1.6, (0.0, -1500.0))), "the bottom first");
        assert_eq!(tall(1.0), Some((1.6, (0.0, 0.0))));
    }

    const AREA: (f32, f32) = (1600.0, 900.0);
    const WIDE: (f32, f32) = (3000.0, 1000.0);

    fn faced(image: (f32, f32), focus: [f32; 4], t: f32) -> (f32, (f32, f32)) {
        super::slide_view(image, WHOLE, AREA, true, t, Some(focus), false).expect("a view")
    }

    /// Where the faces' box lands on the screen, (left, top, right, bottom).
    fn box_on_screen(image: (f32, f32), focus: [f32; 4], t: f32) -> [f32; 4] {
        let (zoom, (x, y)) = faced(image, focus, t);
        [
            x + focus[0] * image.0 * zoom,
            y + focus[1] * image.1 * zoom,
            x + focus[2] * image.0 * zoom,
            y + focus[3] * image.1 * zoom,
        ]
    }

    fn near(a: f32, b: f32) -> bool {
        (a - b).abs() < 0.05
    }

    /// Whether the whole box is on the screen, give or take half a pixel.
    fn box_shows(image: (f32, f32), focus: [f32; 4], t: f32) -> bool {
        let [left, top, right, bottom] = box_on_screen(image, focus, t);
        let slack = 0.5;
        left >= -slack && top >= -slack && right <= AREA.0 + slack && bottom <= AREA.1 + slack
    }

    #[test]
    fn a_face_on_the_right_starts_at_the_left_edge_and_closes_in_on_it() {
        let focus = [0.7, 0.3, 0.8, 0.5];
        let (zoom, (x, y)) = faced(WIDE, focus, 0.0);
        assert!(
            near(zoom, 0.9) && near(x, 0.0) && near(y, 0.0),
            "{zoom} {x} {y}"
        );
        assert!(!box_shows(WIDE, focus, 0.0), "off the right edge");
        assert!(box_shows(WIDE, focus, 0.6), "on screen well before the end");
        // 1.5 times the fill zoom, centered on the box.
        let (zoom, (x, y)) = faced(WIDE, focus, 1.0);
        assert!(near(zoom, 1.35), "{zoom}");
        assert!(near(x, -2237.5) && near(y, -90.0), "{x} {y}");
    }

    #[test]
    fn a_face_on_the_left_starts_at_the_right_edge() {
        let focus = [0.1, 0.3, 0.2, 0.5];
        let (zoom, (x, _)) = faced(WIDE, focus, 0.0);
        assert!(near(zoom, 0.9) && near(x, -1100.0), "{zoom} {x}");
        assert!(!box_shows(WIDE, focus, 0.0), "off the left edge");
        assert!(box_shows(WIDE, focus, 0.6), "on screen well before the end");
    }

    #[test]
    fn once_the_whole_box_shows_it_stays_on_screen_and_the_view_never_zooms_out() {
        let cases = [
            (WIDE, [0.7, 0.3, 0.8, 0.5]),
            (WIDE, [0.1, 0.3, 0.2, 0.5]),
            (WIDE, [0.05, 0.0, 0.3, 0.4]),
            (WIDE, [0.4, 0.1, 0.6, 0.9]),
            (WIDE, [0.9, 0.9, 1.0, 1.0]),
            ((1000.0, 1500.0), [0.4, 0.8, 0.6, 0.95]),
            ((1000.0, 1500.0), [0.0, 0.0, 0.2, 0.1]),
            ((1600.0, 900.0), [0.6, 0.2, 0.7, 0.35]),
        ];
        for (image, focus) in cases {
            let mut last_zoom = 0.0;
            let mut seen = false;
            for step in 0..=200 {
                let t = step as f32 / 200.0;
                let (zoom, (x, y)) = faced(image, focus, t);
                let shows = box_shows(image, focus, t);
                assert!(
                    shows || !seen,
                    "{image:?} {focus:?} t={t}: the box left the screen"
                );
                seen |= shows;
                let slack = 0.5;
                assert!(
                    zoom >= last_zoom - 1e-4,
                    "{image:?} {focus:?} t={t}: zoom fell to {zoom}"
                );
                last_zoom = zoom;
                // The view stays on the photo.
                assert!(
                    x <= slack
                        && y <= slack
                        && x + image.0 * zoom >= AREA.0 - slack
                        && y + image.1 * zoom >= AREA.1 - slack,
                    "{image:?} {focus:?} t={t}: view left the photo at {x} {y}"
                );
            }
            assert!(seen, "{image:?} {focus:?}: the box never showed");
        }
    }

    #[test]
    fn the_view_zooms_in_no_further_than_the_box_filling_the_screen() {
        // 600 x 800 px of a 900 px tall screen: 1.125 times, short of 1.35.
        let focus = [0.4, 0.1, 0.6, 0.9];
        let (zoom, _) = faced(WIDE, focus, 1.0);
        assert!(near(zoom, 1.125), "{zoom}");
        let [_, top, _, bottom] = box_on_screen(WIDE, focus, 1.0);
        assert!(near(top, 0.0) && near(bottom, 900.0), "{top} {bottom}");
        // Wider than the screen already: it only pans.
        let huge = [0.1, 0.0, 0.9, 1.0];
        for t in [0.0, 0.3, 0.6, 1.0] {
            assert!(near(faced(WIDE, huge, t).0, 0.9), "t={t}");
        }
    }

    #[test]
    fn the_pan_and_the_zoom_are_one_motion_that_never_comes_to_rest_midway() {
        let focus = [0.7, 0.3, 0.8, 0.5];
        // How far the view moves in a hundredth of the photo's time, in
        // pixels of pan plus a thousand times the change in zoom.
        let speed = |t: f32| {
            let (z0, (x0, y0)) = faced(WIDE, focus, t);
            let (z1, (x1, y1)) = faced(WIDE, focus, t + 0.01);
            (x1 - x0).hypot(y1 - y0) + (z1 - z0).abs() * 1000.0
        };
        let steps: Vec<f32> = (0..99).map(|i| speed(i as f32 / 100.0)).collect();
        let peak = steps.iter().copied().fold(0.0, f32::max);
        // Two eased movements, one after the other, would stop between.
        for (i, s) in steps.iter().enumerate().take(80).skip(10) {
            assert!(*s > 0.03 * peak, "t={}: speed {s} of a peak {peak}", i);
        }
        assert!(steps[0] < 0.1 * peak, "it starts gently");
    }

    #[test]
    fn a_photo_the_screens_shape_zooms_in_on_its_face_inside_the_photo() {
        let image = (1600.0, 900.0);
        let focus = [0.6, 0.2, 0.7, 0.35];
        let (zoom, pan) = faced(image, focus, 0.0);
        assert!(near(zoom, 1.0) && near(pan.0, 0.0) && near(pan.1, 0.0));
        // The face is near the top, so the view stops at the photo's top.
        let (zoom, (x, y)) = faced(image, focus, 1.0);
        assert!(
            near(zoom, 1.5) && near(x, -760.0) && near(y, 0.0),
            "{zoom} {x} {y}"
        );
    }

    #[test]
    fn a_face_the_crop_cuts_out_or_a_bad_box_pans_like_a_photo_without_faces() {
        let crop = [0.0, 0.0, 0.5, 1.0];
        let plain = |t| slide_view(WIDE, crop, AREA, true, t);
        let with = |focus, t| super::slide_view(WIDE, crop, AREA, true, t, Some(focus), false);
        for t in [0.0, 0.4, 1.0] {
            assert_eq!(with([0.7, 0.3, 0.8, 0.5], t), plain(t), "outside the crop");
            assert_eq!(with([f32::NAN, 0.3, 0.8, 0.5], t), plain(t), "NaN");
            assert_eq!(with([0.8, 0.3, 0.7, 0.5], t), plain(t), "inside out");
        }
        // Without Zoom to Fill the photo just fits.
        let fit = super::slide_view(
            WIDE,
            WHOLE,
            AREA,
            false,
            0.5,
            Some([0.7, 0.3, 0.8, 0.5]),
            false,
        );
        assert_eq!(fit, slide_view(WIDE, WHOLE, AREA, false, 0.5));
    }

    #[test]
    fn a_cropped_photos_faces_are_found_in_its_crop() {
        // The right half of the wide photo: 1500 x 1000 px, which the 1600
        // px wide screen fills at 1.067 times, with no room to pan.
        let crop = [0.5, 0.0, 1.0, 1.0];
        let focus = [0.7, 0.3, 0.8, 0.5];
        let at =
            |t| super::slide_view(WIDE, crop, AREA, true, t, Some(focus), false).expect("a view");
        let (zoom, (x, _)) = at(0.0);
        // The crop starts at 1500 px, on the screen's left edge.
        assert!(near(zoom, 1.0667) && near(x, -1500.0 * zoom), "{zoom} {x}");
        assert!(at(1.0).0 > zoom, "zoomed in on the face");
    }

    #[test]
    fn the_direction_of_a_pan_is_a_coin_flip_each_photo() {
        let mut show = Slideshow {
            sel: None,
            next_at: Instant::now(),
            grid: None,
            fade: None,
            deck: None,
            progress: 0.0,
            held: false,
            reverse: false,
            rng: 42 | 1,
            pin: None,
        };
        let flips: Vec<bool> = (0..64)
            .map(|_| {
                show.roll();
                show.reverse
            })
            .collect();
        let reversed = flips.iter().filter(|&&r| r).count();
        assert!((16..=48).contains(&reversed), "{reversed} of 64 reversed");
        assert!(flips.windows(2).any(|w| w[0] != w[1]), "it changes");
        show.pin = Some(true);
        show.roll();
        assert!(show.reverse, "a test can pin it");
    }

    #[test]
    fn the_last_photo_fades_out_gently_and_is_gone_after_the_fade() {
        let at = |ms| fade_opacity(Duration::from_millis(ms));
        assert_eq!(at(0), Some(1.0));
        assert_eq!(at(400), Some(0.5));
        let (early, late) = (at(80).unwrap_or(0.0), at(720).unwrap_or(1.0));
        assert!(1.0 - early < 0.05 && late < 0.05, "{early} {late}");
        assert_eq!(at(800), None);
        assert_eq!(at(60_000), None);
    }
}
