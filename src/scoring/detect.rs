// SPDX-License-Identifier: MIT OR Apache-2.0

//! Detectors behind one call: which photo, which target, coverage out. The
//! app's detect worker, the MCP headless binary and the probes all go
//! through [`run`]. A detector reads the source file, which edits never
//! change, so its answer is stable under editing.

use std::cell::OnceCell;
use std::fmt;
use std::path::Path;

use crate::decode::image_decode::{DecodedImage, PixelFormat};
use crate::develop::{Coverage, DetectTarget};

/// The longest side of [`DetectInput::preview`]. The sky heuristic's budget
/// is about 30 ms at 1024; its guided filter only gets slower on more
/// pixels, with no gain in the mask.
pub const PREVIEW_SIDE: u32 = 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DetectError {
    /// This build has no detector for the target: Subject off macOS, Sky
    /// until its algorithm lands. The panel greys the button on
    /// [`available`].
    Unsupported,
    /// Vision, the model or the decode failed. Not persisted; Retry re-runs.
    Failed(String),
}

impl fmt::Display for DetectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetectError::Unsupported => write!(f, "not available in this build"),
            DetectError::Failed(why) => write!(f, "{why}"),
        }
    }
}

/// Whether [`run`] can produce `target` in this build, known without a
/// round trip so the panel can grey the button.
pub const fn available(target: DetectTarget) -> bool {
    match target {
        DetectTarget::Subject => cfg!(target_os = "macos"),
        // The browser has no worker that can reach the folder, so a sky
        // detector there needs its own path once one exists.
        DetectTarget::Sky => sky_detect::AVAILABLE && !cfg!(target_arch = "wasm32"),
    }
}

/// Detect `target` on `photo` and store the result beside it. Blocks for
/// 0.1 to 3 s; call on a worker. "Found nothing" is `Ok(Coverage::nothing())`,
/// stored like any other answer. The file is written before this returns, so
/// a sidecar can only ever name a file that exists.
#[cfg(not(target_arch = "wasm32"))]
pub fn run(photo: &Path, target: DetectTarget) -> Result<Coverage, DetectError> {
    let detect = || {
        let coverage = detect(&DetectInput::file(photo), target)?;
        crate::develop::coverage::write(photo, &coverage)
            .map_err(|e| DetectError::Failed(e.to_string()))?;
        Ok(coverage)
    };
    #[cfg(target_os = "macos")]
    {
        // Vision autoreleases freely, and the detect worker is long-lived.
        objc2::rc::autoreleasepool(|_| detect())
    }
    #[cfg(not(target_os = "macos"))]
    {
        detect()
    }
}

/// The pure step: [`run`] natively, or a caller with bytes in hand.
pub fn detect(input: &DetectInput, target: DetectTarget) -> Result<Coverage, DetectError> {
    match target {
        DetectTarget::Subject => input.subject().cloned(),
        DetectTarget::Sky => sky_detect::detect(input),
    }
}

/// An upright RGBA8 frame in source UV.
pub struct Rgba8 {
    pub width: u32,
    pub height: u32,
    /// `width * height * 4` bytes.
    pub pixels: Vec<u8>,
}

impl From<DecodedImage> for Rgba8 {
    fn from(img: DecodedImage) -> Self {
        let pixels = match img.pixel_format {
            PixelFormat::Srgb8 => img.rgba,
            PixelFormat::LinearF16 => crate::develop::image_ops::linear_f16_to_srgb8(&img),
        };
        Rgba8 {
            width: img.width,
            height: img.height,
            pixels,
        }
    }
}

/// Where a detector's photo comes from.
pub enum PhotoSource<'a> {
    #[cfg(not(target_arch = "wasm32"))]
    File(&'a Path),
    /// The file's bytes, for a build that can't open paths.
    #[cfg(not(target_os = "macos"))]
    Bytes { bytes: &'a [u8], is_raw: bool },
    /// Pixels already decoded, upright, at any size: the browser's shown
    /// photo, or a test's synthetic frame.
    Pixels(Rgba8),
}

/// What a detector may read. Each accessor computes once and caches, so a
/// sky detector that wants pixels and the subject (to cut people out of the
/// sky) pays for each once, and one that wants neither pays nothing.
pub struct DetectInput<'a> {
    source: PhotoSource<'a>,
    preview: OnceCell<Result<Rgba8, DetectError>>,
    subject: OnceCell<Result<Coverage, DetectError>>,
}

