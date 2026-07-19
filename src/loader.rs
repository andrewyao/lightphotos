// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background image decoding. A pool of worker threads decodes off the UI thread
//! so the window never blocks; decoded images are cached by path so revisiting
//! prev/next is instant. We preload neighbors to make arrow-key nav feel
//! immediate.
//!
//! Two cache tiers:
//! - the full-image LRU (for the loupe), driven by `request`/`get`/`poll`.
//! - a larger thumbnail LRU (for the grid/filmstrip), driven by
//!   `request_thumb`/`get_thumb`/`poll_thumbs`, backed by the on-disk
//!   `ThumbCache`.
//!
//! Both tiers share one work queue (an `mpsc` request channel fanned out to the
//! workers via an `Arc<Mutex<Receiver>>`) and one results channel drained by the
//! poll methods, which route each result back to its tier.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::SystemTime;

use crate::image_decode::{self, DecodedImage};
use crate::thumbnail::ThumbCache;

/// A unit of work for a worker thread.
enum Job {
    /// Full-resolution decode at `max_dim` for the loupe.
    Full(PathBuf),
    /// Thumbnail of `path` whose longest side is at most `max_px`.
    // Consumed by the grid/filmstrip in a later wave (T5/T6).
    #[allow(dead_code)]
    Thumb(PathBuf, u32),
    /// Capture-time (EXIF/mtime) read for burst grouping. Lowest priority.
    Meta(PathBuf),
}

/// A finished job, carrying its tier back to the poller.
enum JobResult {
    Full(PathBuf, Result<DecodedImage, String>),
    Thumb(PathBuf, u32, Result<Arc<DecodedImage>, String>),
    Meta(PathBuf, Option<SystemTime>),
}

/// A priority work queue: full-image (loupe) jobs are always served before
/// thumbnail (grid/filmstrip) jobs. The loupe image is latency-critical and there
/// is usually just one, whereas thumbnails arrive in floods; without this
/// priority a freshly-opened image waits behind the entire thumbnail backlog
/// (seconds), leaving the magnified low-res placeholder on screen.
///
/// Queue ordering alone isn't enough: a job already popped and *executing* on a
/// worker doesn't respect this priority. When a folder is first opened, a whole
/// wave of thumbnail jobs can be mid-decode across every worker just as a full
/// image is requested, forcing it to wait for one of them to finish. See the
/// dedicated-worker reservation in `Loader::new` for how that's handled.
#[derive(Default)]
struct Queue {
    full: VecDeque<Job>,
    thumbs: VecDeque<Job>,
    /// Capture-time reads — served after full-image and thumbnail work, since
    /// burst badges are not latency-critical.
    meta: VecDeque<Job>,
    /// Set when the `Loader` is dropped so idle workers wake and exit.
    shutdown: bool,
}

/// Shared between the `Loader` and its worker threads.
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled whenever a job is enqueued or on shutdown.
    ready: Condvar,
}

/// Full-image LRU capacity (loupe tier).
const FULL_CAPACITY: usize = 16;
/// In-memory thumbnail LRU capacity (grid/filmstrip tier).
const THUMB_CAPACITY: usize = 512;

pub struct Loader {
    shared: Arc<Shared>,
    res_rx: Receiver<JobResult>,

    // Full-image tier.
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    /// Insertion order for simple LRU eviction.
    order: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    capacity: usize,

    // Thumbnail tier.
    thumb_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    thumb_order: VecDeque<(PathBuf, u32)>,
    thumb_inflight: HashSet<(PathBuf, u32)>,
    /// Keys whose thumbnail decode failed (e.g. file deleted). Negative cache so
    /// we don't re-request them every frame and spin the UI redraw loop.
    thumb_failed: HashSet<(PathBuf, u32)>,
    thumb_capacity: usize,

    /// Paths with a capture-time read in flight, to avoid enqueuing duplicates.
    meta_inflight: HashSet<PathBuf>,
}

