// SPDX-License-Identifier: MIT OR Apache-2.0
//! Photo scores: the technical quality score, the judge's penalty curves, face
//! and blink quality, subject masks, and the Apple Vision requests behind them.

pub mod detect;
pub mod facequality;
pub mod judge;
pub mod quality;
// Built on macOS only for its tests, which need the `rustface` dev-dependency.
#[cfg(any(not(target_os = "macos"), test))]
pub mod seetaface;
pub mod segmentation;
pub mod sky;
#[cfg(target_os = "macos")]
pub mod vision;

#[cfg(test)]
mod test_support {
    use crate::decode::image_encode::{encode_jpeg, JpegQuality};

    /// Writes a fixture under a per-process directory, so two `cargo test` runs
    /// on one machine can't truncate each other's file mid-Vision-request.
    pub(super) fn write_jpeg(name: &str, w: u32, h: u32, rgba: &[u8]) -> std::path::PathBuf {
        let dir = fixture_dir();
        let path = dir.join(name);
        encode_jpeg(&path, w, h, rgba, JpegQuality::Export).expect("encode fixture jpeg");
        path
    }

    fn fixture_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lp-vision-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        dir
    }

    /// Like [`write_jpeg`], but stamps the JPEG with EXIF `orientation`
    /// (`1..=8`) without rotating the stored pixels, the way a camera records a
    /// turned shot. Vision reads this tag off the file, so a test can prove the
    /// upright orientation it hands back. macOS writes the tag through ImageIO.
    #[cfg(target_os = "macos")]
    pub(super) fn write_jpeg_oriented(
        name: &str,
        w: u32,
        h: u32,
        rgba: &[u8],
        orientation: u8,
    ) -> std::path::PathBuf {
        use crate::decode::coregraphics;
        use objc2_core_foundation::{
            kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary, CFNumber,
            CFNumberType, CFString,
        };
        use objc2_image_io::{kCGImagePropertyOrientation, CGImageDestination};
        use std::ffi::c_void;

        let path = fixture_dir().join(name);
        let bytes_per_row = w as usize * 4;
        let mut buffer = rgba.to_vec();
        // SAFETY: buffer holds w*h*4 bytes and outlives the context.
        let ctx = unsafe {
            coregraphics::srgb_bitmap_context(
                buffer.as_mut_ptr() as *mut c_void,
                w,
                h,
                bytes_per_row,
            )
        }
        .expect("bitmap context");
        let image = coregraphics::bitmap_context_image(&ctx).expect("cg image");
        let url = coregraphics::file_url(&path).expect("file url");

        let jpeg_uti = CFString::from_str("public.jpeg");
        // SAFETY: url and type are valid; one image is written.
        let dest = unsafe { CGImageDestination::with_url(&url, &jpeg_uti, 1, None) }
            .expect("image destination");

        let value = orientation as i64;
        // SAFETY: value_ptr points at a valid i64 matching SInt64Type.
        let number = unsafe {
            CFNumber::new(
                None,
                CFNumberType::SInt64Type,
                &value as *const i64 as *const c_void,
            )
        }
        .expect("orientation number");
        // SAFETY: the static key is a valid CFString at runtime.
        let mut keys: [*const c_void; 1] =
            [unsafe { kCGImagePropertyOrientation } as *const CFString as *const c_void];
        let mut values: [*const c_void; 1] = [&*number as *const CFNumber as *const c_void];
        // SAFETY: keys and values each hold one valid CFType pointer; the
        // callbacks retain entries, so `number` may drop afterwards.
        let props = unsafe {
            CFDictionary::new(
                None,
                keys.as_mut_ptr(),
                values.as_mut_ptr(),
                keys.len() as isize,
                &kCFTypeDictionaryKeyCallBacks,
                &kCFTypeDictionaryValueCallBacks,
            )
        }
        .expect("orientation properties");

        // SAFETY: dest/image are valid; props maps a documented image property
        // key to a CFNumber.
        unsafe { CGImageDestination::add_image(&dest, &image, Some(props.as_ref())) };
        // SAFETY: dest is valid; returns false if the file could not be written.
        let ok = unsafe { CGImageDestination::finalize(&dest) };
        assert!(ok, "could not write oriented fixture jpeg");
        path
    }
}
