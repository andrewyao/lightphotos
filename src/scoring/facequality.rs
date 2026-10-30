// SPDX-License-Identifier: MIT OR Apache-2.0

//! Blink detection. Vision finds faces and eye landmark points, then pure
//! geometry scores how open each eye is. [`detect_faces`] is thin Vision glue;
//! the scoring below it is tested with fabricated points.

#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;
use std::thread;

use crate::worker_pool::WorkerPool;

#[cfg(target_os = "macos")]
use objc2::ClassType;
#[cfg(target_os = "macos")]
use objc2_vision::{VNDetectFaceLandmarksRequest, VNFaceLandmarkRegion2D};

#[cfg(target_os = "macos")]
use crate::scoring::vision;

/// Landmark points in Vision's normalized space: origin bottom-left, both axes
/// 0..1 over the whole image, not the face box.
pub type Points = Vec<(f32, f32)>;

/// One face as Vision reports it, before any scoring.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFace {
    /// `(x, y, width, height)`, normalized, origin bottom-left.
    pub bounding_box: (f32, f32, f32, f32),
    /// 0..1.
    pub confidence: f32,
    /// Eye contour points, empty when Vision didn't find that eye.
    pub left_eye: Points,
    pub right_eye: Points,
}

/// Faces with eye landmarks for the image at `path`. No faces is `Ok(vec![])`;
/// `Err` means Vision failed.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn detect_faces(path: &Path) -> Result<Vec<RawFace>, String> {
    // SAFETY: `new` takes no arguments; objc2 marks it unsafe only because
    // every Vision method is. It returns an owned, initialized request.
    let request = unsafe { VNDetectFaceLandmarksRequest::new() };
    vision::perform(vision::Source::File(path), &[request.as_super().as_super()])?;
    Ok(faces_from(&request))
}

/// The faces a finished landmarks request found.
#[cfg(target_os = "macos")]
pub fn faces_from(request: &VNDetectFaceLandmarksRequest) -> Vec<RawFace> {
    // SAFETY: plain getters on a live request and its retained observations;
    // `results` is nil-checked and every returned object is owned by objc2.
    unsafe {
        // Some Vision revisions return nil instead of an empty list for no faces.
        let Some(results) = request.results() else {
            return Vec::new();
        };

        let mut faces = Vec::with_capacity(results.len());
        for obs in results.iter() {
            let bb = obs.boundingBox();
            let (left_eye, right_eye) = match obs.landmarks() {
                Some(marks) => (
                    marks
                        .leftEye()
                        .map(|r| region_points(&r))
                        .unwrap_or_default(),
                    marks
                        .rightEye()
                        .map(|r| region_points(&r))
                        .unwrap_or_default(),
                ),
                None => (Vec::new(), Vec::new()),
            };
            faces.push(RawFace {
                bounding_box: (
                    bb.origin.x as f32,
                    bb.origin.y as f32,
                    bb.size.width as f32,
                    bb.size.height as f32,
                ),
                confidence: obs.confidence(),
                left_eye,
                right_eye,
            });
        }
        faces
    }
}

/// Faces for the image at `path`, boxes only: SeetaFace finds no eye
/// landmarks. No faces is `Ok(vec![])`; `Err` means the photo could not be
/// read.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
pub fn detect_faces(path: &Path) -> Result<Vec<RawFace>, String> {
    detect_upright(path).map(|(faces, _)| faces)
}

/// Copy a landmark region's points out of Vision's buffer.
///
/// # Safety
///
/// `normalizedPoints` returns `pointCount` `CGPoint`s owned by `region`. We copy
/// them before returning, so no caller holds the borrow.
#[cfg(target_os = "macos")]
fn region_points(region: &VNFaceLandmarkRegion2D) -> Points {
    // SAFETY: `normalizedPoints` points at `pointCount` CGPoints owned by
    // `region`, which `&region` keeps alive; null and empty are rejected first.
    unsafe {
        let count = region.pointCount();
        let ptr = region.normalizedPoints();
        if ptr.is_null() || count == 0 {
            return Vec::new();
        }
        let slice = std::slice::from_raw_parts(ptr, count);
        slice.iter().map(|p| (p.x as f32, p.y as f32)).collect()
    }
}

