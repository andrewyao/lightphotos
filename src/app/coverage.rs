//! Coverage for detected masks: the one owner of "does this photo have its
//! subject or sky yet, and is it in memory". Callers ask; this module
//! decides whether that means detect, load, wait, or give up. Asking is
//! idempotent, so callers ask every frame or after every write without
//! bookkeeping. The persisted truth is `ImageRecord.detections`; this is
//! only the in-memory side.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::develop::{Coverage, CoverageError, DetectTarget, Detections, Mask, RasterId};
use crate::scoring::detect;
#[cfg(not(target_arch = "wasm32"))]
use crate::scoring::detect::DetectError;
#[cfg(not(target_arch = "wasm32"))]
use crate::worker_pool::WorkerPool;

/// Ready coverages past this many bytes evict oldest-used first, about 16
/// full-size subjects. An evicted one reloads on the next ask.
const READY_BUDGET: usize = 64 << 20;

/// Where one (photo, target)'s coverage stands in this session.
enum Slot {
    /// A detect job is queued or running.
    Detecting,
    /// The recorded file is being read and decoded.
    Loading(RasterId),
    /// In memory. A re-detect that records a new id makes this stale, and
    /// the next ask reloads.
    Ready(Arc<Coverage>),
    /// Settled without coverage. Not retried until [`CoverageStore::retry`]
    /// or the folder reopens, so a failing photo can't loop.
    Failed(Failure),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Failure {
    /// No detector for this target in this build; nothing recorded.
    Unsupported,
    /// The recorded file is gone or bad and this build can't re-detect.
    Missing,
    /// The detector errored; its message for the tooltip.
    Detect(String),
    /// The recorded file could not be read this session (permission, a
    /// lost folder handle); the id stays recorded, and Retry reads again.
    Read(String),
}

/// What a caller gets back.
pub(crate) enum Lookup {
    Ready(Arc<Coverage>),
    /// Detect or load in flight. A main-thread consumer renders without it
    /// and is redrawn on landing; the export blocker waits on it.
    Pending,
    /// Settled without coverage; `status` says why.
    Failed,
}

/// A finished job, as [`CoverageStore::drain`] hands it to the app.
pub(crate) struct Landed {
    pub photo: PathBuf,
    pub target: DetectTarget,
    pub record: Record,
    pub notice: Option<Notice>,
}

/// What the sidecar should now say about the target.
pub(crate) enum Record {
    Keep,
    /// A fresh detection, or a recorded file found bad and forgotten.
    Set(Option<RasterId>),
}

pub(crate) enum Notice {
    NothingFound,
    Failed(String),
}

/// A detector the store runs: `detect::run`, or a test's stand-in.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type Detector = fn(&Path, DetectTarget) -> Result<Coverage, DetectError>;

#[cfg(not(target_arch = "wasm32"))]
struct DetectJob {
    photo: PathBuf,
    target: DetectTarget,
}

#[cfg(not(target_arch = "wasm32"))]
struct LoadJob {
    photo: PathBuf,
    target: DetectTarget,
    id: RasterId,
}

pub(crate) struct CoverageStore {
    slots: HashMap<(PathBuf, DetectTarget), Slot>,
    /// Ready entries in last-use order, oldest first.
    ready_order: VecDeque<(PathBuf, DetectTarget)>,
    ready_bytes: usize,
    /// One detect worker: Vision and a model both saturate the GPU or the
    /// Neural Engine, so two at once is slower. One load worker apart from
    /// it, so a 5 ms load never waits behind a 3 s detection.
    #[cfg(not(target_arch = "wasm32"))]
    detect: WorkerPool<DetectJob, Coverage>,
    #[cfg(not(target_arch = "wasm32"))]
    load: WorkerPool<LoadJob, Result<Coverage, CoverageError>>,
    /// Work the app must run for the store: File System Access is
    /// main-thread only, so the app reads each file as a task, and a
    /// detection runs on the shown photo's decoded pixels when it is shown.
    /// Results come back through `web_landings`.
    #[cfg(target_arch = "wasm32")]
    wanted_loads: Vec<(PathBuf, DetectTarget, RasterId)>,
    #[cfg(target_arch = "wasm32")]
    wanted_detects: Vec<(PathBuf, DetectTarget)>,
    #[cfg(target_arch = "wasm32")]
    web_landings: (
        std::sync::mpsc::Sender<WebLanding>,
        std::sync::mpsc::Receiver<WebLanding>,
    ),
    /// Jobs submitted and not yet drained, so the frame loop keeps polling.
    in_flight: usize,
    /// Which targets this build can detect: `detect::available`, or a
    /// test's stand-in.
    available: fn(DetectTarget) -> bool,
}

/// Work the app finished for the store, on the web.
#[cfg(target_arch = "wasm32")]
pub(crate) enum WebLanding {
    Loaded {
        photo: PathBuf,
        target: DetectTarget,
        id: RasterId,
        result: Result<Coverage, CoverageError>,
    },
    /// The coverage file is written before this is sent, like native.
    Detected {
        photo: PathBuf,
        target: DetectTarget,
        result: Result<Coverage, String>,
    },
}

impl CoverageStore {
    pub(crate) fn new() -> Self {
        #[cfg(not(target_arch = "wasm32"))]
        {
            Self::with_detector(detect::run)
        }
        #[cfg(target_arch = "wasm32")]
        {
            Self {
                slots: HashMap::new(),
                ready_order: VecDeque::new(),
                ready_bytes: 0,
                wanted_loads: Vec::new(),
                wanted_detects: Vec::new(),
                web_landings: std::sync::mpsc::channel(),
                in_flight: 0,
                available: detect::available,
            }
        }
    }

