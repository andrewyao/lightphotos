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

// Only a detection lands with a notice, and the browser has no detector.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
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
    /// Loads the app must run for the store: File System Access is
    /// main-thread only, so the app reads each file as a task and sends
    /// the result back through `web_landings`.
    #[cfg(target_arch = "wasm32")]
    wanted_loads: Vec<(PathBuf, DetectTarget, RasterId)>,
    #[cfg(target_arch = "wasm32")]
    web_landings: (
        std::sync::mpsc::Sender<WebLanding>,
        std::sync::mpsc::Receiver<WebLanding>,
    ),
    /// Jobs submitted and not yet drained, so the frame loop keeps polling.
    in_flight: usize,
}

/// A coverage file the app read for the store, on the web.
#[cfg(target_arch = "wasm32")]
pub(crate) type WebLanding = (
    PathBuf,
    DetectTarget,
    RasterId,
    Result<Coverage, CoverageError>,
);

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
                web_landings: std::sync::mpsc::channel(),
                in_flight: 0,
            }
        }
    }

    /// The files the app must read for the store, each once.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn take_wanted_loads(&mut self) -> Vec<(PathBuf, DetectTarget, RasterId)> {
        std::mem::take(&mut self.wanted_loads)
    }

    /// Where the app sends what it read.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn web_landing_sender(&self) -> std::sync::mpsc::Sender<WebLanding> {
        self.web_landings.0.clone()
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn with_detector(detector: Detector) -> Self {
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
        for target in DetectTarget::ALL {
            let key = (photo.to_path_buf(), target);
            if masks.iter().any(|m| m.target() == Some(target))
                && detections.get(target).is_none()
                && !self.slots.contains_key(&key)
            {
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
            None if recorded.is_none() && !detect::available(target) => {
                Some(MaskStatus::Failed(Failure::Unsupported))
            }
            None => Some(MaskStatus::Detecting),
        }
    }

    /// How many of `paths` have a detected mask whose target is not yet
    /// recorded and not settled as failed: the export blocker. A recorded
    /// target counts as done even while it loads, since a worker reads the
    /// file itself.
    pub(crate) fn pending_among(
        &self,
        paths: &[PathBuf],
        masks: &HashMap<PathBuf, Vec<Mask>>,
        detections: &HashMap<PathBuf, Detections>,
    ) -> usize {
        paths
            .iter()
            .filter(|p| {
                let Some(masks) = masks.get(*p) else {
                    return false;
                };
                let recorded = detections.get(*p).copied().unwrap_or_default();
                DetectTarget::ALL.iter().any(|&t| {
                    masks.iter().any(|m| m.target() == Some(t))
                        && recorded.get(t).is_none()
                        && !matches!(self.slots.get(&(p.to_path_buf(), t)), Some(Slot::Failed(_)))
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
            while let Ok((photo, target, id, result)) = self.web_landings.1.try_recv() {
                self.in_flight = self.in_flight.saturating_sub(1);
                let key = (photo, target);
                if !matches!(self.slots.get(&key), Some(Slot::Loading(l)) if *l == id) {
                    continue;
                }
                let record = match result {
                    Ok(coverage) => {
                        self.set_ready(&key, Arc::new(coverage));
                        Record::Keep
                    }
                    // The browser has no detector to fall back on.
                    Err(_) => {
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
        }
        #[cfg(not(target_arch = "wasm32"))]
        {
            for (job, result) in self.detect.poll() {
                self.in_flight = self.in_flight.saturating_sub(1);
                let key = (job.photo, job.target);
                if !matches!(self.slots.get(&key), Some(Slot::Detecting)) {
                    continue;
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
                out.push(Landed {
                    photo: key.0,
                    target: key.1,
                    record,
                    notice,
                });
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
                        if matches!(e, CoverageError::Corrupt(_)) {
                            // Content-addressed, so a re-detect would skip
                            // writing over the bad file.
                            crate::develop::coverage::remove(&key.0, job.id);
                        }
                        if detect::available(key.1) {
                            self.slots.remove(&key);
                            self.start_detect(key.clone());
                        } else {
                            self.slots
                                .insert(key.clone(), Slot::Failed(Failure::Missing));
                        }
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
        }
        out
    }

    fn start_detect(&mut self, key: (PathBuf, DetectTarget)) -> Lookup {
        if !detect::available(key.1) {
            self.slots.insert(key, Slot::Failed(Failure::Unsupported));
            return Lookup::Failed;
        }
        #[cfg(not(target_arch = "wasm32"))]
        let submitted = self.detect.submit(DetectJob {
            photo: key.0.clone(),
            target: key.1,
        });
        // No detector is available in the browser, so this is never reached.
        #[cfg(target_arch = "wasm32")]
        let submitted = false;
        if submitted {
            self.in_flight += 1;
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

    #[test]
    fn an_unsupported_target_fails_without_a_worker_and_a_pending_one_blocks_export() {
        let (dir, p) = photo("sky");
        let mut store = CoverageStore::with_detector(half_left);
        assert!(matches!(
            store.ask(&p, DetectTarget::Sky, None),
            Lookup::Failed
        ));
        let mut masks = HashMap::new();
        masks.insert(
            p.clone(),
            vec![
                Mask::detected(DetectTarget::Sky),
                Mask::detected(DetectTarget::Subject),
            ],
        );
        let detections = HashMap::new();
        let paths = vec![p.clone()];
        assert_eq!(
            store.pending_among(&paths, &masks, &detections),
            1,
            "subject not asked yet"
        );
        store.ensure_detected(&p, &masks[&p], &Detections::default());
        drain_until(&mut store, |s| !s.busy());
        assert_eq!(
            store.pending_among(&paths, &masks, &detections),
            1,
            "landed but not yet recorded"
        );
        let mut recorded = HashMap::new();
        let mut d = Detections::default();
        d.set(
            DetectTarget::Subject,
            store.ready(&p, DetectTarget::Subject).map(|c| c.id()),
        );
        recorded.insert(p.clone(), d);
        assert_eq!(
            store.pending_among(&paths, &masks, &recorded),
            0,
            "sky failed, subject recorded"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
