//! wasm32-only: a hand-rolled `web_sys::Worker` pool — the wasm port plan's
//! M4 (real threading). Each worker runs an independent instance of the
//! `wasm_worker` binary (`src/web/wasm_worker.rs`), decoding on its own
//! thread with its own separate wasm linear memory — no `SharedArrayBuffer`/
//! atomics, no nightly toolchain, chosen specifically over
//! `wasm-bindgen-rayon` (whose JS-orchestrated `init()`/`initThreadPool()`
//! usage model doesn't fit this app's binary-crate + `spawn_app` entry
//! point, and whose documented bundler support never mentions `trunk`).
//!
//! `App` holds the `WorkerPool` itself (for `poll`); `app/web.rs`'s spawned
//! byte-read tasks hold a cheaply-`Clone`-able [`WorkerPoolHandle`] (a wrapped
//! `Rc<RefCell<..>>`, safe here because wasm32 is single-threaded — nothing
//! but re-entrant JS callbacks ever touches it) so they can submit a decode
//! job without borrowing `App` across the `spawn_local` future's `'static`
//! bound, the same shape `web_thumb_tx`/`web_preview_tx` already use for the
//! same reason.
//!
//! ## Pipeline position
//! - Main-thread dispatcher for wasm32's Pipeline 1 (Loupe) and Pipeline 2
//!   (Grid/filmstrip) decode: `app/web.rs` calls
//!   `WorkerPoolHandle::submit` after reading a file's bytes
//!   (`web_fs::read_array_buffer`), tagged with a `JobKind` that says which
//!   tier this decode is for.
//! - `submit` derives `quality` from `JobKind` and queues the job; `pump`
//!   hands it to the next idle, ready `Worker` (running `wasm_worker.rs`).
//! - `App::poll` (called every frame) drains `WorkerPool::poll`'s finished
//!   results back into `app/web.rs`'s tier-specific handling, which lands
//!   them in `loader.rs`'s caches.
//! - See `ARCHITECTURE.md`.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};

use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{
    Blob, BlobPropertyBag, ErrorEvent, FileSystemDirectoryHandle, MessageEvent, Url, Worker,
};

const MAX_REPLACEMENT_ATTEMPTS: u8 = 3;
const WORKER_READY_TIMEOUT_MS: i32 = 10_000;

use crate::image_decode::{DecodedImage, PixelFormat};

/// Which cache tier a finished decode belongs in — mirrors
/// `insert_thumb_external`/`insert_preview_external`/`insert_full_external`
/// in `loader.rs` (except `Speed`, which bypasses `loader.rs`'s cache
/// entirely — see `app/web.rs`'s `poll_web_preview` doc comment for why).
/// Also drives `submit()`'s `quality` derivation (see its doc comment):
/// `Preview`/`Full` (Loupe, real content) get full PPG demosaic + linear
/// output; `Thumb`/`Speed` (Grid, and the Loupe's screen-fit first paint)
/// stay on the quarter-res Fast tier.
///
/// A `Speed` tier at screen resolution — a cheap first pass shown before the
/// `Preview`/quality decode lands — was tried once before and reverted: the
/// Loupe's zoom transform was carried across a same-photo tier upgrade
/// rather than recomputed (`app/thumbs.rs::upload_shown`), so an extra tier
/// boundary meant an extra chance for a wrongly-zoomed flash. That's now
/// fixed at the root (`upload_shown` re-fits on any same-photo tier swap
/// while `self.fitted` is still true), so `Speed` is reinstated here, plus a
/// `Full` tier (real full-resolution decode, requested only once the user
/// zooms past what `Preview` holds — `app/loupe.rs::ensure_full_for_zoom`'s
/// wasm32 branch) mirroring native's own `Speed`/`Preview`/`Full` staging.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Thumb,
    Speed,
    Preview,
    Full,
    /// Pipeline 3: full-res decode → bake edits → JPEG encode, in the worker
    /// (`export::bake_jpeg`). Unlike the decode kinds this returns encoded
    /// JPEG bytes, not a `DecodedImage`, so its results come back on a
    /// separate channel (`poll_exports`), not `poll`.
    Export,
}

