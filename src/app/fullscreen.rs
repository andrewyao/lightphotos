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

use super::*;

/// The full-screen view, while it is on.
#[derive(Clone, Copy, PartialEq, Debug)]
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

/// How long the slide show shows each photo.
const SLIDE: Duration = Duration::from_secs(5);

/// The slide show's clock. `sel` is the photo the clock times, so a photo
/// the user steps to gets its full time too.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Slideshow {
    sel: Option<usize>,
    next_at: Instant,
    /// The Grid's panels and focus, for a show started from the Grid.
    grid: Option<(Panels, Focus)>,
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
    /// selected, or the first, back to the first after the last.
    pub(super) fn start_slideshow(&mut self) {
        let grid = (self.mode == ViewMode::Grid).then(|| (self.panels(), self.focus_now()));
        if grid.is_some() {
            if self.sel.is_none() && !self.visible.is_empty() {
                self.sel = Some(0);
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
                next_at: Instant::now() + SLIDE,
                grid,
            });
        }
    }

    pub(crate) fn slideshow_playing(&self) -> bool {
        self.fullscreen_shown() && self.fullscreen.is_some_and(|s| s.slideshow.is_some())
    }

    /// Steps the slide show when its photo's time is up, and returns when
    /// to call again.
    pub(crate) fn tick_slideshow(&mut self, now: Instant) -> Option<Instant> {
        if !self.fullscreen_shown() {
            return None;
        }
        let mut show = self.fullscreen?.slideshow?;
        if show.sel == self.sel {
            if now < show.next_at {
                return Some(show.next_at);
            }
            self.step_loupe(true);
        }
        show.sel = self.sel;
        show.next_at = now + SLIDE;
        self.fullscreen.as_mut()?.slideshow = Some(show);
        Some(show.next_at)
    }

    pub(super) fn exit_fullscreen(&mut self, leave: Leave) {
        let Some(shown) = self.fullscreen.take() else {
            return;
        };
        if leave != Leave::Os && matches!(shown.os, Os::Asked { .. }) {
            if let Some(w) = &self.window {
                w.set_fullscreen(None);
            }
        }
        // A show from the Grid goes back to it, unless the user opened
        // something on the Loupe or left it.
        let grid = shown
            .slideshow
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
        let Some(shown) = self.fullscreen else {
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
                self.fullscreen = Some(Shown {
                    os: Os::Asked { granted: true },
                    ..shown
                });
            }
            (Os::Asked { granted: true } | Os::Already, false) => {
                self.exit_fullscreen(Leave::Os);
            }
            _ => {}
        }
    }
}
