// SPDX-License-Identifier: MIT OR Apache-2.0

//! The update cue's side of `App`: polling the check, opening the page.

use super::App;
use crate::update::Version;

impl App {
    /// Take a finished check. Returns true while one is running, so the frame
    /// loop keeps polling.
    pub(crate) fn poll_update_check(&mut self) -> bool {
        let (running, changed) = self.update.poll();
        if changed {
            self.request_redraw();
        }
        running
    }

    /// The version running, if this build shows it.
    pub(crate) fn current_version(&self) -> Option<Version> {
        self.update.current()
    }

    /// A newer version to point at, if one is known.
    pub(crate) fn update_available(&self) -> Option<Version> {
        self.update.available()
    }

    pub(crate) fn update_check_on(&self) -> bool {
        self.update.auto_check()
    }

    pub(super) fn open_downloads(&mut self) {
        self.egui_ctx
            .open_url(egui::OpenUrl::new_tab(crate::update::downloads_url(
                crate::i18n::lang(),
            )));
        self.request_redraw();
    }
}