pub struct PoolResult {
    pub kind: JobKind,
    pub path: PathBuf,
    pub target: u32,
    pub result: Result<DecodedImage, String>,
    /// The same image encoded as a JPEG, for `JobKind::Thumb` jobs submitted
    /// with `from_cache: false` — the bytes `app/web.rs` writes into
    /// `.lightphotos/`. `None` for every other kind, for a thumbnail that was
    /// itself read back from the cache, and for a decode whose pixels a JPEG
    /// can't represent (the linear RAW tier).
    pub jpeg: Option<Vec<u8>>,
    /// The cache filename. Computed
    /// from the source's size and mtime in `app/web.rs`'s `request_web_thumbs`
    /// and carried through the job, so storing the result needs no second
    /// `get_file()` round-trip. Rides along the way `export_dest` does.
    pub cache_name: Option<String>,
    /// Originating navigation generation, independent of cache metadata.
    pub generation: Option<u64>,
    /// True when the worker decoded cached bytes, including failed jobs.
    pub from_cache: bool,
}

impl PoolResult {
    /// Cache failures are recoverable without spending a source retry.
    pub fn needs_source_decode(&self) -> bool {
        self.kind == JobKind::Thumb && self.from_cache && self.result.is_err()
    }
}

/// A finished (or failed) export job. `dest_dir`/`filename` ride the job
/// through the worker and back so the main thread can hand the bytes to
/// `WebFs::write_atomic` with no side table.
pub struct ExportPoolResult {
    pub path: PathBuf,
    pub folder: FileSystemDirectoryHandle,
    pub dest_dir: PathBuf,
    pub filename: String,
    pub result: Result<Vec<u8>, String>,
}

struct PendingMeta {
    kind: JobKind,
    /// See `PoolResult::cache_name`.
    cache_name: Option<String>,
    generation: Option<u64>,
    from_cache: bool,
    path: PathBuf,
    target: u32,
    /// `Some` only for `JobKind::Export` — the resolved output location,
    /// carried back onto `ExportPoolResult`.
    export_dest: Option<(FileSystemDirectoryHandle, PathBuf, String)>,
}

struct QueuedJob {
    id: u32,
    /// The file's raw bytes as a JS `ArrayBuffer`, not a Rust `Vec<u8>` —
    /// deliberately never copied into the main thread's own wasm memory
    /// (see `WorkerPoolHandle::submit`'s doc comment): it's read directly as
    /// an `ArrayBuffer` and transferred here as-is, so a large RAW file's
    /// bytes exist on the main thread only as this one JS-side buffer.
    bytes: js_sys::ArrayBuffer,
    max_px: u32,
    is_raw: bool,
    /// Derived from `JobKind` at `submit()` time: `Preview` (Loupe) → full
    /// PPG demosaic + linear output, `Thumb` (Grid) → the quarter-res Fast
    /// tier — downgrades quality for thumbnails. See `submit`'s doc comment.
    quality: bool,
    /// Set for a `JobKind::Thumb` job whose bytes came from the source file
    /// rather than the on-disk cache: the worker encodes the decoded image as
    /// a JPEG and returns it for `app/web.rs` to store. See `submit_thumb`.
    encode_jpeg: bool,
    /// `Some` for `JobKind::Export`: `(adjustments_json, touchups_json, rot)`
    /// — the develop/crop/rotation state `bake_jpeg` bakes in. The worker
    /// switches to its export branch whenever this is present.
    export: Option<(String, String, u8)>,
}

struct WorkerSlot {
    worker: Worker,
    /// Set once the worker's own readiness handshake message arrives — a
    /// job posted before that lands in the void (see `wasm_worker.rs`'s
    /// `run` doc comment on why the worker script itself doesn't listen
    /// until it yields to the JS event loop once).
    ready: bool,
    busy: bool,
    active_job: Option<u32>,
    generation: u32,
    replacement_attempts: u8,
    /// Permanently unusable after all replacement attempts are exhausted.
    unavailable: bool,
}

struct Inner {
    workers: Vec<WorkerSlot>,
    next_id: u32,
    pending: HashMap<u32, PendingMeta>,
    /// Decode work is kept ahead of exports so a bulk export cannot make the
    /// grid or Loupe wait behind a large FIFO backlog.
    decode_backlog: VecDeque<QueuedJob>,
    export_backlog: VecDeque<QueuedJob>,
    result_tx: Sender<PoolResult>,
    /// `JobKind::Export` results land here instead of `result_tx` — they
    /// carry JPEG bytes, not a `DecodedImage`.
    export_tx: Sender<ExportPoolResult>,
}

fn get_f64(obj: &JsValue, key: &str) -> Option<f64> {
    Reflect::get(obj, &JsValue::from_str(key)).ok()?.as_f64()
}

