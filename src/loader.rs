// SPDX-License-Identifier: GPL-3.0-or-later

//! Background image decoding. A pool of worker threads decodes off the UI thread
//! so the window never blocks; decoded images are cached by path so revisiting
//! prev/next is instant. We preload neighbors to make arrow-key nav feel
//! immediate.
//!
//! Cache tiers, coarsest to finest:
//! - a thumbnail LRU (for the grid/filmstrip), driven by
//!   `request_thumb`/`get_thumb`/`poll_thumbs`, backed by the on-disk
//!   `ThumbCache`.
//! - the screen-fit view the loupe actually shows, driven by
//!   `request_preview`/`prefetch_preview`/`get_preview`. Internally two passes:
//!   a *quick* one that may return the file's embedded preview, and a *forced*
//!   decode-at-size that runs only when the quick pass came back short of the
//!   requested size. That split is what makes RAW usable — a Sony ARW's
//!   embedded preview lands in ~35ms where demosaicing to the same size takes
//!   ~250ms — while costing formats like JPEG (no embedded preview, so the
//!   quick pass already decodes at size) exactly one decode.
//! - a small full-resolution LRU (the loupe once zoomed in), driven by
//!   `request_full`/`get_full`. Never speculative — a modern camera file costs
//!   seconds and hundreds of megabytes at full resolution.
//!
//! Every tier shares one priority work queue (fanned out to the workers via an
//! `Arc<Mutex<Queue>>` plus a condvar) and one results channel drained by the
//! poll methods, which route each result back to its tier.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::SystemTime;

use crate::image_decode::{self, DecodedImage, ImageMetadata};
use crate::thumbnail::ThumbCache;

/// Whether to print decode/upload timings to stderr. Off unless
/// `LIGHTPHOTOS_TIMING=1` is set — the loupe's responsiveness is the whole point
/// of the tiering in this module, so it needs to stay measurable without a
/// profiler attached.
pub fn timing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LIGHTPHOTOS_TIMING").as_deref() == Ok("1"))
}

/// Process start, so every timing line can be stamped with time-since-launch.
/// Span durations alone hide the thing that actually matters — *when* the span
/// began. A 60 ms decode that starts 900 ms after launch still reads as a
/// second of blur.
fn launched_at() -> std::time::Instant {
    static T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
    *T0.get_or_init(std::time::Instant::now)
}

/// Stamp a timing event with milliseconds since launch.
pub fn mark(what: &str) {
    if timing_enabled() {
        eprintln!("[t+{:>7.1}ms] {what}", launched_at().elapsed().as_secs_f64() * 1000.0);
    }
}

/// Call once at process start so `t+0` means launch, not first timed event.
pub fn start_clock() {
    launched_at();
}

/// A unit of work for a worker thread.
enum Job {
    /// Whatever ImageIO can produce fastest at roughly the carried size —
    /// crucially, it is allowed to hand back the file's *embedded* preview.
    ///
    /// This is what makes RAW usable. A Sony ARW carries a 1616px JPEG preview
    /// that comes back in ~35ms, where demosaicing the raw sensor data to the
    /// same size takes ~250ms and full resolution ~750ms. Formats with no useful
    /// embedded preview degrade gracefully: a JPEG has none, so ImageIO decodes
    /// at size and this single job is already the final answer; a HEIC's is a
    /// 240px stub, so it paints something immediately and `Preview` follows.
    Quick(PathBuf, u32),
    /// Screen-fit decode of `path` capped at the carried `max_dim`, ignoring any
    /// embedded preview. Enqueued only when `Quick` came back short of the
    /// target, so formats that answered in one pass never pay for two.
    Preview(PathBuf, u32),
    /// Full-resolution decode at the carried `max_dim` (the GPU's max texture
    /// size, i.e. effectively "don't downscale"). Only requested once the user
    /// zooms in past what the preview holds — it costs seconds and hundreds of
    /// megabytes on a modern camera file, so it is never speculative.
    Full(PathBuf, u32),
    /// Thumbnail of `path` whose longest side is at most `max_px`.
    // Consumed by the grid/filmstrip in a later wave (T5/T6).
    #[allow(dead_code)]
    Thumb(PathBuf, u32),
    /// Camera/lens/exposure metadata read for the info panel. Only ever
    /// requested for the single currently-viewed image, so it outranks
    /// thumbnails but never the full-image decode itself.
    Exif(PathBuf),
    /// Capture-time (EXIF/mtime) read for burst grouping. Lowest priority.
    Meta(PathBuf),
}

