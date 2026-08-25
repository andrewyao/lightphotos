// SPDX-License-Identifier: GPL-3.0-or-later

//! Background JPEG export. Exporting a photo means a full-resolution decode, a
//! per-pixel bake of the develop/crop/rotation edits, and an ImageIO JPEG write
//! — hundreds of milliseconds per image. Running that on the UI thread freezes
//! the window (and leaves the bulk-export confirmation modal stuck on screen),
//! so we hand it to a small worker pool here and drain the results once per
//! frame, mirroring `loader.rs`.
//!
//! The main thread does only cheap work up front: gather each photo's edits and
//! its final (collision-free) destination path, then `submit` a self-contained
//! `ExportJob`. Workers never touch `App` state — everything they need travels
//! in the job — so there is no shared-state coupling.
//!
//! ## Pipeline position
//! This *is* Pipeline 3, start to finish:
//! - `app/export.rs` builds one `ExportJob` per photo and calls `submit`.
//! - A worker here calls `image_decode::decode` (full resolution) ->
//!   `image_ops::bake_edited` (crop/tone/rotate) ->
//!   `image_encode::encode_jpeg`.
//! - The main thread's frame loop drains finished jobs via `poll`.
//! - Native only: `submit` doesn't even exist on wasm32 (see the `#[cfg]`
//!   below) — `app/export.rs`'s `start_export` is a no-op stub there, so
//!   nothing in this file ever runs on the browser build. See
//!   `ARCHITECTURE.md`.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::develop::{Adjustments, TouchUp};
use crate::{image_decode, image_encode};

/// A self-contained unit of export work. `dest` is the exact, already
/// collision-resolved output path (see `paths::jpg_export_target`), so the
/// worker just writes there — no filename races between workers.
pub struct ExportJob {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub rot: u8,
}

/// A finished export, carrying its source path back so the UI can track
/// progress and report per-file failures.
pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<PathBuf, String>,
}

pub struct Exporter {
    // wasm32's `start_export` is a stub that never submits a job (see
    // `app/export.rs`) — the sender still exists (the worker loop below is
    // unconditional) but nothing on this platform ever reads it back out via
    // `submit`, hence the gate here matching `submit`'s own.
    #[cfg(not(target_arch = "wasm32"))]
    job_tx: Sender<ExportJob>,
    res_rx: Receiver<ExportOutcome>,
}

impl Exporter {
    pub fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<ExportJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ExportOutcome>();
        // One receiver fanned out to every worker; whichever is idle grabs the
        // next job.
        let job_rx = Arc::new(Mutex::new(job_rx));

        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = cores.saturating_sub(2).max(1);

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("export-worker-{i}"))
                .spawn(move || loop {
                    // Block until a job is available. Lock only to receive, then
                    // release before the (slow) decode/bake/encode.
                    let job = {
                        let rx = match job_rx.lock() {
                            Ok(rx) => rx,
                            Err(_) => return,
                        };
                        match rx.recv() {
                            Ok(job) => job,
                            // All senders dropped (Exporter gone) → shut down.
                            Err(_) => return,
                        }
                    };

                    let src = job.src.clone();
                    let result = do_export(job);
                    if res_tx.send(ExportOutcome { src, result }).is_err() {
                        break; // UI side gone.
                    }
                });
            // See loader.rs's identical fallback: not every target has real
            // threads yet (e.g. wasm32 pre-Web-Worker-pool) — degrade
            // instead of crashing the app at startup.
            if let Err(e) = spawned {
                eprintln!("[export] could not spawn export worker {i}: {e}");
            }
        }

        #[cfg(target_arch = "wasm32")]
        drop(job_tx);

        Self {
            #[cfg(not(target_arch = "wasm32"))]
            job_tx,
            res_rx,
        }
    }

    /// Queue a photo for export. Ignored if the workers are gone (shutdown).
    #[cfg(not(target_arch = "wasm32"))]
    pub fn submit(&self, job: ExportJob) {
        let _ = self.job_tx.send(job);
    }

    /// Drain all finished exports (non-blocking).
    pub fn poll(&self) -> Vec<ExportOutcome> {
        let mut out = Vec::new();
        while let Ok(o) = self.res_rx.try_recv() {
            out.push(o);
        }
        out
    }
}

/// Decode `src` at full resolution, bake in its edits, and write the JPEG to
/// `dest`. The heavy, thread-safe half of the old `App::export_image`.
fn do_export(job: ExportJob) -> Result<PathBuf, String> {
    // Full resolution: u32::MAX means `fit_within` never downscales.
    let img = image_decode::decode(&job.src, u32::MAX)?;
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, &job.adj, &job.touchups, job.rot);
    // Encode to a temp sibling then rename, so a crash mid-encode can't leave a
    // truncated `.jpg` at the final path (the rename is atomic on one volume).
    let tmp = job.dest.with_extension("jpg.tmp");
    image_encode::encode_jpeg(&tmp, w, h, &rgba)?;
    if let Err(e) = std::fs::rename(&tmp, &job.dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(job.dest)
}
