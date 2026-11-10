// SPDX-License-Identifier: MIT OR Apache-2.0

//! "Geotag from GPX…" and "Geotag from iCloud…": the App side of
//! `crate::geotag`. A run takes its tracks from a [`GpxSource`], a file the
//! user picked or LightRemote's day files in iCloud, which may first need
//! downloading off the UI thread. A worker thread then reads each photo's
//! capture time and GPS and matches it on the track; each location lands in
//! its photo's sidecar as it arrives, through `save_patch`, so a run that
//! outlives its folder still writes to the right photos. The photo files
//! are never written.
//!
//! Tracks without LightRemote's clock data need the camera's UTC offset.
//! The menu asks for it in a small prompt; a command takes it as a param.
//!
//! Desktop only: the browser has no file picker for a GPX file yet. iCloud
//! is macOS only.

mod icloud;
mod source;

use super::*;

use std::sync::mpsc::{Receiver, TryRecvError};

use chrono::NaiveDate;

use crate::command::CommandError;
use crate::geotag::{GeoSource, MatchOutcome};
use crate::persist::folder_store::Patch;
pub(crate) use icloud::{Day, DayState};
pub(crate) use source::GpxSource;
use source::{Loaded, Note};

/// Every piece of geotagging state the App holds.
pub(crate) struct Geotag {
    job: Option<Job>,
    prompt: Option<Prompt>,
    #[cfg(target_os = "macos")]
    form: Option<ICloudForm>,
    /// LightRemote's iCloud folder. `None` off macOS.
    icloud: Option<icloud::Folder>,
}

impl Geotag {
    pub(crate) fn new() -> Geotag {
        Geotag {
            job: None,
            prompt: None,
            #[cfg(target_os = "macos")]
            form: None,
            icloud: icloud::Folder::default_for_this_mac(),
        }
    }
}

/// Point `--drive` at a fake iCloud folder.
pub(crate) fn override_icloud_dir(dir: PathBuf) {
    icloud::override_dir(dir);
}

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
            MatchOutcome::NoCaptureTime | MatchOutcome::NoClock => &mut self.no_time,
        };
        *n += 1;
    }
}

/// A run. At most one at a time.
enum Job {
    /// Waiting for iCloud to download day files; the run goes on from
    /// `then` when the wait ends.
    Downloading { done: Receiver<()>, then: Pending },
    Matching {
        results: Receiver<(PathBuf, MatchOutcome)>,
        tally: GeotagTally,
        notes: Vec<Note>,
    },
}

/// A run whose tracks are not read yet.
struct Pending {
    /// The photos, as the folder held them when the run began.
    paths: Vec<PathBuf>,
    source: GpxSource,
    offset: Option<i64>,
    /// Whether a missing offset opens the prompt (the menu) or fails (a
    /// command).
    ask: bool,
    /// How iCloud's days stood when the run began.
    before: Vec<Day>,
}

/// A run whose tracks are read.
struct Ready {
    paths: Vec<PathBuf>,
    loaded: Loaded,
}

/// The offset prompt over tracks that have no clock data.
struct Prompt {
    ready: Ready,
    text: String,
}

/// Why a run did not start.
enum StartError {
    /// The tracks have no clock data and no offset was given.
    NeedsOffset(Box<Ready>),
    Failed(String),
}

const NEEDS_OFFSET: &str = "these GPX tracks have no LightRemote clock data; pass \
                            camera_minus_utc_s, the camera clock's UTC offset in seconds";

impl App {
    /// The GPX picker, then the run, or the offset prompt first.
    pub(super) fn geotag_dialog(&mut self) {
        let Some(gpx) = crate::shell::dialog::pick_gpx_file() else {
            return;
        };
        self.geotag_or_ask(GpxSource::File(gpx), None);
    }

    fn geotag_or_ask(&mut self, source: GpxSource, offset: Option<i64>) {
        let started = self.begin_geotag(source, offset, true);
        self.settle_geotag(started, true);
    }

    /// Report a run that didn't start: the offset prompt when `ask`, else
    /// the reason.
    fn settle_geotag(&mut self, started: Result<usize, StartError>, ask: bool) {
        match started {
            Ok(_) => {}
            Err(StartError::NeedsOffset(ready)) if ask => {
                let text = crate::geotag::format_utc_offset(self.offset_guess());
                self.geotag.prompt = Some(Prompt {
                    ready: *ready,
                    text,
                });
            }
            Err(StartError::NeedsOffset(_)) => {
                self.set_status(StatusKind::Error, NEEDS_OFFSET.into());
            }
            Err(StartError::Failed(reason)) => self.set_status(StatusKind::Error, reason),
        }
        self.request_redraw();
    }