/// Openness below this counts as a blink. Open eyes run about 0.25 to 0.45 and
/// closed eyes about 0.05 to 0.10. Not yet checked against real photos; use
/// `src/bin/face_probe.rs` for that.
pub const CLOSED_EYE_RATIO: f32 = 0.15;

pub use crate::scoring::quality::EyeState;

/// A photo's face-based culling signals.
///
/// Serializable because `signalcache` persists it: a Vision face pass costs
/// 73 ms per photo, so a second visit to a folder must not repeat it.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
pub struct FaceQuality {
    pub faces: u32,
    /// The least-open eye in the frame, so one blinker marks a group shot.
    /// `None` when no eye landmarks were found.
    pub min_eye_openness: Option<f32>,
    /// A square around the face nearest the top-left corner, for Compare's
    /// zoom square to start on:
    /// its center in display-oriented uv and its side as a fraction of the
    /// photo's short side. `None` with no faces, and for an analysis cached
    /// before this was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<FaceFrame>,
    /// The box around every face, for the slide show to pan to and zoom on.
    /// `None` with no faces, and for an analysis cached before this was
    /// recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bounds: Option<FaceBounds>,
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FaceFrame {
    pub center: (f32, f32),
    pub side: f32,
}

/// A box in the upright photo's uv, 0 to 1, origin top-left.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FaceBounds {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
}

/// Room around the face, so the square shows it whole.
const FRAME_MARGIN: f32 = 1.3;

/// The square around one face in `faces`, the one whose center is nearest
/// the upright photo's top-left corner, on an upright `dw`×`dh` photo. Vision
/// applies a file's EXIF orientation itself, so its boxes are already
/// upright, with the origin bottom-left; the square is in the upright photo's
/// uv, origin top-left.
pub fn face_frame(faces: &[RawFace], (dw, dh): (u32, u32)) -> Option<FaceFrame> {
    let short = dw.min(dh) as f32;
    if short <= 0.0 {
        return None;
    }
    // In pixels, so a wide photo's x counts as much as its y.
    let distance = |(x0, y0, x1, y1): (f32, f32, f32, f32)| {
        ((x0 + x1) / 2.0 * dw as f32).hypot((y0 + y1) / 2.0 * dh as f32)
    };
    let (x0, y0, x1, y1) = faces
        .iter()
        .map(upright_rect)
        .filter(|b| distance(*b).is_finite())
        .min_by(|a, b| distance(*a).total_cmp(&distance(*b)))?;
    let extent = ((x1 - x0) * dw as f32).max((y1 - y0) * dh as f32);
    let side = (extent * FRAME_MARGIN / short).clamp(0.02, 1.0);
    let center = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);
    (side.is_finite() && center.0.is_finite() && center.1.is_finite())
        .then_some(FaceFrame { center, side })
}

/// The box around every face in `faces`, in the upright photo's uv, found
/// on a `w`×`h` image. `None` with no faces, no image, or no box left after
/// clamping to the photo.
pub fn face_bounds(faces: &[RawFace], (w, h): (u32, u32)) -> Option<FaceBounds> {
    if w == 0 || h == 0 {
        return None;
    }
    let (left, top, right, bottom) = faces
        .iter()
        .map(upright_rect)
        .filter(|&(l, t, r, b)| [l, t, r, b].iter().all(|v| v.is_finite()))
        .reduce(|a, b| (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)))?;
    let [left, top, right, bottom] = [left, top, right, bottom].map(|v| v.clamp(0.0, 1.0));
    (right > left && bottom > top).then_some(FaceBounds {
        left,
        top,
        right,
        bottom,
    })
}

/// `face`'s box as (left, top, right, bottom) in the upright photo's uv,
/// origin top-left. Vision reports it with the origin bottom-left, and
/// already turned by the file's EXIF orientation: a rectangle in a photo
/// marked orientation 6 came back with its upright width and height.
fn upright_rect(face: &RawFace) -> (f32, f32, f32, f32) {
    let (x, y, bw, bh) = face.bounding_box;
    (x, 1.0 - y - bh, x + bw, 1.0 - y)
}