    /// The files the app must read for the store, each once.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn take_wanted_loads(&mut self) -> Vec<(PathBuf, DetectTarget, RasterId)> {
        std::mem::take(&mut self.wanted_loads)
    }

    /// The detections the app can run now: those whose photo `can_run`
    /// accepts (the shown one, with its pixels decoded). The rest wait,
    /// still `Detecting`, until their photo is shown.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn take_wanted_detects(
        &mut self,
        can_run: impl Fn(&Path) -> bool,
    ) -> Vec<(PathBuf, DetectTarget)> {
        let (now, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.wanted_detects)
            .into_iter()
            .partition(|(photo, _)| can_run(photo));
        self.wanted_detects = later;
        self.in_flight += now.len();
        now
    }

    /// Where the app sends what it read.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn web_landing_sender(&self) -> std::sync::mpsc::Sender<WebLanding> {
        self.web_landings.0.clone()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn with_detector(detector: Detector) -> Self {
        Self::with_detector_and_availability(detector, detect::available)
    }

    /// [`with_detector`](Self::with_detector) with `available` in place of
    /// `detect::available`, so a test can make a target unsupported.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn with_detector_and_availability(
        detector: Detector,
        available: fn(DetectTarget) -> bool,
    ) -> Self {
        let detect = WorkerPool::spawn(
            "detect",
            1,
            || {},
            move |job: &DetectJob| detector(&job.photo, job.target).map_err(|e| e.to_string()),
            |job| format!("detecting {:?} panicked", job.target),
        );
        let load = WorkerPool::spawn(
            "coverage-load",
            1,
            || {},
            |job: &LoadJob| Ok(crate::develop::coverage::read(&job.photo, job.id)),
            |_| "reading a coverage file panicked".into(),
        );
        Self {
            slots: HashMap::new(),
            ready_order: VecDeque::new(),
            ready_bytes: 0,
            detect,
            load,
            in_flight: 0,
            available,
        }
    }

    /// The coverage `photo` should render for `target`, given what its
    /// sidecar records. Starts whatever is missing.
    pub(crate) fn ask(
        &mut self,
        photo: &Path,
        target: DetectTarget,
        recorded: Option<RasterId>,
    ) -> Lookup {
        let key = (photo.to_path_buf(), target);
        match (self.slots.get(&key), recorded) {
            (Some(Slot::Ready(c)), Some(id)) if c.id() == id => {
                let c = Arc::clone(c);
                self.touch(&key);
                Lookup::Ready(c)
            }
            (Some(Slot::Ready(c)), None) => Lookup::Ready(Arc::clone(c)),
            (Some(Slot::Failed(_)), _) => Lookup::Failed,
            (Some(Slot::Loading(loading)), Some(id)) if *loading == id => Lookup::Pending,
            (Some(Slot::Detecting), _) => Lookup::Pending,
            // A `Loading` of another id, or nothing yet: the recorded file
            // is the truth, so read it.
            (_, Some(id)) => self.start_load(key, id),
            (Some(Slot::Loading(_)), None) => Lookup::Pending,
            (None, None) => self.start_detect(key),
        }
    }

    /// The coverage in memory for `target`, without starting anything.
    pub(crate) fn ready(&self, photo: &Path, target: DetectTarget) -> Option<Arc<Coverage>> {
        match self.slots.get(&(photo.to_path_buf(), target)) {
            Some(Slot::Ready(c)) => Some(Arc::clone(c)),
            _ => None,
        }
    }

    /// Queues a detection for every target `masks` use that `detections`
    /// doesn't record and this session hasn't tried. No loads, so pasting
    /// onto 500 photos reads nothing into memory.
    pub(crate) fn ensure_detected(
        &mut self,
        photo: &Path,
        masks: &[Mask],
        detections: &Detections,
    ) {
        for target in used_targets(masks) {
            let key = (photo.to_path_buf(), target);
            if detections.get(target).is_none() && !self.slots.contains_key(&key) {
                self.start_detect(key);
            }
        }
    }

    /// Clears a settled slot (failed, or a remembered "nothing") so the
    /// next ask tries again. The caller forgets a recorded id it wants
    /// re-detected; a job in flight is left to land.
    pub(crate) fn retry(&mut self, photo: &Path, target: DetectTarget) {
        let key = (photo.to_path_buf(), target);
        if matches!(self.slots.get(&key), Some(Slot::Failed(_) | Slot::Ready(_))) {
            self.forget_ready(&key);
            self.slots.remove(&key);
        }
    }

    /// How the panel should mark a detected mask of `target`: `None` when
    /// its coverage is in hand.
    pub(crate) fn status(
        &self,
        photo: &Path,
        target: DetectTarget,
        recorded: Option<RasterId>,
    ) -> Option<MaskStatus> {
        match self.slots.get(&(photo.to_path_buf(), target)) {
            Some(Slot::Ready(c)) => c.is_nothing().then_some(MaskStatus::NothingFound),
            Some(Slot::Failed(f)) => Some(MaskStatus::Failed(f.clone())),
            Some(Slot::Detecting | Slot::Loading(_)) => Some(MaskStatus::Detecting),
            None if recorded.is_none() && !(self.available)(target) => {
                Some(MaskStatus::Failed(Failure::Unsupported))
            }
            None => Some(MaskStatus::Detecting),
        }
    }

    /// What `target` of `photo` needs before a bake can use it. `in_memory`
    /// is the browser's rule: a recorded coverage must be loaded, since no
    /// worker there can read the file; native workers read it themselves.
    fn need(
        &self,
        photo: &Path,
        target: DetectTarget,
        recorded: Option<RasterId>,
        in_memory: bool,
    ) -> Need {
        let slot = self.slots.get(&(photo.to_path_buf(), target));
        match (slot, recorded) {
            (Some(Slot::Failed(_)), _) => Need::Ready,
            (Some(Slot::Ready(c)), Some(id)) if c.id() == id => Need::Ready,
            (Some(Slot::Ready(_)), None) => Need::Ready,
            (_, Some(id)) if !in_memory => {
                let _ = id;
                Need::Ready
            }
            (Some(Slot::Loading(l)), Some(id)) if *l == id => Need::Pending,
            (_, Some(id)) => Need::Load(id),
            (Some(Slot::Detecting | Slot::Loading(_)), None) => Need::Pending,
            (None, None) => Need::Detect,
        }
    }

    /// Whether every target `masks` use is ready for a bake of `photo`,
    /// starting whatever is missing: loads where the bake needs the pixels
    /// in memory, detections where nothing is recorded. A target this build
    /// can't detect settles as failed and counts as ready, since it renders
    /// as nothing under a signature that says so. Every bake, score and
    /// export asks here, so a photo never shown still gets its work done.
    pub(crate) fn readiness(
        &mut self,
        photo: &Path,
        masks: &[Mask],
        detections: &Detections,
        in_memory: bool,
    ) -> Readiness {
        let mut out = Readiness::Ready;
        for target in used_targets(masks) {
            match self.need(photo, target, detections.get(target), in_memory) {
                Need::Ready => {}
                Need::Pending => out = Readiness::Pending,
                Need::Load(id) => {
                    self.start_load((photo.to_path_buf(), target), id);
                    out = Readiness::Pending;
                }
                Need::Detect => {
                    if matches!(
                        self.start_detect((photo.to_path_buf(), target)),
                        Lookup::Pending
                    ) {
                        out = Readiness::Pending;
                    }
                }
            }
        }
        out
    }

    /// [`readiness`](Self::readiness) without starting anything, for a
    /// query from the UI. Work it reports pending is started by the frame
    /// loop's [`readiness`] over the selection, so pending never stands.
    pub(crate) fn is_ready(
        &self,
        photo: &Path,
        masks: &[Mask],
        detections: &Detections,
        in_memory: bool,
    ) -> bool {
        used_targets(masks).all(
            |t| match self.need(photo, t, detections.get(t), in_memory) {
                Need::Ready => true,
                Need::Detect => !(self.available)(t),
                Need::Pending | Need::Load(_) => false,
            },
        )
    }

    /// How many of `paths` a bake can't yet serve: the export blocker.
    pub(crate) fn pending_among(
        &self,
        paths: &[PathBuf],
        masks: &HashMap<PathBuf, Vec<Mask>>,
        detections: &HashMap<PathBuf, Detections>,
    ) -> usize {
        paths
            .iter()
            .filter(|p| {
                masks.get(*p).is_some_and(|masks| {
                    let recorded = detections.get(*p).cloned().unwrap_or_default();
                    !self.is_ready(p, masks, &recorded, cfg!(target_arch = "wasm32"))
                })
            })
            .count()
    }

    /// Whether a job is still out, so the frame loop keeps polling.
    pub(crate) fn busy(&self) -> bool {
        self.in_flight > 0
    }

    /// Folder switch: drop every slot. A job already running finishes and
    /// writes its file (harmless, content-addressed); its landing finds no
    /// slot and is dropped, and the next open re-detects onto the same name.
    pub(crate) fn clear(&mut self) {
        self.slots.clear();
        self.ready_order.clear();
        self.ready_bytes = 0;
    }

    /// Finished work, for the app to record and redraw. A load whose id no
    /// longer matches the slot is dropped (a re-detect overtook it). A
    /// recorded file found missing or bad is forgotten and re-detected
    /// where a detector exists, else settled as `Failed(Missing)`.
    pub(crate) fn drain(&mut self) -> Vec<Landed> {
        let mut out = Vec::new();
        #[cfg(target_arch = "wasm32")]
        {
            while let Ok(landing) = self.web_landings.1.try_recv() {
                self.in_flight = self.in_flight.saturating_sub(1);
                match landing {
                    WebLanding::Loaded {
                        photo,
                        target,
                        id,
                        result,
                    } => {
                        let key = (photo, target);
                        if !matches!(self.slots.get(&key), Some(Slot::Loading(l)) if *l == id) {
                            continue;
                        }
                        let record = match result {
                            Ok(coverage) => {
                                self.set_ready(&key, Arc::new(coverage));
                                Record::Keep
                            }
                            // A read that failed this session keeps the id
                            // for Retry; a file that is gone or bad is
                            // forgotten. A Subject file can't be re-detected
                            // here, and a Sky one is re-detected only when
                            // its photo is shown, so both settle rather
                            // than wait.
                            Err(CoverageError::Io(why)) => {
                                self.slots
                                    .insert(key.clone(), Slot::Failed(Failure::Read(why)));
                                Record::Keep
                            }
                            Err(CoverageError::Missing | CoverageError::Corrupt(_)) => {
                                self.slots
                                    .insert(key.clone(), Slot::Failed(Failure::Missing));
                                Record::Set(None)
                            }
                        };
                        out.push(Landed {
                            photo: key.0,
                            target: key.1,
                            record,
                            notice: None,
                        });
                    }
                    WebLanding::Detected {
                        photo,
                        target,
                        result,
                    } => {
                        if let Some(landed) = self.land_detected((photo, target), result) {
                            out.push(landed);
                        }
                    }
                }
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            for (job, result) in self.detect.poll() {
                self.in_flight = self.in_flight.saturating_sub(1);
                if let Some(landed) = self.land_detected((job.photo, job.target), result) {
                    out.push(landed);
                }
            }
            for (job, result) in self.load.poll() {
                self.in_flight = self.in_flight.saturating_sub(1);
                let key = (job.photo, job.target);
                if !matches!(self.slots.get(&key), Some(Slot::Loading(id)) if *id == job.id) {
                    continue;
                }
                let result = result.unwrap_or_else(|why| Err(CoverageError::Corrupt(why)));
                let record = match result {
                    Ok(coverage) => {
                        self.set_ready(&key, Arc::new(coverage));
                        Record::Keep
                    }
                    Err(e) => {
                        // A read that failed this session (permission, a
                        // device gone) keeps the id for Retry. Only a file
                        // that is gone or bad is forgotten.
                        if let CoverageError::Io(why) = e {
                            self.slots
                                .insert(key.clone(), Slot::Failed(Failure::Read(why)));
                            Record::Keep
                        } else {
                            if matches!(e, CoverageError::Corrupt(_)) {
                                // Content-addressed, so a re-detect would
                                // skip writing over the bad file.
                                crate::develop::coverage::remove(&key.0, job.id);
                            }
                            if (self.available)(key.1) {
                                self.slots.remove(&key);
                                self.start_detect(key.clone());
                            } else {
                                self.slots
                                    .insert(key.clone(), Slot::Failed(Failure::Missing));
                            }
                            Record::Set(None)
                        }
                    }
                };
                out.push(Landed {
                    photo: key.0,
                    target: key.1,
                    record,
                    notice: None,
                });
            }
        }
        out
    }

    /// A finished detection for `key`, unless its slot was cleared meanwhile
    /// (a folder switch): "nothing found" is remembered like any answer.
    fn land_detected(
        &mut self,
        key: (PathBuf, DetectTarget),
        result: Result<Coverage, String>,
    ) -> Option<Landed> {
        if !matches!(self.slots.get(&key), Some(Slot::Detecting)) {
            return None;
        }
        let (record, notice) = match result {
            Ok(coverage) => {
                let notice = coverage.is_nothing().then_some(Notice::NothingFound);
                let id = coverage.id();
                self.set_ready(&key, Arc::new(coverage));
                (Record::Set(Some(id)), notice)
            }
            Err(why) => {
                self.slots
                    .insert(key.clone(), Slot::Failed(Failure::Detect(why.clone())));
                (Record::Keep, Some(Notice::Failed(why)))
            }
        };
        Some(Landed {
            photo: key.0,
            target: key.1,
            record,
            notice,
        })
    }

    fn start_detect(&mut self, key: (PathBuf, DetectTarget)) -> Lookup {
        if !(self.available)(key.1) {
            self.slots.insert(key, Slot::Failed(Failure::Unsupported));
            return Lookup::Failed;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let submitted = self.detect.submit(DetectJob {
            photo: key.0.clone(),
            target: key.1,
        });
        // The app runs it on the shown photo's pixels; until then it waits
        // here without counting as in flight.
        #[cfg(target_arch = "wasm32")]
        let submitted = {
            self.wanted_detects.push(key.clone());
            true
        };
        if submitted {
            #[cfg(not(target_arch = "wasm32"))]
            {
                self.in_flight += 1;
            }
            self.slots.insert(key, Slot::Detecting);
            return Lookup::Pending;
        }
        let why = Failure::Detect("the detect worker is gone".into());
        self.slots.insert(key, Slot::Failed(why));
        Lookup::Failed
    }

    fn start_load(&mut self, key: (PathBuf, DetectTarget), id: RasterId) -> Lookup {
        #[cfg(not(target_arch = "wasm32"))]
        let submitted = self.load.submit(LoadJob {
            photo: key.0.clone(),
            target: key.1,
            id,
        });
        #[cfg(target_arch = "wasm32")]
        let submitted = {
            self.wanted_loads.push((key.0.clone(), key.1, id));
            true
        };
        if submitted {
            self.in_flight += 1;
            self.forget_ready(&key);
            self.slots.insert(key, Slot::Loading(id));
            return Lookup::Pending;
        }
        self.slots.insert(key, Slot::Failed(Failure::Missing));
        Lookup::Failed
    }

    fn set_ready(&mut self, key: &(PathBuf, DetectTarget), coverage: Arc<Coverage>) {
        self.forget_ready(key);
        self.ready_bytes += coverage.alpha().len();
        self.slots.insert(key.clone(), Slot::Ready(coverage));
        self.ready_order.push_back(key.clone());
        while self.ready_bytes > READY_BUDGET && self.ready_order.len() > 1 {
            let Some(old) = self.ready_order.pop_front() else {
                break;
            };
            if let Some(Slot::Ready(c)) = self.slots.get(&old) {
                self.ready_bytes = self.ready_bytes.saturating_sub(c.alpha().len());
                self.slots.remove(&old);
            }
        }
    }

    /// Drops a Ready slot's accounting ahead of replacing the slot.
    fn forget_ready(&mut self, key: &(PathBuf, DetectTarget)) {
        if let Some(Slot::Ready(c)) = self.slots.get(key) {
            self.ready_bytes = self.ready_bytes.saturating_sub(c.alpha().len());
            self.ready_order.retain(|k| k != key);
        }
    }

    fn touch(&mut self, key: &(PathBuf, DetectTarget)) {
        if self.ready_order.back() == Some(key) {
            return;
        }
        self.ready_order.retain(|k| k != key);
        self.ready_order.push_back(key.clone());
    }
}

/// What one target needs before a bake can use it. See [`CoverageStore::need`].
enum Need {
    Ready,
    Pending,
    Load(RasterId),
    Detect,
}

/// Whether a photo's detected masks are ready for a bake.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Readiness {
    Ready,
    Pending,
}

