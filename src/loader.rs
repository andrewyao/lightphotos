// SPDX-License-Identifier: MIT OR Apache-2.0

//! Background image decoding and the decoded-image caches. Worker threads
//! decode off the UI thread. Results land in one of three LRU caches:
//! thumbnails (grid and filmstrip), the screen-fit preview the loupe shows, and
//! a small full-resolution cache used once the loupe zooms in.
//!
//! All tiers share one priority queue (see `Queue`) and one results channel
//! that `poll_all` drains. On wasm32 the workers are wasm threads over the
//! app's shared memory. They cannot open files by path, so the browser reads
//! each file on the main thread and submits its bytes through `WebDecoder`,
//! and `app/web.rs` moves the results into the same caches through the
//! `*_external` methods.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Condvar, Mutex};
#[cfg(not(target_arch = "wasm32"))]
use std::thread;
use std::time::SystemTime;

use crate::cache_limits::CacheLimits;
use crate::decode::image_decode::{self, DecodedImage, ImageMetadata};
use crate::develop::{Adjustments, TouchUp};
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
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    Full(PathBuf, u32),
    /// Thumbnail whose longest side is at most the carried size.
    #[allow(dead_code)]
    Thumb(PathBuf, u32),
    /// Camera, lens, and exposure metadata for the info panel.
    Exif(PathBuf),
    /// Capture time (EXIF, else mtime) for burst grouping.
    Meta(PathBuf),
    /// A cached thumbnail with its photo's edits baked in, for the grid.
    Bake(Box<BakeJob>),
    /// wasm32: a decode of bytes the main thread already read, since a
    /// thread cannot open a File System Access handle by path.
    #[cfg(target_arch = "wasm32")]
    Web(Box<crate::web_decode::WebJob>),
    /// wasm32: a batch export. Native exports run on `export.rs`'s own pool.
    #[cfg(target_arch = "wasm32")]
    WebExport(Box<crate::web_decode::WebExportJob>),
    /// wasm32: `Exif` over bytes the main thread already read.
    #[cfg(target_arch = "wasm32")]
    WebExif(Box<crate::web_decode::WebExifJob>),
    /// wasm32: a Remove Chromatic Aberration measurement over bytes the main
    /// thread already read.
    #[cfg(target_arch = "wasm32")]
    WebMeasure(Box<crate::web_decode::WebMeasureJob>),
}

/// Everything a worker needs to bake one edited thumbnail, owned so the
/// worker never reads app state.
struct BakeJob {
    path: PathBuf,
    px: u32,
    sig: u64,
    img: Arc<DecodedImage>,
    adj: Adjustments,
    touchups: Vec<TouchUp>,
    rot: u8,
}

impl BakeJob {
    fn run(self) -> BakedThumb {
        let (width, height, rgba) =
            crate::image_ops::bake_edited(&self.img, &self.adj, &self.touchups, self.rot);
        BakedThumb {
            path: self.path,
            px: self.px,
            sig: self.sig,
            width,
            height,
            rgba,
        }
    }
}

/// An edited thumbnail ready to upload. `sig` is the edit signature it was
/// baked from, so the caller can drop one the user has since edited again.
/// Opaque, so premultiplied and straight alpha are the same bytes.
pub struct BakedThumb {
    pub path: PathBuf,
    pub px: u32,
    pub sig: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
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
    /// `None` when the bake panicked.
    Baked(PathBuf, u32, u64, Option<BakedThumb>),
    #[cfg(target_arch = "wasm32")]
    Web(Box<crate::web_decode::PoolResult>),
    /// The export's id, its source, and the JPEG.
    #[cfg(target_arch = "wasm32")]
    WebExport(u64, PathBuf, Result<Vec<u8>, String>),
    #[cfg(target_arch = "wasm32")]
    WebMeasure(PathBuf, Result<crate::develop::CaScale, String>),
}

/// The shared job queue. Workers take jobs in this priority order:
/// speed, preview, full, exif, edit bakes, viewport thumbnails, background
/// thumbnails, meta.
///
/// The loupe image comes first because the user is waiting on it, and
/// thumbnails arrive by the hundreds. Previews outrank full-resolution decodes
/// so that stepping to the next photo does not wait behind the previous
/// photo's multi-second full decode.
///
/// Thumbnails come in two classes. `thumbs_viewport` is what the grid or the
/// filmstrip shows right now: `set_viewport_thumbs` replaces the whole list
/// every frame, and a queued job that scrolled off is dropped, so a new
/// viewport never waits behind the cells passed on the way to it. `thumbs` is
/// the background work (burst scoring, duplicate hashing, Auto Tone) that
/// must finish wherever the user scrolls, so it is plain FIFO and never
/// pruned.
///
/// An edit bake is an on-screen cell whose thumbnail already decoded, so it
/// goes ahead of the thumbnails still to decode. It is 10-20 ms of CPU per
/// 512px thumbnail, which is why it runs here and not on the frame.
///
/// Priority only applies to jobs still in the queue. A thumbnail already
/// running on a worker cannot be preempted, so worker 0 is reserved and never
/// takes thumbnails (see `Loader::new`).
#[derive(Default)]
struct Queue {
    speed: VecDeque<Job>,
    preview: VecDeque<Job>,
    full: VecDeque<Job>,
    thumbs_viewport: VecDeque<Job>,
    thumbs: VecDeque<Job>,
    exif: VecDeque<Job>,
    bake: VecDeque<Job>,
    /// wasm32 exports: a batch the user walks away from, so it runs after
    /// every thumbnail and never on the reserved worker.
    export: VecDeque<Job>,
    meta: VecDeque<Job>,
    /// Set when the `Loader` is dropped so idle workers wake and exit.
    shutdown: bool,
}

/// Takes the queue lock from the app's own thread. On wasm32 that is the
/// browser's main thread, where a contended `Mutex::lock` would wait with
/// `Atomics.wait` and throw, so it spins on `try_lock` instead. Workers only
/// hold the lock to push or pop one job, so the spin is short.
fn lock_queue(shared: &Shared) -> std::sync::LockResult<std::sync::MutexGuard<'_, Queue>> {
    #[cfg(target_arch = "wasm32")]
    loop {
        match shared.queue.try_lock() {
            Ok(q) => return Ok(q),
            Err(std::sync::TryLockError::Poisoned(e)) => return Err(e),
            Err(std::sync::TryLockError::WouldBlock) => std::hint::spin_loop(),
        }
    }
    #[cfg(not(target_arch = "wasm32"))]
    shared.queue.lock()
}