impl FaceQuality {
    /// `None` means unknown. Callers must not treat unknown as closed.
    pub fn eye_state(&self) -> Option<EyeState> {
        self.min_eye_openness.map(|o| {
            if o < CLOSED_EYE_RATIO {
                EyeState::Closed
            } else {
                EyeState::Open
            }
        })
    }
}

/// Height over width of one eye contour: high when open, near 0 when blinking.
/// `None` for fewer than 3 points or a degenerate contour.
///
/// `aspect_wh` is image width / height. Vision normalizes x and y by different
/// lengths, so y is rescaled into x's units first. The widest point pair is
/// taken as the eye corners, which ignores Vision's point order (it differs
/// between the 65- and 76-point sets) and handles a tilted head.
pub fn eye_openness(points: &[(f32, f32)], aspect_wh: f32) -> Option<f32> {
    if points.len() < 3 || aspect_wh.is_nan() || aspect_wh <= 0.0 {
        return None;
    }
    let pts: Vec<(f32, f32)> = points.iter().map(|&(x, y)| (x, y / aspect_wh)).collect();

    let mut width = 0.0f32;
    let mut corners = (pts[0], pts[0]);
    for (i, &a) in pts.iter().enumerate() {
        for &b in &pts[i + 1..] {
            let d = (b.0 - a.0).hypot(b.1 - a.1);
            if d > width {
                width = d;
                corners = (a, b);
            }
        }
    }
    if width <= f32::EPSILON {
        return None;
    }

    let (a, b) = corners;
    let (ux, uy) = ((b.0 - a.0) / width, (b.1 - a.1) / width);
    let (nx, ny) = (-uy, ux);

    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for &(x, y) in &pts {
        let d = (x - a.0) * nx + (y - a.1) * ny;
        lo = lo.min(d);
        hi = hi.max(d);
    }
    Some((hi - lo) / width)
}

/// Score all faces in a photo, keeping the least-open eye.
pub fn face_quality(faces: &[RawFace], aspect_wh: f32) -> FaceQuality {
    let min_eye_openness = faces
        .iter()
        .flat_map(|f| {
            [
                eye_openness(&f.left_eye, aspect_wh),
                eye_openness(&f.right_eye, aspect_wh),
            ]
        })
        .flatten()
        .fold(None::<f32>, |acc, o| Some(acc.map_or(o, |a: f32| a.min(o))));

    FaceQuality {
        faces: faces.len() as u32,
        min_eye_openness,
        frame: None,
        bounds: None,
    }
}

/// `size`, a stored image's pixel size, as it shows upright under EXIF
/// `orientation`: orientations 5 to 8 turn it on its side.
#[cfg(any(target_os = "macos", test))]
fn upright_size((w, h): (u32, u32), orientation: u8) -> (u32, u32) {
    if (5..=8).contains(&orientation) {
        (h, w)
    } else {
        (w, h)
    }
}

/// What a detector found: the faces, and the size of the upright image their
/// boxes are on, when it is known.
type Detected = (Vec<RawFace>, Option<(u32, u32)>);

/// The faces in `path`, and the size of the upright image their boxes are
/// on, when it is known.
#[cfg(target_os = "macos")]
fn detect_upright(path: &Path) -> Result<Detected, String> {
    let faces = detect_faces(path)?;
    let orientation = crate::decode::image_decode::orientation_of(path);
    let size = crate::decode::image_decode::pixel_size(path).map(|s| upright_size(s, orientation));
    Ok((faces, size))
}

/// The faces in `path`, and the size of the upright image they were found on.
/// That is a shrunken copy, which is fine, since boxes are fractions of it.
#[cfg(all(not(target_os = "macos"), not(target_arch = "wasm32")))]
fn detect_upright(path: &Path) -> Result<Detected, String> {
    let image = crate::decode::image_decode::decode(path, crate::scoring::seetaface::DETECT_SIDE)?;
    detect_in(&image)
}

