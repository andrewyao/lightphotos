//! The Loupe's full-screen view: the window goes full screen and only the
//! photo is drawn, as Lightroom Classic's Full Screen Preview (F). Opening a
//! panel, a tool or Compare ends it, whichever key, menu or click did so,
//! because `sync_fullscreen` compares the Loupe's chrome against the
//! snapshot taken on entry.

use super::*;

/// Whether the full-screen view is on. `os` is whether entering it put the
/// window into OS full screen, so leaving it takes the window back out only
/// then: a window already full screen through the green button stays so.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) enum Fullscreen {
    Off,
    /// Asked the OS for full screen, which may take a moment or be refused.
    /// The view shows only the photo either way.
    Entering {
        chrome: LoupeChrome,
        os: bool,
    },
    On {
        chrome: LoupeChrome,
        os: bool,
    },
}

/// What the Loupe shows around the photo. A change from the entry snapshot
/// means the user opened something the full-screen view would hide.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(super) struct LoupeChrome {
    rail: Option<RailItem>,
    compare: bool,
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
        // A hidden window is a `--drive` run, which must not take over the
        // desktop.
        let os = self
            .window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_none() && w.is_visible() != Some(false));
        if os {
            if let Some(w) = &self.window {
                w.set_fullscreen(Some(winit::window::Fullscreen::Borderless(None)));
            }
        }
        let chrome = self.loupe_chrome();
        self.fullscreen = if os {
            Fullscreen::Entering { chrome, os }
        } else {
            Fullscreen::On { chrome, os }
        };
        self.normalize_focus();
        self.request_redraw();
    }

    pub(super) fn exit_fullscreen(&mut self) {
        let os = match self.fullscreen {
            Fullscreen::Off => return,
            Fullscreen::Entering { os, .. } | Fullscreen::On { os, .. } => os,
        };
        self.fullscreen = Fullscreen::Off;
        if os {
            if let Some(w) = &self.window {
                w.set_fullscreen(None);
            }
        }
        self.request_redraw();
    }

    fn loupe_chrome(&self) -> LoupeChrome {
        LoupeChrome {
            rail: self.rail_lit(),
            compare: self.compare,
            tool: self.tool,
            cropping: self.cropping(),
        }
    }

    /// Ends the view when the Loupe was left or something was opened on it,
    /// and follows the window in and out of OS full screen. Leaving through
    /// the OS (the green button, or the browser's own Esc, which never
    /// reaches the page) ends the view too.
    pub(crate) fn sync_fullscreen(&mut self) {
        let (chrome, os, entering) = match self.fullscreen {
            Fullscreen::Off => return,
            Fullscreen::Entering { chrome, os } => (chrome, os, true),
            Fullscreen::On { chrome, os } => (chrome, os, false),
        };
        if self.mode != ViewMode::Loupe || self.loupe_chrome() != chrome {
            self.exit_fullscreen();
            return;
        }
        if !os {
            return;
        }
        let window_full = self
            .window
            .as_ref()
            .is_some_and(|w| w.fullscreen().is_some());
        if entering && window_full {
            self.fullscreen = Fullscreen::On { chrome, os };
        } else if !entering && !window_full {
            self.fullscreen = Fullscreen::Off;
            self.request_redraw();
        }
    }
}
