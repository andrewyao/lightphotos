// SPDX-License-Identifier: MIT OR Apache-2.0

//! "Import GPS Tracks…": the App side of `crate::geotag`. The user chooses
//! GPX files, folders of them, or both, and confirms each camera's time
//! zone setting in the import dialog. A worker thread then downloads any
//! file still in iCloud, joins every file into one track, and walks the
//! open folder and its subfolders a batch of photos at a time
//! ([`walk::PhotoWalk`]): it reads each photo's camera, capture time and
//! GPS, matches it on the track, and sends the batch's outcomes. Each
//! location lands in its photo's sidecar through `save_patch`, so a run
//! that outlives its folder still writes to the right photos. The photo
//! files are never written. Clearing the GPX locations walks the same way
//! on the same job slot, so a run and a clear never overlap.
//!
//! Each photo's camera clock comes from LightRemote's measurements when the
//! camera was paired with it and a track has them, else from its camera's
//! time zone setting. The dialog asks both once per camera and the app's
//! prefs remember them. While the dialog is open, a reader walks the
//! photos off the UI thread and sends only the cameras each batch holds,
//! so the rows fill in as it goes; the run reads each photo again itself.
//!
//! Desktop only: the browser has no picker for GPX files yet.

mod cameras;
mod source;
mod walk;

use super::*;

use std::collections::BTreeMap;
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use crate::command::CommandError;
use crate::decode::image_decode::CaptureStamp;
use crate::geotag::{CameraZone, ClockModel, ClockSource, GeoSource, MatchOutcome};
use crate::persist::folder_store::Patch;
use cameras::CameraClocks;
pub(crate) use cameras::{CameraClock, CameraKey};
use source::{Found, Loaded, Note};
use walk::PhotoWalk;

/// How long a run waits for iCloud to download the files it needs.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);

/// A shorter wait, as `--drive` sets inside its 30 s `idle`.
static DOWNLOAD_TIMEOUT_OVERRIDE: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

/// Wait at most `timeout` for iCloud downloads. Set once, before the app
/// starts.
pub(crate) fn override_download_timeout(timeout: Duration) {
    let _ = DOWNLOAD_TIMEOUT_OVERRIDE.set(timeout);
}

/// Every piece of geotagging state the App holds.
pub(crate) struct Geotag {
    job: Option<Job>,
    dialog: Option<Dialog>,
    /// Each camera's remembered clock.
    clocks: CameraClocks,
    download_timeout: Duration,
}

impl Geotag {
    pub(crate) fn new() -> Geotag {
        Geotag {
            job: None,
            dialog: None,
            // A test must never read or write the developer's own settings.
            clocks: if cfg!(test) {
                CameraClocks::default()
            } else {
                CameraClocks::load()
            },
            download_timeout: DOWNLOAD_TIMEOUT_OVERRIDE
                .get()
                .copied()
                .unwrap_or(DOWNLOAD_TIMEOUT),
        }
    }
}

/// How many photos each outcome had, for the closing toast.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct GeotagTally {
    pub(crate) tagged: usize,
    pub(crate) had_gps: usize,
    pub(crate) outside: usize,
    pub(crate) ambiguous: usize,
    pub(crate) no_time: usize,
    pub(crate) no_clock: usize,
}

impl GeotagTally {
    fn count(&mut self, outcome: &MatchOutcome) {
        let n = match outcome {
            MatchOutcome::Located(_) => &mut self.tagged,
            MatchOutcome::AlreadyHasGps => &mut self.had_gps,
            MatchOutcome::OutsideTrack => &mut self.outside,
            MatchOutcome::Ambiguous => &mut self.ambiguous,
            MatchOutcome::NoCaptureTime => &mut self.no_time,
            MatchOutcome::NoClock => &mut self.no_clock,
        };
        *n += 1;
    }
}

/// A run or a clear. At most one at a time, so neither saves under the
/// other.
struct Job {
    events: Receiver<Event>,
    work: Work,
}

/// What a job does, and what it has counted so far.
enum Work {
    Import(GeotagTally),
    /// How many photos lost a GPX location.
    Clear(usize),
}

