// SPDX-License-Identifier: GPL-3.0-or-later

//! Encode RGBA8 pixels to a JPEG file — the encode counterpart to
//! `image_decode`.
//!
//! macOS: Apple's ImageIO + CoreGraphics, no third-party codecs. Pipeline:
//! build a CGBitmapContext over the pixels (sRGB, byte order R,G,B,A),
//! snapshot a CGImage from it, then hand that to a CGImageDestination pointed
//! at the output file and finalize.
//!
//! Non-mac: `mozjpeg-rs`'s pure-Rust encoder, via its `encode_rgba` entry
//! point (reads RGBA directly, ignores alpha — no separate RGB conversion
//! buffer needed).

use std::path::Path;

#[cfg(target_os = "macos")]
use std::ffi::c_void;

#[cfg(target_os = "macos")]
use objc2_core_foundation::CFString;
#[cfg(target_os = "macos")]
use objc2_image_io::CGImageDestination;

#[cfg(target_os = "macos")]
use crate::coregraphics;

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB; alpha may be opaque or
/// premultiplied — export produces opaque) to a JPEG at `out`.
#[cfg(target_os = "macos")]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    // The context reads from `buffer` while it lives; CreateImage snapshots the
    // pixels into an independent CGImage, so `buffer` can drop afterwards.
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
    // SAFETY: url/type are valid; count 1; default options (ImageIO's default
    // JPEG quality). The destination is +1 retained and released on drop.
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

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB; alpha may be opaque or
/// premultiplied — export produces opaque) to a JPEG at `out`, via
/// `mozjpeg-rs`.
///
/// Quality 90 (mozjpeg-rs's own default preset quality is 75, tuned for
/// general-purpose web images) — chosen to sit closer to the mac arm's
/// ImageIO default, which favors fidelity for a photo-editing tool's export
/// path over file size.
#[cfg(not(target_os = "macos"))]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    let jpeg_data = mozjpeg_rs::Encoder::new(mozjpeg_rs::Preset::default())
        .quality(90)
        .encode_rgba(rgba, width, height)
        .map_err(|e| e.to_string())?;

    std::fs::write(out, jpeg_data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_decode;

    /// End-to-end through the real ImageIO encoder: write a solid-red JPEG,
    /// decode it back, and confirm dimensions and (approximately) the color.
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
        // JPEG is lossy, so allow a generous tolerance; just confirm it's a
        // predominantly-red image, not black/garbage. Decode is premultiplied
        // sRGB8, alpha 255 so RGB is straight.
        let (r, g, b) = (decoded.rgba[0], decoded.rgba[1], decoded.rgba[2]);
        assert!(r > 150, "red channel should be high, got {r}");
        assert!(g < 100 && b < 100, "green/blue should be low, got {g},{b}");

        std::fs::remove_file(&out).ok();
    }
}
