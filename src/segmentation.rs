// SPDX-License-Identifier: MIT OR Apache-2.0

//! Subject/foreground segmentation via Apple's Vision framework — the mask
//! behind the Loupe's "Show Selection" overlay.
//!
//! Two requests, tried in order, because they answer different questions:
//! `VNGeneratePersonSegmentationRequest` is purpose-built for people and gives
//! a clean soft-edged matte, but returns nothing useful on a photo with no
//! person in it. `VNGenerateForegroundInstanceMaskRequest` is the general
//! "whatever the subject is" fallback — a dog, a plate of food, a bike.
//!
//! Both hand back a `CVPixelBuffer` at *their* chosen resolution, not the
//! photo's, so [`Mask`] carries its own dimensions and callers must scale (see
//! [`crate::image_ops::upsample_mask_bilinear`]).
//!
//! Vision decodes the file itself, so a path is the whole input — the handler
//! setup is shared with the other Vision features in `vision.rs`.

use std::path::Path;

use objc2::ClassType;
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_vision::{
    VNGenerateForegroundInstanceMaskRequest, VNGeneratePersonSegmentationRequest,
    VNGeneratePersonSegmentationRequestQualityLevel,
};

use crate::vision;

/// `kCVPixelFormatType_OneComponent8` — one 8-bit channel, the format person
/// segmentation produces.
const ONE_COMPONENT_8: u32 = u32::from_be_bytes(*b"L008");
/// `kCVPixelFormatType_OneComponent32Float` — one 32-bit float channel, which
/// the instance-mask request can produce instead.
const ONE_COMPONENT_32F: u32 = u32::from_be_bytes(*b"L00f");

/// Which request produced a mask. Worth surfacing: the two behave differently
/// enough on real photos that "why does this look like that" usually starts
/// with knowing which one ran.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSource {
    /// `VNGeneratePersonSegmentationRequest` — a person was found.
    Person,
    /// `VNGenerateForegroundInstanceMaskRequest` — the general subject fallback.
    ForegroundInstance,
}

/// A single-channel coverage mask at whatever resolution Vision chose:
/// `0` = background, `255` = fully foreground, in between = soft edge.
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes, tightly packed (Vision's row padding is already
    /// stripped out).
    pub alpha: Vec<u8>,
    pub source: MaskSource,
}

impl Mask {
    /// Coverage at `(x, y)`, or 0 outside the mask.
    pub fn at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.alpha[(y * self.width + x) as usize]
    }

    /// This mask resampled to `width × height`, for overlaying at display size.
    ///
    /// Bilinear, so the model's soft matte edge survives the stretch — see
    /// [`crate::image_ops::resample_bilinear_u8`]. Returns `self` unchanged when
    /// the size already matches.
    pub fn resized(&self, width: u32, height: u32) -> Mask {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        Mask {
            width,
            height,
            alpha: crate::image_ops::resample_bilinear_u8(
                &self.alpha,
                self.width,
                self.height,
                width,
                height,
            ),
            source: self.source,
        }
    }

    /// This mask reoriented per an EXIF orientation (`1..=8`), so it lines up
    /// with the decoded image. See [`crate::image_ops::orient_mask`].
    pub fn oriented(self, orientation: u8) -> Mask {
        if orientation <= 1 {
            return self;
        }
        let (width, height, alpha) =
            crate::image_ops::orient_mask(&self.alpha, self.width, self.height, orientation);
        Mask {
            width,
            height,
            alpha,
            source: self.source,
        }
    }

    /// Mean coverage, 0.0..=1.0. Cheap way to spot a mask that came back empty
    /// (nothing found) or saturated (everything "foreground").
    pub fn coverage(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let total: u64 = self.alpha.iter().map(|&a| a as u64).sum();
        total as f32 / (self.alpha.len() as f32 * 255.0)
    }

    /// Fraction of the mask that is *confidently* foreground (over half
    /// coverage), as opposed to [`coverage`](Self::coverage)'s mean.
    ///
    /// The emptiness test reads this rather than the mean, because a mask can
    /// carry a respectable mean while committing to nothing. It is a test for
    /// *nothing found*, and nothing more — it does not detect a wrong answer.
    /// Measured on Apple's abstract `iMac Blue` wallpaper, which contains no
    /// person anywhere, person segmentation returns 13.2% mean / 13.4% solid:
    /// a confident, well-formed, completely imaginary subject. No coverage
    /// statistic separates that from a real one, so nothing here tries to.
    pub fn solid_coverage(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let solid = self.alpha.iter().filter(|&&a| a > 128).count();
        solid as f32 / self.alpha.len() as f32
    }
}

/// Solid coverage below this counts as "no person found", triggering the
/// fallback. Person segmentation on a person-free photo doesn't error, so an
/// empty result is the only in-band way it can say no.
///
/// Deliberately low: it catches the *nothing* case, and a small-but-real
/// subject (someone a few metres back) must stay on the person path. It cannot
/// catch a confident wrong answer — see [`Mask::solid_coverage`] for a measured
/// example of one — so a photo with no person in it may still come back with a
/// person mask rather than falling through to the general request. Whether that
/// matters in practice is one of the questions this exploratory plan exists to
/// answer on real photographs.
const EMPTY_COVERAGE: f32 = 0.01;

