// SPDX-License-Identifier: MIT OR Apache-2.0

//! "Geotag from GPX…": the App side of `crate::geotag`. A worker thread reads
//! each photo's capture time and GPS and matches it on the track; each
//! location lands in its photo's sidecar as it arrives, through `save_patch`,
//! so a run that outlives its folder still writes to the right photos. The
//! photo files are never written.
//!
//! A GPX file without LightRemote's clock data needs the camera's UTC offset.
//! The menu asks for it in a small prompt; a command takes it as a param.
//!
//! Desktop only: the browser has no file picker for a GPX file yet.

use super::*;

use crate::command::CommandError;
use crate::geotag::{GeoSource, MatchOutcome};
use crate::persist::folder_store::Patch;

/// How many photos each outcome had, for the closing toast.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GeotagTally {
    pub(crate) tagged: usize,
    pub(crate) had_gps: usize,
    pub(crate) outside: usize,
    pub(crate) ambiguous: usize,
    pub(crate) no_time: usize,
}

impl GeotagTally {
    fn count(&mut self, outcome: &MatchOutcome) {
        let n = match outcome {
            MatchOutcome::Located(_) => &mut self.tagged,
            MatchOutcome::AlreadyHasGps => &mut self.had_gps,
            MatchOutcome::OutsideTrack => &mut self.outside,
            MatchOutcome::Ambiguous => &mut self.ambiguous,
            MatchOutcome::NoCaptureTime => &mut self.no_time,
        };
        *n += 1;
    }
}

/// A running match. At most one at a time.
pub(crate) struct GeotagJob {
    results: std::sync::mpsc::Receiver<(PathBuf, MatchOutcome)>,
    tally: GeotagTally,
}

/// The offset prompt over a picked GPX file that has no clock data.
pub(crate) struct GeotagPrompt {
    gpx: PathBuf,
    text: String,
}

/// Why a run did not start.
enum StartError {
    /// The GPX file has no clock data and no offset was given.
    NeedsOffset,
    Failed(String),
}

impl App {
    /// The GPX picker, then the run, or the offset prompt first.
    pub(super) fn geotag_dialog(&mut self) {
        let Some(gpx) = crate::shell::dialog::pick_gpx_file() else {
            return;
        };
        self.geotag_or_ask(gpx, None);
    }

    fn geotag_or_ask(&mut self, gpx: PathBuf, offset: Option<i64>) {
        match self.begin_geotag(&gpx, offset) {
            Ok(_) => {}
            Err(StartError::NeedsOffset) => {
                let text = crate::geotag::format_utc_offset(self.offset_guess());
                self.geotag_prompt = Some(GeotagPrompt { gpx, text });
            }
            Err(StartError::Failed(reason)) => self.set_status(StatusKind::Error, reason),
        }
        self.request_redraw();
    }

    /// `geotag.from_gpx`: start a run over the open folder. Returns how
    /// many photos it covers.
    pub(super) fn start_geotag(
        &mut self,
        gpx: &Path,
        offset: Option<i64>,
    ) -> Result<usize, CommandError> {
        self.begin_geotag(gpx, offset).map_err(|e| match e {
            StartError::NeedsOffset => CommandError::Failed {
                reason: "this GPX file has no LightRemote clock data; pass \
                         camera_minus_utc_s, the camera clock's UTC offset in seconds"
                    .into(),
            },
            StartError::Failed(reason) => CommandError::Failed { reason },
        })
    }

    fn begin_geotag(&mut self, gpx: &Path, offset: Option<i64>) -> Result<usize, StartError> {
        let t = crate::i18n::t();
        if self.geotag_job.is_some() {
            return Err(StartError::Failed(t.geotag_busy.into()));
        }
        if let Some(reason) = self.folder_read_only_text() {
            return Err(StartError::Failed(reason));
        }
        let paths: Vec<PathBuf> = self
            .playlist
            .as_ref()
            .map(|pl| pl.entries().to_vec())
            .unwrap_or_default();
        if paths.is_empty() {
            return Err(StartError::Failed(CommandError::NoPhotos.to_string()));
        }
        let track = read_track(gpx).map_err(|e| StartError::Failed((t.geotag_read_failed)(&e)))?;
        let clock =
            crate::geotag::ClockModel::select(&track, offset).ok_or(StartError::NeedsOffset)?;

        let (tx, results) = std::sync::mpsc::channel();
        let total = paths.len();
        let spawned = std::thread::Builder::new()
            .name("geotag".into())
            .spawn(move || {
                for path in paths {
                    // A decoder that panics on one hostile file costs that
                    // photo its location, not the run.
                    let outcome = std::panic::catch_unwind(|| {
                        let has_gps = image_decode::read_metadata(&path).gps.is_some();
                        let stamp = image_decode::capture_stamp(&path);
                        crate::geotag::locate(
                            &track,
                            &clock,
                            has_gps,
                            stamp.as_ref().map(|s| s.local.as_str()),
                        )
                    })
                    .unwrap_or(MatchOutcome::NoCaptureTime);
                    if tx.send((path, outcome)).is_err() {
                        return;
                    }
                }
            });
        if let Err(e) = spawned {
            return Err(StartError::Failed((t.geotag_read_failed)(&e.to_string())));
        }
        self.geotag_job = Some(GeotagJob {
            results,
            tally: GeotagTally::default(),
        });
        self.set_status(StatusKind::Progress, t.geotag_running.into());
        self.request_redraw();
        Ok(total)
    }

