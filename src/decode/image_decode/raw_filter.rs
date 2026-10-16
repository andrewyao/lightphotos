// SPDX-License-Identifier: MIT OR Apache-2.0

//! macOS RAW decode through CoreImage's `CIRAWFilter`. It runs the same Apple
//! RAW engine as ImageIO's `CGImageSourceCreateImageAtIndex`, and with Apple's
//! tone curve on the pixels agree to within 8-bit rounding, but it renders on
//! the GPU, demosaics at the requested size and keeps half-float depth. A
//! full-size 24 MP ARW takes about 250 ms here against about 550 ms through
//! ImageIO and a CPU draw.

use std::ptr::NonNull;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2_core_foundation::{CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{kCGColorSpaceExtendedLinearSRGB, CGColorSpace};
use objc2_core_image::{
    kCIContextCacheIntermediates, kCIFormatRGBAh, CIContext, CIImage, CIRAWFilter, CIVector,
};
use objc2_foundation::{ns_string, NSDictionary, NSNumber, NSString, NSURL};

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

/// `image` with negative color components raised to 0 and alpha set to 1.
/// The extended color space keeps out-of-gamut colors as small negatives and
/// CoreImage leaves alpha a hair under 1 in places. The 8-bit render clipped
/// both, and the Develop math assumes neither. Highlights above 1 stay.
fn clamp_to_display_range(image: &CIImage) -> Retained<CIImage> {
    // The largest finite half float, so the upper bound never clips.
    const MAX: CGFloat = 65504.0;
    // SAFETY: class constructors with plain float arguments.
    let (low, high) = unsafe {
        (
            CIVector::vectorWithX_Y_Z_W(0.0, 0.0, 0.0, 1.0),
            CIVector::vectorWithX_Y_Z_W(MAX, MAX, MAX, 1.0),
        )
    };
    let params = NSDictionary::<NSString, AnyObject>::from_slices(
        &[
            ns_string!("inputMinComponents"),
            ns_string!("inputMaxComponents"),
        ],
        &[low.as_ref(), high.as_ref()],
    );
    // SAFETY: `CIColorClamp` is a built-in filter, and `params` maps its two
    // documented vector inputs to `CIVector`s. The getter and the crop act
    // on live images.
    unsafe {
        let clamped =
            image.imageByApplyingFilter_withInputParameters(ns_string!("CIColorClamp"), &params);
        // Forcing alpha to 1 makes the clear plane around the photo opaque,
        // so the result's extent is infinite until cropped back.
        clamped.imageByCroppingToRect(image.extent())
    }
}

/// Decode the RAW at `path` upright, scaled so neither side exceeds
/// `max_dim`, as linear half-float RGBA: `TonedF16` with Apple's tone curve
/// (`apple_curve`), else scene-linear `LinearF16`. `Err` when CoreImage
/// can't read the file or the OS is older than macOS 12; the caller then
/// falls back to ImageIO.
#[hotpath::measure]
pub(super) fn decode(
    path: &std::path::Path,
    max_dim: u32,
    apple_curve: bool,
) -> Result<DecodedImage, String> {
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
        // Boost is Apple's tone curve: 1 is its default look, and 0 leaves
        // the output linear in scene light.
        // SAFETY: a setter on a live filter; 0 and 1 are both in range.
        unsafe { filter.setBoostAmount(if apple_curve { 1.0 } else { 0.0 }) };

        // SAFETY: a getter on a live filter; nil maps to `None`.
        let image =
            unsafe { filter.outputImage() }.ok_or("CoreImage could not develop this RAW")?;
        let image = clamp_to_display_range(&image);
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

        // Extended, so highlights above 1.0 survive for an exposure pull.
        // SAFETY: `kCGColorSpaceExtendedLinearSRGB` is an immutable
        // CoreGraphics constant that lives for the whole process.
        let linear = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceExtendedLinearSRGB }))
            .ok_or("could not create linear sRGB color space")?;
        let row_bytes = w as usize * PixelFormat::TonedF16.bytes_per_pixel();
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
            // call, which renders `w x h` RGBAh pixels into it and returns.
            // The allocator aligns it to at least 16 bytes, more than a half
            // float needs. `kCIFormatRGBAh` is an immutable CoreImage
            // constant.
            unsafe {
                ctx.render_toBitmap_rowBytes_bounds_format_colorSpace(
                    &image,
                    data,
                    row_bytes as isize,
                    bounds,
                    kCIFormatRGBAh,
                    Some(&linear),
                )
            }
        });

        Ok(DecodedImage::new_tracked(DecodedImageFields {
            width: w,
            height: h,
            rgba,
            pixel_format: if apple_curve {
                PixelFormat::TonedF16
            } else {
                PixelFormat::LinearF16
            },
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
        assert!(decode(&empty, u32::MAX, true).is_err());
        assert!(super::super::decode(&empty, u32::MAX).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The RAWs in `LIGHTPHOTOS_TEST_RAWS`, a colon-separated list of
    /// paths with a portrait shot among them, or none when it is unset.
    fn real_raws() -> Vec<std::path::PathBuf> {
        let Ok(list) = std::env::var("LIGHTPHOTOS_TEST_RAWS") else {
            eprintln!("skipping: LIGHTPHOTOS_TEST_RAWS not set");
            return Vec::new();
        };
        list.split(':').map(std::path::PathBuf::from).collect()
    }

    /// With Apple's tone curve, CoreImage and ImageIO give the same upright
    /// picture, at full size and scaled, once the half floats are encoded
    /// to sRGB8 as the Loupe shows them.
    #[test]
    fn matches_imageio_on_real_raws() {
        for path in real_raws() {
            for max_dim in [u32::MAX, 2560] {
                let ci = decode(&path, max_dim, true).unwrap();
                assert_eq!(ci.pixel_format, PixelFormat::TonedF16);
                let io = super::super::decode_imageio(&path, max_dim).unwrap();
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
                let ci = crate::develop::image_ops::to_srgb8(&ci);
                assert_eq!(ci.len(), io.rgba.len());
                let diff: u64 = ci
                    .iter()
                    .zip(&io.rgba)
                    .map(|(a, b)| u64::from(a.abs_diff(*b)))
                    .sum();
                let mean = diff as f64 / ci.len() as f64;
                eprintln!("{}: mean difference {mean:.3}", path.display());
                assert!(mean < 0.1, "{}: mean difference {mean}", path.display());
            }
        }
    }

    /// Without Apple's tone curve the decode is scene-linear: the same size,
    /// and darker in the middle, since Apple's curve lifts the midtones.
    #[test]
    fn without_the_apple_curve_a_real_raw_decodes_scene_linear() {
        let median_green = |img: &DecodedImage| {
            let mut g: Vec<f32> = img
                .rgba
                .as_chunks::<8>()
                .0
                .iter()
                .map(|px| half::f16::from_le_bytes([px[2], px[3]]).to_f32())
                .collect();
            g.sort_by(f32::total_cmp);
            g[g.len() / 2]
        };
        for path in real_raws() {
            let toned = decode(&path, 1024, true).unwrap();
            let linear = decode(&path, 1024, false).unwrap();
            assert_eq!(linear.pixel_format, PixelFormat::LinearF16);
            for img in [&toned, &linear] {
                let px = img.rgba.as_chunks::<8>().0;
                let f = |b: [u8; 2]| half::f16::from_le_bytes(b).to_f32();
                assert!(px.iter().all(|p| f([p[6], p[7]]) == 1.0), "alpha is 1");
                assert!(
                    px.iter()
                        .all(|p| [0, 2, 4].iter().all(|&i| f([p[i], p[i + 1]]) >= 0.0)),
                    "no negative channel"
                );
            }
            assert_eq!((linear.width, linear.height), (toned.width, toned.height));
            let (lin, ton) = (median_green(&linear), median_green(&toned));
            eprintln!(
                "{}: median green {lin:.4} linear, {ton:.4} toned",
                path.display()
            );
            assert!(lin < 0.8 * ton, "{}: {lin} vs {ton}", path.display());
        }
    }
}
