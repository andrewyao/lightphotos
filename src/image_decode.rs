//! Decode any image macOS understands (JPEG/PNG/GIF/TIFF/BMP/HEIC/RAW) to
//! RGBA8 bytes using Apple's ImageIO + CoreGraphics. No third-party codecs.
//!
//! Pipeline: CFURL -> CGImageSource -> CGImage -> draw into a CGBitmapContext
//! backed by our own buffer (sRGB, premultiplied RGBA, big-endian byte order),
//! then read the buffer back.

use std::ffi::c_void;
use std::path::Path;

use objc2_core_foundation::{
    CFNumber, CFNumberType, CFRetained, CFString, CFURL, CFURLPathStyle, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
    CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_image_io::{kCGImagePropertyOrientation, CGImageSource};

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
/// Open `path` as a `CGImageSource` (the shared CFURL + ImageIO open path used
/// by both full-resolution decode and thumbnail generation).
pub fn open_image_source(path: &Path) -> Result<CFRetained<CGImageSource>, String> {
    let path_str = path.to_string_lossy();
    let cf_path = CFString::from_str(&path_str);
    let url = CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or("could not build CFURL")?;

    // SAFETY: url is a valid CFURL; passing no decode options. The returned
    // CGImageSource is +1 retained and wrapped in CFRetained, released on drop.
    unsafe { CGImageSource::with_url(&url, None) }.ok_or_else(|| "ImageIO could not open file".into())
}

pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    let source = open_image_source(path)?;

    let image: CFRetained<CGImage> = unsafe { source.image_at_index(0, None) }
        .ok_or("ImageIO could not decode image")?;

    let src_w = CGImage::width(Some(&image)) as u32;
    let src_h = CGImage::height(Some(&image)) as u32;
    if src_w == 0 || src_h == 0 {
        return Err("decoded image has zero dimension".into());
    }

    // Downscale to fit max_dim while preserving aspect ratio.
    let (w, h) = fit_within(src_w, src_h, max_dim);

    let decoded = cgimage_to_rgba(&image, w, h)?;
    // `image_at_index` returns raw pixels; apply the file's EXIF orientation so
    // the full decode matches the thumbnails (which orient via ImageIO's
    // WithTransform). Loupe, crop, and export all consume `decode()`, so this
    // keeps every downstream view upright and consistent.
    Ok(apply_exif_orientation(decoded, read_orientation(&source)))
}

/// The image's EXIF orientation tag (`1..=8`), or `1` when absent/unreadable.
/// Never panics — any missing property yields the identity orientation.
fn read_orientation(source: &CGImageSource) -> u8 {
    // SAFETY: index 0 exists (we already decoded it); no options passed. The
    // returned dictionary is +1 retained and released on drop.
    let Some(props) = (unsafe { source.properties_at_index(0, None) }) else {
        return 1;
    };
    // SAFETY: the orientation key is a valid CFString option key; `value` returns
    // a borrowed (non-owned) pointer to the CFNumber, or null if absent.
    let ptr = unsafe { props.value(kCGImagePropertyOrientation as *const CFString as *const c_void) };
    if ptr.is_null() {
        return 1;
    }
    // SAFETY: for the orientation key the value is a CFNumber; read it as SInt32.
    let number = unsafe { &*(ptr as *const CFNumber) };
    let mut out: i32 = 0;
    let ok = unsafe {
        number.value(CFNumberType::SInt32Type, &mut out as *mut i32 as *mut c_void)
    };
    if ok && (1..=8).contains(&out) {
        out as u8
    } else {
        1
    }
}

/// Reorient tightly-packed RGBA8 pixels per an EXIF orientation (`1..=8`),
/// returning the corrected image. Orientation `1` is returned untouched (fast
/// path). Cases `5..=8` swap width/height.
///
/// Mapping is `out(xo, yo) = in(xs, ys)`; see the EXIF orientation table. Shares
/// its shape with `app::rotate_rgba`, extended to cover the mirrored cases.
fn apply_exif_orientation(img: DecodedImage, orientation: u8) -> DecodedImage {
    if orientation <= 1 {
        return img;
    }
    let (w, h) = (img.width, img.height);
    let swaps = matches!(orientation, 5 | 6 | 7 | 8);
    let (nw, nh) = if swaps { (h, w) } else { (w, h) };
    let mut dst = vec![0u8; (nw * nh * 4) as usize];
    let src_idx = |x: u32, y: u32| ((y * w + x) * 4) as usize;
    for yo in 0..nh {
        for xo in 0..nw {
            let (xs, ys) = match orientation {
                2 => (w - 1 - xo, yo),         // mirror horizontal
                3 => (w - 1 - xo, h - 1 - yo), // rotate 180
                4 => (xo, h - 1 - yo),         // mirror vertical
                5 => (yo, xo),                 // transpose
                6 => (yo, h - 1 - xo),         // rotate 90° CW
                7 => (w - 1 - yo, h - 1 - xo), // transverse
                _ => (w - 1 - yo, xo),         // 8: rotate 270° CW
            };
            let s = src_idx(xs, ys);
            let d = ((yo * nw + xo) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&img.rgba[s..s + 4]);
        }
    }
    DecodedImage { width: nw, height: nh, rgba: dst }
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

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn orientation_1_is_identity() {
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 1);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(10));
        assert_eq!(&out.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_6_rotates_90cw_and_swaps_dims() {
        // A,B side by side (w=2,h=1). Rotate 90° CW → 1×2 column A over B.
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 6);
        assert_eq!((out.width, out.height), (1, 2));
        assert_eq!(&out.rgba[0..4], &px(10)); // top
        assert_eq!(&out.rgba[4..8], &px(20)); // bottom
    }

    #[test]
    fn orientation_8_rotates_270cw() {
        // 90° CW then 90° CCW must return to the original layout.
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let cw = apply_exif_orientation(img, 6); // 1×2 [10; 20]
        let back = apply_exif_orientation(cw, 8); // rot270 CW → back to 2×1 [10,20]
        assert_eq!((back.width, back.height), (2, 1));
        assert_eq!(&back.rgba[0..4], &px(10));
        assert_eq!(&back.rgba[4..8], &px(20));
    }

    #[test]
    fn orientation_2_mirrors_horizontally_keeping_dims() {
        let img = DecodedImage { width: 2, height: 1, rgba: [px(10), px(20)].concat() };
        let out = apply_exif_orientation(img, 2);
        assert_eq!((out.width, out.height), (2, 1));
        assert_eq!(&out.rgba[0..4], &px(20)); // columns swapped
        assert_eq!(&out.rgba[4..8], &px(10));
    }
}