/// Segment the subject of the photo at `path`, in *display* orientation.
///
/// Tries person segmentation first and falls back to the general
/// foreground-instance request when no person is found. Callers should treat an
/// error as "no selection available for this photo" rather than as a bug —
/// plenty of photographs simply have no subject to isolate.
///
/// The result is reoriented to match [`crate::image_decode::decode`]'s output.
/// Vision reads the file in its stored orientation and knows nothing about the
/// EXIF tag, so without this a portrait shot from a camera that records
/// rotation in metadata would come back with its mask lying on its side.
pub fn segment(path: &Path) -> Result<Mask, String> {
    let mask = match segment_person(path) {
        Ok(mask) if mask.solid_coverage() >= EMPTY_COVERAGE => mask,
        // Either no person, or the request itself failed — both mean "ask the
        // general-purpose request instead". Its error is the one worth
        // reporting, since it's the last word.
        _ => segment_foreground(path)?,
    };
    Ok(mask.oriented(crate::image_decode::orientation_of(path)))
}

/// `VNGeneratePersonSegmentationRequest` at accurate quality.
///
/// Accurate rather than balanced/fast because this runs once, on demand, for
/// the single photo the user is looking at — there's no folder-wide pass to
/// keep cheap, and a ragged matte would undermine the whole point of looking
/// at the selection.
pub fn segment_person(path: &Path) -> Result<Mask, String> {
    unsafe {
        let request = VNGeneratePersonSegmentationRequest::new();
        request.setQualityLevel(VNGeneratePersonSegmentationRequestQualityLevel::Accurate);
        request.setOutputPixelFormat(ONE_COMPONENT_8);
        vision::perform_request(path, request.as_super().as_super().as_super())?;

        let observation = request
            .results()
            .and_then(|r| r.firstObject())
            .ok_or("Vision returned no person-segmentation mask")?;
        let buffer = observation.pixelBuffer();
        pixel_buffer_to_mask(&buffer, MaskSource::Person)
    }
}

/// `VNGenerateForegroundInstanceMaskRequest`, merging every instance it found
/// into one mask.
///
/// Merged rather than per-instance because this plan's selection is a single
/// foreground/background split — per-instance selection would be a different
/// (and much larger) feature.
pub fn segment_foreground(path: &Path) -> Result<Mask, String> {
    unsafe {
        let request = VNGenerateForegroundInstanceMaskRequest::new();
        vision::perform_request(path, request.as_super().as_super())?;

        let observation = request
            .results()
            .and_then(|r| r.firstObject())
            .ok_or("Vision found no foreground subject")?;
        let buffer = observation
            .generateMaskForInstances_error(&observation.allInstances())
            .map_err(|e| e.localizedDescription().to_string())?;
        pixel_buffer_to_mask(&buffer, MaskSource::ForegroundInstance)
    }
}

/// Copy a Vision mask buffer into a tightly-packed `Vec<u8>`.
///
/// Handles both single-channel formats Vision uses, and strips the row padding
/// (`bytes_per_row` is generally wider than `width`, aligned for the GPU).
fn pixel_buffer_to_mask(buffer: &CVPixelBuffer, source: MaskSource) -> Result<Mask, String> {
    let width = CVPixelBufferGetWidth(buffer);
    let height = CVPixelBufferGetHeight(buffer);
    let format = CVPixelBufferGetPixelFormatType(buffer);
    if width == 0 || height == 0 {
        return Err("Vision returned an empty mask buffer".into());
    }

    // SAFETY: read-only lock held for exactly the span of the copy below; every
    // read stays inside `height` rows of `bytes_per_row`, as reported by the
    // buffer itself.
    unsafe {
        let lock = CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        if lock != 0 {
            return Err(format!("could not lock Vision's mask buffer (CVReturn {lock})"));
        }
        let base = CVPixelBufferGetBaseAddress(buffer);
        let stride = CVPixelBufferGetBytesPerRow(buffer);
        let result = if base.is_null() {
            Err("Vision's mask buffer has no base address".to_string())
        } else {
            match format {
                ONE_COMPONENT_8 => Ok(copy_u8_rows(base.cast::<u8>(), width, height, stride)),
                ONE_COMPONENT_32F => Ok(copy_f32_rows(base.cast::<f32>(), width, height, stride)),
                other => Err(format!(
                    "unsupported Vision mask pixel format {:?}",
                    other.to_be_bytes().map(|b| b as char)
                )),
            }
        };
        CVPixelBufferUnlockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);

        Ok(Mask {
            width: width as u32,
            height: height as u32,
            alpha: result?,
            source,
        })
    }
}

/// # Safety
///
/// `base` must point at `height` rows of at least `width` bytes, each row
/// `stride` bytes apart.
unsafe fn copy_u8_rows(base: *const u8, width: usize, height: usize, stride: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = std::slice::from_raw_parts(base.add(y * stride), width);
        out.extend_from_slice(row);
    }
    out
}