impl<'a> DetectInput<'a> {
    #[cfg(not(target_arch = "wasm32"))]
    pub fn file(path: &'a Path) -> Self {
        Self::from(PhotoSource::File(path))
    }

    #[cfg(not(target_os = "macos"))]
    pub fn bytes(bytes: &'a [u8], is_raw: bool) -> Self {
        Self::from(PhotoSource::Bytes { bytes, is_raw })
    }

    pub fn pixels(frame: Rgba8) -> Self {
        Self::from(PhotoSource::Pixels(frame))
    }

    fn from(source: PhotoSource<'a>) -> Self {
        DetectInput {
            source,
            preview: OnceCell::new(),
            subject: OnceCell::new(),
        }
    }

    /// For platform APIs that decode the file themselves, such as Vision.
    /// `None` when the photo came as bytes.
    pub fn path(&self) -> Option<&Path> {
        match &self.source {
            #[cfg(not(target_arch = "wasm32"))]
            PhotoSource::File(p) => Some(p),
            #[cfg(not(target_os = "macos"))]
            PhotoSource::Bytes { .. } => None,
            PhotoSource::Pixels(_) => None,
        }
    }

    /// The photo upright in source UV, longest side at most
    /// [`PREVIEW_SIDE`], through the platform's normal decode. Pixels given
    /// ready are returned as they are.
    pub fn preview(&self) -> Result<&Rgba8, DetectError> {
        if let PhotoSource::Pixels(frame) = &self.source {
            return Ok(frame);
        }
        self.preview
            .get_or_init(|| self.decode_preview())
            .as_ref()
            .map_err(Clone::clone)
    }

    fn decode_preview(&self) -> Result<Rgba8, DetectError> {
        let img = match &self.source {
            #[cfg(not(target_arch = "wasm32"))]
            PhotoSource::File(p) => crate::decode::image_decode::decode(p, PREVIEW_SIDE),
            #[cfg(not(target_os = "macos"))]
            PhotoSource::Bytes { bytes, is_raw } => {
                if *is_raw {
                    crate::decode::rawler::decode_raw_nonmac_from_bytes(bytes, PREVIEW_SIDE)
                } else {
                    crate::decode::image_decode::decode_nonraw_from_bytes(bytes, PREVIEW_SIDE)
                }
            }
            PhotoSource::Pixels(_) => return Err(DetectError::Failed("no decode needed".into())),
        }
        .map_err(DetectError::Failed)?;
        Ok(Rgba8::from(img))
    }

    /// The subject coverage, for detectors that subtract it. `Unsupported`
    /// off macOS; a sky detector must work without it.
    pub fn subject(&self) -> Result<&Coverage, DetectError> {
        self.subject
            .get_or_init(|| self.detect_subject())
            .as_ref()
            .map_err(Clone::clone)
    }

