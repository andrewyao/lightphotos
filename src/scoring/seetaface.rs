// SPDX-License-Identifier: MIT OR Apache-2.0

//! Face boxes where Apple Vision is not: SeetaFace's funnel-structured
//! cascade through the `rustface` crate, with its model embedded
//! (`assets/models/`, BSD-2). It finds boxes only, with no eye landmarks, so
//! blink detection stays macOS only. Its boxes come back in the same form
//! Vision's do, so `facequality` treats them alike.

use std::cell::RefCell;

use rustface::{Detector, ImageData};

use crate::decode::image_decode::{DecodedImage, PixelFormat};
use crate::scoring::facequality::RawFace;

/// The long side of the image faces are looked for on. Larger finds smaller
/// faces and costs time: about 35 to 105 ms a photo at this size.
pub const DETECT_SIDE: u32 = 1000;

static MODEL: &[u8] = include_bytes!("../../assets/models/seeta_fd_frontal_v1.0.bin");

/// The smallest face the model finds, in pixels of the image it runs on.
const MIN_FACE: u32 = 20;

/// Scores below this are not faces. A clear portrait scores 12 to 40 and a
/// small face in a crowd 2.5 to 9.
const SCORE_FLOOR: f64 = 2.0;

/// A face must score at least this share of the photo's best, or it is
/// dropped. A stray box on a patterned background scores about a tenth of the
/// real face beside it, and the slide show aims at the box around them all.
const RELATIVE_SCORE: f64 = 0.12;

thread_local! {
    /// One detector per thread, since parsing the model costs more than the
    /// first photo's work and a `Detector` is not `Send`.
    static DETECTOR: RefCell<Option<Box<dyn Detector>>> = const { RefCell::new(None) };
}

fn new_detector() -> Result<Box<dyn Detector>, String> {
    let model = rustface::read_model(MODEL).map_err(|e| format!("face model: {e}"))?;
    let mut detector = rustface::create_detector_with_model(model);
    detector.set_min_face_size(MIN_FACE);
    detector.set_score_thresh(SCORE_FLOOR);
    detector.set_pyramid_scale_factor(0.8);
    detector.set_slide_window_step(4, 4);
    Ok(detector)
}

/// The faces in an upright decoded `image`, which should be at most
/// [`DETECT_SIDE`] on its long side. An error means the pixels could not be
/// read; no faces is `Ok(vec![])`.
pub fn detect(image: &DecodedImage) -> Result<Vec<RawFace>, String> {
    let gray = gray_of(image)?;
    detect_gray(&gray, image.width, image.height)
}

/// `gray`, one byte a pixel and `width` x `height`, searched for faces.
pub(crate) fn detect_gray(gray: &[u8], width: u32, height: u32) -> Result<Vec<RawFace>, String> {
    if (width as usize).checked_mul(height as usize) != Some(gray.len()) {
        return Err("the pixels do not match the image's size".into());
    }
    // Too small to hold a face the model can see.
    if width < 2 * MIN_FACE || height < 2 * MIN_FACE {
        return Ok(Vec::new());
    }
    let found = DETECTOR.with(|slot| -> Result<Vec<Found>, String> {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(new_detector()?);
        }
        let detector = slot.as_mut().ok_or("no face detector")?;
        let faces = detector.detect(&ImageData::new(gray, width, height));
        Ok(faces
            .iter()
            .map(|f| {
                let b = f.bbox();
                Found {
                    x: b.x(),
                    y: b.y(),
                    w: b.width(),
                    h: b.height(),
                    score: f.score(),
                }
            })
            .collect())
    })?;
    Ok(raw_faces(&found, width, height))
}

/// One detection, in pixels of the image it was found on. The box can hang
/// over the image's edge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Found {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub score: f64,
}

/// `found` as the boxes Vision gives: normalized, origin bottom-left, kept to
/// the image. Weak detections beside a strong one are dropped.
pub(crate) fn raw_faces(found: &[Found], width: u32, height: u32) -> Vec<RawFace> {
    let best = found
        .iter()
        .map(|f| f.score)
        .filter(|s| s.is_finite())
        .fold(0.0, f64::max);
    let keep = SCORE_FLOOR.max(RELATIVE_SCORE * best);
    let (iw, ih) = (f64::from(width), f64::from(height));
    found
        .iter()
        .filter(|f| f.score.is_finite() && f.score >= keep)
        .filter_map(|f| {
            let (left, top) = (f64::from(f.x).max(0.0), f64::from(f.y).max(0.0));
            let right = (f64::from(f.x) + f64::from(f.w)).min(iw);
            let bottom = (f64::from(f.y) + f64::from(f.h)).min(ih);
            if !(right > left && bottom > top) {
                return None;
            }
            Some(RawFace {
                bounding_box: (
                    (left / iw) as f32,
                    (1.0 - bottom / ih) as f32,
                    ((right - left) / iw) as f32,
                    ((bottom - top) / ih) as f32,
                ),
                confidence: (f.score / 20.0).clamp(0.0, 1.0) as f32,
                left_eye: Vec::new(),
                right_eye: Vec::new(),
            })
        })
        .collect()
}