/// # Safety
///
/// Same contract as [`copy_u8_rows`], with rows of `width` `f32`s. `stride` is
/// still in *bytes*, hence the division.
unsafe fn copy_f32_rows(base: *const f32, width: usize, height: usize, stride: usize) -> Vec<u8> {
    let stride_f32 = stride / std::mem::size_of::<f32>();
    let mut out = Vec::with_capacity(width * height);
    for y in 0..height {
        let row = std::slice::from_raw_parts(base.add(y * stride_f32), width);
        out.extend(row.iter().map(|&v| (v.clamp(0.0, 1.0) * 255.0 + 0.5) as u8));
    }
    out
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

    #[test]
    fn coverage_reads_the_fraction_of_the_mask_that_is_filled() {
        let half = Mask {
            width: 2,
            height: 2,
            alpha: vec![255, 255, 0, 0],
            source: MaskSource::Person,
        };
        assert!((half.coverage() - 0.5).abs() < 1e-6);

        let empty = Mask {
            alpha: vec![0; 4],
            ..half.clone()
        };
        assert_eq!(empty.coverage(), 0.0);
        assert!(
            empty.solid_coverage() < EMPTY_COVERAGE,
            "an empty mask must trip the fallback"
        );
    }

    // Why the emptiness test reads the solid fraction and not the mean: a mask
    // can carry a respectable mean while committing to nothing, and a small
    // crisp subject can carry a lower mean than that smear while being exactly
    // what we want to keep.
    #[test]
    fn a_low_confidence_smear_is_not_mistaken_for_a_subject() {
        let smear = Mask {
            width: 10,
            height: 10,
            alpha: vec![70; 100], // 27% mean coverage, nothing committed
            source: MaskSource::Person,
        };
        assert!(smear.coverage() > EMPTY_COVERAGE, "mean alone would be fooled");
        assert_eq!(smear.solid_coverage(), 0.0);

        let mut small_subject = vec![0u8; 100];
        small_subject[..8].fill(250); // 8% of the frame, fully committed
        let subject = Mask {
            alpha: small_subject,
            ..smear.clone()
        };
        assert!(
            subject.solid_coverage() >= EMPTY_COVERAGE,
            "a small but confident subject must stay on the person path"
        );
        assert!(
            subject.coverage() < smear.coverage(),
            "and it does so despite a *lower* mean than the smear"
        );
    }

    #[test]
    fn resizing_stretches_the_mask_to_display_size() {
        let small = Mask {
            width: 2,
            height: 2,
            alpha: vec![0, 255, 255, 0],
            source: MaskSource::ForegroundInstance,
        };

        let same = small.resized(2, 2);
        assert_eq!(same, small, "a no-op resize must not disturb the mask");

        let big = small.resized(8, 8);
        assert_eq!(big.width, 8);
        assert_eq!(big.height, 8);
        assert_eq!(big.alpha.len(), 64);
        assert_eq!(big.source, small.source, "resizing must not relabel the source");
        // Corners keep their original values; the soft interior is what
        // bilinear buys over nearest-neighbour.
        assert_eq!(big.at(0, 0), 0);
        assert_eq!(big.at(7, 0), 255);
        let mid = big.at(3, 3);
        assert!(mid > 0 && mid < 255, "expected a soft edge, got {mid}");
    }

    #[test]
    fn sampling_outside_the_mask_reads_as_background() {
        let m = Mask {
            width: 2,
            height: 1,
            alpha: vec![10, 20],
            source: MaskSource::Person,
        };
        assert_eq!(m.at(0, 0), 10);
        assert_eq!(m.at(1, 0), 20);
        assert_eq!(m.at(2, 0), 0);
        assert_eq!(m.at(0, 1), 0);
    }

    // Real Vision round trip, like `facequality.rs`'s: a synthetic image has no
    // subject, so what this pins down is that both requests run, that the
    // person request's emptiness routes to the fallback rather than being
    // mistaken for a mask, and that whatever comes back is either a
    // correctly-shaped buffer or a clean error — never a crash or a mask whose
    // alpha length disagrees with its dimensions.
    #[test]
    fn segmentation_runs_end_to_end_on_a_subjectless_image() {
        let (w, h) = (96, 64);
        let flat = vec![120u8; (w * h * 4) as usize];
        let path = write_jpeg("segmentation_test_flat.jpg", w, h, &flat);

        match segment(&path) {
            Ok(mask) => {
                assert_eq!(
                    mask.alpha.len(),
                    (mask.width * mask.height) as usize,
                    "mask buffer must match its own dimensions"
                );
                assert!(mask.width > 0 && mask.height > 0);
            }
            Err(e) => assert!(!e.is_empty(), "an error must say something"),
        }

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn segmentation_errors_on_a_missing_file() {
        let path = std::env::temp_dir().join("segmentation_does_not_exist.jpg");
        let _ = std::fs::remove_file(&path);
        assert!(segment(&path).is_err());
    }
}
