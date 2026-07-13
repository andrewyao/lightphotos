//! Encode RGBA8 pixels to a JPEG file using Apple's ImageIO + CoreGraphics —
//! the encode counterpart to `image_decode`. No third-party codecs.
//!
//! Pipeline: build a CGBitmapContext over the pixels (sRGB, byte order R,G,B,A),
//! snapshot a CGImage from it, then hand that to a CGImageDestination pointed at
//! the output file and finalize.

use std::ffi::c_void;
use std::path::Path;

use objc2_core_foundation::{CFRetained, CFString, CFURL, CFURLPathStyle};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_graphics::{CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo};
use objc2_image_io::CGImageDestination;

// CGBitmapContextCreate / …CreateImage are the classic (non-block) CoreGraphics
// symbols not surfaced by objc2-core-graphics 0.3. They are stable and the
// framework is already linked, so we declare them — same pattern as
// `image_decode::CGBitmapContextCreate`.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGBitmapContextCreate(
        data: *mut c_void,
        width: usize,
        height: usize,
        bits_per_component: usize,
        bytes_per_row: usize,
        space: *const CGColorSpace,
        bitmap_info: u32,
    ) -> *mut CGContext;
    fn CGBitmapContextCreateImage(ctx: *const CGContext) -> *mut CGImage;
}

/// Encode `rgba` (tightly packed RGBA8, row-major, sRGB; alpha may be opaque or
/// premultiplied — export produces opaque) to a JPEG at `out`.
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err("cannot encode a zero-sized image".into());
    }
    let bytes_per_row = width as usize * 4;
    if rgba.len() < bytes_per_row * height as usize {
        return Err("pixel buffer too small for the given dimensions".into());
    }

    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or("could not create sRGB color space")?;
    // Byte order R,G,B,A, matching the decode path.
    let bitmap_info: u32 =
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;

    // The context reads from `buffer` while it lives; CreateImage snapshots the
    // pixels into an independent CGImage, so `buffer` can drop afterwards.
    let mut buffer = rgba.to_vec();
    // SAFETY: buffer is width*height*4 bytes; color_space is valid for the call.
    let ctx_ptr = unsafe {
        CGBitmapContextCreate(
            buffer.as_mut_ptr() as *mut c_void,
            width as usize,
            height as usize,
            8,
            bytes_per_row,
            &*color_space as *const CGColorSpace,
            bitmap_info,
        )
    };
    if ctx_ptr.is_null() {
        return Err("CGBitmapContextCreate failed".into());
    }
    // SAFETY: non-null (checked), +1 retained → released on drop.
    let ctx: CFRetained<CGContext> =
        unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ctx_ptr)) };

    // SAFETY: ctx is a valid bitmap context.
    let img_ptr = unsafe { CGBitmapContextCreateImage(&*ctx as *const CGContext) };
    if img_ptr.is_null() {
        return Err("CGBitmapContextCreateImage failed".into());
    }
    // SAFETY: non-null (checked), +1 retained → released on drop.
    let image: CFRetained<CGImage> =
        unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(img_ptr)) };

    // Output CFURL (same construction as image_decode::open_image_source).
    let path_str = out.to_string_lossy();
    let cf_path = CFString::from_str(&path_str);
    let url = CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or("could not build output CFURL")?;

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