fn get_bool(obj: &JsValue, key: &str) -> bool {
    Reflect::get(obj, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn get_string(obj: &JsValue, key: &str) -> Option<String> {
    Reflect::get(obj, &JsValue::from_str(key)).ok()?.as_string()
}

/// Number of workers that can actually accept work right now. A slot whose
/// replacement is still starting (or has already failed) must not count
/// toward export capacity or the scheduler's interactive-decode reservation.
fn ready_worker_count(inner: &Inner) -> usize {
    inner
        .workers
        .iter()
        .filter(|slot| slot.ready && !slot.unavailable)
        .count()
}

fn export_capacity_for(inner: &Inner) -> usize {
    let ready_workers = ready_worker_count(inner);
    if ready_workers > 1 {
        ready_workers.saturating_sub(1)
    } else {
        ready_workers
    }
}

/// Dispatch as many queued jobs as there are idle, ready workers — called
/// both right after `submit` (in case a slot is already free) and whenever
/// a slot frees up (a result lands, or a worker's readiness handshake
/// arrives).
fn pump(inner: &Rc<RefCell<Inner>>) {
    loop {
        let mut inner_mut = inner.borrow_mut();
        let has_backlog =
            !inner_mut.decode_backlog.is_empty() || !inner_mut.export_backlog.is_empty();
        let no_viable_workers =
            inner_mut.workers.is_empty() || inner_mut.workers.iter().all(|slot| slot.unavailable);
        if has_backlog && no_viable_workers {
            drop(inner_mut);
            fail_queued_jobs(
                inner,
                "no worker capacity remains after replacement failures",
            );
            return;
        }
        let Some(slot_idx) = inner_mut
            .workers
            .iter()
            .position(|s| s.ready && !s.busy && !s.unavailable)
        else {
            return;
        };
        let job = if let Some(job) = inner_mut.decode_backlog.pop_front() {
            job
        } else {
            // Keep one worker available for interactive decode work whenever
            // the pool has more than one worker. A single-worker pool still
            // makes progress on exports.
            let export_limit = export_capacity_for(&inner_mut);
            let active_exports = inner_mut
                .workers
                .iter()
                .filter(|s| s.busy && s.active_job.is_some())
                .filter(|s| {
                    inner_mut
                        .pending
                        .get(&s.active_job.unwrap())
                        .is_some_and(|m| m.kind == JobKind::Export)
                })
                .count();
            if active_exports >= export_limit {
                return;
            }
            let Some(job) = inner_mut.export_backlog.pop_front() else {
                return;
            };
            job
        };
        inner_mut.workers[slot_idx].busy = true;
        inner_mut.workers[slot_idx].active_job = Some(job.id);
        let worker = inner_mut.workers[slot_idx].worker.clone();
        drop(inner_mut);

        let msg = Object::new();
        let _ = Reflect::set(
            &msg,
            &JsValue::from_str("id"),
            &JsValue::from_f64(job.id as f64),
        );
        let _ = Reflect::set(
            &msg,
            &JsValue::from_str("maxPx"),
            &JsValue::from_f64(job.max_px as f64),
        );
        let _ = Reflect::set(
            &msg,
            &JsValue::from_str("isRaw"),
            &JsValue::from_bool(job.is_raw),
        );
        let _ = Reflect::set(
            &msg,
            &JsValue::from_str("quality"),
            &JsValue::from_bool(job.quality),
        );
        let _ = Reflect::set(
            &msg,
            &JsValue::from_str("encodeJpeg"),
            &JsValue::from_bool(job.encode_jpeg),
        );
        if let Some((adj_json, touchups_json, rot)) = &job.export {
            let _ = Reflect::set(&msg, &JsValue::from_str("export"), &JsValue::TRUE);
            let _ = Reflect::set(
                &msg,
                &JsValue::from_str("adjustments"),
                &JsValue::from_str(adj_json),
            );
            let _ = Reflect::set(
                &msg,
                &JsValue::from_str("touchups"),
                &JsValue::from_str(touchups_json),
            );
            let _ = Reflect::set(
                &msg,
                &JsValue::from_str("rot"),
                &JsValue::from_f64(*rot as f64),
            );
        }
        // No `Uint8Array::from(...)` copy here — `job.bytes` is already the
        // JS ArrayBuffer read straight off the file (see
        // `WorkerPoolHandle::submit`'s doc comment); it goes into the
        // transfer list as-is, so this dispatch never touches the main
        // thread's own wasm memory at all.
        let _ = Reflect::set(&msg, &JsValue::from_str("bytes"), &job.bytes);
        let transfer = Array::new();
        transfer.push(&job.bytes);
        if let Err(e) = worker.post_message_with_transfer(&msg, &transfer.into()) {
            handle_worker_failure(inner, slot_idx, format!("post_message failed: {e:?}"));
            return;
        }
    }
}

/// Directory the main app's own JS/wasm was loaded from — origin alone
/// isn't enough, since a deploy can nest the trunk output under a subpath
/// (e.g. lightphotos.app serves it from `/app/`, not site root; trunk's own
/// dev server serves it from `/`). Read off the `<link rel="modulepreload">`
/// href trunk always emits alongside the main bundle (see index.html /
/// dist/index.html and the site's public/app.html), since that's the one
/// place the actual deployed path is known at runtime — falls back to bare
/// origin if that tag is missing for some reason.
fn asset_base_url() -> String {
    let window = match web_sys::window() {
        Some(w) => w,
        None => return String::new(),
    };
    let origin = window.location().origin().unwrap_or_default();
    let dir = window
        .document()
        .and_then(|d| d.query_selector("link[rel=modulepreload]").ok().flatten())
        .and_then(|el| el.get_attribute("href"))
        .and_then(|href| href.rfind('/').map(|i| href[..i].to_string()));
    match dir {
        Some(dir) => format!("{origin}{dir}"),
        None => origin,
    }
}

/// Build one `Worker`, its script loaded via the same Blob+`importScripts`
/// trick trunk's own webworker example uses — `wasm_worker`'s output
/// filenames are stable (not content-hashed, unlike the main app's own
/// trunk output), per `data-type="worker"`'s documented behavior, so this
/// URL needs no build-hash knowledge beyond the base directory (see
/// `asset_base_url`).
fn spawn_worker(base: &str) -> Result<Worker, String> {
    let script = Array::new();
    script.push(
        &format!(
            r#"importScripts("{base}/wasm_worker.js");wasm_bindgen("{base}/wasm_worker_bg.wasm");"#
        )
        .into(),
    );
    let opts = BlobPropertyBag::new();
    opts.set_type("text/javascript");
    let blob = Blob::new_with_str_sequence_and_options(&script, &opts)
        .map_err(|e| format!("blob creation failed: {e:?}"))?;
    let url =
        Url::create_object_url_with_blob(&blob).map_err(|e| format!("object URL failed: {e:?}"))?;
    Worker::new(&url).map_err(|e| format!("Worker::new failed: {e:?}"))
}

/// Worker count for `WorkerPool::new` — `navigator.hardwareConcurrency`
/// capped at 4 rather than reused unchanged like native's `cores - 2`
/// formula (`loader.rs`): concurrent large-RAW decodes each carry their own
/// scratch-buffer peak against one shared 4GB wasm32 address space per
/// worker instance, unlike native's per-thread 64-bit space, so unbounded
/// worker fan-out risks that peak multiplying badly on a many-core machine.
pub fn worker_count() -> usize {
    let cores = web_sys::window()
        .map(|w| w.navigator().hardware_concurrency() as usize)
        .unwrap_or(4);
    cores.clamp(1, 4)
}

pub struct WorkerPool {
    inner: Rc<RefCell<Inner>>,
    result_rx: Receiver<PoolResult>,
    export_rx: Receiver<ExportPoolResult>,
}

/// A cheap-clone submit handle — see the module doc comment for why this
/// exists separately from `WorkerPool` itself.
#[derive(Clone)]
pub struct WorkerPoolHandle(Rc<RefCell<Inner>>);

impl WorkerPool {
    /// `worker_count`: the plan's M4 goal explicitly calls for capping this
    /// deliberately on wasm32 rather than reusing native's `cores - 2`
    /// formula unchanged, since concurrent large-RAW decodes share one 4GB
    /// linear-memory budget per worker instance, not native's per-thread
    /// 64-bit address space. Callers should pass an already-capped count
    /// (see `App`'s construction site for the actual cap).
    pub fn new(worker_count: usize) -> Self {
        let (result_tx, result_rx) = mpsc::channel();
        let (export_tx, export_rx) = mpsc::channel();
        let inner = Rc::new(RefCell::new(Inner {
            workers: Vec::new(),
            next_id: 0,
            pending: HashMap::new(),
            decode_backlog: VecDeque::new(),
            export_backlog: VecDeque::new(),
            result_tx,
            export_tx,
        }));

        let base = asset_base_url();

        for _ in 0..worker_count.max(1) {
            match spawn_worker(&base) {
                Ok(worker) => {
                    let slot_idx = {
                        let mut inner_mut = inner.borrow_mut();
                        inner_mut.workers.push(WorkerSlot {
                            worker: worker.clone(),
                            ready: false,
                            busy: false,
                            active_job: None,
                            generation: 0,
                            replacement_attempts: 0,
                            unavailable: false,
                        });
                        inner_mut.workers.len() - 1
                    };
                    attach_worker(&inner, slot_idx, worker, 0);
                }
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web_worker_pool] failed to spawn worker: {e}").into(),
                    );
                }
            }
        }

        WorkerPool {
            inner,
            result_rx,
            export_rx,
        }
    }

    pub fn handle(&self) -> WorkerPoolHandle {
        WorkerPoolHandle(self.inner.clone())
    }

    /// Drain every result that's landed since the last poll — same
    /// one-shot-per-frame convention as every other `poll_*` in this
    /// codebase (`app/web.rs`, `app/catalog.rs`).
    pub fn poll(&self) -> Vec<PoolResult> {
        let mut out = Vec::new();
        while let Ok(r) = self.result_rx.try_recv() {
            out.push(r);
        }
        out
    }

    /// Drain every finished `JobKind::Export` result since the last poll —
    /// the export counterpart of `poll` (JPEG bytes, separate channel).
    pub fn poll_exports(&self) -> Vec<ExportPoolResult> {
        let mut out = Vec::new();
        while let Ok(r) = self.export_rx.try_recv() {
            out.push(r);
        }
        out
    }
}

