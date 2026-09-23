// SPDX-License-Identifier: GPL-3.0-or-later

//! Background JPEG export. Each export decodes at full resolution, bakes in
//! the edits, and encodes a JPEG, which takes hundreds of milliseconds. A
//! worker pool does that off the UI thread, and the frame loop drains results
//! with `poll`. Each `ExportJob` carries everything its worker needs, so
//! workers never touch `App`.

use std::path::PathBuf;
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::time::SystemTime;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Sender;
#[cfg(not(target_arch = "wasm32"))]
use std::thread;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::{Arc, Mutex};

use crate::develop::{Adjustments, TouchUp};
use crate::{image_decode, image_encode};

/// Subfolder of the current folder that exports go to, on every platform.
pub(crate) const EXPORTS_DIR: &str = "Exports";

/// What the export form is set to. Remembered across launches in `prefs`.
#[derive(Clone, Debug, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct ExportSettings {
    pub target: ExportTarget,
    pub size: ExportSize,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum ExportTarget {
    Folder(FolderChoice),
    /// The server `App` is connected to. The browser can't reach one: Immich
    /// sends no CORS headers.
    #[cfg(not(target_arch = "wasm32"))]
    Immich,
}

impl Default for ExportTarget {
    fn default() -> Self {
        ExportTarget::Folder(FolderChoice::ExportsSubfolder)
    }
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FolderChoice {
    /// `<current folder>/Exports`, resolved per batch, so each folder's
    /// exports land beside it.
    ExportsSubfolder,
    #[cfg(not(target_arch = "wasm32"))]
    Custom(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum ExportSize {
    #[default]
    Full,
    /// Longest side of the finished (cropped, rotated) image, in pixels.
    LongEdge(u32),
}

impl ExportSize {
    pub const CHOICES: [ExportSize; 4] = [
        ExportSize::Full,
        ExportSize::LongEdge(4096),
        ExportSize::LongEdge(2048),
        ExportSize::LongEdge(1024),
    ];

    pub fn max_px(self) -> u32 {
        match self {
            ExportSize::Full => u32::MAX,
            ExportSize::LongEdge(px) => px,
        }
    }
}

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
    max_px: u32,
) -> Result<Vec<u8>, String> {
    let img = if is_raw {
        image_decode::decode_raw_nonmac_from_bytes(src_bytes, u32::MAX)?
    } else {
        image_decode::decode_nonraw_from_bytes(src_bytes, u32::MAX)?
    };
    let (w, h, rgba) = bake_sized(&img, adj, touchups, rot, max_px);
    image_encode::encode_jpeg_to_vec(w, h, &rgba)
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
    max_px: u32,
) -> Result<Vec<u8>, String> {
    let img = if is_raw {
        image_decode::decode_raw_nonmac_from_shared_vec(src_bytes.clone(), u32::MAX)?
    } else {
        image_decode::decode_nonraw_from_bytes(src_bytes.as_slice(), u32::MAX)?
    };
    let (w, h, rgba) = bake_sized(&img, adj, touchups, rot, max_px);
    image_encode::encode_jpeg_to_vec(w, h, &rgba)
}

/// Bake the edits into a full-resolution decode, then shrink the result to
/// `max_px` on its long side. The limit applies after crop and rotation
/// because that is what "2048 px" means to someone exporting, so the decode
/// itself can't be the smaller one.
fn bake_sized(
    img: &image_decode::DecodedImage,
    adj: &Adjustments,
    touchups: &[TouchUp],
    rot: u8,
    max_px: u32,
) -> (u32, u32, Vec<u8>) {
    let (w, h, rgba) = crate::image_ops::bake_edited(img, adj, touchups, rot);
    crate::image_ops::fit_long_edge(w, h, rgba, max_px)
}

/// Where one photo's JPEG goes. The browser writes its exports from the Web
/// Worker pool instead, so it has no jobs.
#[cfg(not(target_arch = "wasm32"))]
pub enum ExportDest {
    /// Already collision-free (see `paths::jpg_export_target`), so workers
    /// never race on file names.
    Folder(PathBuf),
    Immich {
        server: Arc<crate::immich::ImmichServer>,
        filename: String,
        /// Sent as the asset's date, which Immich falls back on because the
        /// exported JPEG carries no EXIF.
        taken: SystemTime,
        /// 0 leaves the asset unrated.
        stars: u8,
    },
}

/// One photo to export, with everything its worker needs.
#[cfg(not(target_arch = "wasm32"))]
pub struct ExportJob {
    pub src: PathBuf,
    pub dest: ExportDest,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub rot: u8,
    /// Long-edge limit; `u32::MAX` keeps full size.
    pub max_px: u32,
}

/// What an export produced.
#[derive(Debug)]
pub enum ExportLanding {
    File(PathBuf),
    #[cfg(not(target_arch = "wasm32"))]
    Asset {
        id: String,
        /// The server already had these exact bytes and kept its copy.
        duplicate: bool,
    },
}

/// A finished export, keyed by source path so the UI can report failures.
pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<ExportLanding, String>,
}

pub struct Exporter {
    // wasm32 exports run on the Web Worker pool instead, so it has no `submit`.
    #[cfg(not(target_arch = "wasm32"))]
    job_tx: Sender<ExportJob>,
    res_rx: Receiver<ExportOutcome>,
}

impl Exporter {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn new() -> Self {
        Self::with_runner(do_export)
    }

