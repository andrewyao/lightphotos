// SPDX-License-Identifier: MIT OR Apache-2.0

//! The Develop panel's Optics section: Remove Chromatic Aberration decodes
//! and measures the photo on screen on its own thread, then stores the
//! scales as an edit.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};

use super::{App, CaState, StatusKind};
use crate::develop::{Adjustments, CaScale};
#[cfg(not(target_arch = "wasm32"))]
use crate::jobs::thumbnail::EmbeddedPreview;

/// The size the measurement decodes at. `chroma`'s search window is sized
/// for it: at full resolution the corner fringes run past `MAX_SHIFT`.
const MEASURE_PX: u32 = 2560;

type Outcome = (PathBuf, Result<CaScale, String>);

pub(super) struct Optics {
    /// The photo being measured, so its checkbox shows on while it runs.
    pending: Option<PathBuf>,
    /// Photos waiting their turn. One is measured at a time, since each
    /// decode can take a large share of memory.
    queue: VecDeque<PathBuf>,
    /// Photos measured in this run, for the "n of m" status.
    done: usize,
    tx: Sender<Outcome>,
    rx: Receiver<Outcome>,
}

impl Optics {
    pub(super) fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self {
            pending: None,
            queue: VecDeque::new(),
            done: 0,
            tx,
            rx,
        }
    }

    fn waiting(&self, path: &PathBuf) -> bool {
        self.pending.as_ref() == Some(path) || self.queue.contains(path)
    }
}

impl App {
    /// The Remove Chromatic Aberration checkbox over the photos the panel
    /// acts on: on once measured, and while waiting to be measured.
    pub(crate) fn remove_ca_state(&self) -> CaState {
        let paths = self.action_paths();
        let on = paths
            .iter()
            .filter(|p| {
                self.optics.waiting(p)
                    || self
                        .edits
                        .get(*p)
                        .is_some_and(|a| a.chromatic_aberration.is_some())
            })
            .count();
        match on {
            0 => CaState::Off,
            n if n == paths.len() => CaState::On,
            _ => CaState::Mixed,
        }
    }

    pub(crate) fn remove_ca_pending(&self) -> bool {
        self.optics.pending.is_some() || !self.optics.queue.is_empty()
    }

    /// Turn Remove Chromatic Aberration on or off for every photo the panel
    /// acts on. Turning it on measures the photos that lack it, one by one.
    pub(super) fn set_remove_ca(&mut self, on: bool) {
        let paths = self.action_paths();
        if paths.is_empty() {
            return;
        }
        if !on {
            self.optics.queue.retain(|p| !paths.contains(p));
            if self
                .optics
                .pending
                .as_ref()
                .is_some_and(|p| paths.contains(p))
            {
                self.optics.pending = None;
            }
            self.edit_each(&paths, |adj| Adjustments {
                chromatic_aberration: None,
                ..adj
            });
            self.start_next_measure();
            return;
        }
        if !self.remove_ca_pending() {
            self.optics.done = 0;
        }
        for path in paths {
            let has = self
                .edits
                .get(&path)
                .is_some_and(|a| a.chromatic_aberration.is_some());
            if !has && !self.optics.waiting(&path) {
                self.optics.queue.push_back(path);
            }
        }
        self.start_next_measure();
    }

    /// Start measuring the next queued photo unless one is under way. A photo
    /// that cannot start is reported and skipped.
    fn start_next_measure(&mut self) {
        while self.optics.pending.is_none() {
            let Some(path) = self.optics.queue.pop_front() else {
                return;
            };
            match self.start_measure(path.clone()) {
                Ok(()) => {
                    self.optics.pending = Some(path);
                    self.show_measure_progress();
                }
                Err(e) => self.set_status(
                    StatusKind::Error,
                    format!("{}: {e}", crate::i18n::t().remove_ca_failed),
                ),
            }
        }
    }

    fn show_measure_progress(&mut self) {
        let t = crate::i18n::t();
        let total = self.optics.done + 1 + self.optics.queue.len();
        let text = if total > 1 {
            (t.remove_ca_measuring_n)(self.optics.done + 1, total)
        } else {
            t.remove_ca_measuring.into()
        };
        self.set_status(StatusKind::Progress, text);
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
                let scale = crate::jobs::thumbnail::decode_at_size(
                    &path,
                    MEASURE_PX,
                    EmbeddedPreview::Never,
                )
                .map(|img| crate::develop::chroma::measure(&img));
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
            match crate::web::web_fs::read_bytes(&handle).await {
                Ok(bytes) => pool.submit_measure(crate::web::web_decode::WebMeasureJob {
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

    /// Store a finished measurement on its photo and start the next. A
    /// result for a photo whose box was cleared meanwhile is dropped.
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
            self.optics.done += 1;
            match scale {
                Ok(scale) => {
                    self.edit_each(std::slice::from_ref(&path), |adj| Adjustments {
                        chromatic_aberration: Some(scale),
                        ..adj
                    });
                    if self.optics.queue.is_empty() {
                        self.set_status(
                            StatusKind::Success,
                            crate::i18n::t().remove_ca_applied.into(),
                        );
                    }
                }
                Err(e) => self.set_status(
                    StatusKind::Error,
                    format!("{}: {e}", crate::i18n::t().remove_ca_failed),
                ),
            }
            self.start_next_measure();
            self.request_redraw();
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    /// Remove CA measures every photo that lacks it, one at a time, skipping
    /// a photo it cannot decode, and clears them all again.
    #[test]
    fn remove_ca_measures_the_selection_in_turn() {
        let (mut app, dir, paths) = crate::app::test_support::folder_app("ca-many", 3);
        app.selected = (0..3).collect();
        app.sel = Some(0);
        let pixels = vec![128u8; 64 * 48 * 4];
        crate::decode::image_encode::encode_jpeg(
            &paths[0],
            64,
            48,
            &pixels,
            crate::decode::image_encode::JpegQuality::Export,
        )
        .unwrap();
        // paths[2] stays an empty file, which cannot be decoded.
        let scale = crate::develop::CaScale {
            red: 0.001,
            blue: -0.001,
        };
        app.edits.insert(
            paths[1].clone(),
            Adjustments {
                chromatic_aberration: Some(scale),
                ..Default::default()
            },
        );
        assert_eq!(app.remove_ca_state(), CaState::Mixed);

        app.set_remove_ca(true);
        assert_eq!(
            app.remove_ca_state(),
            CaState::On,
            "waiting photos count as on"
        );
        assert_eq!(app.optics.pending.as_ref(), Some(&paths[0]));
        assert_eq!(app.optics.queue, [paths[2].clone()], "one at a time");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while app.remove_ca_pending() {
            assert!(std::time::Instant::now() < deadline, "measuring timed out");
            app.poll_remove_ca();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let ca = |app: &App, i: usize| {
            app.edits
                .get(&paths[i])
                .and_then(|a| a.chromatic_aberration)
        };
        assert!(ca(&app, 0).is_some(), "the JPEG was measured");
        assert_eq!(ca(&app, 1), Some(scale), "a measured photo is left alone");
        assert!(
            ca(&app, 2).is_none(),
            "the empty file failed and was skipped"
        );
        assert_eq!(app.remove_ca_state(), CaState::Mixed);

        app.selected = BTreeSet::from([0, 1]);
        assert_eq!(app.remove_ca_state(), CaState::On);
        app.set_remove_ca(false);
        assert!(ca(&app, 0).is_none() && ca(&app, 1).is_none());
        assert_eq!(app.remove_ca_state(), CaState::Off);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