fn handle_worker_message(
    inner: &Rc<RefCell<Inner>>,
    slot_idx: usize,
    generation: u32,
    msg: MessageEvent,
) {
    let data = msg.data();

    // Readiness handshake: `{ready: true}`, no `id` field.
    if get_bool(&data, "ready") {
        let mut inner_mut = inner.borrow_mut();
        if let Some(slot) = inner_mut.workers.get_mut(slot_idx) {
            if slot.generation == generation {
                slot.ready = true;
                slot.replacement_attempts = 0;
                slot.unavailable = false;
            }
        }
        drop(inner_mut);
        pump(inner);
        return;
    }

    let Some(id) = get_f64(&data, "id") else {
        return;
    };
    let id = id as u32;
    let ok = get_bool(&data, "ok");

    let mut inner_mut = inner.borrow_mut();
    if let Some(slot) = inner_mut.workers.get_mut(slot_idx) {
        if slot.generation != generation {
            return;
        }
        slot.busy = false;
        slot.active_job = None;
    }
    let Some(meta) = inner_mut.pending.remove(&id) else {
        drop(inner_mut);
        pump(inner);
        return;
    };
    let tx = inner_mut.result_tx.clone();
    let export_tx = inner_mut.export_tx.clone();
    drop(inner_mut);

    if meta.kind == JobKind::Export {
        let (folder, dest_dir, filename) = meta
            .export_dest
            .expect("Export pending meta always carries its dest");
        let result = if ok {
            let jpeg_val =
                Reflect::get(&data, &JsValue::from_str("jpeg")).unwrap_or(JsValue::UNDEFINED);
            Ok(Uint8Array::new(&jpeg_val).to_vec())
        } else {
            Err(get_string(&data, "error").unwrap_or_else(|| "unknown worker error".to_string()))
        };
        let _ = export_tx.send(ExportPoolResult {
            path: meta.path,
            folder,
            dest_dir,
            filename,
            result,
        });
        pump(inner);
        return;
    }

    let result = if ok {
        let width = get_f64(&data, "width").unwrap_or(0.0) as u32;
        let height = get_f64(&data, "height").unwrap_or(0.0) as u32;
        let rgba_val =
            Reflect::get(&data, &JsValue::from_str("rgba")).unwrap_or(JsValue::UNDEFINED);
        let rgba = Uint8Array::new(&rgba_val).to_vec();
        let pixel_format = if get_bool(&data, "linear") {
            PixelFormat::LinearF16
        } else {
            PixelFormat::Srgb8
        };
        Ok(DecodedImage {
            width,
            height,
            rgba,
            pixel_format,
        })
    } else {
        Err(get_string(&data, "error").unwrap_or_else(|| "unknown worker error".to_string()))
    };

    let jpeg = Reflect::get(&data, &JsValue::from_str("jpeg"))
        .ok()
        .filter(|v| !v.is_undefined() && !v.is_null())
        .map(|v| Uint8Array::new(&v).to_vec());

    let _ = tx.send(PoolResult {
        kind: meta.kind,
        path: meta.path,
        target: meta.target,
        result,
        jpeg,
        cache_name: meta.cache_name,
        generation: meta.generation,
        from_cache: meta.from_cache,
    });
    pump(inner);
}