/// A finished job, carrying its tier back to the poller.
enum JobResult {
    Quick(PathBuf, u32, Result<DecodedImage, String>),
    Preview(PathBuf, u32, Result<DecodedImage, String>),
    Full(PathBuf, Result<DecodedImage, String>),
    Thumb(PathBuf, u32, Result<Arc<DecodedImage>, String>),
    Exif(PathBuf, ImageMetadata),
    Meta(PathBuf, Option<SystemTime>),
}

/// A priority work queue: loupe jobs are always served before thumbnail
/// (grid/filmstrip) jobs. The loupe image is latency-critical and there is
/// usually just one, whereas thumbnails arrive in floods; without this priority
/// a freshly-opened image waits behind the entire thumbnail backlog (seconds),
/// leaving the magnified low-res placeholder on screen.
///
/// Within the loupe tiers, previews outrank full-resolution decodes. A full
/// decode takes seconds; if it were served first, stepping to the next photo
/// would queue that photo's (fast) preview behind the previous photo's (slow)
/// full-res pass, and arrow-key navigation would crawl.
///
/// Queue ordering alone isn't enough: a job already popped and *executing* on a
/// worker doesn't respect this priority. When a folder is first opened, a whole
/// wave of thumbnail jobs can be mid-decode across every worker just as a
/// preview is requested, forcing it to wait for one of them to finish. See the
/// dedicated-worker reservation in `Loader::new` for how that's handled.
#[derive(Default)]
struct Queue {
    quick: VecDeque<Job>,
    preview: VecDeque<Job>,
    full: VecDeque<Job>,
    thumbs: VecDeque<Job>,
    /// Metadata reads for the currently-viewed image — served right after
    /// full-image work, ahead of the thumbnail flood, since it's about the
    /// one photo the user is actively looking at.
    exif: VecDeque<Job>,
    /// Capture-time reads — served after full-image and thumbnail work, since
    /// burst badges are not latency-critical.
    meta: VecDeque<Job>,
    /// Set when the `Loader` is dropped so idle workers wake and exit.
    shutdown: bool,
}

impl Queue {
    /// Take the highest-priority job this worker is allowed to run, or `None`
    /// when there is nothing for it to do.
    ///
    /// `dedicated` is worker 0's reservation: it declines thumbnail work
    /// entirely so a loupe decode never has to wait for a thumbnail that is
    /// already mid-flight on every worker (queue order alone can't preempt a
    /// job that has already been popped).
    fn take_next(&mut self, dedicated: bool) -> Option<Job> {
        self.quick
            .pop_front()
            .or_else(|| self.preview.pop_front())
            .or_else(|| self.full.pop_front())
            .or_else(|| self.exif.pop_front())
            .or_else(|| {
                if dedicated {
                    None
                } else {
                    self.thumbs.pop_front()
                }
            })
            .or_else(|| self.meta.pop_front())
    }
}

/// Print how long a loupe decode took, when timing is enabled.
fn report_decode(
    tier: &str,
    path: &Path,
    target: u32,
    started: std::time::Instant,
    result: &Result<DecodedImage, String>,
) {
    if !timing_enabled() {
        return;
    }
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    match result {
        Ok(img) => mark(&format!(
            "decode {tier} {name}: took {:?} -> {}x{} (target {target})",
            started.elapsed(),
            img.width,
            img.height,
        )),
        Err(e) => mark(&format!(
            "decode {tier} {name}: failed after {:?} ({e})",
            started.elapsed()
        )),
    }
}

