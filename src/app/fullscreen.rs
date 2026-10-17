//! The Loupe's full-screen view: the window goes full screen and only the
//! photo is drawn, as Lightroom Classic's Full Screen Preview (F). Entering
//! puts the Loupe's tools and the group Compare away; before/after Compare
//! (Y) stays and shows both photos full screen. Opening a panel or a tool
//! ends it, whichever key, menu or click did so, because
//! `sync_fullscreen` compares the Loupe's chrome against the snapshot taken
//! on entry.

use super::*;

/// Whether the full-screen view is on.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Fullscreen {
    Off,
    On(Shown),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct Shown {
    chrome: LoupeChrome,
    os: Os,
    /// The focus before entering, given back on leaving.
    focus: (Region, FocusLevel),
}

/// How the view relates to the window's OS full screen.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Os {
    /// No window, or a hidden one: a `--drive` run, which must not take
    /// over the desktop.
    Untouched,
    /// Entering asked the OS for full screen, which may take a moment or be
    /// refused. The view shows only the photo either way. `granted` once the
    /// window reports it, so leaving takes the window back out.
    Asked { granted: bool },
    /// The window was already full screen through the green button, and
    /// stays so when the view ends.
    Already,
}

/// What the Loupe shows around the photo. A change from the entry snapshot
/// means the user opened something the full-screen view would hide.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct LoupeChrome {
    rail: Option<RailItem>,
    tool: LoupeTool,
    cropping: bool,
}

impl App {
    /// Whether the Loupe shows only the photo.
    pub(crate) fn fullscreen_shown(&self) -> bool {
        self.fullscreen != Fullscreen::Off && self.mode == ViewMode::Loupe
    }

    /// F, and the button after the Loupe title's signal bars.
    pub(super) fn toggle_fullscreen(&mut self) {
        if self.fullscreen_shown() {
            self.exit_fullscreen();
            return;
        }
        if self.mode != ViewMode::Loupe {
            return;
        }
        self.clear_loupe_for_fullscreen();
        let os = match &self.window {
            Some(w) if w.is_visible() == Some(false) => Os::Untouched,
            Some(w) if w.fullscreen().is_some() => Os::Already,
            Some(w) => {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
                Os::Asked { granted: false }
            }
            None => Os::Untouched,
        };
        self.fullscreen = Fullscreen::On(Shown {
            chrome: self.loupe_chrome(),
            os,
            focus: (self.focus, self.focus_level),
        });
        self.normalize_focus();
        self.request_redraw();
    }

    /// Puts away what would draw over the photo: a crop is kept, as leaving
    /// the Crop page keeps it, the brush and pickers are put down, and the
    /// group Compare closes.
    fn clear_loupe_for_fullscreen(&mut self) {
        self.commit_crop();
        self.tool = LoupeTool::None;
        self.touchup_selected = None;
        if self.group_view() == GroupView::Compare {
            self.set_group_view(GroupView::Edit);
        }
    }

    pub(super) fn exit_fullscreen(&mut self) {
        let Fullscreen::On(shown) = self.fullscreen else {
            return;
        };
        self.fullscreen = Fullscreen::Off;
        if matches!(shown.os, Os::Asked { .. }) {
            if let Some(w) = &self.window {
                w.set_fullscreen(None);
            }
        }
        let (region, level) = shown.focus;
        if self.region_available(region) {
            self.focus = region;
            self.focus_level = level;
            self.on_focus_changed();
        }
        self.normalize_focus();
        self.request_redraw();
    }

    fn loupe_chrome(&self) -> LoupeChrome {
        LoupeChrome {
            rail: self.rail_lit(),
            tool: self.tool,
            cropping: self.cropping(),
        }
    }

    /// Ends the view when the Loupe was left or something was opened on it,
    /// and follows the window in and out of OS full screen. Leaving through
    /// the OS (the green button, or the browser's own Esc, which never
    /// reaches the page) ends the view too.
    pub(crate) fn sync_fullscreen(&mut self) {
        let Fullscreen::On(mut shown) = self.fullscreen else {
            return;
        };
        if self.mode != ViewMode::Loupe || self.loupe_chrome() != shown.chrome {
            self.exit_fullscreen();
            return;
        }
        let window_full = self
            .window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_some());
        match (shown.os, window_full) {
            (Os::Asked { granted: false }, true) => {
                shown.os = Os::Asked { granted: true };
                self.fullscreen = Fullscreen::On(shown);
            }
            (Os::Asked { granted: true } | Os::Already, false) => {
                // The window is out already, so don't ask it out again.
                shown.os = Os::Already;
                self.fullscreen = Fullscreen::On(shown);
                self.exit_fullscreen();
            }
            _ => {}
        }
    }
}
