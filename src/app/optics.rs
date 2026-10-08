// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Develop panel's Optics section: Remove Chromatic Aberration decodes
//! and measures the photo on screen on its own thread, then stores the
//! scales as an edit.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use super::{App, StatusKind};
use crate::develop::{Adjustments, CaScale};
use crate::thumbnail::EmbeddedPreview;

/// The size the measurement decodes at. `chroma`'s search window is sized
/// for it: at full resolution the corner fringes run past `MAX_SHIFT`.
const MEASURE_PX: u32 = 2560;

type Outcome = (PathBuf, Result<CaScale, String>);

pub(super) struct Optics {
    /// The photo being measured, so the checkbox shows on while it runs.
    pending: Option<PathBuf>,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
}

impl Optics {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            pending: None,
            tx,
            rx,
        }
    }
}

impl App {
    /// The Remove Chromatic Aberration checkbox: on once measured, and while
    /// the photo on screen is being measured.
    pub(crate) fn remove_ca_on(&self) -> bool {
        self.current_adjustments().chromatic_aberration.is_some()
            || (self.optics.pending.is_some()
                && self.optics.pending.as_deref() == self.shown.path())
    }

    pub(crate) fn remove_ca_pending(&self) -> bool {
        self.optics.pending.is_some()
    }

    pub(super) fn set_remove_ca(&mut self, on: bool) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if !on {
            if self.optics.pending.as_ref() == Some(&path) {
                self.optics.pending = None;
            }
            let off = Adjustments {
                chromatic_aberration: None,
                ..self.current_adjustments()
            };
            self.apply_adjustments_kind(off, "remove_ca");
            return;
        }
        if self.remove_ca_on() {
            return;
        }
        let tx = self.optics.tx.clone();
        let thread_path = path.clone();
        // Not the Loupe's cached image: that can be the camera's embedded
        // JPEG, which the camera has already corrected and may frame
        // differently from the sensor data the edit applies to.
        let spawned = std::thread::Builder::new()
            .name("chroma-measure".to_string())
            .spawn(move || {
                let scale = crate::thumbnail::decode_at_size(
                    &thread_path,
                    MEASURE_PX,
                    EmbeddedPreview::Never,
                )
                .map(|img| crate::chroma::measure(&img));
                let _ = tx.send((thread_path, scale));
            });
        if let Err(e) = spawned {
            self.set_status(
                StatusKind::Error,
                format!("{}: {e}", crate::i18n::t().remove_ca_failed),
            );
            return;
        }
        self.optics.pending = Some(path);
        self.set_status(
            StatusKind::Progress,
            crate::i18n::t().remove_ca_measuring.into(),
        );
    }

    /// Store a finished measurement, unless the Loupe has moved to another
    /// photo or the box was cleared meanwhile.
    pub(crate) fn poll_remove_ca(&mut self) {
        while let Ok((path, scale)) = self.optics.rx.try_recv() {
            if self.optics.pending.as_ref() != Some(&path) {
                continue;
            }
            self.optics.pending = None;
            if self.shown.path() != Some(path.as_path()) {
                continue;
            }
            let scale = match scale {
                Ok(scale) => scale,
                Err(e) => {
                    self.set_status(
                        StatusKind::Error,
                        format!("{}: {e}", crate::i18n::t().remove_ca_failed),
                    );
                    self.request_redraw();
                    continue;
                }
            };
            let fixed = Adjustments {
                chromatic_aberration: Some(scale),
                ..self.current_adjustments()
            };
            self.apply_adjustments_kind(fixed, "remove_ca");
            self.set_status(
                StatusKind::Success,
                crate::i18n::t().remove_ca_applied.into(),
            );
            self.request_redraw();
        }
    }
}