    #[cfg(target_os = "macos")]
    fn detect_subject(&self) -> Result<Coverage, DetectError> {
        let Some(path) = self.path() else {
            return Err(DetectError::Unsupported);
        };
        if !path.is_file() {
            return Err(DetectError::Failed(format!(
                "{} is not a file",
                path.display()
            )));
        }
        match crate::scoring::segmentation::segment_subject(path).map_err(DetectError::Failed)? {
            Some(mask) if mask.solid_coverage() >= crate::scoring::segmentation::EMPTY_COVERAGE => {
                Coverage::fit(mask.width, mask.height, &mask.alpha)
                    .map_err(|e| DetectError::Failed(e.to_string()))
            }
            _ => Ok(Coverage::nothing()),
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn detect_subject(&self) -> Result<Coverage, DetectError> {
        Err(DetectError::Unsupported)
    }
}

mod sky_detect {
    //! The seam for the sky detector. Its algorithm is `scoring::sky`, a pure
    //! function over the preview; changing it changes nothing outside this
    //! module. Photos keep the coverage they stored; Retry adopts the new
    //! algorithm.

    use super::{Coverage, DetectError, DetectInput};

    pub const AVAILABLE: bool = true;

    /// The heuristic over the preview, with the subject cut out where this
    /// build can find one (people's blue shirts are not sky).
    pub fn detect(input: &DetectInput) -> Result<Coverage, DetectError> {
        let p = input.preview()?;
        let subject = input.subject().ok().map(|c| c.resampled(p.width, p.height));
        let alpha =
            crate::scoring::sky::sky_coverage(&p.pixels, p.width, p.height, subject.as_deref());
        Coverage::fit(p.width, p.height, &alpha).map_err(|e| DetectError::Failed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_follows_the_build() {
        assert_eq!(available(DetectTarget::Sky), !cfg!(target_arch = "wasm32"));
        assert_eq!(available(DetectTarget::Subject), cfg!(target_os = "macos"));
    }

    /// A blue sky over brown ground, through the seam: the sky is covered,
    /// the ground is not, with no file and no platform decode involved.
    #[test]
    fn sky_covers_the_blue_half_of_a_synthetic_frame_and_not_the_ground() {
        let (w, h) = (96u32, 64u32);
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // A faint gradient, since a real sky is never perfectly
                // flat; the ground is textured.
                let px = if y < h / 2 {
                    [120 + (y % 7) as u8, 170 + (x % 5) as u8, 235, 255]
                } else {
                    let n = ((x * 7 + y * 13) % 40) as u8;
                    [90 + n, 70 + n / 2, 40, 255]
                };
                pixels.extend(px);
            }
        }
        let input = DetectInput::pixels(Rgba8 {
            width: w,
            height: h,
            pixels,
        });
        let coverage = detect(&input, DetectTarget::Sky).unwrap();
        assert_eq!((coverage.width(), coverage.height()), (w, h));
        assert!(!coverage.is_nothing());
        let at = |x: u32, y: u32| coverage.alpha()[(y * w + x) as usize];
        assert!(at(w / 2, 4) > 200, "sky: {}", at(w / 2, 4));
        assert!(at(w / 2, h - 5) < 50, "ground: {}", at(w / 2, h - 5));
        assert_eq!(input.path(), None);
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod file_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-detect-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_missing_photo_fails_without_writing_anything() {
        let dir = temp_dir("missing");
        let photo = dir.join("gone.jpg");
        for target in DetectTarget::ALL {
            let err = run(&photo, target).unwrap_err();
            assert!(
                matches!(err, DetectError::Failed(_) | DetectError::Unsupported),
                "{target:?}: {err:?}"
            );
        }
        assert!(!dir.join(".lightphotos").exists());
        assert!(matches!(
            run(&photo, DetectTarget::Sky),
            Err(DetectError::Failed(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Runs real Vision on a flat image with no subject: the answer is
    /// "nothing", stored as a file whose decode matches its id.
    #[cfg(target_os = "macos")]
    #[test]
    fn subject_on_a_subjectless_photo_stores_nothing_and_reads_back() {
        let dir = temp_dir("flat");
        let photo = dir.join("flat.jpg");
        let (w, h) = (96, 64);
        crate::decode::image_encode::encode_jpeg(
            &photo,
            w,
            h,
            &vec![120u8; (w * h * 4) as usize],
            crate::decode::image_encode::JpegQuality::Export,
        )
        .unwrap();
        let coverage = run(&photo, DetectTarget::Subject).unwrap();
        assert!(
            coverage.is_nothing() || coverage.width() > 1,
            "{coverage:?}"
        );
        let back = crate::develop::coverage::read(&photo, coverage.id()).unwrap();
        assert_eq!(back, coverage);
        let input = DetectInput::file(&photo);
        let p = input.preview().unwrap();
        assert_eq!(
            (p.width, p.height, p.pixels.len()),
            (w, h, (w * h * 4) as usize)
        );
        assert_eq!(input.subject().unwrap().id(), coverage.id());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
