// SPDX-License-Identifier: GPL-3.0-or-later

//! Rendered pixels in, a quality score out. Shared by the app's scoring
//! workers and `src/bin/score_probe.rs`.
//!
//! On macOS one Vision pass over the pixels already in hand finds the faces
//! and, from macOS 15, the aesthetics score. Elsewhere, and when Vision
//! fails, the score is technical only.

use crate::quality::{self, Aesthetics, EyeState, QualityScore};

/// Score opaque sRGB8 RGBA pixels.
#[hotpath::measure]
pub fn judge(rgba: &[u8], width: u32, height: u32) -> QualityScore {
    let technical = quality::technical(rgba, width, height);
    let signals = vision_signals(rgba, width, height).unwrap_or_default();
    quality::score(&technical, signals.aesthetics.as_ref(), signals.eyes)
}

/// What Vision adds to the pixel measurements. Empty when unavailable.
#[derive(Default, Debug)]
pub struct VisionSignals {
    pub aesthetics: Option<Aesthetics>,
    pub eyes: Option<EyeState>,
}

#[cfg(not(target_os = "macos"))]
pub fn vision_signals(_rgba: &[u8], _w: u32, _h: u32) -> Result<VisionSignals, String> {
    Err("Vision is macOS-only".into())
}

/// Face landmarks always, aesthetics where the runtime has the request, both
/// on one handler over the pixels the score measures, so Vision never
/// decodes the file again.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn vision_signals(rgba: &[u8], width: u32, height: u32) -> Result<VisionSignals, String> {
    use objc2::ClassType;
    use objc2_vision::{VNCalculateImageAestheticsScoresRequest, VNDetectFaceLandmarksRequest};

    if width == 0 || height == 0 || rgba.len() < width as usize * height as usize * 4 {
        return Err("no pixels to analyze".into());
    }
    let image = cgimage(rgba, width, height)?;
    // Worker threads have no autorelease pool of their own, and Vision
    // autoreleases freely; without one each photo would leak until exit.
    objc2::rc::autoreleasepool(|_| {
        let faces = unsafe { VNDetectFaceLandmarksRequest::new() };
        let aesthetics =
            crate::vision::require_class(c"VNCalculateImageAestheticsScoresRequest", "15.0")
                .ok()
                .map(|()| unsafe { VNCalculateImageAestheticsScoresRequest::new() });

        let mut requests = vec![faces.as_super().as_super()];
        if let Some(a) = &aesthetics {
            requests.push(a.as_super().as_super());
        }
        crate::vision::perform(crate::vision::Source::Image(&image), &requests)?;

        let raw = crate::facequality::faces_from(&faces);
        let eyes = crate::facequality::face_quality(&raw, width as f32 / height as f32).eye_state();
        let aesthetics = aesthetics
            .and_then(|r| unsafe { r.results() })
            .and_then(|r| r.firstObject())
            .map(|o| unsafe {
                Aesthetics {
                    overall: o.overallScore(),
                    utility: o.isUtility(),
                }
            });
        Ok(VisionSignals { aesthetics, eyes })
    })
}

/// An independent `CGImage` copy of opaque RGBA pixels.
#[cfg(target_os = "macos")]
fn cgimage(
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<objc2_core_foundation::CFRetained<objc2_core_graphics::CGImage>, String> {
    let row = width as usize * 4;
    let mut buffer = rgba[..row * height as usize].to_vec();
    // SAFETY: `buffer` holds `row * height` bytes and outlives the context,
    // and `bitmap_context_image` copies the pixels out before it drops.
    let ctx = unsafe {
        crate::coregraphics::srgb_bitmap_context(buffer.as_mut_ptr().cast(), width, height, row)?
    };
    crate::coregraphics::bitmap_context_image(&ctx)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    /// Vision needs a GPU or ANE context some CI hosts lack; the face test
    /// in `facequality.rs` skips the same way.
    #[test]
    fn a_vision_pass_over_pixels_finds_no_face_in_a_flat_frame() {
        let (w, h) = (320u32, 240u32);
        let rgba = [90u8, 120, 150, 255].repeat((w * h) as usize);
        match vision_signals(&rgba, w, h) {
            Ok(s) => {
                assert_eq!(s.eyes, None, "no face, so no eye verdict");
                if crate::vision::require_class(c"VNCalculateImageAestheticsScoresRequest", "15.0")
                    .is_ok()
                {
                    let a = s.aesthetics.expect("macOS 15 scores aesthetics");
                    assert!((-1.0..=1.0).contains(&a.overall), "{a:?}");
                }
            }
            Err(e) => eprintln!("skipping: Vision unavailable here: {e}"),
        }
        let score = judge(&rgba, w, h);
        assert!(score.value <= 100);
    }
}
