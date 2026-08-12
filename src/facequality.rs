// SPDX-License-Identifier: MIT OR Apache-2.0

//! Face detection + eye-openness ("did they blink?") scoring, via Apple's
//! Vision framework (`VNDetectFaceLandmarksRequest`).
//!
//! Same shape as `featureprint.rs`: Vision decodes the file itself from a
//! path, so nothing here touches lightphotos' own decode/thumbnail pipeline.
//! Unlike feature prints, though, the *interesting* part isn't the framework
//! call — it's the geometry we run on the landmark points afterwards. So this
//! module is deliberately split the way `coregraphics.rs`/`image_decode.rs`
//! are: [`detect_faces`] is thin, untestable framework glue that returns raw
//! normalized landmark points, and the scoring built on top of those points is
//! pure and unit-tested against fabricated arrays.
//!
//! Eye openness is classic geometry (an eye-aspect-ratio over the eye contour
//! points), not a trained model — consistent with the roadmap's
//! heuristic-first, on-device constraint.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread;

use objc2::ClassType;
use objc2_vision::{
    VNDetectFaceLandmarksRequest, VNDetectFaceRectanglesRequest, VNFaceLandmarkRegion2D,
};

use crate::vision;

/// A landmark region's points in Vision's normalized image space: origin
/// bottom-left, both axes 0..1, relative to the *whole image* (not the face
/// bounding box).
pub type Points = Vec<(f32, f32)>;

/// One detected face, as Vision reports it — raw framework output with no
/// interpretation applied. The scoring layer consumes this; keeping it a plain
/// data struct is what lets that layer be tested without Vision in the loop.
#[derive(Debug, Clone, PartialEq)]
pub struct RawFace {
    /// Face bounding box `(x, y, width, height)`, normalized, origin bottom-left.
    pub bounding_box: (f32, f32, f32, f32),
    /// Vision's own detection confidence, 0..1.
    pub confidence: f32,
    /// Left-eye contour points (empty if Vision didn't resolve that region).
    pub left_eye: Points,
    /// Right-eye contour points (empty if Vision didn't resolve that region).
    pub right_eye: Points,
}

