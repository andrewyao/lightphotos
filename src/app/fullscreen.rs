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
//! view, and one started from the Grid goes back to it.

use std::time::Duration;

use super::slideshow::Deck;
use super::*;

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
            });
        }
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
        let show = self.fullscreen.as_mut()?.slideshow.as_mut()?;
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
            return Some(show.next_at);
        }
        show.next_at = now + slide;
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
        Some(show.next_at)
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