    #[cfg(target_arch = "wasm32")]
    pub fn new() -> Self {
        // No workers here, so nothing ever sends on the result channel.
        let (_, res_rx) = std::sync::mpsc::channel::<ExportOutcome>();
        Self { res_rx }
    }

    /// `new`, with the per-photo work swapped out so a test can make it panic.
    #[cfg(not(target_arch = "wasm32"))]
    fn with_runner(run: fn(ExportJob) -> Result<ExportLanding, String>) -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<ExportJob>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<ExportOutcome>();
        Self::spawn_workers(job_rx, res_tx, run);
        Self { job_tx, res_rx }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn spawn_workers(
        job_rx: Receiver<ExportJob>,
        res_tx: Sender<ExportOutcome>,
        run: fn(ExportJob) -> Result<ExportLanding, String>,
    ) {
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
            if let Err(e) = spawned {
                eprintln!("[export] could not spawn export worker {i}: {e}");
            }
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
    use super::*;
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    use std::io::Cursor;

    /// Callers count outcomes to know a batch is done, so a photo that panics
    /// the exporter must still produce one, and the worker must live on.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_panicking_export_still_reports_and_the_worker_survives() {
        use std::time::{Duration, Instant};

        fn panics_on_bad(job: ExportJob) -> Result<ExportLanding, String> {
            if job.src.ends_with("bad.raw") {
                panic!("malformed file");
            }
            Ok(ExportLanding::File(PathBuf::from("out.jpg")))
        }
        let job = |name: &str| ExportJob {
            src: PathBuf::from(name),
            dest: ExportDest::Folder(PathBuf::from("out.jpg")),
            adj: Default::default(),
            touchups: Vec::new(),
            rot: 0,
            max_px: u32::MAX,
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

    #[cfg(not(target_arch = "wasm32"))]
    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-export-test-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The size limit applies to the cropped, rotated result, not the decode.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_folder_export_honors_the_long_edge_after_crop_and_rotation() {
        let dir = scratch_dir("long-edge");
        let src = dir.join("src.jpg");
        image_encode::encode_jpeg(&src, 400, 200, &vec![128u8; 400 * 200 * 4]).unwrap();
        let dest = dir.join("out.jpg");
        let adj = Adjustments {
            crop: Some(crate::develop::Crop {
                left: 0.0,
                top: 0.0,
                right: 0.5,
                bottom: 1.0,
            }),
            ..Adjustments::default()
        };

        let landing = do_export(ExportJob {
            src,
            dest: ExportDest::Folder(dest.clone()),
            adj,
            touchups: Vec::new(),
            rot: 1,
            max_px: 50,
        })
        .unwrap();

        assert!(matches!(landing, ExportLanding::File(ref p) if *p == dest));
        let out = image_decode::decode(&dest, u32::MAX).unwrap();
        assert_eq!((out.width, out.height), (50, 50), "200x200 crop, rotated, fit to 50");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn bytes_for_an_upload_leave_no_staging_file_behind() {
        let staged = || {
            std::fs::read_dir(std::env::temp_dir())
                .unwrap()
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with(&format!("lightphotos-upload-{}-", std::process::id()))
                })
                .count()
        };
        let jpeg = jpeg_bytes(8, 4, &[200u8; 8 * 4 * 4]).unwrap();
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG SOI marker");
        assert_eq!(staged(), 0);
    }

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

        let jpeg = bake_jpeg(&png, false, &Adjustments::default(), &[], 0, u32::MAX)
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

        let jpeg = bake_jpeg(&png, false, &Adjustments::default(), &[], 1, u32::MAX)
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

/// Decode `job.src` at full resolution, bake in its edits and size, and
/// deliver the JPEG to `job.dest`.
#[cfg(not(target_arch = "wasm32"))]
#[hotpath::measure]
fn do_export(job: ExportJob) -> Result<ExportLanding, String> {
    let (w, h, rgba) = bake_job(&job)?;
    match job.dest {
        ExportDest::Folder(dest) => {
            write_jpeg(&dest, w, h, &rgba)?;
            Ok(ExportLanding::File(dest))
        }
        ExportDest::Immich {
            server,
            filename,
            taken,
            stars,
        } => {
            let jpeg = jpeg_bytes(w, h, &rgba)?;
            let asset = server.upload(&jpeg, &filename, taken)?;
            if (1..=5).contains(&stars) {
                server.set_rating(&asset.id, stars)?;
            }
            Ok(ExportLanding::Asset {
                id: asset.id,
                duplicate: asset.duplicate,
            })
        }
    }
}

#[cfg(target_os = "macos")]
fn bake_job(job: &ExportJob) -> Result<(u32, u32, Vec<u8>), String> {
    let img = image_decode::decode(&job.src, u32::MAX)?;
    Ok(bake_sized(&img, &job.adj, &job.touchups, job.rot, job.max_px))
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn bake_job(job: &ExportJob) -> Result<(u32, u32, Vec<u8>), String> {
    let img = if image_decode::is_raw_extension(&job.src) {
        // rawler's path-based decoder memory-maps the file, which avoids
        // reading the whole RAW into a Vec first.
        image_decode::decode_raw_nonmac(&job.src, u32::MAX)?
    } else {
        let bytes = pollster::block_on(NativeFs.read_source(&job.src))?;
        image_decode::decode_nonraw_from_bytes(&bytes, u32::MAX)?
    };
    Ok(bake_sized(&img, &job.adj, &job.touchups, job.rot, job.max_px))
}

#[cfg(target_os = "macos")]
fn write_jpeg(dest: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    // Temp file then rename, as in `NativeFs::write_atomic`.
    let tmp = dest.with_extension("jpg.tmp");
    image_encode::encode_jpeg(&tmp, w, h, rgba)?;
    if let Err(e) = std::fs::rename(&tmp, dest) {
        let _ = std::fs::remove_file(&tmp);
        return Err(format!("rename: {e}"));
    }
    Ok(())
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn write_jpeg(dest: &std::path::Path, w: u32, h: u32, rgba: &[u8]) -> Result<(), String> {
    let jpeg = image_encode::encode_jpeg_to_vec(w, h, rgba)?;
    pollster::block_on(NativeFs.write_atomic(dest, &jpeg))
}

/// The macOS encoder only writes to a path, so bake to a staging file, read it
/// back, and remove it before returning. The bytes are then identical to what
/// a folder export writes, and nothing is left behind whatever the upload does.
#[cfg(target_os = "macos")]
fn jpeg_bytes(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let staging = std::env::temp_dir().join(format!(
        "lightphotos-upload-{}-{}.jpg",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let read = image_encode::encode_jpeg(&staging, w, h, rgba)
        .and_then(|()| std::fs::read(&staging).map_err(|e| format!("read staged JPEG: {e}")));
    let _ = std::fs::remove_file(&staging);
    read
}

#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn jpeg_bytes(w: u32, h: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    image_encode::encode_jpeg_to_vec(w, h, rgba)
}
