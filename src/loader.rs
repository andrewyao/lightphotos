// SPDX-License-Identifier: GPL-3.0-or-later

//! Background image decoding and the decoded-image caches. Worker threads
//! decode off the UI thread. Results land in one of three LRU caches:
//! thumbnails (grid and filmstrip), the screen-fit preview the loupe shows, and
//! a small full-resolution cache used once the loupe zooms in.
//!
//! All tiers share one priority queue (see `Queue`) and one results channel
//! that `poll_all` drains. On wasm32 there are no OS threads, so
//! `web/web_worker_pool.rs` decodes in Web Workers and feeds the same caches
//! through the `*_external` methods.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::SystemTime;

use crate::image_decode::{self, DecodedImage, ImageMetadata};
use crate::thumbnail::ThumbCache;

/// True when `LIGHTPHOTOS_TIMING=1`, which prints decode and upload timings to
/// stderr.
pub fn timing_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("LIGHTPHOTOS_TIMING").as_deref() == Ok("1"))
}

/// Process start time, so timing lines show when each span began, not only
/// how long it took.
///
/// Uses `web_time::Instant` because `std::time::Instant::now()` panics on
/// wasm32-unknown-unknown. On native targets `web_time` is `std::time`.
fn launched_at() -> web_time::Instant {
    static T0: std::sync::OnceLock<web_time::Instant> = std::sync::OnceLock::new();
    *T0.get_or_init(web_time::Instant::now)
}

/// Stamp a timing event with milliseconds since launch.
pub fn mark(what: &str) {
    if timing_enabled() {
        eprintln!(
            "[t+{:>7.1}ms] {what}",
            launched_at().elapsed().as_secs_f64() * 1000.0
        );
    }
}

/// Call once at process start so `t+0` means launch, not first timed event.
pub fn start_clock() {
    launched_at();
}

enum Job {
    /// The fastest decode near the target size, which may be the file's
    /// embedded preview. For RAW this is the win: a Sony ARW's 1616px embedded
    /// JPEG loads in about 35 ms, while demosaicing to that size takes about
    /// 250 ms. A JPEG has no embedded preview, so this pass already decodes at
    /// size and is the final answer.
    Speed(PathBuf, u32),
    /// Decode at the target size, ignoring any embedded preview. Queued only
    /// when `Speed` came back smaller than the target.
    Preview(PathBuf, u32),
    /// Full-resolution decode, capped at the GPU's max texture size. Requested
    /// only when the user zooms past the preview, because it costs seconds and
    /// hundreds of megabytes.
    Full(PathBuf, u32),
    /// Thumbnail whose longest side is at most the carried size.
    #[allow(dead_code)]
    Thumb(PathBuf, u32),
    /// Camera, lens, and exposure metadata for the info panel.
    Exif(PathBuf),
    /// Capture time (EXIF, else mtime) for burst grouping.
    Meta(PathBuf),
}

#[derive(PartialEq, Eq)]
enum Enqueued {
    Yes,
    NoWorkers,
    Poisoned,
}

/// A finished job, carrying its tier back to the poller.
enum JobResult {
    Speed(PathBuf, u32, Result<DecodedImage, String>),
    Preview(PathBuf, u32, Result<DecodedImage, String>),
    Full(PathBuf, Result<DecodedImage, String>),
    Thumb(PathBuf, u32, Result<Arc<DecodedImage>, String>),
    Exif(PathBuf, ImageMetadata),
    Meta(PathBuf, Option<SystemTime>),
}

/// The shared job queue. Workers take jobs in this priority order:
/// speed, preview, full, exif, thumbnails, meta.
///
/// The loupe image comes first because the user is waiting on it, and
/// thumbnails arrive by the hundreds. Previews outrank full-resolution decodes
/// so that stepping to the next photo does not wait behind the previous
/// photo's multi-second full decode.
///
/// Priority only applies to jobs still in the queue. A thumbnail already
/// running on a worker cannot be preempted, so worker 0 is reserved and never
/// takes thumbnails (see `Loader::new`).
#[derive(Default)]
struct Queue {
    speed: VecDeque<Job>,
    preview: VecDeque<Job>,
    full: VecDeque<Job>,
    thumbs: VecDeque<Job>,
    exif: VecDeque<Job>,
    meta: VecDeque<Job>,
    /// Set when the `Loader` is dropped so idle workers wake and exit.
    shutdown: bool,
}