/// What a job's worker reports.
enum Event {
    /// Waiting for iCloud to download this many files.
    Downloading(usize),
    /// One batch of photos, matched.
    Matched(Vec<(PathBuf, MatchOutcome)>),
    /// One batch of photos that may hold a GPX location, each with whether
    /// its sidecar on disk holds one. A photo of the folder the clear began
    /// in is sent either way: the folder's cache can hold a location not yet
    /// on disk.
    Clear(Vec<(PathBuf, bool)>),
    Done(Report),
    /// No track to match on, or no photo, and why.
    Failed(String),
}

/// What the user should hear when a run ends, past the tally.
#[derive(Default)]
struct Report {
    notes: Vec<Note>,
    /// Each camera LightRemote measured far from its setting, and how far
    /// (measured minus set, seconds).
    disagreements: BTreeMap<CameraKey, i64>,
}

/// The GPX files a run matches on: found while the dialog was open, or
/// still to look for in the files and folders a command named.
enum Tracks {
    Found(Found),
    Search(Vec<PathBuf>),
}

/// Each camera's clock, as a run reads it.
#[derive(Clone, Debug, Default)]
struct Settings {
    clocks: CameraClocks,
    /// One camera-minus-UTC offset for every camera, as a command gives it.
    all: Option<i64>,
}

impl Settings {
    /// The camera's time zone setting, and whether LightRemote measured
    /// it. The dialog remembers every camera it shows, so only a command's
    /// run meets a camera never asked about. That camera counts as paired:
    /// it takes LightRemote's measurements when the tracks have them, as
    /// the dialog's default does for a folder's only camera.
    fn for_camera(&self, camera: &CameraKey) -> (Option<i64>, bool) {
        let clock = self.clocks.get(camera);
        let offset = self
            .all
            .or_else(|| clock.map(|c| c.zone.camera_minus_utc_s()));
        (offset, clock.is_none_or(|c| c.paired))
    }
}

/// What a run needs from one photo, read once.
#[derive(Clone, Debug)]
struct Photo {
    path: PathBuf,
    /// `None` when its decoder panicked.
    facts: Option<Facts>,
}

#[derive(Clone, Debug)]
struct Facts {
    camera: CameraKey,
    has_gps: bool,
    stamp: Option<CaptureStamp>,
}

/// Each camera the dialog's reader saw, with the first offset one of its
/// photos recorded in `OffsetTimeOriginal`.
type Sightings = BTreeMap<CameraKey, Option<i64>>;

/// Work the dialog waits on, done off the UI thread.
enum Pending<T> {
    Waiting(Receiver<T>),
    Done(T),
}

impl<T: Send + 'static> Pending<T> {
    fn spawn(name: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<Pending<T>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        spawn(name, move || {
            let _ = tx.send(work());
        })?;
        Ok(Pending::Waiting(rx))
    }

    /// Take the result if it has landed, or `lost` if the work died.
    /// Returns true when it just landed.
    fn poll(&mut self, lost: impl FnOnce() -> T) -> bool {
        let Pending::Waiting(rx) = self else {
            return false;
        };
        let done = match rx.try_recv() {
            Ok(done) => done,
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => lost(),
        };
        *self = Pending::Done(done);
        true
    }

    fn done(&self) -> Option<&T> {
        match self {
            Pending::Done(done) => Some(done),
            Pending::Waiting(_) => None,
        }
    }
}

/// The import dialog: what was chosen and the GPX files in it, and a row
/// per camera among the folder's photos, filled in as they're read.
struct Dialog {
    sources: Vec<PathBuf>,
    found: Pending<Found>,
    /// Each batch's cameras as the reader sends them. `None` once it's done.
    reading: Option<Receiver<Sightings>>,
    seen: Sightings,
    /// The clock the user chose for each camera, kept as rows are rebuilt.
    chosen: BTreeMap<CameraKey, CameraClock>,
    cameras: Vec<(CameraKey, CameraClock)>,
}

/// What the import dialog shows.
pub(crate) struct ImportView<'a> {
    pub(crate) sources: &'a [PathBuf],
    /// GPX files found, and how many of them are still in iCloud. `None`
    /// while still looking.
    pub(crate) files: Option<(usize, usize)>,
    /// One row per camera read so far.
    pub(crate) cameras: &'a [(CameraKey, CameraClock)],
    /// The photos are still being read, so more rows may come.
    pub(crate) reading: bool,
}