impl Loader {
    pub fn new(max_dim: u32) -> Self {
        let (res_tx, res_rx) = std::sync::mpsc::channel::<JobResult>();

        // Shared priority work queue: each worker locks, pops one job
        // (full-image first), unlocks, then processes it (so a slow decode never
        // holds the queue).
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
        });
        let thumbs = Arc::new(ThumbCache::new());

        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = cores.saturating_sub(2).max(1);

        for i in 0..workers {
            let shared = Arc::clone(&shared);
            let res_tx = res_tx.clone();
            let thumbs = Arc::clone(&thumbs);
            // Worker 0 is reserved for full-image (loupe) and meta work only,
            // never thumbnails — but only when there's at least one other
            // worker left to service the thumbnail flood. With a single
            // worker, dedicating it would starve thumbnails entirely, which is
            // worse than the contention it's meant to fix.
            let dedicated_full = i == 0 && workers > 1;
            thread::Builder::new()
                .name(format!("decode-worker-{i}"))
                .spawn(move || loop {
                    // Lock only long enough to take one job, preferring full-image
                    // work over thumbnails. Wait on the condvar while idle.
                    let job = {
                        let mut q = match shared.queue.lock() {
                            Ok(q) => q,
                            Err(_) => return,
                        };
                        loop {
                            if q.shutdown {
                                return;
                            }
                            let next = if dedicated_full {
                                q.full.pop_front().or_else(|| q.meta.pop_front())
                            } else {
                                q.full
                                    .pop_front()
                                    .or_else(|| q.thumbs.pop_front())
                                    .or_else(|| q.meta.pop_front())
                            };
                            if let Some(j) = next {
                                break j;
                            }
                            q = match shared.ready.wait(q) {
                                Ok(q) => q,
                                Err(_) => return,
                            };
                        }
                    };

                    // Decode on a background thread can panic (e.g. an
                    // unexpected state across the ImageIO FFI boundary). Catch it
                    // so the worker survives and, crucially, so the path still
                    // gets a result and is cleared from the caller's in-flight set
                    // instead of spinning forever. AssertUnwindSafe: a panic here
                    // leaves no shared state in an observably broken condition.
                    let result = match job {
                        Job::Full(path) => {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::decode(&path, max_dim)
                            }))
                            .unwrap_or_else(|_| Err(format!("decode panicked: {}", path.display())));
                            JobResult::Full(path, r)
                        }
                        Job::Thumb(path, max_px) => {
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                thumbs.get_or_make(&path, max_px)
                            }))
                            .unwrap_or_else(|_| {
                                Err(format!("thumbnail panicked: {}", path.display()))
                            });
                            JobResult::Thumb(path, max_px, r)
                        }
                        Job::Meta(path) => {
                            // capture_time never panics by contract, but the FFI
                            // boundary is caught for parity with the other arms.
                            let t = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::capture_time(&path)
                            }))
                            .unwrap_or(None);
                            JobResult::Meta(path, t)
                        }
                    };

                    // If the UI side is gone, stop.
                    if res_tx.send(result).is_err() {
                        break;
                    }
                })
                .expect("spawn decode worker");
        }

        Self {
            shared,
            res_rx,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            capacity: FULL_CAPACITY,
            thumb_cache: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
            thumb_capacity: THUMB_CAPACITY,
            meta_inflight: HashSet::new(),
        }
    }

    // ---- Full-image tier (backward-compatible API) ----

    /// Ask a worker to decode `path` unless it's already cached or in flight.
    /// Full-image jobs jump ahead of any pending thumbnails.
    pub fn request(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.inflight.contains(&path) {
            return;
        }
        // Only record the job as in-flight once it's actually enqueued. If the
        // queue mutex is poisoned (a worker panicked), enqueuing early plus a
        // permanent `inflight` marker would strand this path as "loading forever";
        // instead we skip it and let a later `request` retry.
        if let Ok(mut q) = self.shared.queue.lock() {
            q.full.push_back(Job::Full(path.clone()));
            self.inflight.insert(path);
            // notify_all, not notify_one: the dedicated full-image worker (see
            // `Loader::new`) ignores thumbnail jobs, so a notify_one that happens
            // to wake it while only thumbnails are queued would strand them
            // asleep until some other enqueue wakes a general worker.
            self.shared.ready.notify_all();
        }
    }

    /// Drain finished decodes into the caches and return full-image arrivals.
    ///
    /// Thin wrapper over [`poll_all`](Self::poll_all): the single drain still
    /// routes thumbnail arrivals into the thumb tier, but their arrival list is
    /// discarded here. Calling both `poll` and `poll_thumbs` in the same frame
    /// starves one tier (the second call finds the queue already drained), so
    /// frame loops should prefer `poll_all`.
    // The frame loop uses `poll_all`; kept for API symmetry with the thumb tier.
    #[allow(dead_code)]
    pub fn poll(&mut self) -> Vec<PathBuf> {
        self.poll_all().0
    }

    pub fn get(&self, path: &PathBuf) -> Option<Arc<DecodedImage>> {
        self.cache.get(path).cloned()
    }

    // ---- Thumbnail tier ----
    // These are consumed by the grid/filmstrip UI in a later wave (T5/T6);
    // unused until then.

    /// Ask a worker to build a thumbnail of `path` at `max_px` unless it's
    /// already cached in memory or in flight.
    #[allow(dead_code)]
    pub fn request_thumb(&mut self, path: PathBuf, max_px: u32) {
        let key = (path.clone(), max_px);
        if self.thumb_cache.contains_key(&key)
            || self.thumb_inflight.contains(&key)
            || self.thumb_failed.contains(&key)
        {
            return;
        }
        // Mark in-flight only after a successful enqueue (see `request`): a
        // poisoned queue mutex must not strand this key as permanently loading.
        if let Ok(mut q) = self.shared.queue.lock() {
            q.thumbs.push_back(Job::Thumb(path, max_px));
            self.thumb_inflight.insert(key);
            // See the comment in `request`: must be notify_all so a general
            // (non-dedicated) worker is guaranteed to wake and pick this up.
            self.shared.ready.notify_all();
        }
    }

    /// Ask a worker to read `path`'s capture time unless already in flight.
    /// Results arrive in the third bucket of [`poll_all`](Self::poll_all).
    #[allow(dead_code)]
    pub fn request_meta(&mut self, path: PathBuf) {
        if self.meta_inflight.contains(&path) {
            return;
        }
        if let Ok(mut q) = self.shared.queue.lock() {
            q.meta.push_back(Job::Meta(path.clone()));
            self.meta_inflight.insert(path);
            // See the comment in `request` for why this must be notify_all.
            self.shared.ready.notify_all();
        }
    }

    /// In-memory thumbnail lookup keyed by `(path, max_px)`.
    #[allow(dead_code)]
    pub fn get_thumb(&self, path: &Path, max_px: u32) -> Option<Arc<DecodedImage>> {
        self.thumb_cache.get(&(path.to_path_buf(), max_px)).cloned()
    }

    /// True if this thumbnail's decode permanently failed (negative cache), so
    /// callers can stop treating it as "still loading".
    #[allow(dead_code)]
    pub fn thumb_failed(&self, path: &Path, max_px: u32) -> bool {
        self.thumb_failed.contains(&(path.to_path_buf(), max_px))
    }

    /// Drain finished jobs into the caches and return thumbnail `(path, max_px)`
    /// arrivals.
    ///
    /// Thin wrapper over [`poll_all`](Self::poll_all): the single drain still
    /// routes full-image arrivals into the full tier, but their arrival list is
    /// discarded here. Calling both `poll` and `poll_thumbs` in the same frame
    /// starves one tier (the second call finds the queue already drained), so
    /// frame loops should prefer `poll_all`.
    #[allow(dead_code)]
    pub fn poll_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        self.poll_all().1
    }

    // ---- Shared internals ----

    /// Drain every pending result exactly once, routing each into its tier
    /// (`Full` → full cache/LRU, `Thumb` → thumb cache/LRU), and return the
    /// arrivals for *both* tiers as `(full_arrivals, thumb_arrivals)`.
    ///
    /// Prefer this in frame loops: it avoids the footgun where calling `poll`
    /// and `poll_thumbs` separately makes the first call drain results destined
    /// for the other tier, starving it.
    #[allow(dead_code)]
    pub fn poll_all(
        &mut self,
    ) -> (
        Vec<PathBuf>,
        Vec<(PathBuf, u32)>,
        Vec<(PathBuf, Option<SystemTime>)>,
    ) {
        self.drain()
    }

    /// Drain every pending result, routing each into its tier. Returns the
    /// arrivals for both tiers; callers keep only the tier they care about.
    fn drain(
        &mut self,
    ) -> (
        Vec<PathBuf>,
        Vec<(PathBuf, u32)>,
        Vec<(PathBuf, Option<SystemTime>)>,
    ) {
        let mut full = vec![];
        let mut thumbs = vec![];
        let mut metas = vec![];
        while let Ok(result) = self.res_rx.try_recv() {
            match result {
                JobResult::Full(path, r) => {
                    self.inflight.remove(&path);
                    match r {
                        Ok(img) => {
                            self.insert(path.clone(), Arc::new(img));
                            full.push(path);
                        }
                        Err(e) => eprintln!("decode failed for {}: {e}", path.display()),
                    }
                }
                JobResult::Thumb(path, max_px, r) => {
                    let key = (path.clone(), max_px);
                    self.thumb_inflight.remove(&key);
                    match r {
                        Ok(img) => {
                            self.insert_thumb(key.clone(), img);
                            thumbs.push(key);
                        }
                        Err(e) => {
                            eprintln!("thumbnail failed for {} @ {max_px}: {e}", path.display());
                            self.thumb_failed.insert(key);
                        }
                    }
                }
                JobResult::Meta(path, t) => {
                    self.meta_inflight.remove(&path);
                    metas.push((path, t));
                }
            }
        }
        (full, thumbs, metas)
    }

    fn insert(&mut self, path: PathBuf, img: Arc<DecodedImage>) {
        if !self.cache.contains_key(&path) {
            self.order.push_back(path.clone());
        }
        self.cache.insert(path, img);
        while self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    fn insert_thumb(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if !self.thumb_cache.contains_key(&key) {
            self.thumb_order.push_back(key.clone());
        }
        self.thumb_cache.insert(key, img);
        while self.thumb_order.len() > self.thumb_capacity {
            if let Some(old) = self.thumb_order.pop_front() {
                self.thumb_cache.remove(&old);
            }
        }
    }
}

impl Drop for Loader {
    /// Signal idle workers (blocked on the condvar) to wake and exit.
    fn drop(&mut self) {
        if let Ok(mut q) = self.shared.queue.lock() {
            q.shutdown = true;
        }
        self.shared.ready.notify_all();
    }
}
