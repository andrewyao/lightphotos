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

/// Subfolder under the current folder that exported JPEGs are written to, on
/// every platform. `app/export.rs` joins it natively; `web_export_fs` creates
/// it under the picked folder's directory handle.
pub(crate) const EXPORTS_DIR: &str = "Exports";

/// Decode `src_bytes` at full resolution, bake in the develop/crop/rotation
/// edits, and encode to JPEG bytes. The platform-neutral heart of Pipeline 3,
/// shared verbatim by native non-mac export (`do_export` below) and the
/// wasm32 export worker (`wasm_worker.rs`) — the only thing that differs
/// between those is how the source bytes arrive and where the JPEG goes
/// (`ExportFs`). macOS export keeps its own ImageIO decode/encode path.
///
/// RAW goes through `decode_raw_nonmac_from_bytes` (rawler `RawDevelop`: PPG
/// demosaic + the auto-denoise/boost pipeline, premul sRGB8 out — byte-for-
/// byte what native's own `&Path` decode produces); everything else through
/// `decode_nonraw_from_bytes`. Both yield the premul-sRGB8 shape
/// `image_ops::bake_edited` expects.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// Used by `do_export_nonmac`; on a mac+raw-probe build only the round-trip
// test calls it.
#[allow(dead_code)]
pub fn bake_jpeg(
    src_bytes: &[u8],
    is_raw: bool,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> Result<Vec<u8>, String> {
    let img = if is_raw {
        image_decode::decode_raw_nonmac_from_bytes(src_bytes, u32::MAX)?
    } else {
        image_decode::decode_nonraw_from_bytes(src_bytes, u32::MAX)?
    };
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, adj, touchups, rot);
    image_encode::encode_jpeg_to_vec(w, h, &rgba)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// `bake_jpeg` wires decode → `bake_edited` → `encode_jpeg_to_vec` into
    /// one platform-neutral call. With default adjustments and no rotation a
    /// solid-colour source should survive the round trip at its original
    /// dimensions and roughly its original colour.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn bake_jpeg_nonraw_round_trips_identity_edit() {
        let (w, h) = (10u32, 8u32);
        let src = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 40, 40, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode source png");

        let jpeg = bake_jpeg(&png, false, &Adjustments::default(), &[], 0)
            .expect("bake_jpeg should succeed");

        let out = image::load_from_memory(&jpeg)
            .expect("decode baked jpeg")
            .into_rgba8();
        assert_eq!(out.dimensions(), (w, h));
        let px = out.get_pixel(0, 0).0;
        assert!(px[0] > 140, "red channel should stay high, got {}", px[0]);
        assert!(px[1] < 100 && px[2] < 100, "g/b should stay low, got {},{}", px[1], px[2]);
    }

    /// A 90° rotation swaps the baked output's width and height.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn bake_jpeg_applies_rotation() {
        let (w, h) = (12u32, 6u32);
        let src = image::RgbaImage::from_pixel(w, h, image::Rgba([120, 120, 120, 255]));
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(src)
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .expect("encode source png");

        let jpeg = bake_jpeg(&png, false, &Adjustments::default(), &[], 1)
            .expect("bake_jpeg should succeed");
        let out = image::load_from_memory(&jpeg).expect("decode").into_rgba8();
        assert_eq!(out.dimensions(), (h, w), "90 deg rotation swaps dimensions");
    }
}

/// The filesystem seam between native non-mac export and wasm32 export — the
/// only thing that differs once `bake_jpeg` produces the JPEG bytes. Native
/// (`NativeFs`) reads with `std::fs` and does a tmp-write + atomic rename;
/// wasm32 (`web::web_export_fs::WebFs`) reads a `FileSystemFileHandle` and
/// writes a File System Access writable stream (atomic swap on `close`).
///
/// Generic, never `dyn` — `NativeFs`'s method bodies contain no `.await`, so
/// the worker threads `pollster::block_on` the already-ready futures for
/// free, and `WebFs` runs on `wasm_bindgen_futures::spawn_local`.
#[cfg(not(target_os = "macos"))]
#[allow(async_fn_in_trait)] // crate-internal; no Send bound needed
pub(crate) trait ExportFs {
    /// Full source-file bytes for `src` (an absolute path natively, a
    /// picked-folder-relative key on wasm32).
    async fn read_source(&self, src: &std::path::Path) -> Result<Vec<u8>, String>;
    /// Write `bytes` to `dest` atomically (no reader ever sees a partial
    /// file). `dest` already has its collision-free `Exports/<stem>.jpg`
    /// name resolved.
    async fn write_atomic(&self, dest: &std::path::Path, bytes: &[u8]) -> Result<(), String>;
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct NativeFs;

#[cfg(not(target_os = "macos"))]
impl ExportFs for NativeFs {
    async fn read_source(&self, src: &std::path::Path) -> Result<Vec<u8>, String> {
        std::fs::read(src).map_err(|e| e.to_string())
    }

    async fn write_atomic(&self, dest: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
        // Temp sibling then rename, so a crash mid-write can't leave a
        // truncated `.jpg` at the final path (the rename is atomic on one
        // volume).
        let tmp = dest.with_extension("jpg.tmp");
        std::fs::write(&tmp, bytes).map_err(|e| format!("write: {e}"))?;
        if let Err(e) = std::fs::rename(&tmp, dest) {
            let _ = std::fs::remove_file(&tmp);
            return Err(format!("rename: {e}"));
        }
        Ok(())
    }
}

/// Decode `src` at full resolution, bake in its edits, and write the JPEG to
/// `dest`. The heavy, thread-safe half of the old `App::export_image`.
fn do_export(job: ExportJob) -> Result<PathBuf, String> {
    #[cfg(target_os = "macos")]
    {
        do_export_macos(job)
    }
    #[cfg(not(target_os = "macos"))]
    {
        do_export_nonmac(job)
    }
}

/// macOS: ImageIO decode (RAW + non-RAW alike) and ImageIO JPEG encode,
/// straight to a temp file then atomic rename. Unchanged from the original
/// `do_export` — the shared `bake_jpeg`/`ExportFs` path is non-mac only.
#[cfg(target_os = "macos")]
fn do_export_macos(job: ExportJob) -> Result<PathBuf, String> {
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

/// Native Linux/Windows: the exact pipeline wasm32 export runs — `bake_jpeg`
/// (shared decode → bake → encode) plus `NativeFs` for the byte IO.
#[cfg(not(target_os = "macos"))]
fn do_export_nonmac(job: ExportJob) -> Result<PathBuf, String> {
    let fs = NativeFs;
    let bytes = pollster::block_on(fs.read_source(&job.src))?;
    let is_raw = image_decode::is_raw_extension(&job.src);
    let jpeg = bake_jpeg(&bytes, is_raw, &job.adj, &job.touchups, job.rot)?;
    pollster::block_on(fs.write_atomic(&job.dest, &jpeg))?;
    Ok(job.dest)
}
