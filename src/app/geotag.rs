// SPDX-License-Identifier: MIT OR Apache-2.0

//! "Import GPS Tracks…": the App side of `crate::geotag`. The user chooses
//! GPX files, folders of them, or both, and confirms each camera's time
//! zone setting in the import dialog. A worker thread then downloads any
//! file still in iCloud, joins every file into one track, reads the camera,
//! capture time and GPS of each photo in the open folder and its
//! subfolders, and matches it on the track; each
//! location lands in its photo's sidecar as it arrives, through
//! `save_patch`, so a run that outlives its folder still writes to the
//! right photos. The photo files are never written.
//!
//! Each photo's camera clock comes from LightRemote's measurements when the
//! camera was paired with it and a track has them, else from its camera's
//! time zone setting. The dialog asks both once per camera and the app's
//! prefs remember them. The photos and the chosen folders are read
//! off the UI thread while the dialog is open, and the run reuses what the
//! dialog read.
//!
//! Desktop only: the browser has no picker for GPX files yet.

mod cameras;
mod source;

use super::*;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::time::Duration;

use crate::command::CommandError;
use crate::decode::image_decode::CaptureStamp;
use crate::geotag::{CameraZone, ClockModel, ClockSource, GeoSource, MatchOutcome};
use crate::persist::folder_store::Patch;
use cameras::CameraClocks;
pub(crate) use cameras::{CameraClock, CameraKey};
use source::{Found, Loaded, Note};

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

/// A run. At most one at a time.
struct Job {
    events: Receiver<Event>,
    tally: GeotagTally,
}

/// What a run's worker reports.
enum Event {
    /// Waiting for iCloud to download this many files.
    Downloading(usize),
    Matched(PathBuf, MatchOutcome),
    Done(Report),
    /// No track to match on, and why.
    Failed(String),
}

/// What the user should hear when a run ends, past the tally.
struct Report {
    notes: Vec<Note>,
    /// Each camera LightRemote measured far from its setting, and how far
    /// (measured minus set, seconds).
    disagreements: BTreeMap<CameraKey, i64>,
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
    /// it. A camera never asked about counts as paired when it is the
    /// folder's `only_camera`.
    fn for_camera(&self, camera: &CameraKey, only_camera: bool) -> (Option<i64>, bool) {
        let clock = self.clocks.get(camera);
        let offset = self
            .all
            .or_else(|| clock.map(|c| c.zone.camera_minus_utc_s()));
        (offset, clock.map_or(only_camera, |c| c.paired))
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

/// Work the dialog waits on, done off the UI thread.
enum Pending<T> {
    Waiting(Receiver<T>),
    Done(T),
}

impl<T: Send + 'static> Pending<T> {
    fn spawn(name: &str, work: impl FnOnce() -> T + Send + 'static) -> Result<Pending<T>, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let _ = tx.send(work());
            })
            .map_err(|e| (crate::i18n::t().geotag_read_failed)(&e.to_string()))?;
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

/// The import dialog: what was chosen and the GPX files in it, the
/// folder's photos, and a row per camera among them.
struct Dialog {
    sources: Vec<PathBuf>,
    found: Pending<Found>,
    photos: Pending<Vec<Photo>>,
    /// Filled once `photos` lands.
    cameras: Vec<(CameraKey, CameraClock)>,
}

/// What the import dialog shows.
pub(crate) struct ImportView<'a> {
    pub(crate) sources: &'a [PathBuf],
    /// GPX files found, and how many of them are still in iCloud. `None`
    /// while still looking.
    pub(crate) files: Option<(usize, usize)>,
    /// One row per camera, once they're read.
    pub(crate) cameras: Option<&'a [(CameraKey, CameraClock)]>,
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

    /// Open the dialog over `sources`. The photos of the folder and its
    /// subfolders, and the chosen folders, are read off the UI thread.
    pub(super) fn open_import_dialog(&mut self, sources: Vec<PathBuf>) {
        let dir = self.open_dir();
        let opened = Pending::spawn("geotag-photos", move || {
            let paths = dir.map(|dir| photos_under(&dir)).unwrap_or_default();
            read_photos(&paths, Vec::new())
        })
        .and_then(|photos| Ok((photos, search(&sources)?)));
        let (photos, found) = match opened {
            Ok(opened) => opened,
            Err(reason) => {
                self.set_status(StatusKind::Error, reason);
                return;
            }
        };
        self.geotag.dialog = Some(Dialog {
            sources,
            found,
            photos,
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
            cameras: d.photos.done().map(|_| &d.cameras[..]),
        })
    }

    pub(super) fn set_camera_clock(&mut self, row: usize, clock: CameraClock) {
        if let Some(r) = self
            .geotag
            .dialog
            .as_mut()
            .and_then(|d| d.cameras.get_mut(row))
        {
            r.1 = clock;
        }
        self.request_redraw();
    }

