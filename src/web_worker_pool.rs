//! wasm32-only: a hand-rolled `web_sys::Worker` pool — the wasm port plan's
//! M4 (real threading). Each worker runs an independent instance of the
//! `wasm_worker` binary (`src/bin/wasm_worker.rs`), decoding on its own
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

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};

use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use web_sys::{Blob, BlobPropertyBag, MessageEvent, Url, Worker};

use crate::image_decode::DecodedImage;

/// Which cache tier a finished decode belongs in — mirrors
/// `insert_thumb_external` vs. `insert_preview_external` in `loader.rs`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobKind {
    Thumb,
    Preview,
}

pub struct PoolResult {
    pub kind: JobKind,
    pub path: PathBuf,
    pub target: u32,
    pub result: Result<DecodedImage, String>,
}

struct PendingMeta {
    kind: JobKind,
    path: PathBuf,
    target: u32,
}

struct QueuedJob {
    id: u32,
    bytes: Vec<u8>,
    max_px: u32,
    is_raw: bool,
}

struct WorkerSlot {
    worker: Worker,
    /// Set once the worker's own readiness handshake message arrives — a
    /// job posted before that lands in the void (see `wasm_worker.rs`'s
    /// `run` doc comment on why the worker script itself doesn't listen
    /// until it yields to the JS event loop once).
    ready: bool,
    busy: bool,
}

struct Inner {
    workers: Vec<WorkerSlot>,
    next_id: u32,
    pending: HashMap<u32, PendingMeta>,
    /// Jobs submitted before an idle ready worker was available. Drained
    /// whenever a slot frees up (a result lands) or a new slot becomes
    /// ready.
    backlog: VecDeque<QueuedJob>,
    result_tx: Sender<PoolResult>,
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

/// Dispatch as many queued jobs as there are idle, ready workers — called
/// both right after `submit` (in case a slot is already free) and whenever
/// a slot frees up (a result lands, or a worker's readiness handshake
/// arrives).
fn pump(inner: &Rc<RefCell<Inner>>) {
    loop {
        let mut inner_mut = inner.borrow_mut();
        let Some(slot_idx) = inner_mut
            .workers
            .iter()
            .position(|s| s.ready && !s.busy)
        else {
            return;
        };
        let Some(job) = inner_mut.backlog.pop_front() else {
            return;
        };
        inner_mut.workers[slot_idx].busy = true;
        let worker = inner_mut.workers[slot_idx].worker.clone();
        drop(inner_mut);

        let msg = Object::new();
        let _ = Reflect::set(&msg, &JsValue::from_str("id"), &JsValue::from_f64(job.id as f64));
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
        let bytes = Uint8Array::from(job.bytes.as_slice());
        let _ = Reflect::set(&msg, &JsValue::from_str("bytes"), &bytes.buffer());
        let transfer = Array::new();
        transfer.push(&bytes.buffer());
        if let Err(e) = worker.post_message_with_transfer(&msg, &transfer.into()) {
            web_sys::console::error_1(&format!("[web_worker_pool] post_message failed: {e:?}").into());
        }
    }
}

/// Build one `Worker`, its script loaded via the same Blob+`importScripts`
/// trick trunk's own webworker example uses — `wasm_worker`'s output
/// filenames are stable (not content-hashed, unlike the main app's own
/// trunk output), per `data-type="worker"`'s documented behavior, so this
/// URL needs no build-hash knowledge.
fn spawn_worker(origin: &str) -> Result<Worker, String> {
    let script = Array::new();
    script.push(
        &format!(
            r#"importScripts("{origin}/wasm_worker.js");wasm_bindgen("{origin}/wasm_worker_bg.wasm");"#
        )
        .into(),
    );
    let mut opts = BlobPropertyBag::new();
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
        let inner = Rc::new(RefCell::new(Inner {
            workers: Vec::new(),
            next_id: 0,
            pending: HashMap::new(),
            backlog: VecDeque::new(),
            result_tx,
        }));

        let origin = web_sys::window()
            .and_then(|w| w.location().origin().ok())
            .unwrap_or_default();

        for _ in 0..worker_count.max(1) {
            match spawn_worker(&origin) {
                Ok(worker) => {
                    let slot_idx = {
                        let mut inner_mut = inner.borrow_mut();
                        inner_mut.workers.push(WorkerSlot {
                            worker: worker.clone(),
                            ready: false,
                            busy: false,
                        });
                        inner_mut.workers.len() - 1
                    };
                    let inner_for_closure = inner.clone();
                    let onmessage = Closure::wrap(Box::new(move |msg: MessageEvent| {
                        handle_worker_message(&inner_for_closure, slot_idx, msg);
                    }) as Box<dyn Fn(MessageEvent)>);
                    worker.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
                    onmessage.forget();
                }
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web_worker_pool] failed to spawn worker: {e}").into(),
                    );
                }
            }
        }

        WorkerPool { inner, result_rx }
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
}

