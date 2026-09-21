// SPDX-License-Identifier: GPL-3.0-or-later

//! Blink detection. Vision finds faces and eye landmark points, then pure
//! geometry scores how open each eye is. [`detect_faces`] is thin Vision glue;
//! the scoring below it is tested with fabricated points.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

#[cfg(target_os = "macos")]
use objc2::ClassType;
#[cfg(target_os = "macos")]
use objc2_vision::{VNDetectFaceLandmarksRequest, VNFaceLandmarkRegion2D};

#[cfg(target_os = "macos")]
use crate::vision;

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
    unsafe {
        let request = VNDetectFaceLandmarksRequest::new();
        vision::perform_request(path, request.as_super().as_super())?;

        // Some Vision revisions return nil instead of an empty list for no faces.
        let Some(results) = request.results() else {
            return Ok(Vec::new());
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
        Ok(faces)
    }
}

#[cfg(not(target_os = "macos"))]
pub fn detect_faces(_path: &Path) -> Result<Vec<RawFace>, String> {
    Err("face detection is unsupported on this platform".into())
}

/// Copy a landmark region's points out of Vision's buffer.
///
/// # Safety
///
/// `normalizedPoints` returns `pointCount` `CGPoint`s owned by `region`. We copy
/// them before returning, so no caller holds the borrow.
#[cfg(target_os = "macos")]
fn region_points(region: &VNFaceLandmarkRegion2D) -> Points {
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

/// What the eye geometry says about a photo, once every face has been scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeState {
    /// Every detected eye is open.
    Open,
    /// At least one detected eye is closed.
    Closed,
}

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
    if points.len() < 3 || !(aspect_wh > 0.0) {
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
    }
}

/// Detect and score one photo. Assumes a square image if its size can't be
/// read, which skews the ratio but keeps the signal.
#[hotpath::measure]
pub fn analyze(path: &Path) -> Result<FaceQuality, String> {
    let faces = detect_faces(path)?;
    let aspect_wh = crate::image_decode::pixel_size(path)
        .map(|(w, h)| w as f32 / h as f32)
        .unwrap_or(1.0);
    Ok(face_quality(&faces, aspect_wh))
}

pub struct FaceOutcome {
    pub path: PathBuf,
    pub result: Result<FaceQuality, String>,
}

/// Background workers for [`analyze`], which makes Vision decode the full-size
/// file and is too slow for the UI thread.
pub struct FacePool {
    job_tx: Sender<PathBuf>,
    res_rx: Receiver<FaceOutcome>,
}

impl FacePool {
    /// `None` when no worker could start, which is the whole story on targets
    /// that can't spawn threads. The caller must then hold no pool at all. A
    /// pool with no workers accepts jobs that never run, and the callers that
    /// mark those jobs pending would spin the frame loop waiting for them.
    pub fn new() -> Option<Self> {
        // Only burst and duplicate members are analyzed, so two workers are
        // enough and keep contention for Vision and the Neural Engine low.
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        Self::with_workers(cores.saturating_sub(2).clamp(1, 2))
    }

    fn with_workers(workers: usize) -> Option<Self> {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<FaceOutcome>();
        let job_rx = Arc::new(Mutex::new(job_rx));
        let mut running = 0usize;

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            let spawned = thread::Builder::new()
                .name(format!("facequality-worker-{i}"))
                .spawn(move || loop {
                    let path = {
                        let rx = match job_rx.lock() {
                            Ok(rx) => rx,
                            Err(_) => return,
                        };
                        match rx.recv() {
                            Ok(p) => p,
                            Err(_) => return,
                        }
                    };
                    let result = analyze(&path);
                    if res_tx.send(FaceOutcome { path, result }).is_err() {
                        break;
                    }
                });
            match spawned {
                Ok(_) => running += 1,
                // Some targets (wasm32) can't spawn threads. Log instead of
                // crashing at startup.
                Err(e) => eprintln!("[facequality] could not spawn worker {i}: {e}"),
            }
        }

        (running > 0).then_some(Self { job_tx, res_rx })
    }

    /// Queue an analysis. Ignored if the workers are gone (shutdown).
    pub fn submit(&self, path: PathBuf) {
        let _ = self.job_tx.send(path);
    }

    /// Drain finished analyses without blocking.
    pub fn poll(&self) -> Vec<FaceOutcome> {
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
    use crate::image_encode::encode_jpeg;

    // Per process, because two `cargo test` runs on one machine otherwise
    // share these fixture names and one truncates a file while the other's
    // Vision request is still reading it.
    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        let path = dir.join(name);
        encode_jpeg(&path, w, h, rgba).expect("encode fixture jpeg");
        path
    }

    #[test]
    fn a_pool_that_started_no_workers_is_never_handed_to_a_caller() {
        assert!(
            FacePool::with_workers(0).is_none(),
            "a pool with no workers would accept jobs nothing runs"
        );
    }

    // The counterpart to the test above: a pool that did start a worker must
    // still carry a job all the way through Vision and back.
    #[test]
    fn a_running_pool_returns_an_analysis_for_a_submitted_photo() {
        let (w, h) = (64, 64);
        let flat = vec![128u8; (w * h * 4) as usize];
        let path = write_jpeg("facequality_pool_blank.jpg", w, h, &flat);

        let pool = FacePool::with_workers(1).expect("one worker should start");
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
    #[test]
    fn detect_faces_runs_and_finds_none_in_a_blank_image() {
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