fn handle_worker_error(
    inner: &Rc<RefCell<Inner>>,
    slot_idx: usize,
    generation: u32,
    event: ErrorEvent,
) {
    let error = if event.message().is_empty() {
        "worker crashed or trapped".to_string()
    } else {
        event.message()
    };

    let (old_worker, active_job) = {
        let mut inner_mut = inner.borrow_mut();
        let Some(slot) = inner_mut.workers.get_mut(slot_idx) else {
            return;
        };
        if slot.generation != generation {
            return;
        }
        let old_worker = slot.worker.clone();
        let active_job = slot.active_job.take();
        slot.ready = false;
        slot.busy = false;
        (old_worker, active_job)
    };
    old_worker.terminate();

    if let Some(id) = active_job {
        fail_pending_job(inner, id, error.clone());
    }

    replace_worker(inner, slot_idx);
    pump(inner);
}

fn handle_worker_failure(inner: &Rc<RefCell<Inner>>, slot_idx: usize, error: String) {
    let generation = inner.borrow().workers.get(slot_idx).map(|s| s.generation);
    if let Some(generation) = generation {
        handle_worker_error_with_generation(inner, slot_idx, generation, error);
    }
}

fn handle_worker_error_with_generation(
    inner: &Rc<RefCell<Inner>>,
    slot_idx: usize,
    generation: u32,
    error: String,
) {
    let (old_worker, active_job) = {
        let mut inner_mut = inner.borrow_mut();
        let Some(slot) = inner_mut.workers.get_mut(slot_idx) else {
            return;
        };
        if slot.generation != generation {
            return;
        }
        let active_job = slot.active_job.take();
        slot.ready = false;
        slot.busy = false;
        (slot.worker.clone(), active_job)
    };
    old_worker.terminate();
    if let Some(id) = active_job {
        fail_pending_job(inner, id, error);
    }
    replace_worker(inner, slot_idx);
    pump(inner);
}

