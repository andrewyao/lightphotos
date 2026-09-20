// SPDX-License-Identifier: GPL-3.0-or-later

//! Encode RGBA8 pixels to JPEG. macOS uses ImageIO through a CoreGraphics
//! bitmap context. Other targets use the pure-Rust `mozjpeg-rs` encoder.
//! Export and the on-disk thumbnail cache both write through here.

use std::path::Path;

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CFString;
#[cfg(target_os = "macos")]
use objc2_image_io::CGImageDestination;

#[cfg(target_os = "macos")]
use crate::coregraphics;

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB, alpha opaque or
/// premultiplied) to a JPEG at `out`.
#[cfg(target_os = "macos")]
#[hotpath::measure]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    // `bitmap_context_image` copies the pixels, so `buffer` may drop after it.
    let mut buffer = rgba.to_vec();
    // SAFETY: buffer is width*height*4 bytes and outlives `ctx`.
    let ctx = unsafe {
        coregraphics::srgb_bitmap_context(
            buffer.as_mut_ptr() as *mut c_void,
            width,
            height,
            bytes_per_row,
        )?
    };

    let image = coregraphics::bitmap_context_image(&ctx)?;

    let url = coregraphics::file_url(out)?;

    let jpeg_uti = CFString::from_str("public.jpeg");
    // SAFETY: url and type are valid. No options, so ImageIO uses its default
    // JPEG quality.
    let dest = unsafe { CGImageDestination::with_url(&url, &jpeg_uti, 1, None) }
        .ok_or("could not create image destination (unwritable path?)")?;

    // SAFETY: dest/image are valid; no per-image properties.
    unsafe { CGImageDestination::add_image(&dest, &image, None) };
    // SAFETY: dest is valid; returns false if the file could not be written.
    let ok = unsafe { CGImageDestination::finalize(&dest) };
    if !ok {
        return Err("CGImageDestinationFinalize failed (could not write file)".into());
    }
    Ok(())
}

/// Encode `rgba` (same pixel contract as [`encode_jpeg`]) to JPEG bytes with
/// `mozjpeg-rs`. The wasm32 export path uses this because it has no `std::fs`.
///
/// Quality is 90, not mozjpeg's default 75, to stay close to ImageIO's
/// default on macOS. Built on macOS only under `raw-probe`, for its tests.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn encode_jpeg_to_vec(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::default())
        .quality(90)
        .encode_rgba(rgba, width, height)
        .map_err(|e| e.to_string())
}

/// Encode `rgba` to a JPEG file at `out` with `mozjpeg-rs`. The wasm32 build
/// has no caller, hence `dead_code`.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
#[hotpath::measure]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let jpeg_data = encode_jpeg_to_vec(width, height, rgba)?;
    std::fs::write(out, jpeg_data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_decode;

    /// Encode solid red, decode the bytes back, and check that the size and
    /// the approximate color survive.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_round_trips() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[220, 30, 30, 255]);
        }

        let jpeg = encode_jpeg_to_vec(w, h, &rgba).expect("encode should succeed");
        assert!(!jpeg.is_empty(), "should have produced JPEG bytes");

        let path = std::env::temp_dir().join(format!(
            "lightphotos-encode-vec-test-{}.jpg",
            std::process::id()
        ));
        std::fs::write(&path, jpeg).expect("write encoded JPEG should succeed");
        let decoded = image_decode::decode(&path, u32::MAX).expect("re-decode should succeed");
        assert_eq!((decoded.width, decoded.height), (w, h));
        let px = &decoded.rgba[..4];
        assert!(px[0] > 150, "red channel should be high, got {}", px[0]);
        assert!(
            px[1] < 100 && px[2] < 100,
            "green/blue should be low, got {},{}",
            px[1],
            px[2]
        );
        std::fs::remove_file(path).ok();
    }

    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_zero_size() {
        assert!(encode_jpeg_to_vec(0, 4, &[]).is_err());
        assert!(encode_jpeg_to_vec(4, 0, &[]).is_err());
    }

    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_short_buffer() {
        assert!(encode_jpeg_to_vec(4, 4, &[0u8; 16]).is_err());
    }

    /// Write solid red through the platform encoder, decode it back, and check
    /// the size and the approximate color.
    #[test]
    fn encode_then_decode_round_trips() {
        let (w, h) = (8u32, 6u32);
        let mut rgba = Vec::with_capacity((w * h * 4) as usize);
        for _ in 0..w * h {
            rgba.extend_from_slice(&[220, 30, 30, 255]);
        }

        let out = std::env::temp_dir().join(format!("iv-encode-test-{}.jpg", std::process::id()));
        encode_jpeg(&out, w, h, &rgba).expect("encode should succeed");
        assert!(out.exists(), "jpeg file should have been written");

        let decoded = image_decode::decode(&out, 16384).expect("re-decode should succeed");
        assert_eq!((decoded.width, decoded.height), (w, h));
        // JPEG is lossy, so only check that the result is mostly red.
        let (r, g, b) = (decoded.rgba[0], decoded.rgba[1], decoded.rgba[2]);
        assert!(r > 150, "red channel should be high, got {r}");
        assert!(g < 100 && b < 100, "green/blue should be low, got {g},{b}");

        std::fs::remove_file(&out).ok();
    }
}