fn push_job(shared: &Shared, job: Job) -> Enqueued {
    let Ok(mut q) = lock_queue(shared) else {
        return Enqueued::Poisoned;
    };
    let lane = match &job {
        Job::Speed(..) => &mut q.speed,
        Job::Preview(..) => &mut q.preview,
        Job::Full(..) => &mut q.full,
        Job::Thumb(..) => &mut q.thumbs,
        Job::Exif(..) => &mut q.exif,
        Job::Meta(..) => &mut q.meta,
        Job::Bake(..) => &mut q.bake,
        // A browser thumbnail is always an on-screen or Auto Tone request,
        // and `retain_web_thumbs` prunes the ones that scroll away.
        #[cfg(target_arch = "wasm32")]
        Job::Web(web) => match web.kind {
            crate::web_decode::JobKind::Speed => &mut q.speed,
            crate::web_decode::JobKind::Preview => &mut q.preview,
            crate::web_decode::JobKind::Full => &mut q.full,
            crate::web_decode::JobKind::Thumb => &mut q.thumbs_viewport,
        },
        #[cfg(target_arch = "wasm32")]
        Job::WebExport(..) => &mut q.export,
        // The thread that runs the Loupe's `Preview` decodes also parses
        // its metadata, see `WebExifJob`.
        #[cfg(target_arch = "wasm32")]
        Job::WebExif(..) => &mut q.preview,
        // A full decode the user waits on, like a zoom's.
        #[cfg(target_arch = "wasm32")]
        Job::WebMeasure(..) => &mut q.full,
    };
    lane.push_back(job);
    // notify_all because notify_one might wake only the reserved worker,
    // which skips thumbnails and would leave them queued.
    shared.ready.notify_all();
    Enqueued::Yes
}

impl Queue {
    /// Pops the highest-priority job this worker may run. `dedicated` marks
    /// the reserved worker, which skips thumbnails and bakes. `previews` is
    /// false for a worker that skips the `Preview` lane, see
    /// `Worker::takes_previews`.
    fn take_next(&mut self, dedicated: bool, previews: bool) -> Option<Job> {
        self.speed
            .pop_front()
            .or_else(|| previews.then(|| self.preview.pop_front()).flatten())
            .or_else(|| self.full.pop_front())
            .or_else(|| self.exif.pop_front())
            .or_else(|| {
                if dedicated {
                    None
                } else {
                    self.bake
                        .pop_front()
                        .or_else(|| self.thumbs_viewport.pop_front())
                        .or_else(|| self.thumbs.pop_front())
                        .or_else(|| self.export.pop_front())
                }
            })
            .or_else(|| self.meta.pop_front())
    }
}

/// wasm32: submits the browser's decodes onto the loader's threads from
/// `spawn_local` futures. Cheap to clone. Results come back through
/// `Loader::take_web_results`, failures included, so the caller clears its
/// in-flight markers in one place.
#[cfg(target_arch = "wasm32")]
#[derive(Clone)]
pub struct WebDecoder {
    shared: Arc<Shared>,
    res_tx: std::sync::mpsc::Sender<JobResult>,
    workers: usize,
}

#[cfg(target_arch = "wasm32")]
impl WebDecoder {
    /// Decode `bytes` for a thumbnail. `from_cache` marks bytes read from
    /// the disk cache rather than the source, which also skips encoding a
    /// new cache JPEG.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_thumb(
        &self,
        path: PathBuf,
        px: u32,
        bytes: js_sys::ArrayBuffer,
        is_raw: bool,
        cache_name: Option<String>,
        generation: u64,
        from_cache: bool,
    ) {
        self.push(crate::web_decode::WebJob {
            kind: crate::web_decode::JobKind::Thumb,
            path,
            target: px,
            bytes: Arc::new(js_sys::Uint8Array::new(&bytes).to_vec()),
            is_raw,
            cache_name,
            generation: Some(generation),
            from_cache,
        });
    }

    /// Decode `bytes` for the Loupe: `Speed`, `Preview` or `Full`.
    pub fn submit(
        &self,
        path: PathBuf,
        target: u32,
        bytes: Arc<Vec<u8>>,
        is_raw: bool,
        kind: crate::web_decode::JobKind,
    ) {
        self.push(crate::web_decode::WebJob {
            kind,
            path,
            target,
            bytes,
            is_raw,
            cache_name: None,
            generation: None,
            from_cache: false,
        });
    }

    /// Queue an export. Its JPEG comes back through
    /// `Loader::take_web_exports` under `job.id`.
    pub fn submit_export(&self, job: crate::web_decode::WebExportJob) {
        let (id, path) = (job.id, job.path.clone());
        if self.workers == 0
            || push_job(&self.shared, Job::WebExport(Box::new(job))) != Enqueued::Yes
        {
            let _ = self.res_tx.send(JobResult::WebExport(
                id,
                path,
                Err("no decode thread can take the export".to_string()),
            ));
        }
    }

    /// Queue a Remove Chromatic Aberration measurement. Its scales come back
    /// through `Loader::take_web_measures`.
    pub fn submit_measure(&self, job: crate::web_decode::WebMeasureJob) {
        let path = job.path.clone();
        if self.workers == 0
            || push_job(&self.shared, Job::WebMeasure(Box::new(job))) != Enqueued::Yes
        {
            let _ = self.res_tx.send(JobResult::WebMeasure(
                path,
                Err("no decode thread can take the measurement".to_string()),
            ));
        }
    }

    /// Parse `job`'s bytes for the info panel on the thread that runs
    /// `Preview` decodes. Submit it before the photo's `Preview`, which it
    /// then runs ahead of. With no thread to take it, the file facts it
    /// already holds are the result.
    pub fn submit_exif(&self, job: crate::web_decode::WebExifJob) {
        let (path, failed) = (job.path.clone(), job.failed());
        if self.workers == 0 || push_job(&self.shared, Job::WebExif(Box::new(job))) != Enqueued::Yes
        {
            self.finish_exif(path, failed);
        }
    }

    /// Report metadata that needs no parse, such as the file facts of a
    /// file that could not be read.
    pub fn finish_exif(&self, path: PathBuf, meta: ImageMetadata) {
        let _ = self.res_tx.send(JobResult::Exif(path, meta));
    }

    /// How many threads can decode, so a batch can size how much it keeps
    /// in flight.
    pub fn threads(&self) -> usize {
        self.workers
    }

    /// Report a job that failed before it could be queued, such as a file
    /// read error, so it clears like any decode failure.
    pub fn fail(
        &self,
        path: PathBuf,
        target: u32,
        kind: crate::web_decode::JobKind,
        generation: Option<u64>,
        error: String,
    ) {
        let job = crate::web_decode::WebJob {
            kind,
            path,
            target,
            bytes: Default::default(),
            is_raw: false,
            cache_name: None,
            generation,
            from_cache: false,
        };
        let _ = self.res_tx.send(JobResult::Web(Box::new(
            job.failed(crate::web_decode::Failure::Read(error)),
        )));
    }

    fn push(&self, job: crate::web_decode::WebJob) {
        let failed = (self.workers == 0).then(|| {
            job.failed(crate::web_decode::Failure::Decode(
                "no decode threads started".to_string(),
            ))
        });
        if let Some(failed) = failed {
            let _ = self.res_tx.send(JobResult::Web(Box::new(failed)));
            return;
        }
        let failed = job.failed(crate::web_decode::Failure::Decode(
            "the decode queue is poisoned".to_string(),
        ));
        if push_job(&self.shared, Job::Web(Box::new(job))) != Enqueued::Yes {
            let _ = self.res_tx.send(JobResult::Web(Box::new(failed)));
        }
    }
}

/// One decode thread and what it needs to run or replace itself.
#[derive(Clone)]
struct Worker {
    index: usize,
    dedicated_full: bool,
    shared: Arc<Shared>,
    res_tx: std::sync::mpsc::Sender<JobResult>,
    thumbs: Arc<ThumbCache>,
}

impl Worker {
    /// On wasm32 only worker 0 runs `Preview` decodes. A RAW `Preview` is a
    /// full demosaic and denoise with hundreds of MB of scratch, and every
    /// thread shares one heap of at most 4 GB. Arrowing through RAWs had each
    /// thread take the `Preview` of a photo already left behind, and the heap
    /// ran out within a dozen presses. Natively every worker takes them.
    fn takes_previews(&self) -> bool {
        !cfg!(target_arch = "wasm32") || self.index == 0
    }

