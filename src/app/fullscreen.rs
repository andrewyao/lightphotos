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
//! overflows, left to right when wider than the screen and top to bottom
//! when taller.

use std::time::Duration;

use super::slideshow::Deck;
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

/// The opacity of the photo fading out, `elapsed` into the fade; `None`
/// once it is gone. Smoothstep, so the fade starts and ends gently.
fn fade_opacity(elapsed: Duration) -> Option<f32> {
    let t = elapsed.as_secs_f32() / FADE.as_secs_f32();
    (t < 1.0).then_some(1.0 - t * t * (3.0 - 2.0 * t))
}

/// The zoom and pan, as `App::zoom` and `App::pan`, that show the `crop`
/// (left, top, right, bottom, 0 to 1 of the photo as shown) of a photo of
/// `image` size in `area`. When `fill`, the crop fills the area, `t` of the
/// way (0 to 1) from showing its left or top edge to its right or bottom
/// one; otherwise it fits, centered. `None` for an empty photo, crop or
/// area.
fn slide_view(
    image: (f32, f32),
    crop: [f32; 4],
    area: (f32, f32),
    fill: bool,
    t: f32,
) -> Option<(f32, (f32, f32))> {
    let ((iw, ih), (aw, ah)) = (image, area);
    let [l, top, r, b] = crop.map(|c| c.clamp(0.0, 1.0));
    let (cw, ch) = ((r - l) * iw, (b - top) * ih);
    // Also false for NaN, which a hostile sidecar's crop could give.
    if !(cw > 0.0 && ch > 0.0 && aw > 0.0 && ah > 0.0) {
        return None;
    }
    let (zoom, along) = if fill {
        ((aw / cw).max(ah / ch), ease_in_out(t))
    } else {
        ((aw / cw).min(ah / ch), 0.5)
    };
    // One axis fits exactly. Filling, the other overflows and pans;
    // fitting, it leaves room, split evenly.
    let x = -l * iw * zoom + (aw - cw * zoom) * along;
    let y = -top * ih * zoom + (ah - ch * zoom) * along;
    Some((zoom, (x, y)))
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
    // Invert uv = R·(d − 0.5) + 0.5. R is a rotation, so R⁻¹ = Rᵀ.
    let shown = |u: f32, v: f32| {
        let (du, dv) = (u - 0.5, v - 0.5);
        (
            rot[0] * du + rot[2] * dv + 0.5,
            rot[1] * du + rot[3] * dv + 0.5,
        )
    };
    let (a, b) = (shown(crop.left, crop.top), shown(crop.right, crop.bottom));
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
            shown.slideshow = Some(Slideshow {
                sel,
                next_at: Instant::now() + settings.slide(),
                grid,
                fade: None,
                deck,
                progress: 0.0,
                held: false,
            });
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
        slide_view(
            self.display_size(),
            crop,
            self.loupe_area(),
            fill,
            show.progress,
        )
    }

    /// The photo a random show takes next, to load ahead.
    pub(super) fn slideshow_next(&self) -> Option<usize> {
        self.slideshow()?
            .deck
            .as_ref()?
            .peek(self.sel, self.visible.len())
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
