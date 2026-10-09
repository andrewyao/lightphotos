// SPDX-License-Identifier: MIT OR Apache-2.0

//! macOS RAW decode through CoreImage's `CIRAWFilter`. It runs the same Apple
//! RAW engine as ImageIO's `CGImageSourceCreateImageAtIndex`, and the pixels
//! agree to within 8-bit rounding, but it renders on the GPU and demosaics at
//! the requested size. A full-size 24 MP ARW takes about 200 ms here against
//! about 550 ms through ImageIO and a CPU draw.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_core_graphics::{kCGColorSpaceSRGB, CGColorSpace};
use objc2_core_image::{kCIContextCacheIntermediates, kCIFormatRGBA8, CIContext, CIRAWFilter};
use objc2_foundation::{NSDictionary, NSNumber, NSString, NSURL};

use super::{check_decode_size, fit_within, DecodedImage, DecodedImageFields, PixelFormat};

thread_local! {
    /// A `CIContext` is costly to create, so each decode thread keeps one.
    static CONTEXT: Retained<CIContext> = new_context();
}

fn new_context() -> Retained<CIContext> {
    // Each RAW is decoded once and then cached by the loader, so CoreImage's
    // own intermediate cache would only hold memory.
    // SAFETY: `kCIContextCacheIntermediates` is an immutable CoreImage
    // constant that lives for the whole process.
    let key: &NSString = unsafe { kCIContextCacheIntermediates };
    let no = NSNumber::new_bool(false);
    let options = NSDictionary::<NSString, AnyObject>::from_slices(&[key], &[no.as_ref()]);
    // SAFETY: `options` maps a documented context key to an NSNumber.
    unsafe { CIContext::contextWithOptions(Some(&options)) }
}

/// Decode the RAW at `path` upright, scaled so neither side exceeds
/// `max_dim`, as sRGB RGBA8. `Err` when CoreImage can't read the file or
/// the OS is older than macOS 12; the caller then falls back to ImageIO.
#[hotpath::measure]
pub(super) fn decode(path: &std::path::Path, max_dim: u32) -> Result<DecodedImage, String> {
    if AnyClass::get(c"CIRAWFilter").is_none() {
        return Err("CIRAWFilter needs macOS 12 or later".into());
    }
    let url = NSURL::from_file_path(path).ok_or("could not build NSURL")?;
    // Worker threads have no autorelease pool, and CoreImage autoreleases
    // freely; without one each photo would leak until exit.
    objc2::rc::autoreleasepool(|_| {
        // SAFETY: the class exists (checked above); a file CoreImage can't
        // read returns nil, which maps to `None`.
        let filter = unsafe { CIRAWFilter::filterWithImageURL(&url) }
            .ok_or("CoreImage can't read this RAW")?;

        // SAFETY: a getter on a live filter.
        let native = unsafe { filter.nativeSize() };
        let long = native.width.max(native.height);
        if !(long.is_finite() && long >= 1.0) {
            return Err("RAW has no size".into());
        }
        let scale = f64::from(max_dim) / long;
        if scale < 1.0 {
            // SAFETY: a setter on a live filter; 0 < scale < 1.
            unsafe { filter.setScaleFactor(scale as f32) };
        }

        // SAFETY: a getter on a live filter; nil maps to `None`.
        let image =
            unsafe { filter.outputImage() }.ok_or("CoreImage could not develop this RAW")?;
        // SAFETY: a getter on a live image.
        let extent = unsafe { image.extent() };
        let (x, y) = (extent.origin.x.floor(), extent.origin.y.floor());
        if !(extent.size.width >= 1.0 && extent.size.height >= 1.0) {
            return Err("developed RAW has no size".into());
        }
        // CoreImage rounds a scaled extent up to whole pixels. Size the
        // output by `fit_within` instead, as ImageIO's decode does, so both
        // agree; the dropped edge row was only partly covered.
        let (nw, nh) = (native.width as u32, native.height as u32);
        let upright = (extent.size.width >= extent.size.height) == (nw >= nh);
        let (w, h) = if upright {
            fit_within(nw, nh, max_dim)
        } else {
            fit_within(nh, nw, max_dim)
        };
        let (w, h) = (
            w.min(extent.size.width as u32),
            h.min(extent.size.height as u32),
        );
        check_decode_size(w, h)?;

        // SAFETY: `kCGColorSpaceSRGB` is an immutable CoreGraphics constant
        // that lives for the whole process.
        let srgb = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
            .ok_or("could not create sRGB color space")?;
        let row_bytes = w as usize * 4;
        let mut rgba = vec![0u8; row_bytes * h as usize];
        let bounds = CGRect {
            origin: CGPoint { x, y },
            size: CGSize {
                width: f64::from(w),
                height: f64::from(h),
            },
        };
        let data = NonNull::new(rgba.as_mut_ptr().cast()).ok_or("no pixel buffer")?;
        CONTEXT.with(|ctx| {
            // SAFETY: `rgba` holds `row_bytes * h` bytes and outlives the
            // call, which renders `w x h` RGBA8 pixels into it and returns.
            // `kCIFormatRGBA8` is an immutable CoreImage constant.
            unsafe {
                ctx.render_toBitmap_rowBytes_bounds_format_colorSpace(
                    &image,
                    data,
                    row_bytes as isize,
                    bounds,
                    kCIFormatRGBA8,
                    Some(&srgb),
                )
            }
        });

        Ok(DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: PixelFormat::Srgb8,
        }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file named like a RAW that isn't one still decodes, through
    /// ImageIO if CoreImage refuses it, and an empty one fails cleanly.
    #[test]
    fn a_jpeg_named_arw_decodes_and_an_empty_one_errs() {
        let dir = std::env::temp_dir().join(format!("raw-filter-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("fake.arw");
        let pixels = vec![200u8; 64 * 48 * 4];
        crate::decode::image_encode::encode_jpeg(
            &fake,
            64,
            48,
            &pixels,
            crate::decode::image_encode::JpegQuality::Export,
        )
        .unwrap();
        let img = super::super::decode(&fake, u32::MAX).unwrap();
        assert_eq!((img.width, img.height), (64, 48));

        let empty = dir.join("empty.arw");
        std::fs::write(&empty, b"").unwrap();
        assert!(decode(&empty, u32::MAX).is_err());
        assert!(super::super::decode(&empty, u32::MAX).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// CoreImage and ImageIO give the same upright picture, at full size and
    /// scaled. Needs real RAWs: set `LIGHTPHOTOS_TEST_RAWS` to a
    /// colon-separated list of paths, with a portrait shot among them.
    #[test]
    fn matches_imageio_on_real_raws() {
        let Ok(list) = std::env::var("LIGHTPHOTOS_TEST_RAWS") else {
            eprintln!("skipping: LIGHTPHOTOS_TEST_RAWS not set");
            return;
        };
        for path in list.split(':').map(std::path::Path::new) {
            for max_dim in [u32::MAX, 2560] {
                let ci = decode(path, max_dim).unwrap();
                let io = super::super::decode_imageio(path, max_dim).unwrap();
                assert_eq!(
                    (ci.width, ci.height),
                    (io.width, io.height),
                    "{} at {max_dim}",
                    path.display()
                );
                if max_dim != u32::MAX {
                    // The two scale with different filters; size is the check.
                    continue;
                }
                let diff: u64 = ci
                    .rgba
                    .iter()
                    .zip(&io.rgba)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum();
                let mean = diff as f64 / ci.rgba.len() as f64;
                eprintln!("{}: mean difference {mean:.3}", path.display());
                assert!(mean < 0.1, "{}: mean difference {mean}", path.display());
            }
        }
    }
}
