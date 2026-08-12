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

use std::path::Path;

use objc2::ClassType;
use objc2_vision::{VNDetectFaceLandmarksRequest, VNFaceLandmarkRegion2D};

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

    #[test]
    fn detect_faces_errors_on_a_missing_file() {
        let path = std::env::temp_dir().join("facequality_does_not_exist.jpg");
        let _ = std::fs::remove_file(&path);
        assert!(detect_faces(&path).is_err());
    }
}
