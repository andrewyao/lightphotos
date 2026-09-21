// SPDX-License-Identifier: GPL-3.0-or-later

//! Subject masks from Apple Vision, used by the Loupe's "Show Selection"
//! overlay. Person segmentation runs first because it gives the cleanest edge
//! on people. If it finds no one, the general foreground request runs instead.
//! Vision picks the mask resolution, so [`Mask`] carries its own size.

use std::path::Path;

#[cfg(target_os = "macos")]
use objc2::ClassType;
#[cfg(target_os = "macos")]
use objc2_core_video::{
    CVPixelBuffer, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetPixelFormatType, CVPixelBufferGetWidth,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
#[cfg(target_os = "macos")]
use objc2_vision::{
    VNGenerateForegroundInstanceMaskRequest, VNGeneratePersonSegmentationRequest,
    VNGeneratePersonSegmentationRequestQualityLevel,
};

#[cfg(target_os = "macos")]
use crate::vision;

/// `kCVPixelFormatType_OneComponent8`, requested from person segmentation.
#[cfg(target_os = "macos")]
const ONE_COMPONENT_8: u32 = u32::from_be_bytes(*b"L008");
/// `kCVPixelFormatType_OneComponent32Float`, which the foreground request can
/// return.
#[cfg(target_os = "macos")]
const ONE_COMPONENT_32F: u32 = u32::from_be_bytes(*b"L00f");

/// Which Vision request produced a mask. The two behave differently enough
/// that debugging a mask starts here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaskSource {
    Person,
    ForegroundInstance,
}

/// A coverage mask at Vision's resolution. `0` is background, `255` is fully
/// foreground, values between are soft edges.
// Before the derives: `track` injects the census field, and `Clone` and
// `PartialEq` have to be generated for the struct that has it.
#[lightwatch::track(manual_measured)]
#[derive(Debug, Clone, PartialEq)]
pub struct Mask {
    pub width: u32,
    pub height: u32,
    /// `width * height` bytes with no row padding.
    pub alpha: Vec<u8>,
    pub source: MaskSource,
}

// See `DecodedImage`: the generated `Measured` counts the struct, not the
// `width * height` bytes hanging off it.
impl lightwatch::Measured for Mask {
    fn bytes(&self) -> usize {
        size_of::<Self>() + self.alpha.capacity()
    }
}

impl Mask {
    /// Coverage at `(x, y)`, or 0 outside the mask.
    // Only tests and `seg_probe` use this. The Loupe samples the mask on the GPU.
    #[allow(dead_code)]
    pub fn at(&self, x: u32, y: u32) -> u8 {
        if x >= self.width || y >= self.height {
            return 0;
        }
        self.alpha[(y * self.width + x) as usize]
    }

    /// This mask resampled bilinearly to `width x height`, which keeps soft edges.
    // Only CPU users like `seg_probe` call this. The Loupe lets the GPU sampler stretch the mask.
    #[allow(dead_code)]
    pub fn resized(&self, width: u32, height: u32) -> Mask {
        if (width, height) == (self.width, self.height) {
            return self.clone();
        }
        Mask::new_tracked(MaskFields {
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
        })
    }

    /// This mask rotated or flipped by an EXIF orientation (`1..=8`) so it lines
    /// up with the decoded image.
    #[cfg(target_os = "macos")]
    pub fn oriented(self, orientation: u8) -> Mask {
        if orientation <= 1 {
            return self;
        }
        let (width, height, alpha) =
            crate::image_ops::orient_mask(&self.alpha, self.width, self.height, orientation);
        Mask::new_tracked(MaskFields {
            width,
            height,
            alpha,
            source: self.source,
        })
    }

    /// Mean coverage, 0.0..=1.0.
    // Only `seg_probe` reports this. The app uses `solid_coverage`.
    #[allow(dead_code)]
    pub fn coverage(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let total: u64 = self.alpha.iter().map(|&a| a as u64).sum();
        total as f32 / (self.alpha.len() as f32 * 255.0)
    }

    /// Fraction of pixels over half coverage. The "nothing found" test uses
    /// this, not the mean, because a faint smear can have a high mean while
    /// covering nothing. It can't catch a confident wrong mask: on Apple's
    /// `iMac Blue` wallpaper, with no person in it, person segmentation
    /// returns 13.4% solid coverage.
    #[cfg(any(target_os = "macos", test))]
    pub fn solid_coverage(&self) -> f32 {
        if self.alpha.is_empty() {
            return 0.0;
        }
        let solid = self.alpha.iter().filter(|&&a| a > 128).count();
        solid as f32 / self.alpha.len() as f32
    }
}

/// Solid coverage below this means "no person found" and triggers the
/// fallback. Person segmentation doesn't error on a photo without people; it
/// returns an empty mask. Kept low so a small, distant person still counts.
#[cfg(any(target_os = "macos", test))]
const EMPTY_COVERAGE: f32 = 0.01;

/// The subject mask for the photo at `path`, in display orientation. An error
/// means no selection is available, which is normal for photos with no subject.
/// Vision ignores EXIF orientation, so we rotate the mask ourselves.
#[cfg(target_os = "macos")]
pub fn segment(path: &Path) -> Result<Mask, String> {
    let mask = match segment_person(path) {
        Ok(mask) if mask.solid_coverage() >= EMPTY_COVERAGE => mask,
        // No person, or the person request failed. Report the fallback's error.
        _ => segment_foreground(path)?,
    };
    Ok(mask.oriented(crate::image_decode::orientation_of(path)))
}

