//! wasm32-only: a pool of `web_sys::Worker`s, each running the `wasm_worker`
//! binary with its own wasm memory. `App` owns the `WorkerPool` and polls it
//! each frame. Async tasks submit jobs through a cloneable
//! [`WorkerPoolHandle`], because a `spawn_local` future cannot borrow `App`.
//! Sharing an `Rc<RefCell<..>>` is safe because wasm32 is single-threaded.

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

use crate::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};

/// What a job is for. `Preview` and `Full` get the full RAW demosaic with
/// linear output. `Thumb` and `Speed` (the Loupe's quick screen-fit first
/// paint) use the fast quarter-res RAW decode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Thumb,
    Speed,
    Preview,
    Full,
    /// Decode, bake edits, and encode a JPEG in the worker. Results arrive
    /// on `poll_exports`, not `poll`.
    Export,
}

pub struct PoolResult {
    pub kind: JobKind,
    pub path: PathBuf,
    pub target: u32,
    pub result: Result<DecodedImage, String>,
    /// The image as a JPEG for the disk cache. Set only for thumbnails decoded
    /// from the source, and only when JPEG can represent the pixels.
    pub jpeg: Option<Vec<u8>>,
    /// The cache entry name, computed when the job was submitted, so storing
    /// the result needs no second `get_file()`.
    pub cache_name: Option<String>,
    /// The navigation generation that requested this job.
    pub generation: Option<u64>,
    /// True when the job decoded bytes from the disk cache, whether or not
    /// it succeeded.
    pub from_cache: bool,
}

impl PoolResult {
    /// A cached thumbnail that failed to decode. The caller re-reads the
    /// source without spending a retry attempt.
    pub fn needs_source_decode(&self) -> bool {
        self.kind == JobKind::Thumb && self.from_cache && self.result.is_err()
    }
}

/// A finished or failed export. The destination rides along with the job,
/// so the main thread can write the bytes without a lookup table.
pub struct ExportPoolResult {
    pub path: PathBuf,
    pub folder: FileSystemDirectoryHandle,
    pub dest_dir: PathBuf,
    pub filename: String,
    pub result: Result<Vec<u8>, String>,
}

struct PendingMeta {
    kind: JobKind,
    cache_name: Option<String>,
    generation: Option<u64>,
    from_cache: bool,
    path: PathBuf,
    target: u32,
    /// `Some` only for `JobKind::Export`.
    export_dest: Option<(FileSystemDirectoryHandle, PathBuf, String)>,
}

struct QueuedJob {
    id: u32,
    /// Kept as a JS `ArrayBuffer` and transferred to the worker, so the bytes
    /// never enter main-thread wasm memory.
    bytes: js_sys::ArrayBuffer,
    max_px: u32,
    is_raw: bool,
    /// Full RAW demosaic instead of the fast decode. Derived from `JobKind`.
    quality: bool,
    /// Ask the worker to also return a JPEG for the disk cache.
    encode_jpeg: bool,
    /// `(adjustments_json, touchups_json, rot)` for an export job. Its
    /// presence switches the worker to the export path.
    export: Option<(String, String, u8)>,
}

struct WorkerSlot {
    worker: Worker,
    /// Set when the worker's ready message arrives. A worker drops jobs
    /// posted before then.
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
    /// Decodes always go before exports, so a bulk export never makes the
    /// grid or Loupe wait.
    decode_backlog: VecDeque<QueuedJob>,
    export_backlog: VecDeque<QueuedJob>,
    result_tx: Sender<PoolResult>,
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

/// Workers that can take a job now. A replacement that is still starting,
/// or has failed, does not count.
fn ready_worker_count(inner: &Inner) -> usize {
    inner
        .workers
        .iter()
        .filter(|slot| slot.ready && !slot.unavailable)
        .count()
}

/// Exports may use every ready worker but one, so a decode can always start.
/// A single-worker pool still runs exports.
fn export_capacity_for(inner: &Inner) -> usize {
    let ready_workers = ready_worker_count(inner);
    if ready_workers > 1 {
        ready_workers.saturating_sub(1)
    } else {
        ready_workers
    }
}

/// Send queued jobs to idle, ready workers. Call after every submit and
/// whenever a worker becomes free or ready.
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
        let _ = Reflect::set(&msg, &JsValue::from_str("bytes"), &job.bytes);
        let transfer = Array::new();
        transfer.push(&job.bytes);
        if let Err(e) = worker.post_message_with_transfer(&msg, &transfer.into()) {
            handle_worker_failure(inner, slot_idx, format!("post_message failed: {e:?}"));
            return;
        }
    }
}