    /// Starts the thread. On wasm32 it is a Web Worker over the app's shared
    /// memory.
    fn spawn(self) -> std::io::Result<()> {
        #[cfg(target_arch = "wasm32")]
        let builder = wasm_thread::Builder::new();
        #[cfg(not(target_arch = "wasm32"))]
        let builder = thread::Builder::new();
        builder
            .name(format!("decode-worker-{}", self.index))
            .spawn(move || self.run())
            .map(drop)
    }

    fn run(self) {
        #[cfg(target_arch = "wasm32")]
        panic_recovery::enter(&self);
        // rayon cannot start threads of its own on wasm32, so its global
        // pool falls back to one worker: whichever thread used rayon first.
        // Every other thread's rawler decode then queues on that one thread,
        // and the decodes run one at a time. A one-thread pool that is this
        // thread runs this thread's rayon work inline instead.
        #[cfg(target_arch = "wasm32")]
        let _rayon = rayon::ThreadPoolBuilder::new()
            .num_threads(1)
            .use_current_thread()
            .build()
            .ok();
        loop {
            // Hold the lock only to take one job, so a slow decode never
            // blocks the queue.
            let job = {
                let mut q = match self.shared.queue.lock() {
                    Ok(q) => q,
                    Err(_) => return,
                };
                loop {
                    if q.shutdown {
                        return;
                    }
                    if let Some(j) = q.take_next(self.dedicated_full, self.takes_previews()) {
                        break j;
                    }
                    q = match self.shared.ready.wait(q) {
                        Ok(q) => q,
                        Err(_) => return,
                    };
                }
            };
            #[cfg(target_arch = "wasm32")]
            panic_recovery::start(&job);
            let result = run_job(job, &self.thumbs);
            #[cfg(target_arch = "wasm32")]
            panic_recovery::finish();
            if self.res_tx.send(result).is_err() {
                break;
            }
        }
    }
}

/// Runs one job to its result. Decoders can panic (for example across the
/// ImageIO FFI). Catching it still sends a result, so the path leaves the
/// caller's in-flight set. AssertUnwindSafe is fine: the closures own no
/// shared state. wasm32 aborts on panic instead, see `panic_recovery`.
fn run_job(job: Job, thumbs: &ThumbCache) -> JobResult {
    match job {
        Job::Speed(path, target) => {
            let t0 = web_time::Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::thumbnail::decode_speed(&path, target)
            }))
            .unwrap_or_else(|_| Err(format!("speed decode panicked: {}", path.display())));
            report_decode("speed", &path, target, t0, &r);
            JobResult::Speed(path, target, r)
        }
        Job::Preview(path, target) => {
            let t0 = web_time::Instant::now();
            // Not `image_decode::decode`: that decodes at full size and then
            // shrinks, which is slower than a decode at the target size.
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                crate::thumbnail::decode_at_size(
                    &path,
                    target,
                    crate::thumbnail::EmbeddedPreview::Never,
                )
            }))
            .unwrap_or_else(|_| Err(format!("preview decode panicked: {}", path.display())));
            report_decode("preview", &path, target, t0, &r);
            JobResult::Preview(path, target, r)
        }
        Job::Full(path, target) => {
            let t0 = web_time::Instant::now();
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                image_decode::decode(&path, target)
            }))
            .unwrap_or_else(|_| Err(format!("decode panicked: {}", path.display())));
            report_decode("full", &path, target, t0, &r);
            JobResult::Full(path, r)
        }
        Job::Thumb(path, max_px) => {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                thumbs.get_or_make(&path)
            }))
            .unwrap_or_else(|_| Err(format!("thumbnail panicked: {}", path.display())));
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
        Job::Bake(job) => {
            let (path, px, sig) = (job.path.clone(), job.px, job.sig);
            let baked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job.run())).ok();
            JobResult::Baked(path, px, sig, baked)
        }
        #[cfg(target_arch = "wasm32")]
        Job::Web(job) => JobResult::Web(Box::new(job.run())),
        #[cfg(target_arch = "wasm32")]
        Job::WebExport(job) => {
            let (id, path) = (job.id, job.path.clone());
            JobResult::WebExport(id, path, job.run())
        }
        #[cfg(target_arch = "wasm32")]
        Job::WebExif(job) => JobResult::Exif(job.path.clone(), job.run()),
        #[cfg(target_arch = "wasm32")]
        Job::WebMeasure(job) => JobResult::WebMeasure(job.path.clone(), job.run()),
    }
}

/// wasm32 builds std with `panic = "abort"`, so a panicking decode kills its
/// thread and `catch_unwind` never returns. A decoder panic still runs the
/// panic hook first, on the dying thread, and the hook uses that moment to
/// send the job's failure, so the caller stops waiting, and to start a
/// replacement thread, so the pool does not shrink one panic at a time. A
/// failed allocation aborts the same way, through the alloc error hook
/// instead. Either way the dying thread's memory is never freed, and its share
/// of `decode_budget` is handed back here.
#[cfg(target_arch = "wasm32")]
mod panic_recovery {
    use super::{Job, JobResult, Worker};
    use std::cell::RefCell;

    thread_local! {
        static WORKER: RefCell<Option<Worker>> = const { RefCell::new(None) };
        static FAILURE: RefCell<Option<JobResult>> = const { RefCell::new(None) };
    }