/// Which picker the import dialog's button opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PickKind {
    /// Files and folders in one panel (macOS).
    #[cfg_attr(
        not(target_os = "macos"),
        allow(dead_code, reason = "only the macOS panel takes both")
    )]
    Any,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "the macOS panel takes both")
    )]
    Files,
    #[cfg_attr(
        target_os = "macos",
        allow(dead_code, reason = "the macOS panel takes both")
    )]
    Folder,
}

impl App {
    /// "Import GPS Tracks…": on macOS the panel, then the dialog; elsewhere
    /// the dialog, whose buttons pick files or a folder. A run that can't
    /// start says why instead.
    pub(super) fn import_gps_tracks(&mut self) {
        if let Err(reason) = self.check_can_geotag() {
            self.set_status(StatusKind::Error, reason);
            self.request_redraw();
            return;
        }
        let mut sources = Vec::new();
        if cfg!(target_os = "macos") {
            sources = crate::shell::dialog::pick_gps_tracks(PickKind::Any);
            if sources.is_empty() {
                return;
            }
        }
        self.open_import_dialog(sources);
    }

    /// Open the dialog over `sources`. The cameras of the folder and its
    /// subfolders, and the chosen folders, are read off the UI thread.
    pub(super) fn open_import_dialog(&mut self, sources: Vec<PathBuf>) {
        let opened =
            read_cameras(self.open_dir()).and_then(|reading| Ok((reading, search(&sources)?)));
        let (reading, found) = match opened {
            Ok(opened) => opened,
            Err(reason) => {
                self.set_status(StatusKind::Error, reason);
                return;
            }
        };
        self.geotag.dialog = Some(Dialog {
            sources,
            found,
            reading: Some(reading),
            seen: Sightings::new(),
            chosen: BTreeMap::new(),
            cameras: Vec::new(),
        });
        self.request_redraw();
    }

    /// Add what the dialog's own picker chose, and look through it all
    /// again.
    pub(super) fn add_gps_tracks(&mut self, kind: PickKind) {
        let picked = crate::shell::dialog::pick_gps_tracks(kind);
        let Some(d) = self.geotag.dialog.as_mut() else {
            return;
        };
        for path in picked {
            if !d.sources.contains(&path) {
                d.sources.push(path);
            }
        }
        match search(&d.sources) {
            Ok(found) => d.found = found,
            Err(reason) => self.set_status(StatusKind::Error, reason),
        }
        self.request_redraw();
    }