/// Shared between the `Loader` and its worker threads.
struct Shared {
    queue: Mutex<Queue>,
    /// Signalled whenever a job is enqueued or on shutdown.
    ready: Condvar,
}

/// Full-image LRU capacity (loupe tier). Deliberately tiny: one entry is the
/// whole image as RGBA8, so a 45 MP file costs ~180 MB. Full-resolution decodes
/// are only requested when the user zooms past what the preview holds, so there
/// is no navigation benefit to retaining more.
const FULL_CAPACITY: usize = 3;
/// Screen-fit preview LRU capacity (loupe tier). These are ~5 MB each, so the
/// budget buys the current photo plus a comfortable run of neighbors in both
/// directions for instant arrow-key stepping.
const PREVIEW_CAPACITY: usize = 8;
/// In-memory thumbnail LRU capacity (grid/filmstrip tier).
const THUMB_CAPACITY: usize = 512;

pub struct Loader {
    shared: Arc<Shared>,
    res_rx: Receiver<JobResult>,

    /// Decode cap for full-resolution jobs — the GPU's max texture dimension,
    /// so `fit_within` only ever kicks in for images too large to upload.
    full_target: u32,

    // Full-image tier.
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    /// Insertion order for simple LRU eviction.
    order: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    capacity: usize,

    // Screen-fit preview tier, keyed by `(path, target_px)` so a window resize
    // that changes the target doesn't silently serve a stale, smaller decode.
    preview_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    preview_order: VecDeque<(PathBuf, u32)>,
    preview_inflight: HashSet<(PathBuf, u32)>,
    preview_capacity: usize,

    // The `Quick` half of the preview tier: same key, but holding whatever came
    // back fastest (often a file's embedded preview). Kept in its own map so a
    // short quick result can be shown immediately *and* replaced in place when
    // the full-quality preview lands behind it.
    quick_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    quick_order: VecDeque<(PathBuf, u32)>,
    quick_inflight: HashSet<(PathBuf, u32)>,
    /// Keys the user has actually looked at, and which therefore deserve the
    /// forced decode if their quick pass came back short. Prefetched neighbors
    /// are absent from this set until they become the current photo.
    escalation_wanted: HashSet<(PathBuf, u32)>,

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

