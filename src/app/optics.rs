// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Develop panel's Optics section: Remove Chromatic Aberration decodes
//! and measures the photo on screen on its own thread, then stores the
//! scales as an edit.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};

use super::{App, StatusKind};
use crate::develop::{Adjustments, CaScale};
#[cfg(not(target_arch = "wasm32"))]
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
        if let Err(e) = self.start_measure(path.clone()) {
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

    /// Decode and measure `path` off the UI thread, sending the outcome to
    /// `optics.tx`.
    ///
    /// Not the Loupe's cached image: that can be the camera's embedded JPEG,
    /// which the camera has already corrected and may frame differently from
    /// the sensor data the edit applies to.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_measure(&self, path: PathBuf) -> Result<(), String> {
        let tx = self.optics.tx.clone();
        std::thread::Builder::new()
            .name("chroma-measure".to_string())
            .spawn(move || {
                let scale =
                    crate::thumbnail::decode_at_size(&path, MEASURE_PX, EmbeddedPreview::Never)
                        .map(|img| crate::chroma::measure(&img));
                let _ = tx.send((path, scale));
            })
            .map(drop)
            .map_err(|e| e.to_string())
    }

    /// The browser cannot open a file by path off the main thread, so this
    /// reads the bytes here and hands them to the decode pool, whose result
    /// `poll_remove_ca` forwards to `optics.tx`.
    #[cfg(target_arch = "wasm32")]
    fn start_measure(&self, path: PathBuf) -> Result<(), String> {
        let handle = self
            .web
            .file_handles()
            .get(&path)
            .cloned()
            .ok_or("the file is not open")?;
        let pool = self
            .loader
            .as_ref()
            .map(|l| l.web_decoder())
            .ok_or("no decode threads started")?;
        let tx = self.optics.tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match crate::web_fs::read_bytes(&handle).await {
                Ok(bytes) => pool.submit_measure(crate::web_decode::WebMeasureJob {
                    is_raw: crate::decode::image_decode::is_raw_extension(&path),
                    path,
                    bytes: std::sync::Arc::new(bytes),
                    max_px: MEASURE_PX,
                }),
                Err(e) => {
                    let _ = tx.send((path, Err(e)));
                }
            }
        });
        Ok(())
    }

    /// Store a finished measurement, unless the Loupe has moved to another
    /// photo or the box was cleared meanwhile.
    pub(crate) fn poll_remove_ca(&mut self) {
        #[cfg(target_arch = "wasm32")]
        if let Some(loader) = self.loader.as_mut() {
            for outcome in loader.take_web_measures() {
                let _ = self.optics.tx.send(outcome);
            }
        }
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