    /// Store the locations that landed. Returns true while a run is going.
    pub(crate) fn pump_geotag(&mut self) -> bool {
        let Some(job) = self.geotag_job.as_mut() else {
            return false;
        };
        let mut landed = Vec::new();
        let finished = loop {
            match job.results.try_recv() {
                Ok(result) => landed.push(result),
                Err(std::sync::mpsc::TryRecvError::Empty) => break false,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break true,
            }
        };
        for (path, outcome) in landed {
            if let Some(job) = self.geotag_job.as_mut() {
                job.tally.count(&outcome);
            }
            // A trashed photo's sidecar must not come back for it.
            if let MatchOutcome::Located(tag) = outcome {
                if path.exists() {
                    self.save_patch(&path, Patch::Location(Some(tag)));
                }
            }
        }
        if finished {
            if let Some(job) = self.geotag_job.take() {
                let n = job.tally;
                let msg = (crate::i18n::t().geotag_done)(
                    n.tagged,
                    n.had_gps,
                    n.outside,
                    n.ambiguous,
                    n.no_time,
                );
                let kind = match n.tagged {
                    0 => StatusKind::Error,
                    _ => StatusKind::Success,
                };
                self.set_status(kind, msg);
            }
            self.request_redraw();
        }
        self.geotag_job.is_some()
    }

    pub(crate) fn geotag_running(&self) -> bool {
        self.geotag_job.is_some()
    }

    /// `geotag.clear`: forget every GPX location in the open folder.
    /// Returns how many photos had one.
    pub(super) fn clear_geotags(&mut self) -> usize {
        let paths: Vec<PathBuf> = self
            .playlist
            .as_ref()
            .map_or(&[][..], |pl| pl.entries())
            .iter()
            .filter(|p| {
                self.folder_store
                    .location(p)
                    .is_some_and(|tag| matches!(tag.source, GeoSource::Gpx { .. }))
            })
            .cloned()
            .collect();
        for path in &paths {
            self.save_patch(path, Patch::Location(None));
        }
        let msg = (crate::i18n::t().gpx_locations_cleared)(paths.len());
        self.set_status(StatusKind::Success, msg);
        self.request_redraw();
        paths.len()
    }

    /// The camera's likely UTC offset: the first photo's recorded
    /// `OffsetTimeOriginal`, else this computer's zone now.
    fn offset_guess(&self) -> i64 {
        let recorded = self
            .playlist
            .as_ref()
            .map_or(&[][..], |pl| pl.entries())
            .iter()
            .take(OFFSET_GUESS_PHOTOS)
            .find_map(|p| {
                let stamp = image_decode::capture_stamp(p)?;
                crate::geotag::parse_utc_offset(stamp.offset.as_deref()?)
            });
        recorded.unwrap_or_else(|| i64::from(chrono::Local::now().offset().local_minus_utc()))
    }

    /// The prompt's text and whether it reads as an offset.
    pub(crate) fn geotag_prompt(&self) -> Option<(&str, bool)> {
        let p = self.geotag_prompt.as_ref()?;
        Some((&p.text, crate::geotag::parse_utc_offset(&p.text).is_some()))
    }

    pub(super) fn set_geotag_offset_text(&mut self, text: String) {
        if let Some(p) = self.geotag_prompt.as_mut() {
            p.text = text;
        }
    }

    /// Run with the offset typed. Text that isn't an offset keeps the
    /// prompt open.
    pub(super) fn commit_geotag_prompt(&mut self) {
        let Some(offset) = self
            .geotag_prompt
            .as_ref()
            .and_then(|p| crate::geotag::parse_utc_offset(&p.text))
        else {
            return;
        };
        if let Some(p) = self.geotag_prompt.take() {
            self.geotag_or_ask(p.gpx, Some(offset));
        }
    }

    pub(super) fn cancel_geotag_prompt(&mut self) {
        self.geotag_prompt = None;
        self.request_redraw();
    }

    pub(super) fn geotag_prompt_open(&self) -> bool {
        self.geotag_prompt.is_some()
    }
}

/// How many photos the offset guess reads, so a folder of photos without
/// an offset doesn't stall the prompt.
const OFFSET_GUESS_PHOTOS: usize = 20;

/// The GPX file at `path`, refused unread past `MAX_GPX_BYTES`.
fn read_track(path: &Path) -> Result<crate::geotag::Track, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(crate::geotag::MAX_GPX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    crate::geotag::Track::parse(&name, &bytes).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests;