    /// Paths with an exif-metadata read in flight, to avoid enqueuing duplicates.
    exif_inflight: HashSet<PathBuf>,
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
            // Worker 0 is reserved for loupe (preview/full), exif, and meta work
            // only, never thumbnails — but only when there's at least one other
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
                            if let Some(j) = q.take_next(dedicated_full) {
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
                        Job::Quick(path, target) => {
                            let t0 = std::time::Instant::now();
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                crate::thumbnail::decode_at_size(
                                    &path,
                                    target,
                                    crate::thumbnail::EmbeddedPreview::UseIfPresent,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(format!("quick decode panicked: {}", path.display()))
                            });
                            report_decode("quick", &path, target, t0, &r);
                            JobResult::Quick(path, target, r)
                        }
                        Job::Preview(path, target) => {
                            let t0 = std::time::Instant::now();
                            // Decode-at-size, not decode-then-shrink:
                            // `image_decode::decode` would expand the full image
                            // first and only then draw it down, which costs
                            // *more* than not downscaling at all and would make
                            // this tier pointless.
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                crate::thumbnail::decode_at_size(
                                    &path,
                                    target,
                                    crate::thumbnail::EmbeddedPreview::Never,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(format!("preview decode panicked: {}", path.display()))
                            });
                            report_decode("preview", &path, target, t0, &r);
                            JobResult::Preview(path, target, r)
                        }
                        Job::Full(path, target) => {
                            let t0 = std::time::Instant::now();
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::decode(&path, target)
                            }))
                            .unwrap_or_else(|_| {
                                Err(format!("decode panicked: {}", path.display()))
                            });
                            report_decode("full", &path, target, t0, &r);
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
                        Job::Exif(path) => {
                            let m = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::read_metadata(&path)
                            }))
                            .unwrap_or_default();
                            JobResult::Exif(path, m)
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
            full_target: max_dim,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            capacity: FULL_CAPACITY,
            preview_cache: HashMap::new(),
            preview_order: VecDeque::new(),
            preview_inflight: HashSet::new(),
            preview_capacity: PREVIEW_CAPACITY,
            quick_cache: HashMap::new(),
            quick_order: VecDeque::new(),
            quick_inflight: HashSet::new(),
            escalation_wanted: HashSet::new(),
            thumb_cache: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
            thumb_capacity: THUMB_CAPACITY,
            meta_inflight: HashSet::new(),
            exif_inflight: HashSet::new(),
        }
    }

    // ---- Loupe tiers: screen-fit preview, then full resolution ----

    /// Ask for a screen-fit view of `path` at `target_px`, unless one is already
    /// cached or in flight. This is what the loupe shows first; it outranks
    /// every other kind of work.
    ///
    /// Callers say *what they want to see*, not how to decode it. Internally
    /// this starts with the fast [`Job::Quick`] pass and escalates to a forced
    /// [`Job::Preview`] decode only if that came back short of `target_px` —
    /// see `escalate_if_short`.
    pub fn request_preview(&mut self, path: PathBuf, target_px: u32) {
        self.enqueue_quick(path, target_px, true);
    }

    /// Same, but for a photo the user hasn't navigated to yet: fetch only the
    /// cheap pass and stop there. Whatever the file's embedded preview offers is
    /// enough to make stepping onto it feel instant, and the moment it *is* the
    /// current photo, `request_preview` escalates it.
    pub fn prefetch_preview(&mut self, path: PathBuf, target_px: u32) {
        self.enqueue_quick(path, target_px, false);
    }

    fn enqueue_quick(&mut self, path: PathBuf, target_px: u32, escalate: bool) {
        let key = (path.clone(), target_px);
        // Record the intent *before* any early return. A photo can be prefetched
        // first and viewed a moment later, while its cheap pass is still in
        // flight; without this the escalation would be dropped and the loupe
        // would sit on an embedded preview forever.
        if escalate {
            self.escalation_wanted.insert(key.clone());
        }
        if self.preview_cache.contains_key(&key) || self.preview_inflight.contains(&key) {
            return;
        }
        // Already have the cheap pass. Nothing more to queue unless this is now
        // the photo being *viewed* and that pass fell short — the prefetch path
        // deliberately leaves that undone.
        if let Some(img) = self.quick_cache.get(&key) {
            if escalate {
                let longest = img.width.max(img.height);
                self.escalate_if_short(&path, target_px, longest);
            }
            return;
        }
        if self.quick_inflight.contains(&key) {
            return;
        }
        // Only record the job as in-flight once it's actually enqueued. If the
        // queue mutex is poisoned (a worker panicked), enqueuing early plus a
        // permanent in-flight marker would strand this path as "loading forever";
        // instead we skip it and let a later request retry.
        if let Ok(mut q) = self.shared.queue.lock() {
            q.quick.push_back(Job::Quick(path, target_px));
            self.quick_inflight.insert(key);
            // notify_all, not notify_one: the dedicated loupe worker (see
            // `Loader::new`) ignores thumbnail jobs, so a notify_one that happens
            // to wake it while only thumbnails are queued would strand them
            // asleep until some other enqueue wakes a general worker.
            self.shared.ready.notify_all();
        }
    }

    /// After a `Quick` result lands, queue the forced decode if what came back
    /// doesn't already reach the requested size.
    ///
    /// This is the whole point of the two-pass split: a JPEG's quick pass *is* a
    /// decode-at-size and returns the full target, so nothing more is queued and
    /// it costs exactly one decode. A RAW's quick pass returns its embedded
    /// preview — good enough to put on screen in 35ms, but short of the target,
    /// so the real decode follows behind it.
    /// Apply the escalation policy to a landed `Quick` result: only photos the
    /// user has actually asked to *see* get the forced decode. Split out from
    /// `drain` so the policy has one home and can be exercised directly.
    fn escalate_from_quick(&mut self, path: &Path, target_px: u32, got_longest: u32) {
        if self
            .escalation_wanted
            .contains(&(path.to_path_buf(), target_px))
        {
            self.escalate_if_short(path, target_px, got_longest);
        }
    }

    fn escalate_if_short(&mut self, path: &Path, target_px: u32, got_longest: u32) {
        if got_longest >= target_px {
            return;
        }
        let key = (path.to_path_buf(), target_px);
        if self.preview_cache.contains_key(&key) || self.preview_inflight.contains(&key) {
            return;
        }
        if let Ok(mut q) = self.shared.queue.lock() {
            q.preview
                .push_back(Job::Preview(path.to_path_buf(), target_px));
            self.preview_inflight.insert(key);
            self.shared.ready.notify_all();
        }
    }

    /// Ask a worker for the full-resolution decode of `path` unless it's already
    /// cached or in flight. Reserved for the moment the user zooms past what the
    /// preview holds — this is the expensive tier.
    pub fn request_full(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.inflight.contains(&path) {
            return;
        }
        // See `request_preview` for why the in-flight marker follows the enqueue.
        if let Ok(mut q) = self.shared.queue.lock() {
            q.full.push_back(Job::Full(path.clone(), self.full_target));
            self.inflight.insert(path);
            self.shared.ready.notify_all();
        }
    }

    /// True while any loupe decode (either tier) is still running. The frame loop
    /// uses this to keep polling instead of sleeping on `ControlFlow::Wait`:
    /// a worker finishing a decode does not wake winit by itself, so without
    /// this a finished image sits in the results channel — and the blurry
    /// placeholder stays on screen — until some unrelated event arrives.
    pub fn has_pending_image(&self) -> bool {
        !self.inflight.is_empty()
            || !self.preview_inflight.is_empty()
            || !self.quick_inflight.is_empty()
    }

    /// Drain finished decodes into the caches and return loupe-image arrivals.
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

    /// The full-resolution decode of `path`, if it has landed.
    pub fn get_full(&self, path: &Path) -> Option<Arc<DecodedImage>> {
        self.cache.get(path).cloned()
    }

    /// The best screen-fit view of `path` at `target_px` that has landed: the
    /// forced decode if it's finished, otherwise the quick pass. Returns `None`
    /// only while both are still outstanding.
    pub fn get_preview(&self, path: &Path, target_px: u32) -> Option<Arc<DecodedImage>> {
        let key = (path.to_path_buf(), target_px);
        self.preview_cache
            .get(&key)
            .or_else(|| self.quick_cache.get(&key))
            .cloned()
    }

    /// The best loupe-quality decode available for `path`: full resolution if
    /// it's been fetched, otherwise the screen-fit preview. Callers that sample
    /// pixels by UV (the touch-up picker) don't care which tier they get.
    pub fn get_best(&self, path: &Path, target_px: u32) -> Option<Arc<DecodedImage>> {
        self.get_full(path)
            .or_else(|| self.get_preview(path, target_px))
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

    /// Ask a worker to read `path`'s camera/lens/exposure metadata unless
    /// already in flight. Results arrive in the fourth bucket of
    /// [`poll_all`](Self::poll_all). Intended to be called only for the
    /// single currently-viewed image, not swept over a whole folder.
    pub fn request_exif(&mut self, path: PathBuf) {
        if self.exif_inflight.contains(&path) {
            return;
        }
        if let Ok(mut q) = self.shared.queue.lock() {
            q.exif.push_back(Job::Exif(path.clone()));
            self.exif_inflight.insert(path);
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
    /// (`Preview`/`Full` → their loupe caches, `Thumb` → thumb cache/LRU), and
    /// return the arrivals for *both* tiers as `(loupe_arrivals,
    /// thumb_arrivals, ...)`. Preview and full arrivals share one list: every
    /// caller uses it only to decide "something for the loupe landed, reconcile
    /// what's on screen", and `try_show` picks the best tier itself.
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
        Vec<(PathBuf, ImageMetadata)>,
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
        Vec<(PathBuf, ImageMetadata)>,
    ) {
        let mut full = vec![];
        let mut thumbs = vec![];
        let mut metas = vec![];
        let mut exifs = vec![];
        while let Ok(result) = self.res_rx.try_recv() {
            match result {
                JobResult::Quick(path, target, r) => {
                    let key = (path.clone(), target);
                    self.quick_inflight.remove(&key);
                    match r {
                        Ok(img) => {
                            let longest = img.width.max(img.height);
                            self.insert_quick(key, Arc::new(img));
                            // Only the photo being viewed escalates; a
                            // prefetched neighbor stops at what it got.
                            self.escalate_from_quick(&path, target, longest);
                            full.push(path);
                        }
                        Err(e) => {
                            eprintln!("quick decode failed for {}: {e}", path.display());
                            // The quick pass is an optimization, not the only
                            // way to get pixels — fall through to the forced
                            // decode rather than leaving the loupe on its
                            // thumbnail forever. Do this even for a prefetch:
                            // a failure here means there is no cached answer at
                            // all, which is different from having a short one.
                            self.escalate_if_short(&path, target, 0);
                        }
                    }
                }
                JobResult::Preview(path, target, r) => {
                    let key = (path.clone(), target);
                    self.preview_inflight.remove(&key);
                    match r {
                        Ok(img) => {
                            self.insert_preview(key, Arc::new(img));
                            full.push(path);
                        }
                        Err(e) => eprintln!("preview decode failed for {}: {e}", path.display()),
                    }
                }
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
                JobResult::Exif(path, m) => {
                    self.exif_inflight.remove(&path);
                    exifs.push((path, m));
                }
                JobResult::Meta(path, t) => {
                    self.meta_inflight.remove(&path);
                    metas.push((path, t));
                }
            }
        }
        (full, thumbs, metas, exifs)
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

    fn insert_quick(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if !self.quick_cache.contains_key(&key) {
            self.quick_order.push_back(key.clone());
        }
        self.quick_cache.insert(key, img);
        // Shares the preview tier's budget: these are the same photos at
        // roughly the same sizes, and one is superseded by the other.
        while self.quick_order.len() > self.preview_capacity {
            if let Some(old) = self.quick_order.pop_front() {
                self.quick_cache.remove(&old);
            }
        }
    }

    fn insert_preview(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if !self.preview_cache.contains_key(&key) {
            self.preview_order.push_back(key.clone());
        }
        self.preview_cache.insert(key, img);
        while self.preview_order.len() > self.preview_capacity {
            if let Some(old) = self.preview_order.pop_front() {
                self.preview_cache.remove(&old);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> PathBuf {
        PathBuf::from(format!("/nonexistent/{name}.jpg"))
    }

    fn label(job: &Job) -> &'static str {
        match job {
            Job::Quick(..) => "quick",
            Job::Preview(..) => "preview",
            Job::Full(..) => "full",
            Job::Exif(_) => "exif",
            Job::Thumb(..) => "thumb",
            Job::Meta(_) => "meta",
        }
    }

    /// One job of every kind, enqueued in the *opposite* of priority order so a
    /// queue that merely preserved insertion order would fail this.
    fn every_kind() -> Queue {
        let mut q = Queue::default();
        q.meta.push_back(Job::Meta(path("m")));
        q.thumbs.push_back(Job::Thumb(path("t"), 192));
        q.exif.push_back(Job::Exif(path("e")));
        q.full.push_back(Job::Full(path("f"), 16384));
        q.preview.push_back(Job::Preview(path("p"), 2048));
        q.quick.push_back(Job::Quick(path("q"), 2048));
        q
    }

    fn drain_labels(q: &mut Queue, dedicated: bool) -> Vec<&'static str> {
        let mut out = vec![];
        while let Some(job) = q.take_next(dedicated) {
            out.push(label(&job));
        }
        out
    }

    #[test]
    fn a_general_worker_serves_every_tier_in_priority_order() {
        assert_eq!(
            drain_labels(&mut every_kind(), false),
            ["quick", "preview", "full", "exif", "thumb", "meta"]
        );
    }

    #[test]
    fn the_dedicated_worker_skips_thumbnails_entirely() {
        // Worker 0 must never pick up thumbnail work, however long the loupe
        // queues have been empty — that reservation is what stops a freshly
        // opened photo waiting on a folder's worth of in-flight thumbnails.
        assert_eq!(
            drain_labels(&mut every_kind(), true),
            ["quick", "preview", "full", "exif", "meta"]
        );
    }

    #[test]
    fn a_preview_outranks_a_full_decode_already_queued() {
        // The ordering that makes arrow-key navigation usable: a full-resolution
        // decode takes seconds, so the next photo's preview must not queue
        // behind the previous photo's full-res pass.
        let mut q = Queue::default();
        q.full.push_back(Job::Full(path("previous"), 16384));
        q.preview.push_back(Job::Preview(path("next"), 2048));
        assert_eq!(drain_labels(&mut q, false), ["preview", "full"]);
    }

    #[test]
    fn an_empty_queue_yields_nothing() {
        assert!(Queue::default().take_next(false).is_none());
        assert!(Queue::default().take_next(true).is_none());
    }

    fn image(w: u32, h: u32) -> Arc<DecodedImage> {
        Arc::new(DecodedImage {
            width: w,
            height: h,
            rgba: vec![0; (w * h * 4) as usize],
        })
    }

    #[test]
    fn the_full_tier_evicts_oldest_first_at_capacity() {
        let mut loader = Loader::new(16384);
        for i in 0..FULL_CAPACITY + 2 {
            loader.insert(path(&i.to_string()), image(1, 1));
        }
        assert_eq!(loader.cache.len(), FULL_CAPACITY);
        // The two oldest are gone; the newest survive.
        assert!(loader.get_full(&path("0")).is_none());
        assert!(loader.get_full(&path("1")).is_none());
        assert!(loader.get_full(&path(&(FULL_CAPACITY + 1).to_string())).is_some());
    }

    #[test]
    fn the_preview_tier_keys_on_target_so_a_resize_does_not_serve_a_stale_size() {
        let mut loader = Loader::new(16384);
        loader.insert_preview((path("a"), 2048), image(2048, 1365));
        assert!(loader.get_preview(&path("a"), 2048).is_some());
        // A window resize asks for a different target: that is a cache miss, not
        // a silent fallback to the smaller decode.
        assert!(loader.get_preview(&path("a"), 2560).is_none());
    }

    #[test]
    fn get_best_prefers_full_resolution_over_the_preview() {
        let mut loader = Loader::new(16384);
        loader.insert_preview((path("a"), 2048), image(2048, 1365));
        assert_eq!(loader.get_best(&path("a"), 2048).unwrap().width, 2048);
        loader.insert(path("a"), image(8192, 5464));
        assert_eq!(loader.get_best(&path("a"), 2048).unwrap().width, 8192);
    }

    /// How many forced decodes are outstanding.
    ///
    /// Deliberately reads the in-flight set rather than the queue: `Loader::new`
    /// starts real workers, and they pop queued jobs the instant they're
    /// notified, so a queue-length assertion races them. The in-flight marker is
    /// only cleared by `drain`, which is the test thread's own call.
    fn queued_previews(loader: &Loader) -> usize {
        loader.preview_inflight.len()
    }

    #[test]
    fn a_quick_pass_that_already_hit_the_target_costs_only_one_decode() {
        // The JPEG case: ImageIO has no embedded preview to hand back, so the
        // quick pass decodes at size and *is* the answer. Queueing the forced
        // decode too would double the work for identical pixels.
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 2560);
        assert_eq!(queued_previews(&loader), 0);
        // Overshooting counts as hitting it too.
        loader.escalate_if_short(&path("b"), 2560, 4096);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn a_short_quick_pass_queues_the_forced_decode_behind_it() {
        // The RAW case: a 1616px embedded preview goes on screen immediately,
        // and the real 2560px decode follows.
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
        assert!(loader.has_pending_image());
    }

    #[test]
    fn a_failed_quick_pass_still_falls_through_to_the_forced_decode() {
        // Otherwise a file whose embedded preview is corrupt would sit on its
        // thumbnail forever with nothing else queued.
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 0);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn escalation_does_not_pile_up_duplicate_jobs() {
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn a_prefetched_neighbor_stops_at_the_cheap_pass() {
        // Escalating prefetches is what put three forced RAW decodes on the pool
        // at once and tripled the time for the photo on screen to sharpen.
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("neighbor"), 2560);
        loader.insert_quick((path("neighbor"), 2560), image(1616, 1080));
        loader.escalate_from_quick(&path("neighbor"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn navigating_onto_a_prefetched_photo_escalates_it_after_all() {
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("a"), 2560);
        loader.insert_quick((path("a"), 2560), image(1616, 1080));
        // The user arrows onto it: now the short embedded preview isn't enough.
        loader.request_preview(path("a"), 2560);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn viewing_a_photo_whose_prefetch_is_still_running_does_not_lose_the_escalation() {
        // The race that a plain flag-on-the-job would drop: prefetch dispatched,
        // user arrows onto it before the cheap pass lands, and the intent has to
        // survive until the result arrives.
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("a"), 2560);
        loader.request_preview(path("a"), 2560); // still in flight
        loader.quick_inflight.remove(&(path("a"), 2560));
        loader.insert_quick((path("a"), 2560), image(1616, 1080));
        loader.escalate_from_quick(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn the_preview_getter_prefers_the_forced_decode_over_the_quick_pass() {
        let mut loader = Loader::new(16384);
        loader.insert_quick((path("a"), 2560), image(1616, 1080));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 1616);
        // Once the sharper one lands it wins, and that difference in size is
        // what tells the app to re-upload.
        loader.insert_preview((path("a"), 2560), image(2560, 1707));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 2560);
    }

    #[test]
    fn requesting_a_preview_that_is_already_answered_enqueues_nothing() {
        let mut loader = Loader::new(16384);
        loader.insert_quick((path("a"), 2560), image(2560, 1707));
        loader.request_preview(path("a"), 2560);
        assert!(!loader.has_pending_image());
    }

    #[test]
    fn pending_image_tracks_both_loupe_tiers_and_ignores_the_others() {
        let mut loader = Loader::new(16384);
        assert!(!loader.has_pending_image());

        // Thumbnail and metadata work is not what the loupe is waiting on, so it
        // must not hold the frame loop awake.
        loader.thumb_inflight.insert((path("t"), 192));
        loader.meta_inflight.insert(path("m"));
        assert!(!loader.has_pending_image());

        loader.preview_inflight.insert((path("p"), 2048));
        assert!(loader.has_pending_image());
        loader.preview_inflight.clear();
        assert!(!loader.has_pending_image());

        loader.inflight.insert(path("f"));
        assert!(loader.has_pending_image());
    }
}