    /// Remember each camera's clock and start the run. Waits while the
    /// photos or the GPX files are still being read, or none was found.
    pub(super) fn commit_import_dialog(&mut self) {
        let ready = self.geotag.dialog.as_ref().is_some_and(|d| {
            d.photos.done().is_some() && d.found.done().is_some_and(|f| !f.files.is_empty())
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
        let (Pending::Done(found), Pending::Done(photos)) = (dialog.found, dialog.photos) else {
            return;
        };
        let paths = photos.iter().map(|p| p.path.clone()).collect();
        if let Err(reason) = self.start_run(found, paths, photos, settings) {
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
    /// remembered setting. Returns how many photos it covers.
    pub(super) fn start_geotag(
        &mut self,
        paths: &[PathBuf],
        camera_minus_utc_s: Option<i64>,
    ) -> Result<usize, CommandError> {
        let found = source::find(paths);
        let settings = Settings {
            clocks: self.geotag.clocks.clone(),
            all: camera_minus_utc_s,
        };
        let photos = self
            .open_dir()
            .map(|dir| photos_under(&dir))
            .unwrap_or_default();
        self.start_run(found, photos, Vec::new(), settings)
            .map_err(|reason| CommandError::Failed { reason })
    }

    /// Why a run can't start now, if it can't.
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

    /// Start matching `photos` on the tracks in `found`. A photo in `known`
    /// is not read again.
    fn start_run(
        &mut self,
        found: Found,
        photos: Vec<PathBuf>,
        known: Vec<Photo>,
        settings: Settings,
    ) -> Result<usize, String> {
        let t = crate::i18n::t();
        self.check_can_geotag()?;
        if photos.is_empty() {
            return Err(CommandError::NoPhotos.to_string());
        }
        if found.files.is_empty() {
            return Err(no_tracks(&found.notes));
        }
        let total = photos.len();
        let timeout = self.geotag.download_timeout;
        let (tx, events) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("geotag".into())
            .spawn(move || {
                let end = match run(found, &photos, known, &settings, timeout, &tx) {
                    Ok(report) => Event::Done(report),
                    Err(reason) => Event::Failed(reason),
                };
                let _ = tx.send(end);
            })
            .map_err(|e| (t.geotag_read_failed)(&e.to_string()))?;
        self.geotag.job = Some(Job {
            events,
            tally: GeotagTally::default(),
        });
        self.set_status(StatusKind::Progress, t.geotag_running.into());
        self.request_redraw();
        Ok(total)
    }

    /// Store the locations that landed, and take what the dialog waits
    /// on. Returns true while a run is going or the dialog waits.
    pub(crate) fn pump_geotag(&mut self) -> bool {
        let waiting = self.pump_dialog();
        if self.geotag.job.is_some() {
            self.pump_events();
        }
        waiting || self.geotag.job.is_some()
    }

    /// Take the dialog's GPX files and photos as they land, and fill its
    /// camera rows from the photos. Returns true while either is still
    /// being read.
    fn pump_dialog(&mut self) -> bool {
        let Some(d) = self.geotag.dialog.as_mut() else {
            return false;
        };
        let mut landed = d.found.poll(Found::default);
        if d.photos.poll(Vec::new) {
            if let Some(photos) = d.photos.done() {
                d.cameras = camera_rows(photos, &self.geotag.clocks);
            }
            landed = true;
        }
        let waiting = d.found.done().is_none() || d.photos.done().is_none();
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
        let mut landed = Vec::new();
        let mut end = None;
        let mut downloading = None;
        loop {
            match job.events.try_recv() {
                Ok(Event::Matched(path, outcome)) => {
                    job.tally.count(&outcome);
                    landed.push((path, outcome));
                }
                Ok(Event::Downloading(n)) => downloading = Some(n),
                Ok(Event::Done(report)) => {
                    end = Some(summary(&job.tally, &report));
                    break;
                }
                Ok(Event::Failed(reason)) => {
                    end = Some((StatusKind::Error, reason));
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    let reason = (t.geotag_read_failed)("the run stopped");
                    end = Some((StatusKind::Error, reason));
                    break;
                }
            }
        }
        for (path, outcome) in landed {
            // A trashed photo's sidecar must not come back for it.
            if let MatchOutcome::Located(tag) = outcome {
                if path.exists() {
                    self.save_patch(&path, Patch::Location(Some(tag)));
                }
            }
        }
        if let Some(n) = downloading {
            self.set_status(StatusKind::Progress, (t.icloud_downloading)(n));
            self.request_redraw();
        }
        if let Some((kind, msg)) = end {
            self.geotag.job = None;
            eprintln!("[lightphotos] geotag: {msg}");
            self.set_status(kind, msg);
            self.request_redraw();
        }
    }

    pub(crate) fn geotag_running(&self) -> bool {
        self.geotag.job.is_some()
    }

    /// `geotag.clear`: forget every GPX location in the open folder and its
    /// subfolders. Returns how many photos had one. Refused while a run is
    /// saving, since a subfolder's locations are read from disk.
    pub(super) fn clear_geotags(&mut self) -> Result<usize, String> {
        if self.geotag.job.is_some() {
            return Err(crate::i18n::t().geotag_busy.into());
        }
        let dir = self.open_dir();
        let is_gpx = |tag: &crate::geotag::GeoTag| matches!(tag.source, GeoSource::Gpx { .. });
        let paths: Vec<PathBuf> = dir
            .as_deref()
            .map(photos_under)
            .unwrap_or_default()
            .into_iter()
            .filter(|p| {
                // The open folder's records are in the cache, which holds
                // edits not yet on disk. A subfolder's are only on disk.
                if p.parent() == dir.as_deref() {
                    self.folder_store.location(p).is_some_and(is_gpx)
                } else {
                    crate::sidecar::read_record(p)
                        .is_ok_and(|rec| rec.location.as_ref().is_some_and(is_gpx))
                }
            })
            .collect();
        for path in &paths {
            self.save_patch(path, Patch::Location(None));
        }
        let msg = (crate::i18n::t().gpx_locations_cleared)(paths.len());
        self.set_status(StatusKind::Success, msg);
        self.request_redraw();
        Ok(paths.len())
    }
}

/// The photos in `dir` and in each subfolder the folder tree lists under it,
/// down to [`source::MAX_DEPTH`] levels. A link to a folder is not followed,
/// so the walk stays inside `dir` and can't loop.
fn photos_under(dir: &Path) -> Vec<PathBuf> {
    let mut photos = Vec::new();
    let mut todo = vec![(dir.to_path_buf(), 0)];
    while let Some((dir, depth)) = todo.pop() {
        photos.extend_from_slice(crate::navigation::Playlist::from_dir(&dir).entries());
        if depth < source::MAX_DEPTH {
            let subdirs = crate::navigation::list_subdirs(&dir)
                .into_iter()
                .filter(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir()));
            todo.extend(subdirs.rev().map(|d| (d, depth + 1)));
        }
    }
    photos
}

/// The GPX files among `sources`, looked for off the UI thread: a folder
/// can be large, or still in iCloud.
fn search(sources: &[PathBuf]) -> Result<Pending<Found>, String> {
    let sources = sources.to_vec();
    Pending::spawn("geotag-search", move || source::find(&sources))
}

/// Each of `paths` with its camera, GPS and capture time, taking those in
/// `known` as already read. A photo with no serial takes its model's one
/// numbered body's.
fn read_photos(paths: &[PathBuf], known: Vec<Photo>) -> Vec<Photo> {
    let mut known: HashMap<PathBuf, Photo> =
        known.into_iter().map(|p| (p.path.clone(), p)).collect();
    let mut photos: Vec<Photo> = paths
        .iter()
        .map(|path| known.remove(path).unwrap_or_else(|| read_photo(path)))
        .collect();
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

/// A row per camera among `photos`: its remembered clock, else the offset
/// the first of its photos to record `OffsetTimeOriginal` gave, else this
/// computer's, and paired with LightRemote when it's the only camera.
fn camera_rows(photos: &[Photo], clocks: &CameraClocks) -> Vec<(CameraKey, CameraClock)> {
    let mut recorded: BTreeMap<&CameraKey, Option<i64>> = BTreeMap::new();
    for f in photos.iter().filter_map(|p| p.facts.as_ref()) {
        let offset = f
            .stamp
            .as_ref()
            .and_then(|s| crate::geotag::parse_utc_offset(s.offset.as_deref()?));
        let known = recorded.entry(&f.camera).or_default();
        if known.is_none() {
            *known = offset;
        }
    }
    let here = i64::from(chrono::Local::now().offset().local_minus_utc());
    let only = recorded.len() == 1;
    recorded
        .into_iter()
        .map(|(camera, offset)| {
            let clock = clocks.get(camera).unwrap_or(CameraClock {
                zone: CameraZone::nearest(offset.unwrap_or(here)),
                paired: only,
            });
            (camera.clone(), clock)
        })
        .collect()
}

/// The worker: download what iCloud holds, join the tracks, then match
/// each photo, sending each outcome as it lands.
fn run(
    found: Found,
    paths: &[PathBuf],
    known: Vec<Photo>,
    settings: &Settings,
    timeout: Duration,
    tx: &Sender<Event>,
) -> Result<Report, String> {
    let Found { files, notes } = found;
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
    let photos = read_photos(paths, known);
    let only_camera = photos
        .iter()
        .filter_map(|p| p.facts.as_ref())
        .map(|f| &f.camera)
        .collect::<BTreeSet<_>>()
        .len()
        == 1;
    let mut clocks: BTreeMap<CameraKey, Option<ClockModel>> = BTreeMap::new();
    for photo in photos {
        let outcome = match photo.facts {
            Some(Facts {
                camera,
                has_gps,
                stamp,
            }) => {
                let (setting, paired) = settings.for_camera(&camera, only_camera);
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
                outcome
            }
            None => MatchOutcome::NoCaptureTime,
        };
        if tx.send(Event::Matched(photo.path, outcome)).is_err() {
            break;
        }
    }
    Ok(report)
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
