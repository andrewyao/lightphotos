// SPDX-License-Identifier: GPL-3.0-or-later

//! Background JPEG export. Each export decodes at full resolution, bakes in
//! the edits, and encodes a JPEG, which takes hundreds of milliseconds. A
//! worker pool does that off the UI thread, and the frame loop drains results
//! with `poll`. Each `ExportJob` carries everything its worker needs, so
//! workers never touch `App`.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::thread;

use crate::develop::{Adjustments, TouchUp};
use crate::{image_decode, image_encode};

/// Subfolder of the current folder that exports go to, on every platform.
pub(crate) const EXPORTS_DIR: &str = "Exports";

/// Decode `src_bytes` at full resolution, bake in the edits, and encode JPEG
/// bytes. Used by non-mac native export for non-RAW files. On macOS it is
/// built only under `raw-probe`, for tests.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn bake_jpeg(
    src_bytes: &[u8],
    is_raw: bool,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> Result<Vec<u8>, String> {
    bake_jpeg_impl(src_bytes, is_raw, adj, touchups, rot)
}

/// [`bake_jpeg`] for the wasm32 export worker, which already owns the bytes
/// in an `Arc` and passes it to rawler without copying.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn bake_jpeg_from_shared_vec(
    src_bytes: std::sync::Arc<Vec<u8>>,
    is_raw: bool,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
) -> Result<Vec<u8>, String> {
    let img = if is_raw {
        image_decode::decode_raw_nonmac_from_shared_vec(src_bytes.clone(), u32::MAX)?
    } else {
        image_decode::decode_nonraw_from_bytes(src_bytes.as_slice(), u32::MAX)?
    };
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, adj, touchups, rot);
    image_encode::encode_jpeg_to_vec(w, h, &rgba)
}

#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
fn bake_jpeg_impl(
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

/// One photo to export. `dest` is already collision-free (see
/// `paths::jpg_export_target`), so workers never race on file names.
pub struct ExportJob {
    pub src: PathBuf,
    pub dest: PathBuf,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub rot: u8,
}

/// A finished export, keyed by source path so the UI can report failures.
pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<PathBuf, String>,
}

pub struct Exporter {
    // wasm32 exports run on the Web Worker pool instead, so it has no `submit`.
    #[cfg(not(target_arch = "wasm32"))]
    job_tx: Sender<ExportJob>,
    res_rx: Receiver<ExportOutcome>,
}

impl Exporter {
    pub fn new() -> Self {
        Self::with_runner(do_export)
    }

    /// `new`, with the per-photo work swapped out so a test can make it panic.
    fn with_runner(run: fn(ExportJob) -> Result<PathBuf, String>) -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<ExportJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ExportOutcome>();
        // Workers share one receiver, so whichever is idle takes the next job.
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
                .spawn(move || {
                    loop {
                        // Hold the lock only while receiving, not during the export.
                        let job = {
                            let rx = match job_rx.lock() {
                                Ok(rx) => rx,
                                Err(_) => return,
                            };
                            match rx.recv() {
                                Ok(job) => job,
                                // The Exporter was dropped.
                                Err(_) => return,
                            }
                        };

                        let src = job.src.clone();
                        // rawler panics on some malformed files. Callers count
                        // outcomes to know a batch is done, so a panic must
                        // still send one, and the worker stays alive for the
                        // rest of the batch.
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(job)))
                                .unwrap_or_else(|_| {
                                    Err(format!("export panicked: {}", src.display()))
                                });
                        if res_tx.send(ExportOutcome { src, result }).is_err() {
                            break;
                        }
                    }
                });
            // wasm32 cannot spawn threads. Log and continue instead of
            // crashing at startup.
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

    /// Queue a photo for export. Dropped silently after shutdown.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn submit(&self, job: ExportJob) {
        let _ = self.job_tx.send(job);
    }

    /// Drain finished exports without blocking.
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
    /// Callers count outcomes to know a batch is done, so a photo that panics
    /// the exporter must still produce one, and the worker must live on.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_panicking_export_still_reports_and_the_worker_survives() {
        use super::{ExportJob, Exporter};
        use std::path::PathBuf;
        use std::time::{Duration, Instant};

        fn panics_on_bad(job: ExportJob) -> Result<PathBuf, String> {
            if job.src.ends_with("bad.raw") {
                panic!("malformed file");
            }
            Ok(job.dest)
        }
        let job = |name: &str| ExportJob {
            src: PathBuf::from(name),
            dest: PathBuf::from("out.jpg"),
            adj: Default::default(),
            touchups: Vec::new(),
            rot: 0,
        };

        let exporter = Exporter::with_runner(panics_on_bad);
        // More jobs than any machine has workers, so a dead worker would strand one.
        let names: Vec<String> = (0..64)
            .map(|i| {
                if i % 2 == 0 {
                    "bad.raw".into()
                } else {
                    format!("{i}.jpg")
                }
            })
            .collect();
        for n in &names {
            exporter.submit(job(n));
        }

        let mut outcomes = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while outcomes.len() < names.len() && Instant::now() < deadline {
            outcomes.extend(exporter.poll());
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(outcomes.len(), names.len(), "every job reports an outcome");
        let failed = outcomes.iter().filter(|o| o.result.is_err()).count();
        assert_eq!(failed, 32, "each panic is reported as a failure");
    }

    // Every test below is gated the same way, so on macOS without `raw-probe`
    // the module is empty and these imports would be unused.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    use super::*;
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    use std::io::Cursor;

    /// With no edits, a solid-color source keeps its size and roughly its
    /// color.
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
        assert!(
            px[1] < 100 && px[2] < 100,
            "g/b should stay low, got {},{}",
            px[1],
            px[2]
        );
    }

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