/// Run `VNDetectFaceLandmarksRequest` over the image at `path`.
///
/// Returns one [`RawFace`] per detected face, in Vision's own result order.
/// An image with no faces is `Ok(vec![])` — only an actual framework failure
/// (unreadable file, Vision error) is an `Err`.
pub fn detect_faces(path: &Path) -> Result<Vec<RawFace>, String> {
    unsafe {
        let request = VNDetectFaceLandmarksRequest::new();
        vision::perform_request(path, request.as_super().as_super())?;

        // No results at all is a legitimate "no faces here", not an error —
        // Vision leaves `results` nil rather than empty in some revisions.
        let Some(results) = request.results() else {
            return Ok(Vec::new());
        };

        let mut faces = Vec::with_capacity(results.len());
        for obs in results.iter() {
            let bb = obs.boundingBox();
            let (left_eye, right_eye) = match obs.landmarks() {
                Some(marks) => (
                    marks.leftEye().map(|r| region_points(&r)).unwrap_or_default(),
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

/// Copy a landmark region's normalized points out of Vision's own buffer.
///
/// # Safety
///
/// `normalizedPoints` hands back a buffer owned by `region` and valid only for
/// as long as `region` lives, holding exactly `pointCount` `CGPoint`s. We copy
/// eagerly here so no caller ever holds that borrow.
fn region_points(region: &VNFaceLandmarkRegion2D) -> Points {
    unsafe {
        let count = region.pointCount();
        let ptr = region.normalizedPoints();
        if ptr.is_null() || count == 0 {
            return Vec::new();
        }
        let slice = std::slice::from_raw_parts(ptr, count);
        slice
            .iter()
            .map(|p| (p.x as f32, p.y as f32))
            .collect()
    }
}

/// Face detection only — `VNDetectFaceRectanglesRequest`, no landmarks.
///
/// The cheap fallback for [`detect_faces`]: it answers "is there a face here,
/// and where" without paying for the landmark constellation, which is the
/// expensive and less reliable half of the request. Returns [`RawFace`]s with
/// empty eye contours so it drops straight into [`face_quality`] — a photo
/// scored this way simply reports `min_eye_openness: None`, i.e. "faces yes,
/// blink unknown", which every consumer already has to handle.
///
/// Useful when landmarks prove too heavy for a background pass, or when a
/// photo's landmarks come back garbage (profiles, small/distant faces) but the
/// face count itself is still worth having.
pub fn detect_face_rects(path: &Path) -> Result<Vec<RawFace>, String> {
    unsafe {
        let request = VNDetectFaceRectanglesRequest::new();
        vision::perform_request(path, request.as_super().as_super())?;

        let Some(results) = request.results() else {
            return Ok(Vec::new());
        };
        Ok(results
            .iter()
            .map(|obs| {
                let bb = obs.boundingBox();
                RawFace {
                    bounding_box: (
                        bb.origin.x as f32,
                        bb.origin.y as f32,
                        bb.size.width as f32,
                        bb.size.height as f32,
                    ),
                    confidence: obs.confidence(),
                    left_eye: Vec::new(),
                    right_eye: Vec::new(),
                }
            })
            .collect())
    }
}

// ---------------------------------------------------------------------------
// Pure scoring layer — no Vision, no filesystem, fully unit-testable.
// ---------------------------------------------------------------------------

/// Openness below this counts as a blink. Tuned to sit well under a relaxed
/// open eye (whose contour runs ~0.25–0.45 tall relative to its width) and well
/// over a closed one (a near-flat contour, ~0.05–0.10) — the gap between those
/// two populations is wide, which is what makes the heuristic workable at all.
///
/// Provisional until [`crate::facequality`]'s probe harness has been run over
/// real open-eyed and blinking frames; it is the one number in this module that
/// synthetic fixtures cannot validate.
pub const CLOSED_EYE_RATIO: f32 = 0.15;

/// What the eye geometry says about a photo, once every face has been scored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EyeState {
    /// Every resolved eye in the frame is open.
    Open,
    /// At least one resolved eye is closed — someone blinked.
    Closed,
}

/// A photo's face-derived culling signals. Cheap, `Copy`, and cached per path
/// in `app.rs` alongside `sharpness`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FaceQuality {
    /// How many faces Vision found.
    pub faces: u32,
    /// The least-open eye anywhere in the frame, as an eye-aspect ratio — so a
    /// group shot is only as good as its worst blinker. `None` when no face had
    /// usable eye landmarks (no faces at all, or a profile/occluded shot).
    pub min_eye_openness: Option<f32>,
}

impl FaceQuality {
    /// Classify the frame, or `None` when there's no eye geometry to judge —
    /// callers must treat "unknown" as "don't penalize", never as "closed".
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

/// Eye-aspect ratio of one eye contour: how tall the contour is relative to how
/// wide, so an open eye scores high and a blink scores near zero.
///
/// `aspect_wh` is the source image's pixel width divided by its height, needed
/// because Vision normalizes x by image width and y by image height
/// *independently* — in a 3:2 frame a vertical distance of 0.1 is only two
/// thirds of a horizontal 0.1. Rescaling y by `1 / aspect_wh` puts both axes
/// back in the same units before any distance is measured.
///
/// The eye's own axes are found from the points themselves — the widest pair is
/// taken as the corners and height is measured perpendicular to that line — so
/// the result doesn't depend on Vision's point ordering (which differs between
/// the 65- and 76-point constellations) and survives head roll.
///
/// `None` when there aren't enough points, or the contour is degenerate.
pub fn eye_openness(points: &[(f32, f32)], aspect_wh: f32) -> Option<f32> {
    if points.len() < 3 || !(aspect_wh > 0.0) {
        return None;
    }
    // Into isotropic units: keep x, express y in the same width-relative scale.
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

    // Unit vector along the corner-to-corner axis, and its normal.
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

/// Reduce Vision's raw detections to the per-photo signal, taking the *worst*
/// eye in the frame (see [`FaceQuality::min_eye_openness`]).
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

/// Detect and score in one step: the entry point `app.rs` calls per photo.
///
/// Falls back to a square aspect if the file's dimensions can't be read, which
/// only skews the ratio rather than losing the signal.
pub fn analyze(path: &Path) -> Result<FaceQuality, String> {
    let faces = detect_faces(path)?;
    let aspect_wh = crate::image_decode::pixel_size(path)
        .map(|(w, h)| w as f32 / h as f32)
        .unwrap_or(1.0);
    Ok(face_quality(&faces, aspect_wh))
}

/// A finished analysis, carrying its path back so the caller can match it up.
pub struct FaceOutcome {
    pub path: PathBuf,
    pub result: Result<FaceQuality, String>,
}

/// Background worker pool for face analysis, mirroring
/// [`crate::featureprint::DistancePool`]: small pool, self-contained jobs,
/// drained once per frame.
///
/// A pool rather than inline work because [`analyze`] makes Vision decode the
/// file at full resolution — far heavier than the thumbnail-based blur and
/// dHash scoring that run straight on the UI thread. Unlike feature prints,
/// though, the result is a plain `Copy` struct, so there's no "compute both
/// halves on one thread" constraint here: [`FaceQuality`] crosses the channel
/// on its own.
pub struct FacePool {
    job_tx: Sender<PathBuf>,
    res_rx: Receiver<FaceOutcome>,
}

impl FacePool {
    pub fn new() -> Self {
        let (job_tx, job_rx) = std::sync::mpsc::channel::<PathBuf>();
        let (res_tx, res_rx) = std::sync::mpsc::channel::<FaceOutcome>();
        let job_rx = Arc::new(Mutex::new(job_rx));

        // Same sizing rationale as the feature-print pool: this only ever runs
        // over burst/duplicate-group members, so a couple of workers is plenty
        // and keeps Vision/ANE contention low.
        let cores = thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(4);
        let workers = cores.saturating_sub(2).clamp(1, 2);

        for i in 0..workers {
            let job_rx = Arc::clone(&job_rx);
            let res_tx = res_tx.clone();
            thread::Builder::new()
                .name(format!("facequality-worker-{i}"))
                .spawn(move || loop {
                    let path = {
                        let rx = match job_rx.lock() {
                            Ok(rx) => rx,
                            Err(_) => return,
                        };
                        match rx.recv() {
                            Ok(p) => p,
                            Err(_) => return, // all senders dropped → shut down
                        }
                    };
                    let result = analyze(&path);
                    if res_tx.send(FaceOutcome { path, result }).is_err() {
                        break; // UI side gone
                    }
                })
                .expect("spawn facequality worker");
        }

        Self { job_tx, res_rx }
    }

    /// Queue an analysis. Ignored if the workers are gone (shutdown).
    pub fn submit(&self, path: PathBuf) {
        let _ = self.job_tx.send(path);
    }

    /// Drain all finished analyses (non-blocking).
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

    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        encode_jpeg(&path, w, h, rgba).expect("encode fixture jpeg");
        path
    }

    // Real Vision round trip, mirroring `featureprint.rs`'s own FFI test: a
    // synthetic image obviously contains no faces, so what this actually pins
    // down is the plumbing — the request runs, the result list is readable,
    // and "no faces" comes back as an empty Ok rather than an error or a
    // crash. Whether the *landmarks* are any good is a question only real
    // portraits can answer; that's what `src/bin/face_probe.rs` is for.
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

    // --- pure scoring layer: fabricated landmarks, no Vision involved --------

    /// An almond eye contour in isotropic (square-pixel) space. Its openness is
    /// exactly `half_h / half_w`, which is what makes these tests assertions
    /// about the geometry rather than about a magic number.
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
        // The same physical eye — 100px wide, 30px tall, ratio 0.3 — sitting in
        // a 3:2 landscape frame and in a 2:3 portrait one. Vision normalizes x
        // and y against different denominators, so without the correction the
        // two frames would disagree about the same eye.
        let pixels = eye_contour(1500.0, 1000.0, 50.0, 15.0);

        let landscape = to_normalized(&pixels, 3000.0, 2000.0);
        let portrait = to_normalized(&pixels, 2000.0, 3000.0);

        let l = eye_openness(&landscape, 3000.0 / 2000.0).unwrap();
        let p = eye_openness(&portrait, 2000.0 / 3000.0).unwrap();
        assert!((l - 0.3).abs() < 1e-4, "landscape ratio was {l}");
        assert!((p - 0.3).abs() < 1e-4, "portrait ratio was {p}");

        // And it genuinely matters: assuming a square frame inflates the same
        // landscape eye by exactly the 3:2 factor.
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
        // Every point identical: no axis to measure against.
        assert_eq!(eye_openness(&[(0.1, 0.1); 6], 1.0), None);
        // A nonsensical aspect must not produce a nonsense score.
        assert_eq!(eye_openness(&eye_contour(0.5, 0.5, 0.05, 0.015), 0.0), None);
    }

    #[test]
    fn a_group_shot_is_only_as_good_as_its_worst_blinker() {
        let open = eye_contour(0.3, 0.6, 0.03, 0.010); // 0.333
        let blink = eye_contour(0.7, 0.6, 0.03, 0.001); // 0.033

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

    // A profile shot resolves at most one eye, and a landscape resolves none.
    // Both must come back "unknown" rather than "closed" — the culling logic
    // penalizes closed eyes, so a false Closed would demote a perfectly good
    // frame.
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
        assert!(detect_face_rects(&path).is_err());
    }

    // Same plumbing check as the landmarks request, for the cheap fallback —
    // and it must feed `face_quality` cleanly, reporting "faces unknown-eyed"
    // rather than tripping over its empty eye contours.
    #[test]
    fn rectangles_only_detection_runs_and_composes_with_scoring() {
        let (w, h) = (64, 64);
        let flat = vec![90u8; (w * h * 4) as usize];
        let path = write_jpeg("facequality_test_rects.jpg", w, h, &flat);

        let faces = detect_face_rects(&path).expect("Vision face-rect request should run");
        assert!(faces.is_empty(), "expected no faces, got {}", faces.len());

        let q = face_quality(&[face_with_eyes(Vec::new(), Vec::new())], 1.0);
        assert_eq!(q.faces, 1);
        assert_eq!(q.eye_state(), None);

        let _ = std::fs::remove_file(&path);
    }
}