    /// `geotag.from_gpx` and `geotag.from_icloud`: start a run over the
    /// open folder. Returns how many photos it covers.
    pub(super) fn start_geotag(
        &mut self,
        source: GpxSource,
        offset: Option<i64>,
    ) -> Result<usize, CommandError> {
        self.begin_geotag(source, offset, false)
            .map_err(|e| CommandError::Failed {
                reason: match e {
                    StartError::NeedsOffset(_) => NEEDS_OFFSET.into(),
                    StartError::Failed(reason) => reason,
                },
            })
    }

    fn begin_geotag(
        &mut self,
        source: GpxSource,
        offset: Option<i64>,
        ask: bool,
    ) -> Result<usize, StartError> {
        let t = crate::i18n::t();
        if self.geotag.job.is_some() {
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
        let total = paths.len();
        if total == 0 {
            return Err(StartError::Failed(CommandError::NoPhotos.to_string()));
        }
        let mut before = Vec::new();
        if let GpxSource::ICloud { first, last } = source {
            let folder = self.icloud_folder(first, last)?;
            before = icloud::scan(&folder.dir, first, last);
            if before.iter().all(|d| d.state == DayState::Missing) {
                return Err(StartError::Failed(no_tracks(first, last)));
            }
            let waiting = before
                .iter()
                .filter(|d| d.state == DayState::InICloud)
                .count();
            if waiting > 0 {
                let (tx, done) = std::sync::mpsc::channel();
                std::thread::Builder::new()
                    .name("icloud-gpx".into())
                    .spawn(move || {
                        icloud::download(&folder, first, last);
                        let _ = tx.send(());
                    })
                    .map_err(|e| StartError::Failed((t.geotag_read_failed)(&e.to_string())))?;
                self.geotag.job = Some(Job::Downloading {
                    done,
                    then: Pending {
                        paths,
                        source,
                        offset,
                        ask,
                        before,
                    },
                });
                self.set_status(StatusKind::Progress, (t.icloud_downloading)(waiting));
                self.request_redraw();
                return Ok(total);
            }
        }
        self.load_and_match(Pending {
            paths,
            source,
            offset,
            ask,
            before,
        })
    }

    /// LightRemote's folder, when this build has one and `first..=last` is
    /// a range a run reads.
    fn icloud_folder(
        &self,
        first: NaiveDate,
        last: NaiveDate,
    ) -> Result<icloud::Folder, StartError> {
        let t = crate::i18n::t();
        let folder = self
            .geotag
            .icloud
            .clone()
            .ok_or_else(|| StartError::Failed(t.icloud_unavailable.into()))?;
        icloud::range(first, last)
            .ok_or_else(|| StartError::Failed(t.icloud_range_invalid.into()))?;
        Ok(folder)
    }

    /// Read the tracks and start matching, or say why not.
    fn load_and_match(&mut self, pending: Pending) -> Result<usize, StartError> {
        let folder = self.geotag.icloud.as_ref().map(|f| f.dir.as_path());
        let resolved = pending.source.resolve(folder, &pending.before);
        let loaded = source::load(resolved).map_err(|reason| {
            StartError::Failed(match &pending.source {
                GpxSource::ICloud { first, last } if reason.is_empty() => no_tracks(*first, *last),
                _ => (crate::i18n::t().geotag_read_failed)(&reason),
            })
        })?;
        let ready = Ready {
            paths: pending.paths,
            loaded,
        };
        self.match_ready(ready, pending.offset)
    }

    fn match_ready(&mut self, ready: Ready, offset: Option<i64>) -> Result<usize, StartError> {
        let Some(clock) = crate::geotag::ClockModel::select(&ready.loaded.track, offset) else {
            return Err(StartError::NeedsOffset(Box::new(ready)));
        };
        let t = crate::i18n::t();
        let Ready {
            paths,
            loaded: Loaded { track, notes },
        } = ready;
        let total = paths.len();
        let (tx, results) = std::sync::mpsc::channel();
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
                            Some(&clock),
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
        self.geotag.job = Some(Job::Matching {
            results,
            tally: GeotagTally::default(),
            notes,
        });
        self.set_status(StatusKind::Progress, t.geotag_running.into());
        self.request_redraw();
        Ok(total)
    }

    /// Store the locations that landed, and go on from a finished download.
    /// Returns true while a run is going, or the iCloud form waits for
    /// capture dates.
    pub(crate) fn pump_geotag(&mut self) -> bool {
        #[cfg(target_os = "macos")]
        let form_waiting = self.pump_icloud_form();
        #[cfg(not(target_os = "macos"))]
        let form_waiting = false;
        match self.geotag.job.as_mut() {
            None => {}
            Some(Job::Downloading { done, .. }) => {
                if !matches!(done.try_recv(), Err(TryRecvError::Empty)) {
                    if let Some(Job::Downloading { then, .. }) = self.geotag.job.take() {
                        let ask = then.ask;
                        let started = self.load_and_match(then);
                        self.settle_geotag(started, ask);
                    }
                }
            }
            Some(Job::Matching { .. }) => self.pump_matches(),
        }
        form_waiting || self.geotag.job.is_some()
    }

    fn pump_matches(&mut self) {
        let Some(Job::Matching { results, tally, .. }) = self.geotag.job.as_mut() else {
            return;
        };
        let mut landed = Vec::new();
        let finished = loop {
            match results.try_recv() {
                Ok(result) => landed.push(result),
                Err(TryRecvError::Empty) => break false,
                Err(TryRecvError::Disconnected) => break true,
            }
        };
        for (_, outcome) in &landed {
            tally.count(outcome);
        }
        for (path, outcome) in landed {
            // A trashed photo's sidecar must not come back for it.
            if let MatchOutcome::Located(tag) = outcome {
                if path.exists() {
                    self.save_patch(&path, Patch::Location(Some(tag)));
                }
            }
        }
        if finished {
            if let Some(Job::Matching {
                tally: n, notes, ..
            }) = self.geotag.job.take()
            {
                let done = (crate::i18n::t().geotag_done)(
                    n.tagged,
                    n.had_gps,
                    n.outside,
                    n.ambiguous,
                    n.no_time,
                );
                let msg = std::iter::once(done)
                    .chain(notes.iter().map(describe))
                    .collect::<Vec<_>>()
                    .join(". ");
                let kind = match n.tagged {
                    0 => StatusKind::Error,
                    _ => StatusKind::Success,
                };
                eprintln!("[lightphotos] geotag: {msg}");
                self.set_status(kind, msg);
            }
            self.request_redraw();
        }
    }

    pub(crate) fn geotag_running(&self) -> bool {
        self.geotag.job.is_some()
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
        let p = self.geotag.prompt.as_ref()?;
        Some((&p.text, crate::geotag::parse_utc_offset(&p.text).is_some()))
    }

    pub(super) fn set_geotag_offset_text(&mut self, text: String) {
        if let Some(p) = self.geotag.prompt.as_mut() {
            p.text = text;
        }
    }

    /// Run with the offset typed. Text that isn't an offset keeps the
    /// prompt open.
    pub(super) fn commit_geotag_prompt(&mut self) {
        let Some(offset) = self
            .geotag
            .prompt
            .as_ref()
            .and_then(|p| crate::geotag::parse_utc_offset(&p.text))
        else {
            return;
        };
        if let Some(p) = self.geotag.prompt.take() {
            let started = self.match_ready(p.ready, Some(offset));
            self.settle_geotag(started, true);
        }
    }

    pub(super) fn cancel_geotag_prompt(&mut self) {
        self.geotag.prompt = None;
        self.request_redraw();
    }

    pub(super) fn geotag_prompt_open(&self) -> bool {
        self.geotag.prompt.is_some()
    }
}

/// How many photos the offset guess reads, so a folder of photos without
/// an offset doesn't stall the prompt.
const OFFSET_GUESS_PHOTOS: usize = 20;

fn no_tracks(first: NaiveDate, last: NaiveDate) -> String {
    (crate::i18n::t().icloud_no_tracks)(&day(first), &day(last))
}

pub(crate) fn day(date: NaiveDate) -> String {
    date.format("%Y-%m-%d").to_string()
}

fn days(dates: &[NaiveDate]) -> String {
    dates.iter().map(|d| day(*d)).collect::<Vec<_>>().join(", ")
}

fn describe(note: &Note) -> String {
    let t = crate::i18n::t();
    match note {
        Note::Missing(dates) => (t.geotag_days_missing)(&days(dates)),
        Note::NotDownloaded(dates) => (t.geotag_days_not_downloaded)(&days(dates)),
        Note::Downloaded(dates) => (t.geotag_days_downloaded)(&days(dates)),
        Note::Skipped { file, reason } => (t.geotag_file_skipped)(file, reason),
    }
}

/// "Geotag from iCloud…": pick the days, see which LightRemote has, run.
#[cfg(target_os = "macos")]
pub(crate) struct ICloudForm {
    from: String,
    to: String,
    /// The picked days as the folder holds them, rescanned on each edit.
    days: Vec<Day>,
    /// The folder's earliest and latest capture dates, being read for the
    /// fields' defaults.
    dates: Option<Receiver<Option<(NaiveDate, NaiveDate)>>>,
}

/// What the iCloud form shows.
#[cfg(target_os = "macos")]
pub(crate) struct ICloudView<'a> {
    pub(crate) from: &'a str,
    pub(crate) to: &'a str,
    /// The picked days, when the range reads.
    pub(crate) days: Option<&'a [Day]>,
    pub(crate) reading_dates: bool,
}

