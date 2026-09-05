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
//!
//! ## Pipeline position
//! - Last stage of Pipeline 3 (export) only.
//! - `export.rs`'s `do_export` calls `encode_jpeg` once the full-resolution
//!   decode has been baked (`image_ops::bake_edited`) into final pixels.
//! - Never called from Pipeline 1 or 2 — the Loupe and Grid only ever
//!   *decode*, they don't write files.
//! - Not reachable on wasm32 — export is a stub there (see `export.rs`).

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
/// premultiplied — export produces opaque) to JPEG bytes, via `mozjpeg-rs`.
/// The file-less half of [`encode_jpeg`] below — wasm32's export path
/// (`export::bake_jpeg`) shares this exact encoder, then hands the bytes to a
/// File System Access writable stream instead of `std::fs`.
///
/// Quality 90 (mozjpeg-rs's own default preset quality is 75, tuned for
/// general-purpose web images) — chosen to sit closer to the mac arm's
/// ImageIO default, which favors fidelity for a photo-editing tool's export
/// path over file size.
///
/// Reachable on mac under `raw-probe` (like the non-mac decode paths) so its
/// round-trip test can run there; the mac `encode_jpeg` above still uses
/// ImageIO and never calls this.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
// On mac under `raw-probe` only the round-trip test calls this (the mac
// `encode_jpeg` uses ImageIO); non-mac wires it into `encode_jpeg` below.
#[allow(dead_code)]
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

/// Encode `rgba` (see [`encode_jpeg_to_vec`] for the pixel contract) to a JPEG
/// file at `out`, via `mozjpeg-rs`. Since export moved to
/// `encode_jpeg_to_vec` + `ExportFs::write_atomic`, the only callers left are
/// `seg_probe` and the fixture setup in several `#[cfg(test)]` modules —
/// hence `dead_code` in a plain non-mac `--bin lightphotos` build.
#[cfg(not(target_os = "macos"))]
#[allow(dead_code)]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    let jpeg_data = encode_jpeg_to_vec(width, height, rgba)?;
    std::fs::write(out, jpeg_data).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_decode;

    /// `encode_jpeg_to_vec` is the file-less half of the non-mac encoder,
    /// shared with the wasm32 export path (which has no `std::fs` to write
    /// to). Encode a solid-red image, decode the returned bytes back, and
    /// confirm dimensions plus (approximately) the colour survive the JPEG
    /// round trip.
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

    /// Zero-sized input is rejected, not silently encoded to garbage.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_zero_size() {
        assert!(encode_jpeg_to_vec(0, 4, &[]).is_err());
        assert!(encode_jpeg_to_vec(4, 0, &[]).is_err());
    }

    /// A pixel buffer shorter than `width * height * 4` is rejected.
    #[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
    #[test]
    fn encode_jpeg_to_vec_rejects_short_buffer() {
        assert!(encode_jpeg_to_vec(4, 4, &[0u8; 16]).is_err());
    }

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