/// Where to load the worker bundle from: the URL directory the app's JS and
/// wasm were served from, plus a `?v=` cache-buster. The site serves the app
/// under `/app/`, so the origin alone is wrong. Both come from the
/// `<link rel="modulepreload">` trunk emits; falls back to the origin.
///
/// The cache-buster is the main bundle's content hash. Trunk does not hash
/// worker filenames, and Cloudflare sends `wasm_worker.js` with a 4-hour
/// max-age but `wasm_worker_bg.wasm` with max-age=0. Without it, a browser
/// that saw the previous deploy pairs its cached glue JS with the new wasm
/// and every decode fails with "wasm.wasm_bindgen_… is not a function".
struct WorkerAssets {
    dir: String,
    version: String,
}

fn worker_assets() -> WorkerAssets {
    let window = match web_sys::window() {
        Some(w) => w,
        None => {
            return WorkerAssets {
                dir: String::new(),
                version: String::new(),
            }
        }
    };
    let origin = window.location().origin().unwrap_or_default();
    let href = window
        .document()
        .and_then(|d| d.query_selector("link[rel=modulepreload]").ok().flatten())
        .and_then(|el| el.get_attribute("href"));
    match href
        .as_deref()
        .and_then(|h| h.rfind('/').map(|i| (&h[..i], &h[i + 1..])))
    {
        Some((dir, file)) => WorkerAssets {
            dir: format!("{origin}{dir}"),
            version: file.to_string(),
        },
        None => WorkerAssets {
            dir: origin,
            version: String::new(),
        },
    }
}

/// Start one worker from a Blob script that `importScripts` the worker
/// bundle, as in trunk's webworker example.
fn spawn_worker(assets: &WorkerAssets) -> Result<Worker, String> {
    let WorkerAssets { dir, version } = assets;
    let script = Array::new();
    script.push(
        &format!(
            r#"importScripts("{dir}/wasm_worker.js?v={version}");wasm_bindgen("{dir}/wasm_worker_bg.wasm?v={version}");"#
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

/// `navigator.hardwareConcurrency`, capped at 4. Each large RAW decode has
/// a big scratch-memory peak, and every worker adds its own wasm heap, so
/// many workers on a many-core machine use too much memory.
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

/// A cloneable handle for submitting jobs from async tasks.
#[derive(Clone)]
pub struct WorkerPoolHandle(Rc<RefCell<Inner>>);

impl WorkerPool {
    /// Pass `worker_count()`. Always starts at least one worker.
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

        let assets = worker_assets();

        for _ in 0..worker_count.max(1) {
            match spawn_worker(&assets) {
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

    /// Drain decode results that arrived since the last call.
    pub fn poll(&self) -> Vec<PoolResult> {
        let mut out = Vec::new();
        while let Ok(r) = self.result_rx.try_recv() {
            out.push(r);
        }
        out
    }

    /// Drain export results that arrived since the last call.
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

    // The ready message is `{ready: true}` with no `id`.
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
        Ok(DecodedImage::new_tracked(DecodedImageFields {
            width,
            height,
            rgba,
            pixel_format,
        }))
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
        match spawn_worker(&worker_assets()) {
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
    /// How many exports may run at once, given the workers ready now.
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

    /// Queue a decode. Read `bytes` with `web_fs::read_array_buffer`, not
    /// `read_bytes`, so the buffer transfers to the worker without a copy.
    /// `is_raw` comes from the caller because the worker sees only bytes.
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

    /// Queue a thumbnail decode. `from_cache` is true when `bytes` came from
    /// the `.lightphotos/` cache. When false, the worker also returns a JPEG
    /// on `PoolResult::jpeg` for the caller to cache.
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

    /// Queue an export. The JPEG arrives on `poll_exports`. `adj_json` and
    /// `touchups_json` are the serde_json develop state. `dest_dir` and
    /// `filename` are returned unchanged on the result.
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

    /// Report an export that failed before submit, such as a failed source
    /// read, through `poll_exports`.
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

    /// Report a decode that failed before submit, such as a failed source
    /// read, through `poll`, so callers handle every failure in one place.
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