/// `image` as one byte a pixel.
fn gray_of(image: &DecodedImage) -> Result<Vec<u8>, String> {
    let pixels = (image.width as usize)
        .checked_mul(image.height as usize)
        .ok_or("the image is too large")?;
    match image.pixel_format {
        PixelFormat::Srgb8 => {
            if image.rgba.len() != pixels.checked_mul(4).ok_or("the image is too large")? {
                return Err("the pixels do not match the image's size".into());
            }
            Ok(image
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| {
                    let (r, g, b) = (u32::from(p[0]), u32::from(p[1]), u32::from(p[2]));
                    ((77 * r + 150 * g + 29 * b) >> 8) as u8
                })
                .collect())
        }
        // Linear light, 16-bit float, as a RAW decodes. A plain gamma is
        // near enough for a detector that only wants the faces' contrast.
        PixelFormat::LinearF16 | PixelFormat::TonedF16 => {
            if image.rgba.len() != pixels.checked_mul(8).ok_or("the image is too large")? {
                return Err("the pixels do not match the image's size".into());
            }
            let channel =
                |px: &[u8], i: usize| half::f16::from_le_bytes([px[2 * i], px[2 * i + 1]]).to_f32();
            Ok(image
                .rgba
                .as_chunks::<8>()
                .0
                .iter()
                .map(|px| {
                    let luma =
                        0.2126 * channel(px, 0) + 0.7152 * channel(px, 1) + 0.0722 * channel(px, 2);
                    // NaN clamps to 0 as well.
                    (luma.clamp(0.0, 1.0).powf(1.0 / 2.2) * 255.0) as u8
                })
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(x: i32, y: i32, w: u32, h: u32, score: f64) -> Found {
        Found { x, y, w, h, score }
    }

    #[test]
    fn the_model_loads() {
        assert!(new_detector().is_ok());
    }

    #[test]
    fn a_box_comes_out_normalized_with_visions_bottom_left_origin() {
        // 100 px in from the left and 50 down, 200 x 100, in a 1000 x 500.
        let faces = raw_faces(&[found(100, 50, 200, 100, 10.0)], 1000, 500);
        assert_eq!(faces.len(), 1);
        let (x, y, w, h) = faces[0].bounding_box;
        let close = |a: f32, b: f32| (a - b).abs() < 1e-5;
        assert!(
            close(x, 0.1) && close(w, 0.2) && close(h, 0.2),
            "{x} {w} {h}"
        );
        // Its bottom edge is 150 px down, so 350 px up from the bottom.
        assert!(close(y, 0.7), "{y}");
    }

    #[test]
    fn a_box_over_the_edge_is_kept_to_the_image_and_one_outside_it_is_dropped() {
        let faces = raw_faces(
            &[
                found(-20, -10, 100, 100, 10.0),
                found(950, 450, 100, 100, 10.0),
                found(2000, 0, 50, 50, 10.0),
                found(5, 5, 0, 40, 10.0),
            ],
            1000,
            500,
        );
        assert_eq!(faces.len(), 2, "the outside and the empty box are dropped");
        let (x, y, w, h) = faces[0].bounding_box;
        assert!(x.abs() < 1e-6 && (w - 0.08).abs() < 1e-5, "{x} {w}");
        assert!(
            (y - 0.82).abs() < 1e-5 && (h - 0.18).abs() < 1e-5,
            "{y} {h}"
        );
        let (x, _, w, _) = faces[1].bounding_box;
        assert!(
            (x - 0.95).abs() < 1e-5 && (w - 0.05).abs() < 1e-5,
            "{x} {w}"
        );
    }

    #[test]
    fn a_weak_box_beside_a_strong_face_is_dropped_but_a_crowd_of_weak_ones_stays() {
        let portrait = [found(300, 150, 400, 400, 39.6), found(350, 35, 68, 68, 3.5)];
        assert_eq!(raw_faces(&portrait, 1000, 800).len(), 1);
        let crowd: Vec<Found> = (0..6)
            .map(|i| found(60 * i, 100, 30, 30, 2.5 + f64::from(i)))
            .collect();
        assert_eq!(raw_faces(&crowd, 1000, 800).len(), 6);
        let nan = [found(10, 10, 50, 50, f64::NAN), found(80, 10, 50, 50, 5.0)];
        assert_eq!(
            raw_faces(&nan, 1000, 800).len(),
            1,
            "a NaN score is no face"
        );
    }

    #[test]
    fn a_blank_or_tiny_image_has_no_faces_and_a_wrong_sized_one_is_an_error() {
        assert_eq!(detect_gray(&vec![128; 200 * 150], 200, 150), Ok(Vec::new()));
        assert_eq!(detect_gray(&vec![128; 30 * 30], 30, 30), Ok(Vec::new()));
        assert_eq!(detect_gray(&[], 0, 0), Ok(Vec::new()));
        assert!(detect_gray(&[0; 10], 200, 150).is_err());
        assert!(detect_gray(&[0; 10], u32::MAX, u32::MAX).is_err());
    }

    fn image(format: PixelFormat, width: u32, height: u32, rgba: Vec<u8>) -> DecodedImage {
        DecodedImage::new_tracked(crate::decode::image_decode::DecodedImageFields {
            width,
            height,
            rgba,
            pixel_format: format,
        })
    }

    #[test]
    fn pixels_become_gray_in_either_format_and_a_short_buffer_is_an_error() {
        let white = image(PixelFormat::Srgb8, 2, 1, vec![255; 8]);
        assert_eq!(gray_of(&white), Ok(vec![255, 255]));
        let one = half::f16::from_f32(1.0).to_le_bytes();
        let half_px: Vec<u8> = (0..8).flat_map(|_| one).collect();
        let linear = image(PixelFormat::LinearF16, 2, 1, half_px);
        assert_eq!(gray_of(&linear), Ok(vec![255, 255]));
        let nan = half::f16::NAN.to_le_bytes();
        let nan_px: Vec<u8> = (0..4).flat_map(|_| nan).collect();
        let nan_image = image(PixelFormat::LinearF16, 1, 1, nan_px);
        assert_eq!(gray_of(&nan_image), Ok(vec![0]));
        assert!(gray_of(&image(PixelFormat::Srgb8, 2, 2, vec![0; 8])).is_err());
        assert!(gray_of(&image(PixelFormat::LinearF16, 2, 1, vec![0; 8])).is_err());
    }
}