fn fail_pending_job(inner: &Rc<RefCell<Inner>>, id: u32, error: String) {
    let (meta, tx, export_tx) = {
        let mut inner_mut = inner.borrow_mut();
        (
            inner_mut.pending.remove(&id),
            inner_mut.result_tx.clone(),
            inner_mut.export_tx.clone(),
        )
    };
    let Some(meta) = meta else { return };
    if meta.kind == JobKind::Export {
        if let Some((folder, dest_dir, filename)) = meta.export_dest {
            let _ = export_tx.send(ExportPoolResult {
                path: meta.path,
                folder,
                dest_dir,
                filename,
                result: Err(error),
            });
        }
    } else {
        let _ = tx.send(PoolResult {
            kind: meta.kind,
            path: meta.path,
            target: meta.target,
            result: Err(error),
            jpeg: None,
            cache_name: meta.cache_name,
            generation: meta.generation,
            from_cache: meta.from_cache,
        });
    }
}

fn fail_queued_jobs(inner: &Rc<RefCell<Inner>>, error: &str) {
    loop {
        let id = {
            let mut inner_mut = inner.borrow_mut();
            inner_mut
                .decode_backlog
                .pop_front()
                .or_else(|| inner_mut.export_backlog.pop_front())
                .map(|job| job.id)
        };
        let Some(id) = id else { return };
        fail_pending_job(inner, id, error.to_string());
    }
}

fn replace_worker(inner: &Rc<RefCell<Inner>>, slot_idx: usize) {
    for _ in 0..MAX_REPLACEMENT_ATTEMPTS {
        if inner
            .borrow()
            .workers
            .get(slot_idx)
            .is_none_or(|slot| slot.replacement_attempts >= MAX_REPLACEMENT_ATTEMPTS)
        {
            break;
        }
        let attempt = {
            let mut inner_mut = inner.borrow_mut();
            let Some(slot) = inner_mut.workers.get_mut(slot_idx) else {
                return;
            };
            slot.replacement_attempts = slot.replacement_attempts.saturating_add(1);
            slot.replacement_attempts
        };
        match spawn_worker(&asset_base_url()) {
            Ok(worker) => {
                let generation = {
                    let mut inner_mut = inner.borrow_mut();
                    let slot = &mut inner_mut.workers[slot_idx];
                    slot.worker = worker.clone();
                    slot.ready = false;
                    slot.busy = false;
                    slot.active_job = None;
                    slot.generation = slot.generation.wrapping_add(1);
                    slot.unavailable = false;
                    slot.generation
                };
                attach_worker(inner, slot_idx, worker, generation);
                return;
            }
            Err(e) => web_sys::console::error_1(
                &format!("[web_worker_pool] replacement attempt {attempt} failed: {e:?}").into(),
            ),
        }
    }
    let exhausted = {
        let mut inner_mut = inner.borrow_mut();
        if let Some(slot) = inner_mut.workers.get_mut(slot_idx) {
            slot.unavailable = true;
        }
        inner_mut.workers.iter().all(|slot| slot.unavailable)
    };
    if exhausted {
        fail_queued_jobs(
            inner,
            "no worker capacity remains after replacement failures",
        );
    }
}