    pub(crate) fn import_dialog(&self) -> Option<ImportView<'_>> {
        let d = self.geotag.dialog.as_ref()?;
        Some(ImportView {
            sources: &d.sources,
            files: d.found.done().map(|found| {
                let in_icloud = found.files.iter().filter(|f| f.in_icloud).count();
                (found.files.len(), in_icloud)
            }),
            cameras: &d.cameras,
            reading: d.reading.is_some(),
        })
    }

    pub(super) fn set_camera_clock(&mut self, camera: CameraKey, clock: CameraClock) {
        if let Some(d) = self.geotag.dialog.as_mut() {
            for row in d.cameras.iter_mut().filter(|r| r.0 == camera) {
                row.1 = clock;
            }
            d.chosen.insert(camera, clock);
        }
        self.request_redraw();
    }

    /// Remember each camera's clock and start the run. Waits while the
    /// photos or the GPX files are still being read, or none was found.
    pub(super) fn commit_import_dialog(&mut self) {
        let ready = self.geotag.dialog.as_ref().is_some_and(|d| {
            d.reading.is_none() && d.found.done().is_some_and(|f| !f.files.is_empty())
        });
        if !ready {
            return;
        }
        let Some(dialog) = self.geotag.dialog.take() else {
            return;
        };
        for (camera, clock) in &dialog.cameras {
            self.geotag.clocks.set(camera, *clock);
        }
        if !cfg!(test) {
            self.geotag.clocks.save();
        }
        let settings = Settings {
            clocks: self.geotag.clocks.clone(),
            all: None,
        };
        let Pending::Done(found) = dialog.found else {
            return;
        };
        if let Err(reason) = self.start_run(Tracks::Found(found), settings) {
            self.set_status(StatusKind::Error, reason);
        }
        self.request_redraw();
    }

    pub(super) fn cancel_import_dialog(&mut self) {
        self.geotag.dialog = None;
        self.request_redraw();
    }

    pub(super) fn import_dialog_open(&self) -> bool {
        self.geotag.dialog.is_some()
    }

    /// `geotag.import`: a run over the tracks in `paths` without the dialog.
    /// Each camera takes `camera_minus_utc_s` when given, else its
    /// remembered setting. How many photos it covers is known only when it
    /// ends.
    pub(super) fn start_geotag(
        &mut self,
        paths: &[PathBuf],
        camera_minus_utc_s: Option<i64>,
    ) -> Result<(), CommandError> {
        let settings = Settings {
            clocks: self.geotag.clocks.clone(),
            all: camera_minus_utc_s,
        };
        self.start_run(Tracks::Search(paths.to_vec()), settings)
            .map_err(|reason| CommandError::Failed { reason })
    }

    /// Why a job can't start now, if it can't.
    fn check_can_geotag(&self) -> Result<(), String> {
        if self.geotag.job.is_some() {
            return Err(crate::i18n::t().geotag_busy.into());
        }
        if let Some(reason) = self.folder_read_only_text() {
            return Err(reason);
        }
        if self.open_dir().is_none() {
            return Err(CommandError::NoPhotos.to_string());
        }
        Ok(())
    }

    fn open_dir(&self) -> Option<PathBuf> {
        self.playlist.as_ref().map(|pl| pl.dir().to_path_buf())
    }

    /// Start matching the open folder's photos on `tracks`.
    fn start_run(&mut self, tracks: Tracks, settings: Settings) -> Result<(), String> {
        let timeout = self.geotag.download_timeout;
        self.start_job(Work::Import(GeotagTally::default()), move |dir, tx| {
            run(tracks, dir, &settings, timeout, tx)
        })?;
        self.set_status(StatusKind::Progress, crate::i18n::t().geotag_running.into());
        self.request_redraw();
        Ok(())
    }

    /// Start `work` on a worker thread over the open folder.
    fn start_job(
        &mut self,
        work: Work,
        worker: impl FnOnce(&Path, &Sender<Event>) -> Result<Report, String> + Send + 'static,
    ) -> Result<(), String> {
        self.check_can_geotag()?;
        let dir = self
            .open_dir()
            .ok_or_else(|| CommandError::NoPhotos.to_string())?;
        let (tx, events) = std::sync::mpsc::channel();
        spawn("geotag", move || {
            let end = match worker(&dir, &tx) {
                Ok(report) => Event::Done(report),
                Err(reason) => Event::Failed(reason),
            };
            let _ = tx.send(end);
        })?;
        self.geotag.job = Some(Job { events, work });
        Ok(())
    }

    /// Store the locations that landed, and take what the dialog waits
    /// on. Returns true while a job is going or the dialog waits.
    pub(crate) fn pump_geotag(&mut self) -> bool {
        let waiting = self.pump_dialog();
        if self.geotag.job.is_some() {
            self.pump_events();
        }
        waiting || self.geotag.job.is_some()
    }

    /// Take the dialog's GPX files and each batch of cameras as they land,
    /// and rebuild its camera rows. Returns true while either is still
    /// being read.
    fn pump_dialog(&mut self) -> bool {
        let Some(d) = self.geotag.dialog.as_mut() else {
            return false;
        };
        let mut landed = d.found.poll(Found::default);
        let mut seen_more = false;
        while let Some(rx) = &d.reading {
            match rx.try_recv() {
                Ok(batch) => {
                    merge(&mut d.seen, batch);
                    seen_more = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    d.reading = None;
                    seen_more = true;
                }
            }
        }
        if seen_more {
            d.cameras = camera_rows(&d.seen, &d.chosen, &self.geotag.clocks);
            landed = true;
        }
        let waiting = d.found.done().is_none() || d.reading.is_some();
        if landed {
            self.request_redraw();
        }
        waiting
    }

    fn pump_events(&mut self) {
        let Some(job) = self.geotag.job.as_mut() else {
            return;
        };
        let t = crate::i18n::t();
        let mut located = Vec::new();
        let mut to_clear = Vec::new();
        let mut end = None;
        let mut downloading = None;
        loop {
            match job.events.try_recv() {
                Ok(Event::Matched(batch)) => {
                    for (path, outcome) in batch {
                        if let Work::Import(tally) = &mut job.work {
                            tally.count(&outcome);
                        }
                        if let MatchOutcome::Located(tag) = outcome {
                            located.push((path, tag));
                        }
                    }
                }
                Ok(Event::Clear(batch)) => to_clear.extend(batch),
                Ok(Event::Downloading(n)) => downloading = Some(n),
                Ok(Event::Done(report)) => {
                    end = Some(Ok(report));
                    break;
                }
                Ok(Event::Failed(reason)) => {
                    end = Some(Err(reason));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    end = Some(Err((t.geotag_read_failed)("the run stopped")));
                    break;
                }
            }
        }
        // A trashed photo's sidecar must not come back for it.
        for (path, tag) in located {
            if path.exists() {
                self.save_patch(&path, Patch::Location(Some(tag)));
            }
        }
        let is_gpx = |tag: &crate::geotag::GeoTag| matches!(tag.source, GeoSource::Gpx { .. });
        let mut cleared = 0;
        for (path, on_disk) in to_clear {
            let has_gpx = if self.folder_store.takes_result(&path) {
                self.folder_store.location(&path).is_some_and(is_gpx)
            } else {
                on_disk
            };
            if has_gpx && path.exists() {
                self.save_patch(&path, Patch::Location(None));
                cleared += 1;
            }
        }
        if let Some(Job {
            work: Work::Clear(n),
            ..
        }) = self.geotag.job.as_mut()
        {
            *n += cleared;
        }
        if let Some(n) = downloading {
            self.set_status(StatusKind::Progress, (t.icloud_downloading)(n));
            self.request_redraw();
        }
        let Some(end) = end else {
            return;
        };
        let Some(job) = self.geotag.job.take() else {
            return;
        };
        let (kind, msg) = match (end, job.work) {
            (Err(reason), _) => (StatusKind::Error, reason),
            (Ok(report), Work::Import(tally)) => summary(&tally, &report),
            (Ok(_), Work::Clear(n)) => (StatusKind::Success, (t.gpx_locations_cleared)(n)),
        };
        eprintln!("[lightphotos] geotag: {msg}");
        self.set_status(kind, msg);
        self.request_redraw();
    }

    pub(crate) fn geotag_running(&self) -> bool {
        self.geotag.job.is_some()
    }

    /// `geotag.clear`: forget every GPX location in the open folder and its
    /// subfolders, on a worker that reads the subfolders' sidecars. How many
    /// photos had one is known only when it ends.
    pub(super) fn clear_geotags(&mut self) -> Result<(), String> {
        self.start_job(Work::Clear(0), |dir, tx| {
            clear(dir, tx);
            Ok(Report::default())
        })?;
        self.request_redraw();
        Ok(())
    }
}