/// File access for non-mac export. `NativeFs` uses `std::fs`, and wasm32's
/// `web_export_fs::WebFs` uses the File System Access API. `NativeFs` never
/// awaits, so worker threads can `pollster::block_on` it at no cost.
#[cfg(not(target_os = "macos"))]
#[allow(async_fn_in_trait)] // crate-internal; no Send bound needed
pub(crate) trait ExportFs {
    /// Full source-file bytes for `src` (an absolute path natively, a
    /// picked-folder-relative key on wasm32).
    async fn read_source(&self, src: &std::path::Path) -> Result<Vec<u8>, String>;
    /// Write `bytes` to `dest` so no reader ever sees a partial file.
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
        // Write a temp sibling, then rename. The rename is atomic on one
        // volume, so a crash cannot leave a truncated `.jpg`.
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
/// `dest`.
#[hotpath::measure]
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

#[cfg(target_os = "macos")]
fn do_export_macos(job: ExportJob) -> Result<PathBuf, String> {
    // Full resolution: u32::MAX means `fit_within` never downscales.
    let img = image_decode::decode(&job.src, u32::MAX)?;
    let (w, h, rgba) = crate::image_ops::bake_edited(&img, &job.adj, &job.touchups, job.rot);
    // Temp file then rename, as in `NativeFs::write_atomic`.
    let tmp = job.dest.with_extension("jpg.tmp");
    image_encode::encode_jpeg(&tmp, w, h, &rgba)?;
    if let Err(e) = std::fs::rename(&tmp, &job.dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(job.dest)
}

#[cfg(not(target_os = "macos"))]
fn do_export_nonmac(job: ExportJob) -> Result<PathBuf, String> {
    let fs = NativeFs;
    let is_raw = image_decode::is_raw_extension(&job.src);

    let jpeg = if is_raw {
        // rawler's path-based decoder memory-maps the file, which avoids
        // reading the whole RAW into a Vec first.
        let img = image_decode::decode_raw_nonmac(&job.src, u32::MAX)?;
        let (w, h, rgba) = crate::image_ops::bake_edited(&img, &job.adj, &job.touchups, job.rot);
        image_encode::encode_jpeg_to_vec(w, h, &rgba)?
    } else {
        let bytes = pollster::block_on(fs.read_source(&job.src))?;
        bake_jpeg(&bytes, false, &job.adj, &job.touchups, job.rot)?
    };

    pollster::block_on(fs.write_atomic(&job.dest, &jpeg))?;
    Ok(job.dest)
}