impl Queue {
    /// Pops the highest-priority job this worker may run. `dedicated` marks
    /// the reserved worker, which skips thumbnails.
    fn take_next(&mut self, dedicated: bool) -> Option<Job> {
        self.speed
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

fn report_decode(
    tier: &str,
    path: &Path,
    target: u32,
    started: web_time::Instant,
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

struct Shared {
    queue: Mutex<Queue>,
    /// Signalled whenever a job is enqueued or on shutdown.
    ready: Condvar,
}

/// Kept tiny because one entry is a whole RGBA8 image (about 180 MB for
/// 45 MP), and full decodes only happen on zoom, so extra entries do not help
/// navigation.
const FULL_CAPACITY: usize = 3;
/// About 5 MB each: the current photo plus a few neighbors either way.
const PREVIEW_CAPACITY: usize = 8;
/// About 700 KB each (512px RGBA8). wasm32 fills this cache too, and a 32-bit
/// address space cannot hold hundreds of megabytes of thumbnails. This is the
/// floor; `set_thumb_working_set_size` raises it for large grids.
const THUMB_CAPACITY: usize = 256;

pub struct Loader {
    shared: Arc<Shared>,
    res_rx: Receiver<JobResult>,

    /// The GPU's max texture dimension, so full decodes only downscale images
    /// too large to upload.
    full_target: u32,

    // Full-resolution tier. `order` is insertion order for LRU eviction.
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    order: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,
    capacity: usize,

    // Preview tier, keyed by `(path, target_px)` so a window resize does not
    // serve a smaller stale decode.
    preview_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    preview_order: VecDeque<(PathBuf, u32)>,
    preview_inflight: HashSet<(PathBuf, u32)>,
    preview_capacity: usize,

    // `Speed` results, same key as the preview tier. A separate map lets a
    // short speed result show at once and be replaced when the preview lands.
    speed_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    speed_order: VecDeque<(PathBuf, u32)>,
    speed_inflight: HashSet<(PathBuf, u32)>,
    /// Keys the user has viewed, which get the `Preview` decode if their speed
    /// pass came back short. Prefetched neighbors are not in this set.
    escalation_wanted: HashSet<(PathBuf, u32)>,

    thumb_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    thumb_order: VecDeque<(PathBuf, u32)>,
    thumb_inflight: HashSet<(PathBuf, u32)>,
    /// Failed thumbnail keys, so callers stop re-requesting them every frame.
    thumb_failed: HashSet<(PathBuf, u32)>,
    thumb_capacity: usize,

    meta_inflight: HashSet<PathBuf>,

    exif_inflight: HashSet<PathBuf>,

    /// Worker threads that actually started. Zero on wasm32, where spawning
    /// fails and the browser's Web Worker pool decodes instead.
    workers: usize,
}

impl Loader {
    pub fn new(max_dim: u32) -> Self {
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::with_workers(max_dim, cores.saturating_sub(2).max(1))
    }

    fn with_workers(max_dim: u32, workers: usize) -> Self {
        let (res_tx, res_rx) = std::sync::mpsc::channel::<JobResult>();

        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
        });
        let thumbs = Arc::new(ThumbCache::new());

        let mut started = 0;
        for i in 0..workers {
            let shared = Arc::clone(&shared);
            let res_tx = res_tx.clone();
            let thumbs = Arc::clone(&thumbs);
            // Worker reservation: worker 0 never takes thumbnails, so a loupe
            // decode always has a free worker even when every other worker is
            // busy with thumbnails. With only one worker, reserving it would
            // starve thumbnails, so there is no reservation.
            let dedicated_full = i == 0 && workers > 1;
            let spawned = thread::Builder::new()
                .name(format!("decode-worker-{i}"))
                .spawn(move || loop {
                    // Hold the lock only to take one job, so a slow decode never
                    // blocks the queue.
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

                    // Decoders can panic (for example across the ImageIO FFI).
                    // Catching it keeps the worker alive and still sends a
                    // result, so the path leaves the caller's in-flight set.
                    // AssertUnwindSafe is fine: the closures own no shared state.
                    let result = match job {
                        Job::Speed(path, target) => {
                            let t0 = web_time::Instant::now();
                            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                crate::thumbnail::decode_at_size(
                                    &path,
                                    target,
                                    crate::thumbnail::EmbeddedPreview::UseIfPresent,
                                )
                            }))
                            .unwrap_or_else(|_| {
                                Err(format!("speed decode panicked: {}", path.display()))
                            });
                            report_decode("speed", &path, target, t0, &r);
                            JobResult::Speed(path, target, r)
                        }
                        Job::Preview(path, target) => {
                            let t0 = web_time::Instant::now();
                            // Not `image_decode::decode`: that decodes at full
                            // size and then shrinks, which is slower than a
                            // decode at the target size.
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
                            let t0 = web_time::Instant::now();
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
                                thumbs.get_or_make(&path)
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
                            let t = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                image_decode::capture_time(&path)
                            }))
                            .unwrap_or(None);
                            JobResult::Meta(path, t)
                        }
                    };

                    if res_tx.send(result).is_err() {
                        break;
                    }
                });
            // Spawning fails on wasm32, which has no OS threads. Log and carry
            // on; queued jobs then go unserviced.
            match spawned {
                Ok(_) => started += 1,
                Err(e) => eprintln!("[loader] could not spawn decode worker {i}: {e}"),
            }
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
            speed_cache: HashMap::new(),
            speed_order: VecDeque::new(),
            speed_inflight: HashSet::new(),
            escalation_wanted: HashSet::new(),
            thumb_cache: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
            thumb_capacity: THUMB_CAPACITY,
            meta_inflight: HashSet::new(),
            exif_inflight: HashSet::new(),
            workers: started,
        }
    }

    /// Requests the screen-fit view the loupe shows for `path` at `target_px`.
    /// Runs a `Speed` pass first and follows with a `Preview` decode only if
    /// the speed result is smaller than `target_px`.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn request_preview(&mut self, path: PathBuf, target_px: u32) {
        self.enqueue_speed(path, target_px, true);
    }

    /// Like `request_preview`, for a neighbor the user has not opened yet. Runs
    /// only the `Speed` pass. `request_preview` adds the `Preview` decode once
    /// the photo is viewed.
    pub fn prefetch_preview(&mut self, path: PathBuf, target_px: u32) {
        self.enqueue_speed(path, target_px, false);
    }

    fn enqueue_speed(&mut self, path: PathBuf, target_px: u32, escalate: bool) {
        let key = (path.clone(), target_px);
        // Record this before any early return. A prefetched photo can be viewed
        // while its speed pass is still running, and the escalation must
        // survive until that result lands.
        if escalate {
            self.escalation_wanted.insert(key.clone());
        }
        if self.preview_cache.contains_key(&key) || self.preview_inflight.contains(&key) {
            return;
        }
        if let Some(img) = self.speed_cache.get(&key) {
            if escalate {
                let longest = img.width.max(img.height);
                self.escalate_if_short(&path, target_px, longest);
            }
            return;
        }
        if self.speed_inflight.contains(&key) {
            return;
        }
        if self.enqueue(Job::Speed(path, target_px)) == Enqueued::Yes {
            self.speed_inflight.insert(key);
        }
    }

    /// Hands `job` to the workers. Callers mark a job in flight only on
    /// `Yes`: with no workers (wasm32) or a poisoned queue nothing would ever
    /// clear the marker, and the frame loop would poll forever.
    fn enqueue(&self, job: Job) -> Enqueued {
        if self.workers == 0 {
            return Enqueued::NoWorkers;
        }
        let Ok(mut q) = self.shared.queue.lock() else {
            return Enqueued::Poisoned;
        };
        let lane = match &job {
            Job::Speed(..) => &mut q.speed,
            Job::Preview(..) => &mut q.preview,
            Job::Full(..) => &mut q.full,
            Job::Thumb(..) => &mut q.thumbs,
            Job::Exif(..) => &mut q.exif,
            Job::Meta(..) => &mut q.meta,
        };
        lane.push_back(job);
        // notify_all because notify_one might wake only the reserved worker,
        // which skips thumbnails and would leave them queued.
        self.shared.ready.notify_all();
        Enqueued::Yes
    }

    /// Queues the `Preview` decode for a landed `Speed` result, but only if the
    /// user is viewing that photo.
    fn escalate_from_speed(&mut self, path: &Path, target_px: u32, got_longest: u32) {
        if self
            .escalation_wanted
            .contains(&(path.to_path_buf(), target_px))
        {
            self.escalate_if_short(path, target_px, got_longest);
        }
    }

    /// Queues the `Preview` decode if the speed result's longest side is below
    /// `target_px`. A JPEG's speed pass already hits the target, so it costs one
    /// decode.
    fn escalate_if_short(&mut self, path: &Path, target_px: u32, got_longest: u32) {
        if got_longest >= target_px {
            return;
        }
        let key = (path.to_path_buf(), target_px);
        if self.preview_cache.contains_key(&key) || self.preview_inflight.contains(&key) {
            return;
        }
        if self.enqueue(Job::Preview(path.to_path_buf(), target_px)) == Enqueued::Yes {
            self.preview_inflight.insert(key);
        }
    }

    /// Requests the full-resolution decode of `path`. Call only when the user
    /// zooms past the preview, because this is the expensive tier.
    pub fn request_full(&mut self, path: PathBuf) {
        if self.cache.contains_key(&path) || self.inflight.contains(&path) {
            return;
        }
        if self.enqueue(Job::Full(path.clone(), self.full_target)) == Enqueued::Yes {
            self.inflight.insert(path);
        }
    }

    /// True while any loupe decode is running. The frame loop keeps polling
    /// while this is true, because a finished worker does not wake winit and
    /// the result would otherwise wait for the next input event.
    pub fn has_pending_image(&self) -> bool {
        !self.inflight.is_empty()
            || !self.preview_inflight.is_empty()
            || !self.speed_inflight.is_empty()
    }

    pub fn get_full(&self, path: &Path) -> Option<Arc<DecodedImage>> {
        self.cache.get(path).cloned()
    }

    /// The `Preview` decode if it has landed, else the `Speed` result, else
    /// `None`.
    pub fn get_preview(&self, path: &Path, target_px: u32) -> Option<Arc<DecodedImage>> {
        let key = (path.to_path_buf(), target_px);
        self.preview_cache
            .get(&key)
            .or_else(|| self.speed_cache.get(&key))
            .cloned()
    }

    /// The full-resolution decode if present, else the preview.
    pub fn get_best(&self, path: &Path, target_px: u32) -> Option<Arc<DecodedImage>> {
        self.get_full(path)
            .or_else(|| self.get_preview(path, target_px))
    }

    #[allow(dead_code)]
    pub fn request_thumb(&mut self, path: PathBuf, max_px: u32) {
        let key = (path.clone(), max_px);
        if self.thumb_cache.contains_key(&key)
            || self.thumb_inflight.contains(&key)
            || self.thumb_failed.contains(&key)
        {
            return;
        }
        match self.enqueue(Job::Thumb(path, max_px)) {
            Enqueued::Yes => {
                self.thumb_inflight.insert(key);
            }
            Enqueued::NoWorkers => {}
            Enqueued::Poisoned => {
                // Poison is permanent and workers exit on it, so report every
                // pending thumbnail as failed rather than loading forever.
                self.thumb_failed.insert(key);
                self.thumb_failed.extend(self.thumb_inflight.drain());
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn poison_thumb_queue_for_test(&mut self, path: PathBuf, max_px: u32) {
        // Hold the lock through enqueue and poison so no worker takes the job
        // first, leaving it stranded in the poisoned queue.
        let shared = Arc::clone(&self.shared);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut queue = shared.queue.lock().unwrap();
            queue.thumbs.push_back(Job::Thumb(path.clone(), max_px));
            self.thumb_inflight.insert((path, max_px));
            panic!("poison thumbnail queue");
        }));
        assert!(result.is_err());
        shared.ready.notify_all();
    }

    /// Requests `path`'s capture time. The result arrives in the third list
    /// returned by [`poll_all`](Self::poll_all).
    #[allow(dead_code)]
    pub fn request_meta(&mut self, path: PathBuf) {
        if self.meta_inflight.contains(&path) {
            return;
        }
        if self.enqueue(Job::Meta(path.clone())) == Enqueued::Yes {
            self.meta_inflight.insert(path);
        }
    }

    /// Requests `path`'s camera, lens, and exposure metadata. The result arrives
    /// in the fourth list returned by [`poll_all`](Self::poll_all). Call it for
    /// the viewed photo only; exif jobs outrank thumbnails.
    pub fn request_exif(&mut self, path: PathBuf) {
        if self.exif_inflight.contains(&path) {
            return;
        }
        if self.enqueue(Job::Exif(path.clone())) == Enqueued::Yes {
            self.exif_inflight.insert(path);
        }
    }

    #[allow(dead_code)]
    pub fn get_thumb(&self, path: &Path, max_px: u32) -> Option<Arc<DecodedImage>> {
        self.thumb_cache.get(&(path.to_path_buf(), max_px)).cloned()
    }

    /// True if this thumbnail failed to decode, so callers stop treating it as
    /// loading.
    pub fn thumb_failed(&self, path: &Path, max_px: u32) -> bool {
        self.thumb_failed.contains(&(path.to_path_buf(), max_px))
    }

    /// Inserts a thumbnail decoded outside this queue (the wasm32 Web Worker
    /// path in `app/web.rs`). Callers track their own in-flight state.
    ///
    /// Also clears any `thumb_inflight` marker for the key. Some code, such as
    /// `enqueue_auto_tone`, calls `request_thumb` on wasm32 too, and that job
    /// never completes there because no worker serves the queue.
    #[cfg(any(target_arch = "wasm32", test))]
    pub fn insert_thumb_external(&mut self, path: PathBuf, max_px: u32, img: Arc<DecodedImage>) {
        let key = (path, max_px);
        self.thumb_inflight.remove(&key);
        self.insert_thumb(key, img);
    }

    /// Records a failed external thumbnail decode. Clears `thumb_inflight` for
    /// the same reason as `insert_thumb_external`.
    #[cfg(target_arch = "wasm32")]
    pub fn mark_thumb_failed_external(&mut self, path: PathBuf, max_px: u32) {
        let key = (path, max_px);
        self.thumb_inflight.remove(&key);
        self.thumb_failed.insert(key);
    }

    /// Inserts a wasm32 `Preview` decode into the preview tier. wasm32 `Speed`
    /// results skip this cache and go straight to the screen (see
    /// `poll_web_preview`).
    #[cfg(target_arch = "wasm32")]
    pub fn insert_preview_external(
        &mut self,
        path: PathBuf,
        target_px: u32,
        img: Arc<DecodedImage>,
    ) {
        self.insert_preview((path, target_px), img);
    }

    /// Inserts a wasm32 zoom-triggered full decode (from `poll_web_full`) into
    /// the full-resolution tier.
    #[cfg(target_arch = "wasm32")]
    pub fn insert_full_external(&mut self, path: PathBuf, img: Arc<DecodedImage>) {
        self.insert(path, img);
    }

    /// Drains every finished job into its cache and returns what arrived:
    /// `(loupe paths, thumbnail keys, capture times, exif metadata)`. Speed,
    /// preview, and full arrivals share the loupe list because callers only
    /// need to know that the loupe should refresh.
    ///
    /// After a worker panic poisons the queue, it also clears every pending
    /// marker that can no longer complete, and marks pending thumbnails failed.
    pub fn poll_all(
        &mut self,
    ) -> (
        Vec<PathBuf>,
        Vec<(PathBuf, u32)>,
        Vec<(PathBuf, Option<SystemTime>)>,
        Vec<(PathBuf, ImageMetadata)>,
    ) {
        let arrivals = self.drain();
        if self.shared.queue.is_poisoned() {
            // Queued image jobs will never run, so clear their markers. Jobs
            // already running keep their markers until their results drain.
            let (queued_full, queued_preview, queued_speed) = {
                let queue = match self.shared.queue.lock() {
                    Ok(queue) => queue,
                    Err(poisoned) => poisoned.into_inner(),
                };
                let mut full = HashSet::new();
                let mut preview = HashSet::new();
                let mut speed = HashSet::new();
                for job in queue.full.iter() {
                    if let Job::Full(path, _) = job {
                        full.insert(path.clone());
                    }
                }
                for job in queue.preview.iter() {
                    if let Job::Preview(path, target) = job {
                        preview.insert((path.clone(), *target));
                    }
                }
                for job in queue.speed.iter() {
                    if let Job::Speed(path, target) = job {
                        speed.insert((path.clone(), *target));
                    }
                }
                (full, preview, speed)
            };
            for path in queued_full {
                self.inflight.remove(&path);
            }
            for key in queued_preview {
                self.preview_inflight.remove(&key);
            }
            for key in queued_speed {
                self.speed_inflight.remove(&key);
            }
            self.thumb_failed.extend(self.thumb_inflight.drain());
            self.meta_inflight.clear();
            self.exif_inflight.clear();
        }
        arrivals
    }

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
                JobResult::Speed(path, target, r) => {
                    let key = (path.clone(), target);
                    self.speed_inflight.remove(&key);
                    match r {
                        Ok(img) => {
                            let longest = img.width.max(img.height);
                            self.insert_speed(key, Arc::new(img));
                            self.escalate_from_speed(&path, target, longest);
                            full.push(path);
                        }
                        Err(e) => {
                            eprintln!("speed decode failed for {}: {e}", path.display());
                            // Fall back to the `Preview` decode, even for a
                            // prefetch: a failure leaves no image at all, which
                            // is worse than a short one.
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

    fn insert_speed(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if !self.speed_cache.contains_key(&key) {
            self.speed_order.push_back(key.clone());
        }
        self.speed_cache.insert(key, img);
        // Shares the preview budget: same photos at similar sizes.
        while self.speed_order.len() > self.preview_capacity {
            if let Some(old) = self.speed_order.pop_front() {
                self.speed_cache.remove(&old);
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

    /// Sets the thumbnail cache capacity to `len` (never below
    /// `THUMB_CAPACITY`), so a visible grid plus its prefetch rows never evicts
    /// itself. Shrinking evicts the oldest entries at once.
    pub fn set_thumb_working_set_size(&mut self, len: usize) {
        self.thumb_capacity = THUMB_CAPACITY.max(len);
        self.trim_thumbs();
    }

    fn insert_thumb(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if !self.thumb_cache.contains_key(&key) {
            self.thumb_order.push_back(key.clone());
        }
        self.thumb_cache.insert(key, img);
        self.trim_thumbs();
    }

    fn trim_thumbs(&mut self) {
        while self.thumb_order.len() > self.thumb_capacity {
            if let Some(old) = self.thumb_order.pop_front() {
                self.thumb_cache.remove(&old);
            }
        }
    }
}

impl Drop for Loader {
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
            Job::Speed(..) => "speed",
            Job::Preview(..) => "preview",
            Job::Full(..) => "full",
            Job::Exif(_) => "exif",
            Job::Thumb(..) => "thumb",
            Job::Meta(_) => "meta",
        }
    }

    /// One job of each kind, pushed in reverse priority order so a queue that
    /// kept insertion order would fail.
    fn every_kind() -> Queue {
        let mut q = Queue::default();
        q.meta.push_back(Job::Meta(path("m")));
        q.thumbs.push_back(Job::Thumb(path("t"), 192));
        q.exif.push_back(Job::Exif(path("e")));
        q.full.push_back(Job::Full(path("f"), 16384));
        q.preview.push_back(Job::Preview(path("p"), 2048));
        q.speed.push_back(Job::Speed(path("q"), 2048));
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
            ["speed", "preview", "full", "exif", "thumb", "meta"]
        );
    }

    #[test]
    fn the_dedicated_worker_skips_thumbnails_entirely() {
        assert_eq!(
            drain_labels(&mut every_kind(), true),
            ["speed", "preview", "full", "exif", "meta"]
        );
    }

    #[test]
    fn a_preview_outranks_a_full_decode_already_queued() {
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
            pixel_format: image_decode::PixelFormat::Srgb8,
        })
    }

    #[test]
    fn thumbnail_cache_retains_large_grid_and_shrinks_after_resize() {
        let mut loader = Loader::new(16384);
        let px = crate::thumbnail::THUMB_PX;
        // 18 columns, 10 visible rows, and three prefetch rows on either side.
        let working_set = 18 * (10 + 6);
        loader.set_thumb_working_set_size(working_set);
        for i in 0..working_set {
            loader.insert_thumb((path(&i.to_string()), px), image(1, 1));
        }
        // Repeated stationary frames must find every requested thumbnail.
        for _ in 0..3 {
            loader.set_thumb_working_set_size(working_set);
            for i in 0..working_set {
                assert!(loader.get_thumb(&path(&i.to_string()), px).is_some());
            }
        }
        loader.set_thumb_working_set_size(0);
        assert_eq!(loader.thumb_cache.len(), THUMB_CAPACITY);
        assert_eq!(loader.thumb_order.len(), THUMB_CAPACITY);
        assert!(loader.get_thumb(&path("0"), px).is_none());
        assert!(loader
            .get_thumb(&path(&(working_set - 1).to_string()), px)
            .is_some());
    }

    #[test]
    fn the_full_tier_evicts_oldest_first_at_capacity() {
        let mut loader = Loader::new(16384);
        for i in 0..FULL_CAPACITY + 2 {
            loader.insert(path(&i.to_string()), image(1, 1));
        }
        assert_eq!(loader.cache.len(), FULL_CAPACITY);
        assert!(loader.get_full(&path("0")).is_none());
        assert!(loader.get_full(&path("1")).is_none());
        assert!(loader
            .get_full(&path(&(FULL_CAPACITY + 1).to_string()))
            .is_some());
    }

    #[test]
    fn the_preview_tier_keys_on_target_so_a_resize_does_not_serve_a_stale_size() {
        let mut loader = Loader::new(16384);
        loader.insert_preview((path("a"), 2048), image(2048, 1365));
        assert!(loader.get_preview(&path("a"), 2048).is_some());
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

    /// Counts `Preview` decodes in flight. Reads the in-flight set, not the
    /// queue, because the real workers pop queued jobs at once. Only `drain`
    /// clears the set, and the test never calls it.
    fn queued_previews(loader: &Loader) -> usize {
        loader.preview_inflight.len()
    }

    #[test]
    fn a_speed_pass_that_already_hit_the_target_costs_only_one_decode() {
        // The JPEG case: the speed pass already decoded at the target size.
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 2560);
        assert_eq!(queued_previews(&loader), 0);
        loader.escalate_if_short(&path("b"), 2560, 4096);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn a_short_speed_pass_queues_the_forced_decode_behind_it() {
        // The RAW case: a 1616px embedded preview, then the 2560px decode.
        let mut loader = Loader::new(16384);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
        assert!(loader.has_pending_image());
    }

    #[test]
    fn without_workers_requests_leave_nothing_pending() {
        // wasm32 has no decode threads. A request that marks itself in flight
        // there is never cleared, and the frame loop polls forever.
        let mut loader = Loader::with_workers(16384, 0);
        loader.prefetch_preview(path("a"), 2560);
        loader.request_full(path("a"));
        loader.request_exif(path("a"));
        loader.request_meta(path("a"));
        loader.request_thumb(path("a"), 192);
        assert!(!loader.has_pending_image());
        assert!(loader.exif_inflight.is_empty());
        assert!(loader.meta_inflight.is_empty());
        assert!(loader.thumb_inflight.is_empty());
        assert!(!loader.thumb_failed(&path("a"), 192));
    }

    #[test]
    fn a_failed_speed_pass_still_falls_through_to_the_forced_decode() {
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
        // Escalating prefetches would compete with the viewed photo's decode.
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("neighbor"), 2560);
        loader.insert_speed((path("neighbor"), 2560), image(1616, 1080));
        loader.escalate_from_speed(&path("neighbor"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn navigating_onto_a_prefetched_photo_escalates_it_after_all() {
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("a"), 2560);
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        loader.request_preview(path("a"), 2560);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn viewing_a_photo_whose_prefetch_is_still_running_does_not_lose_the_escalation() {
        // The user views a photo while its prefetch speed pass is still running.
        let mut loader = Loader::new(16384);
        loader.prefetch_preview(path("a"), 2560);
        loader.request_preview(path("a"), 2560); // still in flight
        loader.speed_inflight.remove(&(path("a"), 2560));
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        loader.escalate_from_speed(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn the_preview_getter_prefers_the_forced_decode_over_the_speed_pass() {
        let mut loader = Loader::new(16384);
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 1616);
        loader.insert_preview((path("a"), 2560), image(2560, 1707));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 2560);
    }

    #[test]
    fn requesting_a_preview_that_is_already_answered_enqueues_nothing() {
        let mut loader = Loader::new(16384);
        loader.insert_speed((path("a"), 2560), image(2560, 1707));
        loader.request_preview(path("a"), 2560);
        assert!(!loader.has_pending_image());
    }

    #[test]
    fn pending_image_tracks_both_loupe_tiers_and_ignores_the_others() {
        let mut loader = Loader::new(16384);
        assert!(!loader.has_pending_image());

        // Thumbnail and metadata work must not keep the frame loop awake.
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