fn attach_worker(inner: &Rc<RefCell<Inner>>, slot_idx: usize, worker: Worker, generation: u32) {
    let inner_for_message = inner.clone();
    let onmessage = Closure::wrap(Box::new(move |msg: MessageEvent| {
        handle_worker_message(&inner_for_message, slot_idx, generation, msg);
    }) as Box<dyn Fn(MessageEvent)>);
    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
    onmessage.forget();

    let inner_for_error = inner.clone();
    let onerror = Closure::wrap(Box::new(move |event: ErrorEvent| {
        handle_worker_error(&inner_for_error, slot_idx, generation, event);
    }) as Box<dyn FnMut(ErrorEvent)>);
    worker.set_onerror(Some(onerror.as_ref().unchecked_ref()));
    onerror.forget();

    let inner_for_message_error = inner.clone();
    let onmessageerror = Closure::wrap(Box::new(move |_event: MessageEvent| {
        handle_worker_error_with_generation(
            &inner_for_message_error,
            slot_idx,
            generation,
            "worker messageerror".to_string(),
        );
    }) as Box<dyn FnMut(MessageEvent)>);
    worker.set_onmessageerror(Some(onmessageerror.as_ref().unchecked_ref()));
    onmessageerror.forget();

    let inner_for_timeout = inner.clone();
    let timeout = Closure::wrap(Box::new(move || {
        if inner_for_timeout
            .borrow()
            .workers
            .get(slot_idx)
            .is_some_and(|s| s.generation == generation && !s.ready)
        {
            handle_worker_error_with_generation(
                &inner_for_timeout,
                slot_idx,
                generation,
                "worker readiness timed out".to_string(),
            );
        }
    }) as Box<dyn FnMut()>);
    if let Some(window) = web_sys::window() {
        let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
            timeout.as_ref().unchecked_ref(),
            WORKER_READY_TIMEOUT_MS,
        );
    }
    timeout.forget();
}

impl WorkerPoolHandle {
    /// Current export capacity, based only on ready workers. This is read
    /// through the cloneable handle because the web export producer runs in a
    /// `'static` task rather than on `WorkerPool` itself.
    pub fn export_capacity(&self) -> usize {
        export_capacity_for(&self.0.borrow())
    }

    pub fn export_in_flight(&self) -> usize {
        self.0
            .borrow()
            .pending
            .values()
            .filter(|meta| meta.kind == JobKind::Export)
            .count()
    }

    /// Submit a decode job. `bytes` should be the file's raw contents read
    /// via `web_fs::read_array_buffer` (NOT `read_bytes`) — deliberately a
    /// JS `ArrayBuffer`, not a Rust `Vec<u8>`: it goes straight into a
    /// `postMessage` transfer list (see `pump`) with no copy into the main
    /// thread's own wasm memory, which matters for a RAW file's tens of MB.
    /// `is_raw` should be `image_decode::is_raw_extension(path)`, decided by
    /// the caller since the job carries no `Path`, only the bytes.
    ///
    /// No `quality` parameter: it's derived internally from `kind`
    /// (`Preview`/`Full` → full PPG demosaic + linear output, `Thumb`/
    /// `Speed` → the quarter-res Fast tier), so `app/web.rs`'s call sites —
    /// which already know exactly this via the `kind` they pass — need no
    /// changes.
    pub fn submit(
        &self,
        path: PathBuf,
        target: u32,
        bytes: js_sys::ArrayBuffer,
        is_raw: bool,
        kind: JobKind,
    ) {
        self.submit_inner(path, target, bytes, is_raw, kind, false, None, None, false);
    }