fn handle_worker_message(inner: &Rc<RefCell<Inner>>, slot_idx: usize, msg: MessageEvent) {
    let data = msg.data();

    // Readiness handshake: `{ready: true}`, no `id` field.
    if get_bool(&data, "ready") {
        let mut inner_mut = inner.borrow_mut();
        if let Some(slot) = inner_mut.workers.get_mut(slot_idx) {
            slot.ready = true;
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
        slot.busy = false;
    }
    let Some(meta) = inner_mut.pending.remove(&id) else {
        drop(inner_mut);
        pump(inner);
        return;
    };
    let tx = inner_mut.result_tx.clone();
    drop(inner_mut);

    let result = if ok {
        let width = get_f64(&data, "width").unwrap_or(0.0) as u32;
        let height = get_f64(&data, "height").unwrap_or(0.0) as u32;
        let rgba_val = Reflect::get(&data, &JsValue::from_str("rgba")).unwrap_or(JsValue::UNDEFINED);
        let rgba = Uint8Array::new(&rgba_val).to_vec();
        Ok(DecodedImage { width, height, rgba })
    } else {
        Err(get_string(&data, "error").unwrap_or_else(|| "unknown worker error".to_string()))
    };

    let _ = tx.send(PoolResult {
        kind: meta.kind,
        path: meta.path,
        target: meta.target,
        result,
    });
    pump(inner);
}

impl WorkerPoolHandle {
    /// Submit a decode job. `bytes` should already be the file's raw bytes
    /// (read on the main thread via `web_fs::read_bytes` — that part stays
    /// where it was, this only offloads the CPU-heavy decode itself);
    /// `is_raw` should be `image_decode::is_raw_extension(path)`, decided by
    /// the caller since the job carries no `Path`, only the bytes.
    pub fn submit(&self, path: PathBuf, target: u32, bytes: Vec<u8>, is_raw: bool, kind: JobKind) {
        let id = {
            let mut inner_mut = self.0.borrow_mut();
            let id = inner_mut.next_id;
            inner_mut.next_id += 1;
            inner_mut.pending.insert(id, PendingMeta { kind, path, target });
            inner_mut.backlog.push_back(QueuedJob {
                id,
                bytes,
                max_px: target,
                is_raw,
            });
            id
        };
        let _ = id;
        pump(&self.0);
    }

    /// Report a failure that happened before a job could even be submitted
    /// (e.g. `web_fs::read_bytes` itself failed) — bypasses the worker
    /// entirely and pushes straight to the result channel, so callers only
    /// need one failure path (`poll`'s `Err` arm) instead of two.
    pub fn fail(&self, path: PathBuf, target: u32, kind: JobKind, error: String) {
        let tx = self.0.borrow().result_tx.clone();
        let _ = tx.send(PoolResult { kind, path, target, result: Err(error) });
    }
}