/// The targets `masks` read, each once.
fn used_targets(masks: &[Mask]) -> impl Iterator<Item = DetectTarget> + '_ {
    DetectTarget::ALL
        .into_iter()
        .filter(move |&t| masks.iter().any(|m| m.target() == Some(t)))
}

/// The panel's mark on a detected mask.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum MaskStatus {
    Detecting,
    NothingFound,
    Failed(Failure),
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// Every photo the stand-in detectors below ran on. Per photo, since
    /// tests run in parallel and each has its own folder.
    static RUNS: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

    fn note_run(photo: &Path) {
        RUNS.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(photo.to_path_buf());
    }

    /// How many times a stand-in detector ran on `photo`.
    pub(crate) fn runs_on(photo: &Path) -> usize {
        RUNS.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|p| p.as_path() == photo)
            .count()
    }

    /// A detector that finds the left half of every photo as its subject
    /// and writes the file as the real one does.
    pub(crate) fn half_left(photo: &Path, _: DetectTarget) -> Result<Coverage, DetectError> {
        note_run(photo);
        let alpha: Vec<u8> = (0..8).map(|i| if i % 4 < 2 { 255 } else { 0 }).collect();
        let c = Coverage::fit(4, 2, &alpha).map_err(|e| DetectError::Failed(e.to_string()))?;
        crate::develop::coverage::write(photo, &c)
            .map_err(|e| DetectError::Failed(e.to_string()))?;
        Ok(c)
    }

    pub(crate) fn always_fails(photo: &Path, _: DetectTarget) -> Result<Coverage, DetectError> {
        note_run(photo);
        Err(DetectError::Failed("no model today".into()))
    }

    /// Polls `store` until `done` holds or ten seconds pass.
    pub(crate) fn drain_until(
        store: &mut CoverageStore,
        mut done: impl FnMut(&CoverageStore) -> bool,
    ) -> Vec<Landed> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut landed = Vec::new();
        while !done(store) {
            assert!(
                std::time::Instant::now() < deadline,
                "coverage work stalled"
            );
            landed.extend(store.drain());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        landed
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::test_support::*;
    use super::*;

    fn photo(tag: &str) -> (PathBuf, PathBuf) {
        let dir = crate::app::test_support::temp_folder(&format!("coverage-{tag}"));
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        (dir, photo)
    }

    #[test]
    fn a_load_overtaken_by_a_newer_id_is_dropped() {
        let (dir, p) = photo("stale");
        let old = Coverage::fit(1, 1, &[1]).unwrap();
        let new = Coverage::fit(1, 1, &[2]).unwrap();
        crate::develop::coverage::write(&p, &old).unwrap();
        crate::develop::coverage::write(&p, &new).unwrap();
        let mut store = CoverageStore::with_detector(half_left);
        let t = DetectTarget::Subject;
        assert!(matches!(store.ask(&p, t, Some(old.id())), Lookup::Pending));
        assert!(matches!(store.ask(&p, t, Some(new.id())), Lookup::Pending));
        let landed = drain_until(&mut store, |s| s.ready(&p, t).is_some());
        assert_eq!(landed.len(), 1, "the overtaken load lands nothing");
        assert_eq!(store.ready(&p, t).unwrap().id(), new.id());
        assert!(matches!(store.ask(&p, t, Some(new.id())), Lookup::Ready(_)));
        assert_eq!(store.status(&p, t, Some(new.id())), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_detection_settles_until_retry() {
        let (dir, p) = photo("failed");
        let mut store = CoverageStore::with_detector(always_fails);
        let t = DetectTarget::Subject;
        let before = runs_on(&p);
        let masks = [Mask::detected(t)];
        store.ensure_detected(&p, &masks, &Detections::default());
        store.ensure_detected(&p, &masks, &Detections::default());
        let landed = drain_until(&mut store, |s| !s.busy());
        assert_eq!(landed.len(), 1);
        assert!(matches!(landed[0].notice, Some(Notice::Failed(_))));
        assert!(matches!(landed[0].record, Record::Keep));
        for _ in 0..3 {
            assert!(matches!(store.ask(&p, t, None), Lookup::Failed));
            store.ensure_detected(&p, &masks, &Detections::default());
            assert!(store.drain().is_empty());
        }
        assert_eq!(
            runs_on(&p),
            before + 1,
            "a failed slot never re-runs on its own"
        );
        assert_eq!(
            store.status(&p, t, None),
            Some(MaskStatus::Failed(Failure::Detect("no model today".into())))
        );
        store.retry(&p, t);
        assert!(matches!(store.ask(&p, t, None), Lookup::Pending));
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(runs_on(&p), before + 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The defect: a bake that needs the pixels in memory (the browser's)
    /// got a set without the recorded coverage, since only the shown photo
    /// ever asked for a load. Readiness must start the load and say so.
    #[test]
    fn readiness_loads_a_recorded_coverage_for_a_photo_never_shown() {
        let (dir, p) = photo("never-shown");
        let c = Coverage::fit(2, 1, &[255, 0]).unwrap();
        crate::develop::coverage::write(&p, &c).unwrap();
        let mut d = Detections::default();
        d.set(DetectTarget::Sky, Some(c.id()));
        let masks = [Mask::detected(DetectTarget::Sky)];
        let mut store = CoverageStore::with_detector(half_left);
        assert!(!store.is_ready(&p, &masks, &d, true));
        assert!(
            store.is_ready(&p, &masks, &d, false),
            "a native worker reads the file itself"
        );
        assert_eq!(store.readiness(&p, &masks, &d, true), Readiness::Pending);
        assert!(store.busy(), "a load is queued");
        assert_eq!(
            store.status(&p, DetectTarget::Sky, Some(c.id())),
            Some(MaskStatus::Detecting)
        );
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(store.readiness(&p, &masks, &d, true), Readiness::Ready);
        assert!(store.is_ready(&p, &masks, &d, true));
        assert_eq!(store.ready(&p, DetectTarget::Sky).unwrap().id(), c.id());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The defect: after a folder reopened, a detected mask with no record
    /// and no slot blocked Export forever, since nothing started its
    /// detection. Readiness starts it, and a target this build can't detect
    /// settles rather than blocks.
    #[test]
    fn readiness_starts_a_detection_nothing_else_asked_for_and_settles_the_unsupported() {
        fn sky_only(t: DetectTarget) -> bool {
            t == DetectTarget::Sky
        }
        let (dir, p) = photo("reopened");
        let masks = [
            Mask::detected(DetectTarget::Sky),
            Mask::detected(DetectTarget::Subject),
        ];
        let none = Detections::default();
        let mut store = CoverageStore::with_detector_and_availability(half_left, sky_only);
        let mut all = HashMap::new();
        all.insert(p.clone(), masks.to_vec());
        let paths = vec![p.clone()];
        let unrecorded = HashMap::new();
        assert_eq!(
            store.pending_among(&paths, &all, &unrecorded),
            1,
            "the sky mask has work to do"
        );
        assert_eq!(
            store.readiness(&p, &masks, &none, false),
            Readiness::Pending
        );
        assert!(store.busy(), "the sky detection started");
        assert_eq!(
            store.status(&p, DetectTarget::Subject, None),
            Some(MaskStatus::Failed(Failure::Unsupported)),
            "the subject settled without a worker"
        );
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(store.readiness(&p, &masks, &none, false), Readiness::Ready);
        assert_eq!(
            store.pending_among(&paths, &all, &unrecorded),
            0,
            "landed sky, unsupported subject: nothing left to wait for"
        );
        assert_eq!(runs_on(&p), 1, "the unsupported target never ran");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The defect: a read that failed for a reason that has nothing to do
    /// with the file (here a directory in its place, as a permission would)
    /// forgot the recorded id, and the next open re-detected or lost the
    /// mask. Only a missing or corrupt file forgets it.
    #[test]
    fn an_io_failure_on_load_keeps_the_recorded_id_and_offers_retry() {
        let (dir, p) = photo("io");
        let c = Coverage::fit(1, 1, &[200]).unwrap();
        let name = crate::develop::coverage::file_name(std::ffi::OsStr::new("a.jpg"), c.id());
        std::fs::create_dir_all(dir.join(".lightphotos").join("masks").join(name)).unwrap();
        let mut store = CoverageStore::with_detector(half_left);
        let t = DetectTarget::Subject;
        assert!(matches!(store.ask(&p, t, Some(c.id())), Lookup::Pending));
        let landed = drain_until(&mut store, |s| !s.busy());
        assert_eq!(landed.len(), 1);
        assert!(matches!(landed[0].record, Record::Keep), "the id stays");
        assert!(matches!(
            store.status(&p, t, Some(c.id())),
            Some(MaskStatus::Failed(Failure::Read(_)))
        ));
        assert!(matches!(store.ask(&p, t, Some(c.id())), Lookup::Failed));
        assert_eq!(runs_on(&p), 0, "no re-detect over a readable-later file");

        std::fs::remove_dir_all(dir.join(".lightphotos")).unwrap();
        crate::develop::coverage::write(&p, &c).unwrap();
        store.retry(&p, t);
        assert!(matches!(store.ask(&p, t, Some(c.id())), Lookup::Pending));
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(
            store.ready(&p, t).unwrap().id(),
            c.id(),
            "Retry reads again"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Export waits for a detection that is queued or in flight, not for
    /// one that landed and is recorded, and not for one that failed.
    #[test]
    fn a_pending_detection_blocks_export_and_a_recorded_or_failed_one_does_not() {
        let (dir, p) = photo("pending");
        let mut masks = HashMap::new();
        masks.insert(
            p.clone(),
            vec![
                Mask::detected(DetectTarget::Sky),
                Mask::detected(DetectTarget::Subject),
            ],
        );
        let unrecorded = HashMap::new();
        let paths = vec![p.clone()];

        let mut store = CoverageStore::with_detector(half_left);
        assert_eq!(
            store.pending_among(&paths, &masks, &unrecorded),
            1,
            "not asked yet"
        );
        store.ensure_detected(&p, &masks[&p], &Detections::default());
        assert_eq!(store.pending_among(&paths, &masks, &unrecorded), 1);
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(
            store.pending_among(&paths, &masks, &unrecorded),
            0,
            "landed: in memory, and recorded in the same frame"
        );
        let mut d = Detections::default();
        for t in DetectTarget::ALL {
            d.set(t, store.ready(&p, t).map(|c| c.id()));
        }
        assert!(d.subject.is_some() && d.sky.is_some());
        let mut recorded = HashMap::new();
        recorded.insert(p.clone(), d);
        assert_eq!(store.pending_among(&paths, &masks, &recorded), 0);

        let mut failing = CoverageStore::with_detector(always_fails);
        failing.ensure_detected(&p, &masks[&p], &Detections::default());
        drain_until(&mut failing, |s| !s.busy());
        assert_eq!(
            failing.pending_among(&paths, &masks, &unrecorded),
            0,
            "a failed target exports as nothing, on purpose"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