#[cfg(not(target_os = "macos"))]
pub fn segment(_path: &Path) -> Result<Mask, String> {
    Err("subject segmentation is unsupported on this platform".into())
}

/// Person segmentation at Accurate quality. It runs on demand for one photo,
/// so speed matters less than a clean edge.
#[cfg(target_os = "macos")]
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

/// Foreground-instance segmentation, with every instance merged into one mask.
#[cfg(target_os = "macos")]
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

/// Copy a Vision mask buffer into packed bytes, dropping row padding
/// (`bytes_per_row` is usually wider than `width`).
#[cfg(target_os = "macos")]
fn pixel_buffer_to_mask(buffer: &CVPixelBuffer, source: MaskSource) -> Result<Mask, String> {
    let width = CVPixelBufferGetWidth(buffer);
    let height = CVPixelBufferGetHeight(buffer);
    let format = CVPixelBufferGetPixelFormatType(buffer);
    if width == 0 || height == 0 {
        return Err("Vision returned an empty mask buffer".into());
    }

    // SAFETY: the read-only lock is held for the whole copy, and every read stays
    // inside `height` rows of `bytes_per_row` as the buffer reports them.
    unsafe {
        let lock = CVPixelBufferLockBaseAddress(buffer, CVPixelBufferLockFlags::ReadOnly);
        if lock != 0 {
            return Err(format!(
                "could not lock Vision's mask buffer (CVReturn {lock})"
            ));
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

        Ok(Mask::new_tracked(MaskFields {
            width: width as u32,
            height: height as u32,
            alpha: result?,
            source,
        }))
    }
}

/// # Safety
///
/// `base` must point at `height` rows of at least `width` bytes, each row
/// `stride` bytes apart.
#[cfg(target_os = "macos")]
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
/// still in bytes.
#[cfg(target_os = "macos")]
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

    // 997 and 331 are deliberately unrelated: see the matching test in
    // `image_decode`. `resized` and `oriented` both rebuild a `Mask` through
    // `MaskFields`, and both swap the pair, so a silent transposition in
    // either constructor fails here.
    #[test]
    fn width_and_height_land_in_their_own_fields() {
        let tall = Mask::new_tracked(MaskFields {
            width: 997,
            height: 331,
            alpha: vec![255; 997 * 331],
            source: MaskSource::Person,
        });
        assert_eq!((tall.width, tall.height), (997, 331));

        let wide = tall.resized(331, 997);
        assert_eq!((wide.width, wide.height), (331, 997));
        assert_eq!(wide.alpha.len(), 331 * 997);
    }
    use crate::image_encode::encode_jpeg;

    fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(name);
        encode_jpeg(&path, w, h, rgba).expect("encode fixture jpeg");
        path
    }

    #[test]
    fn coverage_reads_the_fraction_of_the_mask_that_is_filled() {
        let half = Mask::new_tracked(MaskFields {
            width: 2,
            height: 2,
            alpha: vec![255, 255, 0, 0],
            source: MaskSource::Person,
        });
        assert!((half.coverage() - 0.5).abs() < 1e-6);

        let empty = Mask::new_tracked(MaskFields {
            alpha: vec![0; 4],
            width: half.width,
            height: half.height,
            source: half.source,
        });
        assert_eq!(empty.coverage(), 0.0);
        assert!(
            empty.solid_coverage() < EMPTY_COVERAGE,
            "an empty mask must trip the fallback"
        );
    }

    // A small crisp subject can have a lower mean than a faint smear, so the
    // emptiness test must use solid coverage.
    #[test]
    fn a_low_confidence_smear_is_not_mistaken_for_a_subject() {
        let smear = Mask::new_tracked(MaskFields {
            width: 10,
            height: 10,
            alpha: vec![70; 100],
            source: MaskSource::Person,
        });
        assert!(
            smear.coverage() > EMPTY_COVERAGE,
            "mean alone would be fooled"
        );
        assert_eq!(smear.solid_coverage(), 0.0);

        let mut small_subject = vec![0u8; 100];
        small_subject[..8].fill(250);
        let subject = Mask::new_tracked(MaskFields {
            alpha: small_subject,
            width: smear.width,
            height: smear.height,
            source: smear.source,
        });
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
        let small = Mask::new_tracked(MaskFields {
            width: 2,
            height: 2,
            alpha: vec![0, 255, 255, 0],
            source: MaskSource::ForegroundInstance,
        });

        let same = small.resized(2, 2);
        assert_eq!(same, small, "a no-op resize must not disturb the mask");

        let big = small.resized(8, 8);
        assert_eq!(big.width, 8);
        assert_eq!(big.height, 8);
        assert_eq!(big.alpha.len(), 64);
        assert_eq!(
            big.source, small.source,
            "resizing must not relabel the source"
        );
        // Corners keep their values and the interior blends.
        assert_eq!(big.at(0, 0), 0);
        assert_eq!(big.at(7, 0), 255);
        let mid = big.at(3, 3);
        assert!(mid > 0 && mid < 255, "expected a soft edge, got {mid}");
    }

    #[test]
    fn sampling_outside_the_mask_reads_as_background() {
        let m = Mask::new_tracked(MaskFields {
            width: 2,
            height: 1,
            alpha: vec![10, 20],
            source: MaskSource::Person,
        });
        assert_eq!(m.at(0, 0), 10);
        assert_eq!(m.at(1, 0), 20);
        assert_eq!(m.at(2, 0), 0);
        assert_eq!(m.at(0, 1), 0);
    }

    // Runs real Vision on a flat image with no subject. Both requests run, and
    // the result is either a mask matching its own size or a clean error.
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