    /// Chains onto the panic hook in place, and hooks failed allocations,
    /// which abort without running the panic hook. Called once, from `main`,
    /// after the console hook is set.
    pub(crate) fn install() {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            previous(info);
            on_panic();
        }));
        std::alloc::set_alloc_error_hook(|layout| {
            web_sys::console::error_1(
                &format!("[loader] out of memory allocating {} bytes", layout.size()).into(),
            );
            on_panic();
        });
    }

    pub(super) fn enter(worker: &Worker) {
        WORKER.with(|w| *w.borrow_mut() = Some(worker.clone()));
    }

    /// Records what to report if `job` never finishes.
    pub(super) fn start(job: &Job) {
        let failure = match job {
            Job::Web(j) => Some(JobResult::Web(Box::new(j.failed(
                crate::web_decode::Failure::Decode("decoder panicked".to_string()),
            )))),
            Job::Bake(b) => Some(JobResult::Baked(b.path.clone(), b.px, b.sig, None)),
            Job::WebExport(e) => Some(JobResult::WebExport(
                e.id,
                e.path.clone(),
                Err("export panicked".to_string()),
            )),
            Job::WebExif(e) => Some(JobResult::Exif(e.path.clone(), e.failed())),
            Job::WebMeasure(m) => Some(JobResult::WebMeasure(
                m.path.clone(),
                Err("decoder panicked".to_string()),
            )),
            _ => None,
        };
        FAILURE.with(|f| *f.borrow_mut() = failure);
    }

    pub(super) fn finish() {
        FAILURE.with(|f| f.borrow_mut().take());
    }

    fn on_panic() {
        crate::decode::decode_budget::release_held_by_this_thread();
        let Some(worker) = WORKER.with(|w| w.borrow_mut().take()) else {
            return;
        };
        if let Some(failure) = FAILURE.with(|f| f.borrow_mut().take()) {
            let _ = worker.res_tx.send(failure);
        }
        let index = worker.index;
        if let Err(e) = worker.spawn() {
            web_sys::console::error_1(
                &format!("[loader] could not replace decode worker {index}: {e}").into(),
            );
        }
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use panic_recovery::install as install_panic_recovery;

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

pub struct Loader {
    shared: Arc<Shared>,
    res_rx: Receiver<JobResult>,

    /// The GPU's max texture dimension, so full decodes only downscale images
    /// too large to upload.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    full_target: u32,

    limits: CacheLimits,

    // Full-resolution tier. `order` is insertion order for LRU eviction.
    cache: HashMap<PathBuf, Arc<DecodedImage>>,
    order: VecDeque<PathBuf>,
    inflight: HashSet<PathBuf>,

    // Preview tier, keyed by `(path, target_px)` so a window resize does not
    // serve a smaller stale decode.
    preview_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    preview_order: VecDeque<(PathBuf, u32)>,
    preview_inflight: HashSet<(PathBuf, u32)>,

    // `Speed` results, same key as the preview tier. A separate map lets a
    // short speed result show at once and be replaced when the preview lands.
    speed_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    speed_inflight: HashSet<(PathBuf, u32)>,
    /// Keys the user has viewed, which get the `Preview` decode if their speed
    /// pass came back short. Prefetched neighbors are not in this set.
    escalation_wanted: HashSet<(PathBuf, u32)>,

    thumb_cache: HashMap<(PathBuf, u32), Arc<DecodedImage>>,
    thumb_order: VecDeque<(PathBuf, u32)>,
    /// Background thumbnails asked for and not yet answered.
    thumb_inflight: HashSet<(PathBuf, u32)>,
    /// Viewport thumbnails queued or running. Kept apart from
    /// `thumb_inflight` so pruning a viewport job can never lose a background
    /// request, and a viewport request never waits behind a folder-wide
    /// background backlog. The price is that a thumbnail wanted by both can
    /// be decoded twice, the second time from the on-disk cache.
    viewport_inflight: HashSet<(PathBuf, u32)>,
    /// Failed thumbnail keys, so callers stop re-requesting them every frame.
    thumb_failed: HashSet<(PathBuf, u32)>,
    thumb_capacity: usize,

    meta_inflight: HashSet<PathBuf>,

    exif_inflight: HashSet<PathBuf>,

    /// The edit signature each queued or running bake was asked for. One
    /// per thumbnail: a newer edit replaces a queued older bake.
    bake_inflight: HashMap<(PathBuf, u32), u64>,
    /// Bakes that produced nothing, so a bad thumbnail is not re-queued
    /// every frame. Keyed with the signature, so the next edit retries.
    bake_failed: HashSet<(PathBuf, u32, u64)>,
    /// Finished bakes not yet taken by `take_baked`.
    baked: Vec<BakedThumb>,
    /// wasm32: finished browser decodes not yet taken by `take_web_results`.
    #[cfg(target_arch = "wasm32")]
    web_results: Vec<crate::web_decode::PoolResult>,
    /// wasm32: finished exports not yet taken by `take_web_exports`.
    #[cfg(target_arch = "wasm32")]
    web_exports: Vec<(u64, PathBuf, Result<Vec<u8>, String>)>,
    /// wasm32: finished measurements not yet taken by `take_web_measures`.
    #[cfg(target_arch = "wasm32")]
    web_measures: Vec<(PathBuf, Result<crate::develop::CaScale, String>)>,
    /// Kept so `web_decoder` can hand async readers a way to submit.
    #[cfg(target_arch = "wasm32")]
    res_tx: std::sync::mpsc::Sender<JobResult>,

    /// Worker threads that actually started. Zero on wasm32, where spawning
    /// fails and the browser's Web Worker pool decodes instead.
    workers: usize,
}

impl Loader {
    pub fn new(max_dim: u32, limits: CacheLimits) -> Self {
        #[cfg(target_arch = "wasm32")]
        let cores = wasm_thread::available_parallelism().map(|n| n.get());
        #[cfg(not(target_arch = "wasm32"))]
        let cores = thread::available_parallelism().map(|n| n.get());
        let cores = cores.unwrap_or(4);
        let workers = cores
            .saturating_sub(2)
            .clamp(1, limits.decode_threads.max(1));
        Self::with_workers(max_dim, workers, limits)
    }

    fn with_workers(max_dim: u32, workers: usize, limits: CacheLimits) -> Self {
        let (res_tx, res_rx) = std::sync::mpsc::channel::<JobResult>();

        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue::default()),
            ready: Condvar::new(),
        });
        let thumbs = Arc::new(ThumbCache::new());

        let mut started = 0;
        for i in 0..workers {
            // Worker reservation: worker 0 never takes thumbnails, so a loupe
            // decode always has a free worker even when every other worker is
            // busy with thumbnails. With only one worker, reserving it would
            // starve thumbnails, so there is no reservation.
            let dedicated_full = i == 0 && workers > 1;
            let worker = Worker {
                index: i,
                dedicated_full,
                shared: Arc::clone(&shared),
                res_tx: res_tx.clone(),
                thumbs: Arc::clone(&thumbs),
            };
            match worker.spawn() {
                Ok(()) => started += 1,
                Err(e) => eprintln!("[loader] could not spawn decode worker {i}: {e}"),
            }
        }

        Self {
            shared,
            res_rx,
            full_target: max_dim,
            limits,
            cache: HashMap::new(),
            order: VecDeque::new(),
            inflight: HashSet::new(),
            preview_cache: HashMap::new(),
            preview_order: VecDeque::new(),
            preview_inflight: HashSet::new(),
            speed_cache: HashMap::new(),
            speed_inflight: HashSet::new(),
            escalation_wanted: HashSet::new(),
            thumb_cache: HashMap::new(),
            thumb_order: VecDeque::new(),
            thumb_inflight: HashSet::new(),
            viewport_inflight: HashSet::new(),
            thumb_failed: HashSet::new(),
            thumb_capacity: limits.thumbs,
            meta_inflight: HashSet::new(),
            exif_inflight: HashSet::new(),
            bake_inflight: HashMap::new(),
            bake_failed: HashSet::new(),
            baked: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            web_results: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            web_exports: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            web_measures: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            res_tx,
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
    /// `Yes`: with no workers, a path-only job on wasm32, or a poisoned queue
    /// nothing would ever clear the marker, and the frame loop would poll
    /// forever.
    fn enqueue(&self, job: Job) -> Enqueued {
        if self.workers == 0 {
            return Enqueued::NoWorkers;
        }
        // On wasm32 the threads cannot open files by path, so a path-only
        // decode has nothing to run on. The browser reads the bytes and
        // submits `Job::Web` through `WebDecoder` instead.
        #[cfg(target_arch = "wasm32")]
        if !matches!(job, Job::Bake(_) | Job::Web(_) | Job::WebExport(_)) {
            return Enqueued::NoWorkers;
        }
        push_job(&self.shared, job)
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
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
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

    /// True while a metadata read has not reported back. Workers don't wake
    /// the event loop, so without polling for it the info panel stays blank
    /// until the next input.
    pub fn has_pending_exif(&self) -> bool {
        !self.exif_inflight.is_empty()
    }

    /// True while the full decode of `path` is queued or running.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn full_inflight(&self, path: &Path) -> bool {
        self.inflight.contains(path)
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

    /// Replaces the viewport's thumbnail request with `paths`, in the order
    /// they should load. Queued viewport jobs not in `paths` are dropped;
    /// one a worker already took runs to completion and still lands in the
    /// cache. Cached, failed, and already requested paths are skipped, so
    /// calling this every frame with the same list enqueues nothing.
    pub fn set_viewport_thumbs(&mut self, paths: &[PathBuf], max_px: u32) {
        // wasm32 threads run bakes only for now, see `enqueue`.
        if self.workers == 0 || cfg!(target_arch = "wasm32") {
            return;
        }
        let wanted: Vec<(PathBuf, u32)> = paths
            .iter()
            .map(|p| (p.clone(), max_px))
            .filter(|k| !self.thumb_cache.contains_key(k) && !self.thumb_failed.contains(k))
            .collect();
        let Ok(mut q) = lock_queue(&self.shared) else {
            // Poison is permanent and workers exit on it, so report every
            // pending thumbnail as failed rather than loading forever.
            self.thumb_failed.extend(wanted);
            self.thumb_failed.extend(self.viewport_inflight.drain());
            return;
        };
        let keep: HashSet<&(PathBuf, u32)> = wanted.iter().collect();
        q.thumbs_viewport.retain(|job| {
            let Job::Thumb(path, px) = job else {
                return true;
            };
            let key = (path.clone(), *px);
            if keep.contains(&key) {
                return true;
            }
            self.viewport_inflight.remove(&key);
            false
        });
        let mut pushed = false;
        for key in wanted {
            if self.viewport_inflight.insert(key.clone()) {
                q.thumbs_viewport.push_back(Job::Thumb(key.0, key.1));
                pushed = true;
            }
        }
        if pushed {
            self.shared.ready.notify_all();
        }
    }

    /// Background thumbnails asked for and not yet answered. Callers that
    /// batch over a whole folder are expected to keep this bounded, because
    /// each one in flight becomes a decoded thumbnail the cache has to hold.
    #[cfg(test)]
    pub(crate) fn thumbs_in_flight(&self) -> usize {
        self.thumb_inflight.len()
    }

    /// Viewport thumbnail keys still queued, in queue order.
    #[cfg(test)]
    pub(crate) fn queued_viewport_thumbs(&self) -> Vec<(PathBuf, u32)> {
        let q = self.shared.queue.lock().unwrap();
        q.thumbs_viewport
            .iter()
            .filter_map(|job| match job {
                Job::Thumb(path, px) => Some((path.clone(), *px)),
                _ => None,
            })
            .collect()
    }

    /// Reports one worker but spawns none, so enqueued jobs stay queued and
    /// tests can inspect them.
    #[cfg(test)]
    pub(crate) fn queue_only_for_test() -> Self {
        let mut loader = Self::with_workers(16384, 0, CacheLimits::PLATFORM);
        loader.workers = 1;
        loader
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
    /// returned by [`poll_all`](Self::poll_all). False when no result will.
    pub fn request_meta(&mut self, path: PathBuf) -> bool {
        if self.meta_inflight.contains(&path) {
            return true;
        }
        if self.enqueue(Job::Meta(path.clone())) == Enqueued::Yes {
            self.meta_inflight.insert(path);
            return true;
        }
        false
    }

    /// Requests `path`'s camera, lens, and exposure metadata. The result arrives
    /// in the fourth list returned by [`poll_all`](Self::poll_all). Call it for
    /// the viewed photo only; exif jobs outrank thumbnails.
    // wasm32 goes through `begin_web_exif`, because a thread there cannot
    // open the file by path.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    pub fn request_exif(&mut self, path: PathBuf) {
        if self.exif_inflight.contains(&path) {
            return;
        }
        if self.enqueue(Job::Exif(path.clone())) == Enqueued::Yes {
            self.exif_inflight.insert(path);
        }
    }

    /// wasm32: marks `path`'s metadata in flight and returns true, or false
    /// if it already is. The caller then reads the file and hands the bytes
    /// to [`WebDecoder::submit_exif`], whose result arrives through
    /// [`poll_all`](Self::poll_all) like a native `Exif` job.
    #[cfg(target_arch = "wasm32")]
    pub fn begin_web_exif(&mut self, path: &Path) -> bool {
        self.exif_inflight.insert(path.to_path_buf())
    }

    #[allow(dead_code)]
    /// wasm32: a handle the browser's async file reads submit decodes
    /// through, since a `spawn_local` future cannot borrow the `Loader`.
    #[cfg(target_arch = "wasm32")]
    pub fn web_decoder(&self) -> WebDecoder {
        WebDecoder {
            shared: Arc::clone(&self.shared),
            res_tx: self.res_tx.clone(),
            workers: self.workers,
        }
    }

    /// wasm32: finished browser decodes since the last call. Filled by
    /// `poll_all`, so call it after that.
    #[cfg(target_arch = "wasm32")]
    pub fn take_web_results(&mut self) -> Vec<crate::web_decode::PoolResult> {
        std::mem::take(&mut self.web_results)
    }

    /// wasm32: finished exports since the last call, as `(id, source,
    /// JPEG)`. Filled by `poll_all`.
    #[cfg(target_arch = "wasm32")]
    pub fn take_web_exports(&mut self) -> Vec<(u64, PathBuf, Result<Vec<u8>, String>)> {
        std::mem::take(&mut self.web_exports)
    }

    /// wasm32: finished Remove Chromatic Aberration measurements since the
    /// last call. Filled by `poll_all`.
    #[cfg(target_arch = "wasm32")]
    pub fn take_web_measures(&mut self) -> Vec<(PathBuf, Result<crate::develop::CaScale, String>)> {
        std::mem::take(&mut self.web_measures)
    }

    /// wasm32: drops queued browser thumbnails whose photo `keep` rejects,
    /// for cells that scrolled away before a thread took them, and returns
    /// their keys so the caller clears its in-flight markers. A decode
    /// already running finishes and still lands.
    #[cfg(target_arch = "wasm32")]
    pub fn retain_web_thumbs(&mut self, keep: impl Fn(&Path) -> bool) -> Vec<(PathBuf, u32)> {
        let Ok(mut q) = lock_queue(&self.shared) else {
            return Vec::new();
        };
        let mut dropped = Vec::new();
        q.thumbs_viewport.retain(|job| match job {
            Job::Web(w) if w.kind == crate::web_decode::JobKind::Thumb && !keep(&w.path) => {
                dropped.push((w.path.clone(), w.target));
                false
            }
            _ => true,
        });
        dropped
    }

    /// wasm32: drops queued browser Loupe decodes (`Speed` and `Preview`)
    /// and metadata parses whose photo `keep` rejects. Returns the decodes'
    /// kinds and keys so the caller clears its in-flight markers, and clears
    /// the metadata markers itself. Arrowing through RAWs queues a `Preview`
    /// per photo, and only one thread runs them (see
    /// `Worker::takes_previews`), so without this the photo the user stops
    /// on waits behind every photo passed on the way: 11 s after 19 ARWs.
    /// Each queued job also holds its photo's whole file.
    #[cfg(target_arch = "wasm32")]
    pub fn retain_web_loupe(
        &mut self,
        keep: impl Fn(&Path) -> bool,
    ) -> Vec<(crate::web_decode::JobKind, PathBuf, u32)> {
        let Ok(mut q) = lock_queue(&self.shared) else {
            return Vec::new();
        };
        let mut dropped = Vec::new();
        let mut dropped_exif = Vec::new();
        let mut prune = |job: &Job| match job {
            Job::Web(w) if !keep(&w.path) => {
                dropped.push((w.kind, w.path.clone(), w.target));
                false
            }
            Job::WebExif(e) if !keep(&e.path) => {
                dropped_exif.push(e.path.clone());
                false
            }
            _ => true,
        };
        q.speed.retain(&mut prune);
        q.preview.retain(&mut prune);
        for path in dropped_exif {
            self.exif_inflight.remove(&path);
        }
        dropped
    }

    /// Bake `img` with these edits off the frame. The result comes back
    /// through [`take_baked`](Self::take_baked). A bake already queued for an
    /// older `sig` of the same thumbnail is replaced, so a slider drag leaves
    /// at most one bake per thumbnail behind it. If no worker thread started it
    /// bakes inline, as the frame did before there was a lane for it.
    #[allow(clippy::too_many_arguments)]
    pub fn request_bake(
        &mut self,
        path: &Path,
        px: u32,
        sig: u64,
        img: Arc<DecodedImage>,
        adj: Adjustments,
        touchups: &[TouchUp],
        rot: u8,
    ) {
        let key = (path.to_path_buf(), px);
        if self.bake_inflight.get(&key) == Some(&sig)
            || self.bake_failed.contains(&(key.0.clone(), px, sig))
        {
            return;
        }
        let job = BakeJob {
            path: key.0.clone(),
            px,
            sig,
            img,
            adj,
            touchups: touchups.to_vec(),
            rot,
        };
        if self.workers == 0 {
            let baked = job.run();
            self.land_bake(key.0, px, sig, Some(baked));
            return;
        }
        if self.bake_inflight.contains_key(&key) {
            self.drop_queued_bakes(|k| k == &key);
        }
        if self.enqueue(Job::Bake(Box::new(job))) == Enqueued::Yes {
            self.bake_inflight.insert(key, sig);
        }
    }

    /// Drop queued bakes whose thumbnail `keep` rejects, for cells that
    /// scrolled away before their bake ran. A bake already running finishes.
    pub fn retain_bakes(&mut self, keep: impl Fn(&(PathBuf, u32)) -> bool) {
        if self.bake_inflight.keys().all(&keep) {
            return;
        }
        self.drop_queued_bakes(|k| !keep(k));
    }

    fn drop_queued_bakes(&mut self, drop: impl Fn(&(PathBuf, u32)) -> bool) {
        let Ok(mut q) = lock_queue(&self.shared) else {
            return;
        };
        q.bake.retain(|job| match job {
            Job::Bake(b) => {
                let key = (b.path.clone(), b.px);
                if drop(&key) {
                    self.bake_inflight.remove(&key);
                    false
                } else {
                    true
                }
            }
            _ => true,
        });
    }

    fn land_bake(&mut self, path: PathBuf, px: u32, sig: u64, baked: Option<BakedThumb>) {
        let key = (path, px);
        if self.bake_inflight.get(&key) == Some(&sig) {
            self.bake_inflight.remove(&key);
        }
        match baked {
            Some(b) if b.width > 0 && b.height > 0 => self.baked.push(b),
            _ => {
                self.bake_failed.insert((key.0, px, sig));
            }
        }
    }

    /// Finished bakes since the last call, oldest first. The caller checks
    /// each `sig` against the edit it wants now.
    pub fn take_baked(&mut self) -> Vec<BakedThumb> {
        std::mem::take(&mut self.baked)
    }

    /// True while a bake is queued or running, or finished and not yet taken,
    /// so the frame loop keeps polling for it.
    pub fn bakes_pending(&self) -> bool {
        !self.bake_inflight.is_empty() || self.has_baked()
    }

    /// True when a finished bake is waiting for `take_baked`, which only a
    /// redraw calls.
    pub fn has_baked(&self) -> bool {
        !self.baked.is_empty()
    }

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
    #[cfg(any(target_arch = "wasm32", test))]
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
    #[cfg(any(target_arch = "wasm32", test))]
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
                let queue = match lock_queue(&self.shared) {
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
            self.thumb_failed.extend(self.viewport_inflight.drain());
            self.meta_inflight.clear();
            self.exif_inflight.clear();
            self.bake_inflight.clear();
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
                    self.viewport_inflight.remove(&key);
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
                JobResult::Baked(path, px, sig, baked) => self.land_bake(path, px, sig, baked),
                #[cfg(target_arch = "wasm32")]
                JobResult::Web(r) => self.web_results.push(*r),
                #[cfg(target_arch = "wasm32")]
                JobResult::WebExport(id, path, r) => self.web_exports.push((id, path, r)),
                #[cfg(target_arch = "wasm32")]
                JobResult::WebMeasure(path, r) => self.web_measures.push((path, r)),
            }
        }
        (full, thumbs, metas, exifs)
    }

    fn insert(&mut self, path: PathBuf, img: Arc<DecodedImage>) {
        if !self.cache.contains_key(&path) {
            self.order.push_back(path.clone());
        }
        self.cache.insert(path, img);
        while self.order.len() > self.limits.fulls {
            if let Some(old) = self.order.pop_front() {
                self.cache.remove(&old);
            }
        }
    }

    // Speed and preview results share `preview_order` and one budget. A key
    // lives in at most one of the two maps: a landed preview replaces the
    // speed result, which `get_preview` would never return again.
    fn insert_speed(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        if self.preview_cache.contains_key(&key) {
            return;
        }
        if !self.speed_cache.contains_key(&key) {
            self.preview_order.push_back(key.clone());
        }
        self.speed_cache.insert(key, img);
        self.evict_previews();
    }

    fn insert_preview(&mut self, key: (PathBuf, u32), img: Arc<DecodedImage>) {
        let known =
            self.speed_cache.remove(&key).is_some() || self.preview_cache.contains_key(&key);
        if !known {
            self.preview_order.push_back(key.clone());
        }
        self.preview_cache.insert(key, img);
        self.evict_previews();
    }

    fn evict_previews(&mut self) {
        while self.preview_order.len() > self.limits.previews {
            if let Some(old) = self.preview_order.pop_front() {
                self.preview_cache.remove(&old);
                self.speed_cache.remove(&old);
            }
        }
    }

    /// Sets the thumbnail cache capacity to `len` (never below
    /// `CacheLimits::thumbs`), so a visible grid plus its prefetch rows never evicts
    /// itself. Shrinking evicts the oldest entries at once.
    pub fn set_thumb_working_set_size(&mut self, len: usize) {
        self.thumb_capacity = self.limits.thumbs.max(len);
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
        if let Ok(mut q) = lock_queue(&self.shared) {
            q.shutdown = true;
        }
        self.shared.ready.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::image_decode::DecodedImageFields;

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
            Job::Bake(_) => "bake",
        }
    }

    fn bake_job(name: &str, sig: u64) -> Job {
        Job::Bake(Box::new(BakeJob {
            path: path(name),
            px: 512,
            sig,
            img: image(2, 2),
            adj: Adjustments::default(),
            touchups: Vec::new(),
            rot: 0,
        }))
    }

    fn queued_bakes(loader: &Loader) -> Vec<(PathBuf, u64)> {
        let q = loader.shared.queue.lock().unwrap();
        q.bake
            .iter()
            .map(|job| match job {
                Job::Bake(b) => (b.path.clone(), b.sig),
                _ => unreachable!(),
            })
            .collect()
    }

    /// One job of each kind, pushed in reverse priority order so a queue that
    /// kept insertion order would fail.
    fn every_kind() -> Queue {
        let mut q = Queue::default();
        q.meta.push_back(Job::Meta(path("m")));
        q.thumbs.push_back(Job::Thumb(path("t"), 192));
        q.bake.push_back(bake_job("b", 1));
        q.exif.push_back(Job::Exif(path("e")));
        q.full.push_back(Job::Full(path("f"), 16384));
        q.preview.push_back(Job::Preview(path("p"), 2048));
        q.speed.push_back(Job::Speed(path("q"), 2048));
        q
    }

    fn drain_labels(q: &mut Queue, dedicated: bool) -> Vec<&'static str> {
        let mut out = vec![];
        while let Some(job) = q.take_next(dedicated, true) {
            out.push(label(&job));
        }
        out
    }

    #[test]
    fn a_general_worker_serves_every_tier_in_priority_order() {
        assert_eq!(
            drain_labels(&mut every_kind(), false),
            ["speed", "preview", "full", "exif", "bake", "thumb", "meta"]
        );
    }

    #[test]
    fn the_dedicated_worker_skips_thumbnails_and_bakes_entirely() {
        assert_eq!(
            drain_labels(&mut every_kind(), true),
            ["speed", "preview", "full", "exif", "meta"]
        );
    }

    #[test]
    fn a_worker_that_skips_previews_leaves_them_queued() {
        let mut q = every_kind();
        let mut out = vec![];
        while let Some(job) = q.take_next(false, false) {
            out.push(label(&job));
        }
        assert_eq!(out, ["speed", "full", "exif", "bake", "thumb", "meta"]);
        assert_eq!(drain_labels(&mut q, true), ["preview"]);
    }

    #[test]
    fn a_newer_edit_replaces_the_queued_bake_for_that_thumbnail() {
        let mut loader = Loader::queue_only_for_test();
        let a = path("a");
        loader.request_bake(&a, 512, 1, image(2, 2), Adjustments::default(), &[], 1);
        loader.request_bake(&a, 512, 2, image(2, 2), Adjustments::default(), &[], 1);
        loader.request_bake(&a, 512, 2, image(2, 2), Adjustments::default(), &[], 1);
        assert_eq!(queued_bakes(&loader), [(a, 2)]);
    }

    #[test]
    fn a_cell_that_scrolled_away_loses_its_queued_bake() {
        let mut loader = Loader::queue_only_for_test();
        let (gone, kept) = (path("gone"), path("kept"));
        loader.request_bake(&gone, 512, 1, image(2, 2), Adjustments::default(), &[], 1);
        loader.request_bake(&kept, 512, 1, image(2, 2), Adjustments::default(), &[], 1);
        loader.retain_bakes(|(p, _)| p == &kept);
        assert_eq!(queued_bakes(&loader), [(kept, 1)]);
        // Its marker went with it, so scrolling back queues it again.
        loader.request_bake(&gone, 512, 1, image(2, 2), Adjustments::default(), &[], 1);
        assert_eq!(queued_bakes(&loader).len(), 2);
    }

    #[test]
    fn without_workers_a_bake_runs_inline_with_its_rotation() {
        let mut loader = Loader::with_workers(16384, 0, CacheLimits::PLATFORM);
        loader.request_bake(
            &path("a"),
            512,
            7,
            image(4, 2),
            Adjustments::default(),
            &[],
            1,
        );
        let baked = loader.take_baked();
        assert_eq!(baked.len(), 1);
        assert_eq!((baked[0].sig, baked[0].width, baked[0].height), (7, 2, 4));
        assert!(!loader.bakes_pending());
    }

    #[test]
    fn a_bake_that_yields_nothing_is_not_queued_again_until_the_edit_changes() {
        let mut loader = Loader::queue_only_for_test();
        let a = path("a");
        loader.request_bake(&a, 512, 1, image(0, 0), Adjustments::default(), &[], 1);
        let job = loader
            .shared
            .queue
            .lock()
            .unwrap()
            .bake
            .pop_front()
            .unwrap();
        let Job::Bake(job) = job else { unreachable!() };
        loader.land_bake(a.clone(), 512, 1, Some(job.run()));
        assert!(loader.take_baked().is_empty());
        assert!(!loader.bakes_pending());

        loader.request_bake(&a, 512, 1, image(0, 0), Adjustments::default(), &[], 1);
        assert!(queued_bakes(&loader).is_empty());
        loader.request_bake(&a, 512, 2, image(0, 0), Adjustments::default(), &[], 1);
        assert_eq!(queued_bakes(&loader), [(a, 2)]);
    }

    #[test]
    fn viewport_thumbnails_outrank_background_ones_and_the_dedicated_worker_takes_neither() {
        let mut q = Queue::default();
        q.thumbs.push_back(Job::Thumb(path("background"), 192));
        q.thumbs_viewport
            .push_back(Job::Thumb(path("viewport"), 192));
        assert!(q.take_next(true, true).is_none());
        let names: Vec<PathBuf> = std::iter::from_fn(|| q.take_next(false, true))
            .map(|job| match job {
                Job::Thumb(p, _) => p,
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(names, [path("viewport"), path("background")]);
    }

    fn keys(names: &[&str]) -> Vec<(PathBuf, u32)> {
        names.iter().map(|n| (path(n), 192)).collect()
    }

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(|n| path(n)).collect()
    }

    #[test]
    fn a_new_viewport_drops_the_queued_jobs_that_scrolled_off() {
        let mut loader = Loader::queue_only_for_test();
        loader.set_viewport_thumbs(&paths(&["a", "b", "c"]), 192);
        loader.set_viewport_thumbs(&paths(&["c", "d"]), 192);
        assert_eq!(loader.queued_viewport_thumbs(), keys(&["c", "d"]));
        assert_eq!(
            loader.viewport_inflight,
            keys(&["c", "d"]).into_iter().collect()
        );
    }

    #[test]
    fn the_same_viewport_every_frame_enqueues_nothing_new() {
        let mut loader = Loader::queue_only_for_test();
        loader.set_viewport_thumbs(&paths(&["a", "b"]), 192);
        loader.set_viewport_thumbs(&paths(&["a", "b"]), 192);
        assert_eq!(loader.queued_viewport_thumbs(), keys(&["a", "b"]));
    }

    #[test]
    fn a_viewport_job_a_worker_took_keeps_its_marker_and_is_not_requeued() {
        let mut loader = Loader::queue_only_for_test();
        loader.set_viewport_thumbs(&paths(&["a"]), 192);
        let taken = loader.shared.queue.lock().unwrap().take_next(false, true);
        assert!(matches!(taken, Some(Job::Thumb(..))));
        loader.set_viewport_thumbs(&paths(&["b"]), 192);
        assert!(loader.viewport_inflight.contains(&(path("a"), 192)));
        loader.set_viewport_thumbs(&paths(&["a", "b"]), 192);
        assert_eq!(loader.queued_viewport_thumbs(), keys(&["b"]));
    }

    #[test]
    fn a_background_request_survives_a_viewport_that_scrolled_past_it() {
        // The Auto Tone guarantee: it asks once and waits for the result.
        let mut loader = Loader::queue_only_for_test();
        loader.request_thumb(path("x"), 192);
        loader.set_viewport_thumbs(&paths(&["a"]), 192);
        loader.set_viewport_thumbs(&paths(&["b"]), 192);
        assert_eq!(loader.thumbs_in_flight(), 1);
        let q = loader.shared.queue.lock().unwrap();
        assert!(matches!(q.thumbs.front(), Some(Job::Thumb(p, 192)) if *p == path("x")));
    }

    #[test]
    fn a_poisoned_queue_fails_viewport_requests_instead_of_loading_forever() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.poison_thumb_queue_for_test(path("a"), 192);
        loader.set_viewport_thumbs(&paths(&["b"]), 192);
        assert!(loader.thumb_failed(&path("b"), 192));
        assert!(loader.viewport_inflight.is_empty());
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
        assert!(Queue::default().take_next(false, true).is_none());
        assert!(Queue::default().take_next(true, true).is_none());
    }

    fn image(w: u32, h: u32) -> Arc<DecodedImage> {
        Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba: vec![0; (w * h * 4) as usize],
            pixel_format: image_decode::PixelFormat::Srgb8,
        }))
    }

    #[test]
    fn speed_and_preview_results_share_one_budget() {
        let mut loader = Loader::with_workers(16384, 0, CacheLimits::PLATFORM);
        for i in 0..CacheLimits::PLATFORM.previews {
            loader.insert_speed((path(&i.to_string()), 2560), image(2, 2));
        }
        // The sharper preview replaces its own speed result.
        loader.insert_preview((path("0"), 2560), image(4, 4));
        assert_eq!(loader.get_preview(&path("0"), 2560).unwrap().width, 4);
        assert_eq!(
            loader.speed_cache.len() + loader.preview_cache.len(),
            CacheLimits::PLATFORM.previews
        );
        // A new arrival evicts the oldest entry across both maps.
        loader.insert_preview((path("new"), 2560), image(4, 4));
        assert_eq!(
            loader.speed_cache.len() + loader.preview_cache.len(),
            CacheLimits::PLATFORM.previews
        );
        assert!(loader.get_preview(&path("0"), 2560).is_none());
    }

    #[test]
    fn thumbnail_cache_retains_large_grid_and_shrinks_after_resize() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
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
        assert_eq!(loader.thumb_cache.len(), CacheLimits::PLATFORM.thumbs);
        assert_eq!(loader.thumb_order.len(), CacheLimits::PLATFORM.thumbs);
        assert!(loader.get_thumb(&path("0"), px).is_none());
        assert!(loader
            .get_thumb(&path(&(working_set - 1).to_string()), px)
            .is_some());
    }

    #[test]
    fn the_full_tier_evicts_oldest_first_at_capacity() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        for i in 0..CacheLimits::PLATFORM.fulls + 2 {
            loader.insert(path(&i.to_string()), image(1, 1));
        }
        assert_eq!(loader.cache.len(), CacheLimits::PLATFORM.fulls);
        assert!(loader.get_full(&path("0")).is_none());
        assert!(loader.get_full(&path("1")).is_none());
        assert!(loader
            .get_full(&path(&(CacheLimits::PLATFORM.fulls + 1).to_string()))
            .is_some());
    }

    #[test]
    fn the_preview_tier_keys_on_target_so_a_resize_does_not_serve_a_stale_size() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.insert_preview((path("a"), 2048), image(2048, 1365));
        assert!(loader.get_preview(&path("a"), 2048).is_some());
        assert!(loader.get_preview(&path("a"), 2560).is_none());
    }

    #[test]
    fn get_best_prefers_full_resolution_over_the_preview() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
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
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.escalate_if_short(&path("a"), 2560, 2560);
        assert_eq!(queued_previews(&loader), 0);
        loader.escalate_if_short(&path("b"), 2560, 4096);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn a_metadata_read_is_pending_until_its_result_drains() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.request_exif(path("missing.jpg"));
        assert!(loader.has_pending_exif());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut exifs = Vec::new();
        while exifs.is_empty() && std::time::Instant::now() < deadline {
            exifs = loader.poll_all().3;
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(exifs.len(), 1, "the worker reported the read");
        assert!(!loader.has_pending_exif());
    }

    #[test]
    fn a_short_speed_pass_queues_the_forced_decode_behind_it() {
        // The RAW case: a 1616px embedded preview, then the 2560px decode.
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
        assert!(loader.has_pending_image());
    }

    #[test]
    fn without_workers_requests_leave_nothing_pending() {
        // wasm32 has no decode threads. A request that marks itself in flight
        // there is never cleared, and the frame loop polls forever.
        let mut loader = Loader::with_workers(16384, 0, CacheLimits::PLATFORM);
        loader.prefetch_preview(path("a"), 2560);
        loader.request_full(path("a"));
        loader.request_exif(path("a"));
        loader.request_meta(path("a"));
        loader.request_thumb(path("a"), 192);
        loader.set_viewport_thumbs(&paths(&["a"]), 192);
        assert!(!loader.has_pending_image());
        assert!(loader.exif_inflight.is_empty());
        assert!(loader.meta_inflight.is_empty());
        assert!(loader.thumb_inflight.is_empty());
        assert!(loader.viewport_inflight.is_empty());
        assert!(!loader.thumb_failed(&path("a"), 192));
    }

    #[test]
    fn a_failed_speed_pass_still_falls_through_to_the_forced_decode() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.escalate_if_short(&path("a"), 2560, 0);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn escalation_does_not_pile_up_duplicate_jobs() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        loader.escalate_if_short(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn a_prefetched_neighbor_stops_at_the_cheap_pass() {
        // Escalating prefetches would compete with the viewed photo's decode.
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.prefetch_preview(path("neighbor"), 2560);
        loader.insert_speed((path("neighbor"), 2560), image(1616, 1080));
        loader.escalate_from_speed(&path("neighbor"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 0);
    }

    #[test]
    fn navigating_onto_a_prefetched_photo_escalates_it_after_all() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.prefetch_preview(path("a"), 2560);
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        loader.request_preview(path("a"), 2560);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn viewing_a_photo_whose_prefetch_is_still_running_does_not_lose_the_escalation() {
        // The user views a photo while its prefetch speed pass is still running.
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.prefetch_preview(path("a"), 2560);
        loader.request_preview(path("a"), 2560); // still in flight
        loader.speed_inflight.remove(&(path("a"), 2560));
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        loader.escalate_from_speed(&path("a"), 2560, 1616);
        assert_eq!(queued_previews(&loader), 1);
    }

    #[test]
    fn the_preview_getter_prefers_the_forced_decode_over_the_speed_pass() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.insert_speed((path("a"), 2560), image(1616, 1080));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 1616);
        loader.insert_preview((path("a"), 2560), image(2560, 1707));
        assert_eq!(loader.get_preview(&path("a"), 2560).unwrap().width, 2560);
    }

    #[test]
    fn requesting_a_preview_that_is_already_answered_enqueues_nothing() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
        loader.insert_speed((path("a"), 2560), image(2560, 1707));
        loader.request_preview(path("a"), 2560);
        assert!(!loader.has_pending_image());
    }

    #[test]
    fn pending_image_tracks_both_loupe_tiers_and_ignores_the_others() {
        let mut loader = Loader::new(16384, CacheLimits::PLATFORM);
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
