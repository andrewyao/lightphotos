//! Decode any image macOS understands (JPEG/PNG/GIF/TIFF/BMP/HEIC/RAW) to
//! RGBA8 bytes using Apple's ImageIO + CoreGraphics. No third-party codecs.
//!
//! Pipeline: CFURL -> CGImageSource -> CGImage -> draw into a CGBitmapContext
//! backed by our own buffer (sRGB, premultiplied RGBA, big-endian byte order),
//! then read the buffer back.

use std::ffi::c_void;
use std::path::Path;

use objc2_core_foundation::{CFRetained, CFString, CFURL, CFURLPathStyle, CGRect, CGPoint, CGSize};
use objc2_core_graphics::{
    CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_image_io::CGImageSource;

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    /// Tightly packed RGBA8, row-major, premultiplied alpha.
    pub rgba: Vec<u8>,
}

// The classic CGBitmapContextCreate is not exposed by objc2-core-graphics 0.3
// (only a block-based "Adaptive" variant). It is a stable CoreGraphics symbol,
// and the framework is already linked by objc2-core-graphics, so we declare it.
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
}

/// Decode `path`, optionally downscaling so neither side exceeds `max_dim`
/// (so images larger than the GPU's max texture size still display).
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let path_str = path.to_string_lossy();
    let cf_path = CFString::from_str(&path_str);
    let url = CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or("could not build CFURL")?;

    // SAFETY: url is a valid CFURL; passing no decode options.
    let source = unsafe { CGImageSource::with_url(&url, None) }
        .ok_or("ImageIO could not open file")?;

    let image: CFRetained<CGImage> = unsafe { source.image_at_index(0, None) }
        .ok_or("ImageIO could not decode image")?;

    let src_w = CGImage::width(Some(&image)) as u32;
    let src_h = CGImage::height(Some(&image)) as u32;
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }

    // Downscale to fit max_dim while preserving aspect ratio.
    let (w, h) = fit_within(src_w, src_h, max_dim);

    cgimage_to_rgba(&image, w, h)
}

/// Draw a `CGImage` into a freshly-allocated sRGB bitmap context sized
/// `(target_w, target_h)` and read back the result as tightly-packed,
/// premultiplied RGBA8 (byte order R,G,B,A — matches `Rgba8UnormSrgb`).
///
/// Scales the image into the target rect, so callers can use this both for a
/// full-size decode and for a thumbnail (passing the thumbnail's own size).
pub fn cgimage_to_rgba(
    image: &CGImage,
    target_w: u32,
    target_h: u32,
) -> Result<DecodedImage, String> {
    if target_w == 0 || target_h == 0 {
        return Err("target dimensions must be non-zero".into());
    }

    let bytes_per_row = (target_w as usize) * 4;
    let mut buffer = vec![0u8; bytes_per_row * (target_h as usize)];

    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or("could not create sRGB color space")?;

    // premultiplied RGBA, big-endian => byte order R,G,B,A (matches Rgba8UnormSrgb).
    let bitmap_info: u32 =
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;

    // SAFETY: buffer is large enough (target_w*target_h*4); color_space is valid for its scope.
    let ctx_ptr = unsafe {
        CGBitmapContextCreate(
            buffer.as_mut_ptr() as *mut c_void,
            target_w as usize,
            target_h as usize,
            8,
            bytes_per_row,
            &*color_space as *const CGColorSpace,
            bitmap_info,
        )
    };
    if ctx_ptr.is_null() {
        return Err("CGBitmapContextCreate failed".into());
    }
    // Take ownership so the context is released on drop.
    // SAFETY: ctx_ptr is non-null (checked above) and is a +1 retained context.
    let ctx: CFRetained<CGContext> =
        unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ctx_ptr)) };

    // Draw the image scaled into our (possibly smaller) context rect.
    let rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize { width: target_w as f64, height: target_h as f64 },
    };
    CGContext::draw_image(Some(&ctx), rect, Some(image));

    Ok(DecodedImage { width: target_w, height: target_h, rgba: buffer })
}

fn fit_within(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if w <= max_dim && h <= max_dim {
        return (w, h);
    }
    let scale = (max_dim as f64 / w as f64).min(max_dim as f64 / h as f64);
    let nw = ((w as f64 * scale).floor() as u32).max(1).min(max_dim);
    let nh = ((h as f64 * scale).floor() as u32).max(1).min(max_dim);
    (nw, nh)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// End-to-end decode through ImageIO + our CGBitmapContext path. Requires a
    /// test image at /tmp/iv-test/a.png (created by the dev workflow). Skipped
    /// if absent so the suite still passes on CI without it.
    #[test]
    fn decodes_known_image_to_nonblank_rgba() {
        let path = Path::new("/tmp/iv-test/a.png");
        if !path.exists() {
            eprintln!("skipping: {} not present", path.display());
            return;
        }
        let img = decode(path, 16384).expect("decode should succeed");
        assert!(img.width > 0 && img.height > 0, "non-zero dimensions");
        assert_eq!(img.rgba.len(), (img.width * img.height * 4) as usize);
        // The fixture is a solid color, so the alpha channel must be opaque and
        // the RGB must not be all-zero (which would mean nothing was drawn).
        let any_color = img.rgba.chunks_exact(4).any(|p| p[0] | p[1] | p[2] != 0);
        let opaque = img.rgba.chunks_exact(4).all(|p| p[3] == 255);
        assert!(any_color, "decoded pixels are all black -> draw failed");
        assert!(opaque, "expected opaque alpha for a solid-color image");
    }
}
