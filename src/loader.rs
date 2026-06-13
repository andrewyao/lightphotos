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
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

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
}

/// A finished job, carrying its tier back to the poller.
enum JobResult {
    Full(PathBuf, Result<DecodedImage, String>),
    Thumb(PathBuf, u32, Result<Arc<DecodedImage>, String>),
}

/// Full-image LRU capacity (loupe tier).
const FULL_CAPACITY: usize = 16;
/// In-memory thumbnail LRU capacity (grid/filmstrip tier).
const THUMB_CAPACITY: usize = 512;

pub struct Loader {
    req_tx: Sender<Job>,
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
    thumb_capacity: usize,
}

impl Loader {
    pub fn new(max_dim: u32) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<Job>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<JobResult>();

        // Shared work queue: each worker locks, recvs one job, unlocks, then
        // processes it (so a slow decode doesn't hold the queue).
        let req_rx = Arc::new(Mutex::new(req_rx));
        let thumbs = Arc::new(ThumbCache::new());

        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = cores.saturating_sub(2).max(1);

        for i in 0..workers {
            let req_rx = Arc::clone(&req_rx);
            let res_tx = res_tx.clone();
            let thumbs = Arc::clone(&thumbs);
            thread::Builder::new()
                .name(format!("decode-worker-{i}"))
                .spawn(move || loop {
                    // Lock only long enough to take one job.
                    let job = {
                        let rx = match req_rx.lock() {
                            Ok(rx) => rx,
                            Err(_) => break,
                        };
                        match rx.recv() {
                            Ok(job) => job,
                            // Sender dropped: the Loader is gone.
                            Err(_) => break,
                        }
                    };

                    let result = match job {
                        Job::Full(path) => {
                            let r = image_decode::decode(&path, max_dim);
                            JobResult::Full(path, r)
                        }
                        Job::Thumb(path, max_px) => {
                            let r = thumbs.get_or_make(&path, max_px);
                            JobResult::Thumb(path, max_px, r)
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
            req_tx,
            res_rx,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            capacity: FULL_CAPACITY,
            thumb_cache: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            thumb_capacity: THUMB_CAPACITY,
        }
    }

    // ---- Full-image tier (backward-compatible API) ----

    /// Ask a worker to decode `path` unless it's already cached or in flight.
    pub fn request(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.inflight.contains(&path) {
            return;
        }
        self.inflight.insert(path.clone());
        let _ = self.req_tx.send(Job::Full(path));
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
        if self.thumb_cache.contains_key(&key) || self.thumb_inflight.contains(&key) {
            return;
        }
        self.thumb_inflight.insert(key);
        let _ = self.req_tx.send(Job::Thumb(path, max_px));
    }

    /// In-memory thumbnail lookup keyed by `(path, max_px)`.
    #[allow(dead_code)]
    pub fn get_thumb(&self, path: &Path, max_px: u32) -> Option<Arc<DecodedImage>> {
        self.thumb_cache.get(&(path.to_path_buf(), max_px)).cloned()
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
    pub fn poll_all(&mut self) -> (Vec<PathBuf>, Vec<(PathBuf, u32)>) {
        self.drain()
    }

    /// Drain every pending result, routing each into its tier. Returns the
    /// arrivals for both tiers; callers keep only the tier they care about.
    fn drain(&mut self) -> (Vec<PathBuf>, Vec<(PathBuf, u32)>) {
        let mut full = vec![];
        let mut thumbs = vec![];
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
                            eprintln!("thumbnail failed for {} @ {max_px}: {e}", path.display())
                        }
                    }
                }
            }
        }
        (full, thumbs)
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
