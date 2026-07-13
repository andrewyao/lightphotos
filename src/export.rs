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

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::develop::Adjustments;
use crate::{image_decode, image_encode};

/// A self-contained unit of export work. `dest` is the exact, already
/// collision-resolved output path (see `paths::jpg_export_target`), so the
/// worker just writes there — no filename races between workers.
pub struct ExportJob {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub adj: Adjustments,
    pub rot: u8,
}

/// A finished export, carrying its source path back so the UI can track
/// progress and report per-file failures.
pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<PathBuf, String>,
}

pub struct Exporter {
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

        let cores = thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        let workers = cores.saturating_sub(2).max(1);

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            thread::Builder::new()
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
                })
                .expect("spawn export worker");
        }

        Self { job_tx, res_rx }
    }

    /// Queue a photo for export. Ignored if the workers are gone (shutdown).
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
    let (w, h, rgba) = crate::app::bake_edited(&img, &job.adj, job.rot);
    image_encode::encode_jpeg(&job.dest, w, h, &rgba)?;
    Ok(job.dest)
}