/// The browser cannot open a file by path: the main thread reads the bytes
/// and sends them with the job.
#[cfg(target_arch = "wasm32")]
fn detect_upright(job: &FaceJob) -> Result<Detected, String> {
    let bytes = match &job.bytes {
        Some(Ok(bytes)) => bytes,
        Some(Err(e)) => return Err(e.clone()),
        None => return Err("the photo's bytes were not read".into()),
    };
    let side = crate::scoring::seetaface::DETECT_SIDE;
    let image = if crate::decode::image_decode::is_raw_extension(&job.path) {
        // The JPEG the camera embedded when it is large enough, else the
        // fast quarter-resolution decode the thumbnails use. Never the full
        // demosaic, and the photos go one at a time, so the heap is reused.
        match crate::decode::rawler::rawler_embedded_for_faces(bytes, side)
            .filter(|image| image.width.max(image.height) >= side / 2)
        {
            Some(image) => image,
            None => crate::decode::rawler::raw_preview::decode_raw_from_shared_vec(
                std::sync::Arc::clone(bytes),
                side,
                false,
            )?,
        }
    } else {
        crate::decode::image_decode::decode_nonraw_from_bytes(bytes, side)?
    };
    detect_in(&image)
}

/// The faces in an upright decoded `image`, with its size.
#[cfg(not(target_os = "macos"))]
fn detect_in(image: &crate::decode::image_decode::DecodedImage) -> Result<Detected, String> {
    let faces = crate::scoring::seetaface::detect(image)?;
    Ok((faces, Some((image.width, image.height))))
}

/// One photo to analyze. The browser cannot open it by path, so there its
/// bytes, or why they could not be read, come along.
pub struct FaceJob {
    pub path: PathBuf,
    #[cfg(target_arch = "wasm32")]
    pub bytes: Option<Result<std::sync::Arc<Vec<u8>>, String>>,
}

impl FaceJob {
    pub fn from_path(path: PathBuf) -> Self {
        Self {
            path,
            #[cfg(target_arch = "wasm32")]
            bytes: None,
        }
    }
}

/// [`analyze`] for a pool's job.
fn analyze_job(job: &FaceJob) -> Result<FaceQuality, String> {
    #[cfg(not(target_arch = "wasm32"))]
    let detected = detect_upright(&job.path);
    #[cfg(target_arch = "wasm32")]
    let detected = detect_upright(job);
    score_detected(detected?)
}

/// Detect and score one photo. Assumes a square image if its size can't be
/// read, which skews the ratio but keeps the signal.
#[cfg(not(target_arch = "wasm32"))]
#[hotpath::measure]
pub fn analyze(path: &Path) -> Result<FaceQuality, String> {
    score_detected(detect_upright(path)?)
}

fn score_detected((faces, size): Detected) -> Result<FaceQuality, String> {
    // Landmarks are normalized over the upright image, so its aspect is the
    // one that counts.
    let aspect_wh = size
        .filter(|&(w, h)| w > 0 && h > 0)
        .map_or(1.0, |(w, h)| w as f32 / h as f32);
    let frame = size.and_then(|size| face_frame(&faces, size));
    let bounds = size.and_then(|size| face_bounds(&faces, size));
    Ok(FaceQuality {
        frame,
        bounds,
        ..face_quality(&faces, aspect_wh)
    })
}

pub struct FaceOutcome {
    pub path: PathBuf,
    pub result: Result<FaceQuality, String>,
}

/// Background workers for [`analyze`], which decodes a photo and runs a
/// detector over it, too slow for the UI thread.
pub struct FacePool {
    pool: WorkerPool<FaceJob, FaceQuality>,
}

impl FacePool {
    /// `None` when no worker could start. The caller must then hold no pool
    /// at all. A pool with no workers accepts jobs that never run, and the
    /// callers that mark those jobs pending would spin the frame loop waiting
    /// for them. In the browser it is one Web Worker, to leave the others to
    /// decoding.
    pub fn new() -> Option<Self> {
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = if cfg!(target_arch = "wasm32") {
            1
        } else {
            cores.saturating_sub(2).clamp(1, 2)
        };
        Self::with_workers(workers, analyze_job)
    }