#[cfg(target_os = "macos")]
impl App {
    /// Open the form. Its dates start empty while the folder's capture
    /// dates are read off the UI thread.
    pub(super) fn open_icloud_form(&mut self) {
        let paths: Vec<PathBuf> = self
            .playlist
            .as_ref()
            .map(|pl| pl.entries().to_vec())
            .unwrap_or_default();
        let (tx, rx) = std::sync::mpsc::channel();
        let dates = std::thread::Builder::new()
            .name("capture-dates".into())
            .spawn(move || {
                let _ = tx.send(capture_date_span(&paths));
            })
            .ok()
            .map(|_| rx);
        self.geotag.form = Some(ICloudForm {
            from: String::new(),
            to: String::new(),
            days: Vec::new(),
            dates,
        });
        self.request_redraw();
    }

    pub(crate) fn icloud_form(&self) -> Option<ICloudView<'_>> {
        let f = self.geotag.form.as_ref()?;
        Some(ICloudView {
            from: &f.from,
            to: &f.to,
            days: form_range(f).map(|_| f.days.as_slice()),
            reading_dates: f.dates.is_some(),
        })
    }

    /// The fields as typed. Typing ends the wait for the defaults.
    pub(super) fn set_icloud_dates(&mut self, from: String, to: String) {
        if let Some(f) = self.geotag.form.as_mut() {
            f.from = from;
            f.to = to;
            f.dates = None;
        }
        self.rescan_icloud_form();
    }

    /// Fill the empty fields once the capture dates land. Returns true
    /// while they are still being read.
    fn pump_icloud_form(&mut self) -> bool {
        let Some(f) = self.geotag.form.as_mut() else {
            return false;
        };
        let Some(rx) = f.dates.as_ref() else {
            return false;
        };
        let span = match rx.try_recv() {
            Ok(span) => span,
            Err(TryRecvError::Empty) => return true,
            Err(TryRecvError::Disconnected) => None,
        };
        f.dates = None;
        let today = chrono::Local::now().date_naive();
        let (first, last) = span.unwrap_or((today, today));
        f.from = day(first);
        f.to = day(last);
        self.rescan_icloud_form();
        self.request_redraw();
        false
    }

    fn rescan_icloud_form(&mut self) {
        let dir = self.geotag.icloud.as_ref().map(|f| f.dir.clone());
        let Some(f) = self.geotag.form.as_mut() else {
            return;
        };
        f.days = match (form_range(f), dir) {
            (Some((first, last)), Some(dir)) => icloud::scan(&dir, first, last)
                .into_iter()
                .filter(|d| (first..=last).contains(&d.date))
                .collect(),
            _ => Vec::new(),
        };
    }

    /// Run over the picked days. Dates that don't read keep the form open.
    pub(super) fn commit_icloud_form(&mut self) {
        let Some((first, last)) = self.geotag.form.as_ref().and_then(form_range) else {
            return;
        };
        self.geotag.form = None;
        self.geotag_or_ask(GpxSource::ICloud { first, last }, None);
    }

    pub(super) fn cancel_icloud_form(&mut self) {
        self.geotag.form = None;
        self.request_redraw();
    }

    pub(super) fn icloud_form_open(&self) -> bool {
        self.geotag.form.is_some()
    }
}

#[cfg(target_os = "macos")]
fn form_range(f: &ICloudForm) -> Option<(NaiveDate, NaiveDate)> {
    let date = |s: &str| NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok();
    icloud::range(date(&f.from)?, date(&f.to)?)
}

/// The earliest and latest camera wall-clock dates among `paths`.
#[cfg(target_os = "macos")]
fn capture_date_span(paths: &[PathBuf]) -> Option<(NaiveDate, NaiveDate)> {
    paths
        .iter()
        .filter_map(|p| {
            let stamp = image_decode::capture_stamp(p)?;
            NaiveDate::parse_from_str(stamp.local.get(..10)?, "%Y:%m:%d").ok()
        })
        .fold(None, |span, d| match span {
            None => Some((d, d)),
            Some((a, b)) => Some((a.min(d), b.max(d))),
        })
}

#[cfg(test)]
mod tests;