fn spawn(name: &str, work: impl FnOnce() + Send + 'static) -> Result<(), String> {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(work)
        .map(drop)
        .map_err(|e| (crate::i18n::t().geotag_read_failed)(&e.to_string()))
}

/// The GPX files among `sources`, looked for off the UI thread: a folder
/// can be large, or still in iCloud.
fn search(sources: &[PathBuf]) -> Result<Pending<Found>, String> {
    let sources = sources.to_vec();
    Pending::spawn("geotag-search", move || source::find(&sources))
}

/// Read the cameras of the photos under `dir`, off the UI thread, sending
/// each batch's. The channel closes when the reading is done.
fn read_cameras(dir: Option<PathBuf>) -> Result<Receiver<Sightings>, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    spawn("geotag-photos", move || {
        for paths in dir.iter().flat_map(|dir| PhotoWalk::new(dir)) {
            let photos = read_photos(&paths);
            let mut seen = Sightings::new();
            merge(
                &mut seen,
                photos
                    .iter()
                    .filter_map(|p| p.facts.as_ref())
                    .map(|f| (f.camera.clone(), recorded_offset(f))),
            );
            if tx.send(seen).is_err() {
                return;
            }
        }
    })?;
    Ok(rx)
}

/// Add `from` to `into`, keeping each camera's first recorded offset.
fn merge(into: &mut Sightings, from: impl IntoIterator<Item = (CameraKey, Option<i64>)>) {
    for (camera, offset) in from {
        let known = into.entry(camera).or_default();
        if known.is_none() {
            *known = offset;
        }
    }
}

/// The offset the photo's `OffsetTimeOriginal` recorded.
fn recorded_offset(facts: &Facts) -> Option<i64> {
    let offset = facts.stamp.as_ref()?.offset.as_deref()?;
    crate::geotag::parse_utc_offset(offset)
}