    fn with_workers(
        workers: usize,
        analyze: fn(&FaceJob) -> Result<FaceQuality, String>,
    ) -> Option<Self> {
        // Callers hold the path as pending until its outcome arrives, so a
        // panic must still send one.
        let pool = WorkerPool::spawn_web(
            "facequality",
            workers,
            || {},
            move |job: &FaceJob| analyze(job),
            |_| "face analysis panicked".into(),
        );
        (pool.workers() > 0).then_some(Self { pool })
    }

    /// Queue an analysis. Ignored if the workers are gone (shutdown).
    pub fn submit(&self, path: PathBuf) {
        self.pool.submit(FaceJob::from_path(path));
    }

    /// A way to queue analyses from a task that does not own the pool, such
    /// as the browser's read of a photo's bytes.
    #[cfg(target_arch = "wasm32")]
    pub fn sender(&self) -> std::sync::mpsc::Sender<FaceJob> {
        self.pool.sender()
    }

    /// Drain finished analyses without blocking.
    pub fn poll(&self) -> Vec<FaceOutcome> {
        self.pool
            .poll()
            .into_iter()
            .map(|(job, result)| FaceOutcome {
                path: job.path,
                result,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    use crate::decode::image_encode::encode_jpeg;

    // Per process, because two `cargo test` runs on one machine otherwise
    // share these fixture names and one truncates a file while the other's
    // Vision request is still reading it.
    #[cfg(target_os = "macos")]
    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join(name);
        encode_jpeg(
            &path,
            w,
            h,
            rgba,
            crate::decode::image_encode::JpegQuality::Export,
        )
        .expect("encode fixture jpeg");
        path
    }

    /// Whether Vision can run a model on this machine. GitHub's macOS runners
    /// are VMs with no GPU or Neural Engine, and there a face request fails
    /// with "Unspecified error" (a feature-print request says "Could not
    /// create inference context"). Tests that need a model return early when
    /// this is false. Any other error counts as able, so a real regression
    /// still fails the test that hits it.
    #[cfg(target_os = "macos")]
    fn vision_can_infer() -> bool {
        static CAN: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *CAN.get_or_init(|| {
            let path = write_jpeg("vision_can_infer.jpg", 64, 64, &[128u8; 64 * 64 * 4]);
            let result = detect_faces(&path);
            let _ = std::fs::remove_file(&path);
            match result {
                Err(e)
                    if e.contains("Could not create inference context")
                        || e.contains("Unspecified error") =>
                {
                    eprintln!("skipping: Vision cannot run a model here ({e})");
                    false
                }
                _ => true,
            }
        })
    }

    fn face_at(x: f32, y: f32, w: f32, h: f32) -> RawFace {
        RawFace {
            bounding_box: (x, y, w, h),
            confidence: 1.0,
            left_eye: Vec::new(),
            right_eye: Vec::new(),
        }
    }

    fn close(a: (f32, f32), b: (f32, f32)) -> bool {
        (a.0 - b.0).abs() < 1e-4 && (a.1 - b.1).abs() < 1e-4
    }

    #[test]
    fn the_face_frame_flips_visions_bottom_left_origin() {
        // A face in the stored image's bottom-left corner, on a 4000x2000.
        let f = face_frame(&[face_at(0.0, 0.0, 0.1, 0.2)], (4000, 2000)).expect("one face frames");
        assert!(close(f.center, (0.05, 0.9)), "{f:?}");
        // 0.2 of 2000 tall beats 0.1 of 4000 wide: 400 px, with margin, of a
        // 2000 px short side.
        assert!((f.side - 0.2 * FRAME_MARGIN).abs() < 1e-4, "{f:?}");
    }

    #[test]
    fn the_face_frame_is_measured_against_the_upright_photos_short_side() {
        // Vision has already turned the box, so on the upright 2000x4000 it
        // is 200 x 800 px, and the short side is the width.
        let f = face_frame(&[face_at(0.0, 0.0, 0.1, 0.2)], (2000, 4000)).expect("frames");
        assert!(close(f.center, (0.05, 0.9)), "{f:?}");
        assert!((f.side - 0.4 * FRAME_MARGIN).abs() < 1e-4, "{f:?}");
    }

    #[test]
    fn a_stored_size_turns_on_its_side_only_for_orientations_5_to_8() {
        for orientation in [0, 1, 2, 3, 4, 9] {
            assert_eq!(upright_size((4000, 2000), orientation), (4000, 2000));
        }
        for orientation in 5..=8 {
            assert_eq!(upright_size((4000, 2000), orientation), (2000, 4000));
        }
    }

    #[test]
    fn the_face_frame_holds_only_the_face_nearest_the_top_left() {
        // Vision's origin is bottom-left, so the first face sits low on the
        // photo and the larger second one high on its left.
        let faces = [face_at(0.1, 0.1, 0.1, 0.1), face_at(0.3, 0.6, 0.2, 0.2)];
        let f = face_frame(&faces, (1000, 1000)).expect("frames");
        assert!(close(f.center, (0.4, 0.3)), "{f:?}");
        assert!((f.side - 0.2 * FRAME_MARGIN).abs() < 1e-4, "{f:?}");
    }

    #[test]
    fn the_face_frame_is_none_without_a_face_or_an_image() {
        let faces = [face_at(0.1, 0.4, 0.1, 0.1)];
        assert_eq!(face_frame(&[], (1000, 1000)), None);
        assert_eq!(face_frame(&faces, (0, 0)), None, "an empty image");
    }

    fn bounds_close(b: FaceBounds, want: (f32, f32, f32, f32)) -> bool {
        let got = (b.left, b.top, b.right, b.bottom);
        [
            (got.0, want.0),
            (got.1, want.1),
            (got.2, want.2),
            (got.3, want.3),
        ]
        .iter()
        .all(|(g, w)| (g - w).abs() < 1e-4)
    }

    #[test]
    fn the_face_bounds_hold_every_face_and_flip_visions_origin() {
        // Vision's origin is bottom-left: this face is low on the left, that
        // one high on the right.
        let faces = [face_at(0.1, 0.1, 0.1, 0.1), face_at(0.6, 0.7, 0.2, 0.1)];
        let b = face_bounds(&faces, (1000, 1000)).expect("two faces");
        assert!(bounds_close(b, (0.1, 0.2, 0.8, 0.9)), "{b:?}");
        let one = face_bounds(&faces[..1], (1000, 1000)).expect("one face");
        assert!(bounds_close(one, (0.1, 0.8, 0.2, 0.9)), "{one:?}");
    }

    #[test]
    fn the_face_bounds_skip_what_cannot_be_a_box() {
        let good = face_at(0.2, 0.2, 0.1, 0.1);
        assert_eq!(face_bounds(&[], (1000, 1000)), None, "no faces");
        assert_eq!(
            face_bounds(std::slice::from_ref(&good), (0, 0)),
            None,
            "no image"
        );
        let nan = face_at(f32::NAN, 0.2, 0.1, 0.1);
        let b = face_bounds(&[nan.clone(), good], (1000, 1000)).expect("the good face");
        assert!(bounds_close(b, (0.2, 0.7, 0.3, 0.8)), "{b:?}");
        assert_eq!(face_bounds(&[nan], (1000, 1000)), None, "only a NaN box");
        // Off the photo altogether, so nothing is left after clamping.
        let off = face_at(1.5, 0.2, 0.1, 0.1);
        assert_eq!(face_bounds(&[off], (1000, 1000)), None);
    }

    #[test]
    fn a_pool_that_started_no_workers_is_never_handed_to_a_caller() {
        assert!(
            FacePool::with_workers(0, analyze_job).is_none(),
            "a pool with no workers would accept jobs nothing runs"
        );
    }

    #[test]
    fn a_panicking_analysis_still_reports_and_the_worker_keeps_going() {
        fn panics_on_boom(job: &FaceJob) -> Result<FaceQuality, String> {
            if job.path.ends_with("boom.jpg") {
                panic!("simulated Vision crash");
            }
            Ok(FaceQuality {
                faces: 0,
                min_eye_openness: None,
                frame: None,
                bounds: None,
            })
        }
        let pool = FacePool::with_workers(1, panics_on_boom).expect("one worker should start");
        pool.submit(PathBuf::from("boom.jpg"));
        pool.submit(PathBuf::from("fine.jpg"));

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut outcomes = Vec::new();
        while outcomes.len() < 2 {
            outcomes.extend(pool.poll());
            assert!(
                std::time::Instant::now() < deadline,
                "only {} of 2 outcomes arrived",
                outcomes.len()
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(
            outcomes[0].result.is_err(),
            "the panic reports as a failure"
        );
        assert!(outcomes[1].result.is_ok(), "the worker survived the panic");
    }

    // The counterpart to the test above: a pool that did start a worker must
    // still carry a job all the way through Vision and back.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_running_pool_returns_an_analysis_for_a_submitted_photo() {
        if !vision_can_infer() {
            return;
        }
        let (w, h) = (64, 64);
        let flat = vec![128u8; (w * h * 4) as usize];
        let path = write_jpeg("facequality_pool_blank.jpg", w, h, &flat);

        let pool = FacePool::with_workers(1, analyze_job).expect("one worker should start");
        pool.submit(path.clone());

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let outcome = loop {
            if let Some(o) = pool.poll().into_iter().next() {
                break o;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the worker never returned an outcome"
            );
            thread::sleep(std::time::Duration::from_millis(10));
        };

        assert_eq!(outcome.path, path);
        let quality = outcome.result.expect("a blank image should analyze");
        assert_eq!(quality.faces, 0);

        let _ = std::fs::remove_file(&path);
    }

    // Runs real Vision on a blank image. "No faces" must be an empty Ok, not an
    // error. Landmark quality needs real portraits; see `src/bin/face_probe.rs`.
    #[cfg(target_os = "macos")]
    #[test]
    fn detect_faces_runs_and_finds_none_in_a_blank_image() {
        if !vision_can_infer() {
            return;
        }
        let (w, h) = (64, 64);
        let flat = vec![128u8; (w * h * 4) as usize];
        let path = write_jpeg("facequality_test_blank.jpg", w, h, &flat);

        let faces = detect_faces(&path).expect("Vision face request should run");
        assert!(
            faces.is_empty(),
            "expected no faces in a flat gray image, got {}",
            faces.len()
        );

        let _ = std::fs::remove_file(&path);
    }

    /// An almond eye contour whose openness is exactly `half_h / half_w`.
    fn eye_contour(cx: f32, cy: f32, half_w: f32, half_h: f32) -> Points {
        vec![
            (cx - half_w, cy),
            (cx - half_w * 0.5, cy + half_h),
            (cx + half_w * 0.5, cy + half_h),
            (cx + half_w, cy),
            (cx + half_w * 0.5, cy - half_h),
            (cx - half_w * 0.5, cy - half_h),
        ]
    }

    fn to_normalized(points: &[(f32, f32)], w: f32, h: f32) -> Points {
        points.iter().map(|&(x, y)| (x / w, y / h)).collect()
    }

    fn face_with_eyes(left: Points, right: Points) -> RawFace {
        RawFace {
            bounding_box: (0.4, 0.4, 0.2, 0.2),
            confidence: 0.9,
            left_eye: left,
            right_eye: right,
        }
    }

    #[test]
    fn open_eye_scores_well_above_a_blink() {
        let open = eye_openness(&eye_contour(0.5, 0.5, 0.05, 0.015), 1.0).unwrap();
        let closed = eye_openness(&eye_contour(0.5, 0.5, 0.05, 0.002), 1.0).unwrap();

        assert!((open - 0.3).abs() < 1e-4, "open eye ratio was {open}");
        assert!(closed < open);
        assert!(
            open > CLOSED_EYE_RATIO && closed < CLOSED_EYE_RATIO,
            "threshold should split these: open={open} closed={closed}"
        );
    }

    #[test]
    fn openness_is_corrected_for_the_image_aspect_ratio() {
        // The same 100x30 px eye (ratio 0.3) in a 3:2 and a 2:3 frame must
        // score the same.
        let pixels = eye_contour(1500.0, 1000.0, 50.0, 15.0);

        let landscape = to_normalized(&pixels, 3000.0, 2000.0);
        let portrait = to_normalized(&pixels, 2000.0, 3000.0);

        let l = eye_openness(&landscape, 3000.0 / 2000.0).unwrap();
        let p = eye_openness(&portrait, 2000.0 / 3000.0).unwrap();
        assert!((l - 0.3).abs() < 1e-4, "landscape ratio was {l}");
        assert!((p - 0.3).abs() < 1e-4, "portrait ratio was {p}");

        // Without the correction, the landscape eye is off by exactly 3:2.
        let uncorrected = eye_openness(&landscape, 1.0).unwrap();
        assert!(
            (uncorrected - 0.45).abs() < 1e-4,
            "uncorrected ratio was {uncorrected}"
        );
    }

    #[test]
    fn openness_survives_head_roll() {
        let upright = eye_contour(0.5, 0.5, 0.05, 0.015);
        let (s, c) = (0.6f32).sin_cos();
        let tilted: Points = upright
            .iter()
            .map(|&(x, y)| {
                let (dx, dy) = (x - 0.5, y - 0.5);
                (0.5 + dx * c - dy * s, 0.5 + dx * s + dy * c)
            })
            .collect();

        let a = eye_openness(&upright, 1.0).unwrap();
        let b = eye_openness(&tilted, 1.0).unwrap();
        assert!((a - b).abs() < 1e-4, "upright={a} tilted={b}");
    }

    #[test]
    fn degenerate_contours_have_no_openness() {
        assert_eq!(eye_openness(&[], 1.0), None);
        assert_eq!(eye_openness(&[(0.1, 0.1), (0.2, 0.1)], 1.0), None);
        assert_eq!(eye_openness(&[(0.1, 0.1); 6], 1.0), None);
        assert_eq!(eye_openness(&eye_contour(0.5, 0.5, 0.05, 0.015), 0.0), None);
    }

    #[test]
    fn a_group_shot_is_only_as_good_as_its_worst_blinker() {
        let open = eye_contour(0.3, 0.6, 0.03, 0.010);
        let blink = eye_contour(0.7, 0.6, 0.03, 0.001);

        let q = face_quality(
            &[
                face_with_eyes(open.clone(), open.clone()),
                face_with_eyes(open.clone(), blink),
            ],
            1.0,
        );

        assert_eq!(q.faces, 2);
        let worst = q.min_eye_openness.expect("two faces with eye landmarks");
        assert!((worst - 0.0333).abs() < 1e-3, "worst eye was {worst}");
        assert_eq!(q.eye_state(), Some(EyeState::Closed));
    }

    #[test]
    fn a_frame_of_open_eyes_reads_as_open() {
        let open = eye_contour(0.5, 0.6, 0.03, 0.010);
        let q = face_quality(&[face_with_eyes(open.clone(), open)], 1.0);
        assert_eq!(q.eye_state(), Some(EyeState::Open));
    }

    // No eyes found must read as unknown, never closed, or culling would
    // demote a good frame. One found eye is judged on its own.
    #[test]
    fn missing_eye_landmarks_are_unknown_not_closed() {
        let empty = face_quality(&[], 1.0);
        assert_eq!(empty.faces, 0);
        assert_eq!(empty.min_eye_openness, None);
        assert_eq!(empty.eye_state(), None);

        let faceless_landmarks = face_quality(&[face_with_eyes(Vec::new(), Vec::new())], 1.0);
        assert_eq!(faceless_landmarks.faces, 1);
        assert_eq!(faceless_landmarks.eye_state(), None);

        let open = eye_contour(0.5, 0.6, 0.03, 0.010);
        let profile = face_quality(&[face_with_eyes(open, Vec::new())], 1.0);
        assert_eq!(profile.eye_state(), Some(EyeState::Open));
    }

    #[test]
    fn detect_faces_errors_on_a_missing_file() {
        let path = std::env::temp_dir().join("facequality_does_not_exist.jpg");
        let _ = std::fs::remove_file(&path);
        assert!(detect_faces(&path).is_err());
    }

    #[test]
    fn a_face_with_no_eye_contours_scores_its_blink_as_unknown() {
        let q = face_quality(&[face_with_eyes(Vec::new(), Vec::new())], 1.0);
        assert_eq!(q.faces, 1);
        assert_eq!(q.eye_state(), None);
    }
}