    /// A `JobKind::Thumb` job that also says where its bytes came from.
    ///
    /// `from_cache` is true when `bytes` is an entry already read back from
    /// `.lightphotos/` — decode it and stop. False means the source file was
    /// read because no entry existed, so the worker encodes a JPEG alongside
    /// the decode and returns it on `PoolResult::jpeg` for `app/web.rs` to
    /// write. Encoding there rather than here keeps it off the main thread,
    /// which on wasm is also the thread drawing the grid.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_thumb(
        &self,
        path: PathBuf,
        target: u32,
        bytes: js_sys::ArrayBuffer,
        is_raw: bool,
        cache_name: Option<String>,
        generation: u64,
        from_cache: bool,
    ) {
        self.submit_inner(
            path,
            target,
            bytes,
            is_raw,
            JobKind::Thumb,
            !from_cache,
            cache_name,
            Some(generation),
            from_cache,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn submit_inner(
        &self,
        path: PathBuf,
        target: u32,
        bytes: js_sys::ArrayBuffer,
        is_raw: bool,
        kind: JobKind,
        encode_jpeg: bool,
        cache_name: Option<String>,
        generation: Option<u64>,
        from_cache: bool,
    ) {
        let quality = matches!(kind, JobKind::Preview | JobKind::Full);
        let id = {
            let mut inner_mut = self.0.borrow_mut();
            let id = inner_mut.next_id;
            inner_mut.next_id += 1;
            inner_mut.pending.insert(
                id,
                PendingMeta {
                    kind,
                    cache_name,
                    generation,
                    from_cache,
                    path,
                    target,
                    export_dest: None,
                },
            );
            inner_mut.decode_backlog.push_back(QueuedJob {
                id,
                bytes,
                max_px: target,
                is_raw,
                quality,
                encode_jpeg,
                export: None,
            });
            id
        };
        let _ = id;
        pump(&self.0);
    }

    /// Submit a `JobKind::Export` job: full-res decode + `bake_jpeg` in the
    /// worker, JPEG bytes back on `poll_exports`. `bytes` is the source
    /// file's `ArrayBuffer` (read via `web_fs::read_array_buffer`, same
    /// no-copy transfer as `submit`); `adj_json`/`touchups_json` are the
    /// serde_json-encoded develop/touch-up state; `dest_dir`/`filename` are
    /// the already-resolved output location, carried straight back onto the
    /// `ExportPoolResult`.
    #[allow(clippy::too_many_arguments)]
    pub fn submit_export(
        &self,
        path: PathBuf,
        folder: FileSystemDirectoryHandle,
        dest_dir: PathBuf,
        filename: String,
        bytes: js_sys::ArrayBuffer,
        is_raw: bool,
        adj_json: String,
        touchups_json: String,
        rot: u8,
    ) {
        {
            let mut inner_mut = self.0.borrow_mut();
            let id = inner_mut.next_id;
            inner_mut.next_id += 1;
            inner_mut.pending.insert(
                id,
                PendingMeta {
                    kind: JobKind::Export,
                    cache_name: None,
                    generation: None,
                    from_cache: false,
                    path,
                    target: 0,
                    export_dest: Some((folder, dest_dir, filename)),
                },
            );
            inner_mut.export_backlog.push_back(QueuedJob {
                id,
                bytes,
                max_px: u32::MAX,
                is_raw,
                quality: true,
                encode_jpeg: false,
                export: Some((adj_json, touchups_json, rot)),
            });
        }
        pump(&self.0);
    }

    /// Report an export failure that happened before the job could be
    /// submitted (e.g. the source read failed) — straight to the export
    /// channel, mirroring `fail`.
    pub fn fail_export(
        &self,
        path: PathBuf,
        folder: FileSystemDirectoryHandle,
        dest_dir: PathBuf,
        filename: String,
        error: String,
    ) {
        let tx = self.0.borrow().export_tx.clone();
        let _ = tx.send(ExportPoolResult {
            path,
            folder,
            dest_dir,
            filename,
            result: Err(error),
        });
    }

    /// Report a failure that happened before a job could even be submitted
    /// (e.g. `web_fs::read_bytes` itself failed) — bypasses the worker
    /// entirely and pushes straight to the result channel, so callers only
    /// need one failure path (`poll`'s `Err` arm) instead of two.
    pub fn fail(
        &self,
        path: PathBuf,
        target: u32,
        kind: JobKind,
        generation: Option<u64>,
        error: String,
    ) {
        let tx = self.0.borrow().result_tx.clone();
        let _ = tx.send(PoolResult {
            kind,
            path,
            target,
            result: Err(error),
            jpeg: None,
            cache_name: None,
            generation,
            from_cache: false,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_cached_jpeg_recovers_from_source() {
        let mut result = PoolResult {
            kind: JobKind::Thumb,
            path: PathBuf::from("photo.jpg"),
            target: 32,
            result: crate::image_decode::decode_nonraw_from_bytes(b"corrupt JPEG", 32),
            jpeg: None,
            cache_name: Some("photo.jpg.0123456789abcdef.thumb.jpg".into()),
            generation: Some(0),
            from_cache: true,
        };
        assert!(result.needs_source_decode());

        let pixels = image::RgbImage::from_pixel(8, 8, image::Rgb([120, 80, 40]));
        let mut source = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut source)
            .encode_image(&pixels)
            .unwrap();
        result.result = crate::image_decode::decode_nonraw_from_bytes(&source, 32);
        result.from_cache = false;
        assert!(result.result.is_ok());
        assert!(!result.needs_source_decode());

        result.result = Err("source failed".into());
        assert!(
            !result.needs_source_decode(),
            "source failures use normal retries"
        );
    }
}