/// Each of `paths` with its camera, GPS and capture time. A photo with no
/// serial takes the serial of its model's one numbered body among them, so
/// a batch holding a body's RAWs and JPEGs names one camera.
fn read_photos(paths: &[PathBuf]) -> Vec<Photo> {
    let mut photos: Vec<Photo> = paths.iter().map(|path| read_photo(path)).collect();
    cameras::fold_serials(
        photos
            .iter_mut()
            .filter_map(|p| p.facts.as_mut())
            .map(|f| &mut f.camera),
    );
    photos
}

fn read_photo(path: &Path) -> Photo {
    // A decoder that panics on one hostile file costs that photo its row
    // and its location, not the dialog or the run.
    let facts = std::panic::catch_unwind(|| {
        let meta = image_decode::read_metadata(path);
        Facts {
            camera: CameraKey::of(&meta),
            has_gps: meta.gps.is_some(),
            stamp: image_decode::capture_stamp(path),
        }
    })
    .ok();
    Photo {
        path: path.to_path_buf(),
        facts,
    }
}

/// A row per camera `seen`, one with no serial folded onto its model's one
/// numbered body: the user's choice, else its remembered clock, else the
/// offset its photos recorded, else this computer's, and paired with
/// LightRemote when it's the only camera.
fn camera_rows(
    seen: &Sightings,
    chosen: &BTreeMap<CameraKey, CameraClock>,
    clocks: &CameraClocks,
) -> Vec<(CameraKey, CameraClock)> {
    let mut sightings: Vec<(CameraKey, Option<i64>)> =
        seen.iter().map(|(c, o)| (c.clone(), *o)).collect();
    cameras::fold_serials(sightings.iter_mut().map(|(c, _)| c));
    let mut folded = Sightings::new();
    merge(&mut folded, sightings);
    let here = i64::from(chrono::Local::now().offset().local_minus_utc());
    let only = folded.len() == 1;
    folded
        .into_iter()
        .map(|(camera, offset)| {
            let clock = chosen
                .get(&camera)
                .copied()
                .or_else(|| clocks.get(&camera))
                .unwrap_or(CameraClock {
                    zone: CameraZone::nearest(offset.unwrap_or(here)),
                    paired: only,
                });
            (camera, clock)
        })
        .collect()
}

/// The worker: find and download the tracks, join them, then walk the
/// open folder a batch at a time, matching each photo and sending each
/// batch's outcomes.
fn run(
    tracks: Tracks,
    dir: &Path,
    settings: &Settings,
    timeout: Duration,
    tx: &Sender<Event>,
) -> Result<Report, String> {
    let Found { files, notes } = match tracks {
        Tracks::Found(found) => found,
        Tracks::Search(paths) => source::find(&paths),
    };
    if files.is_empty() {
        return Err(no_tracks(&notes));
    }
    let waiting = files.iter().filter(|f| f.in_icloud).count();
    if waiting > 0 {
        let _ = tx.send(Event::Downloading(waiting));
    }
    let (files, downloads) = source::download(files, timeout, source::start_download);
    let notes = notes.into_iter().chain(downloads).collect();
    let Loaded { track, notes } =
        source::load(&files, notes).map_err(|notes| load_failed(&notes))?;
    let mut report = Report {
        notes,
        disagreements: BTreeMap::new(),
    };
    let mut clocks: BTreeMap<CameraKey, Option<ClockModel>> = BTreeMap::new();
    let mut any = false;
    for paths in PhotoWalk::new(dir) {
        any = true;
        let matched = read_photos(&paths)
            .into_iter()
            .map(|photo| {
                let Some(Facts {
                    camera,
                    has_gps,
                    stamp,
                }) = photo.facts
                else {
                    return (photo.path, MatchOutcome::NoCaptureTime);
                };
                let (setting, paired) = settings.for_camera(&camera);
                let clock = clocks
                    .entry(camera.clone())
                    .or_insert_with(|| ClockModel::select(&track, setting, paired));
                let local = stamp.as_ref().map(|s| s.local.as_str());
                let outcome = crate::geotag::locate(&track, clock.as_ref(), has_gps, local);
                let off = measured_offset(&outcome)
                    .zip(setting)
                    .and_then(|(measured, set)| crate::geotag::disagreement(measured, set));
                if let Some(off) = off {
                    let worst = report.disagreements.entry(camera).or_insert(off);
                    if off.unsigned_abs() > worst.unsigned_abs() {
                        *worst = off;
                    }
                }
                (photo.path, outcome)
            })
            .collect();
        if tx.send(Event::Matched(matched)).is_err() {
            break;
        }
    }
    if !any {
        return Err(CommandError::NoPhotos.to_string());
    }
    Ok(report)
}

/// The clear's worker: send each batch's photos that may hold a GPX
/// location.
fn clear(dir: &Path, tx: &Sender<Event>) {
    let is_gpx = |tag: &crate::geotag::GeoTag| matches!(tag.source, GeoSource::Gpx { .. });
    for paths in PhotoWalk::new(dir) {
        let open = paths.first().and_then(|p| p.parent()) == Some(dir);
        let candidates: Vec<(PathBuf, bool)> = paths
            .into_iter()
            .filter_map(|p| {
                let on_disk = crate::sidecar::read_record(&p)
                    .is_ok_and(|rec| rec.location.as_ref().is_some_and(is_gpx));
                (open || on_disk).then_some((p, on_disk))
            })
            .collect();
        if !candidates.is_empty() && tx.send(Event::Clear(candidates)).is_err() {
            return;
        }
    }
}

/// Why no track could be read: each file skipped, and those that didn't
/// download.
fn load_failed(notes: &[Note]) -> String {
    let reasons: Vec<String> = notes
        .iter()
        .filter_map(|n| match n {
            Note::Skipped { file, reason } => Some(format!("{file}: {reason}")),
            Note::NotDownloaded(_) => Some(describe(n)),
            _ => None,
        })
        .collect();
    if reasons.is_empty() {
        return no_tracks(&[]);
    }
    (crate::i18n::t().geotag_read_failed)(&reasons.join("; "))
}

/// The offset a location used, when LightRemote measured it.
fn measured_offset(outcome: &MatchOutcome) -> Option<i64> {
    match outcome {
        MatchOutcome::Located(crate::geotag::GeoTag {
            source:
                GeoSource::Gpx {
                    camera_minus_utc_s,
                    clock: Some(ClockSource::Measured),
                    ..
                },
            ..
        }) => Some(*camera_minus_utc_s),
        _ => None,
    }
}

/// That no GPX file could be read, with the reasons the search gave.
fn no_tracks(notes: &[Note]) -> String {
    let t = crate::i18n::t();
    std::iter::once(t.geotag_no_tracks.to_string())
        .chain(notes.iter().map(describe))
        .collect::<Vec<_>>()
        .join(". ")
}

/// The closing toast: the tally, each disagreement, then each note.
fn summary(n: &GeotagTally, report: &Report) -> (StatusKind, String) {
    let t = crate::i18n::t();
    let done = (t.geotag_done)(
        n.tagged,
        n.had_gps,
        n.outside,
        n.ambiguous,
        n.no_time,
        n.no_clock,
    );
    let off = report.disagreements.iter().map(|(camera, s)| {
        let minutes = s.unsigned_abs().saturating_add(30) / 60;
        (t.geotag_measured_off)(&camera_label(camera), minutes / 60, minutes % 60)
    });
    let msg = std::iter::once(done)
        .chain(off)
        .chain(report.notes.iter().map(describe))
        .collect::<Vec<_>>()
        .join(". ");
    let kind = match n.tagged {
        0 => StatusKind::Error,
        _ => StatusKind::Success,
    };
    (kind, msg)
}

/// How the dialog and the toast name a camera.
pub(crate) fn camera_label(camera: &CameraKey) -> String {
    camera
        .label()
        .unwrap_or_else(|| crate::i18n::t().camera_unknown.into())
}

fn describe(note: &Note) -> String {
    let t = crate::i18n::t();
    match note {
        Note::Skipped { file, reason } => (t.geotag_file_skipped)(file, reason),
        Note::NotDownloaded(n) => (t.geotag_files_not_downloaded)(*n),
        Note::Downloaded(n) => (t.geotag_files_downloaded)(*n),
        Note::TooMany => (t.geotag_too_many)(source::MAX_FILES),
        Note::TooDeep => (t.geotag_too_deep)(source::MAX_DEPTH),
        Note::TooBig => (t.geotag_too_big)(source::MAX_ENTRIES),
    }
}

#[cfg(test)]
mod tests;
