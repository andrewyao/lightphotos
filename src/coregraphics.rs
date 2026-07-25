// SPDX-License-Identifier: MIT OR Apache-2.0

//! Thin shared wrappers over the classic (non-block) CoreGraphics symbols that
//! `objc2-core-graphics` 0.3 doesn't surface, plus the CFURL/bitmap-context
//! setup common to `image_decode` and `image_encode`. Both symbols are stable
//! and the framework is already linked, so we declare them here once instead of
//! in each codec module.

use std::ffi::c_void;
use std::path::Path;

use objc2_core_foundation::{CFRetained, CFString, CFURLPathStyle, CFURL};
use objc2_core_graphics::kCGColorSpaceSRGB;
use objc2_core_graphics::{
    CGColorSpace, CGContext, CGImage, CGImageAlphaInfo, CGImageByteOrderInfo,
};

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

/// Build a POSIX-path file `CFURL` for `path` (the shared construction used by
/// both the ImageIO decode open-path and the encode destination).
pub(crate) fn file_url(path: &Path) -> Result<CFRetained<CFURL>, String> {
    let path_str = path.to_string_lossy();
    let cf_path = CFString::from_str(&path_str);
    CFURL::with_file_system_path(
        None,
        Some(&cf_path),
        CFURLPathStyle::CFURLPOSIXPathStyle,
        false,
    )
    .ok_or_else(|| "could not build CFURL".into())
}

/// Create a CGBitmapContext over `data` sized `width × height` with the sRGB
/// color space and byte order R,G,B,A (premultiplied, big-endian — matches
/// `Rgba8UnormSrgb`). `data` must point at a buffer of at least
/// `bytes_per_row * height` bytes and must outlive the returned context.
///
/// # Safety
/// `data` must be valid and large enough for the given geometry, and it must
/// remain alive and exclusively owned for as long as the returned context is
/// used.
pub(crate) unsafe fn srgb_bitmap_context(
    data: *mut c_void,
    width: u32,
    height: u32,
    bytes_per_row: usize,
) -> Result<CFRetained<CGContext>, String> {
    let color_space = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceSRGB }))
        .ok_or("could not create sRGB color space")?;
    let bitmap_info: u32 =
        CGImageAlphaInfo::PremultipliedLast.0 | CGImageByteOrderInfo::Order32Big.0;

    // SAFETY: caller guarantees `data` is valid for `bytes_per_row * height`;
    // color_space is valid for the call.
    let ctx_ptr = unsafe {
        CGBitmapContextCreate(
            data,
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
    Ok(unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ctx_ptr)) })
}

/// Snapshot the pixels currently in a bitmap `ctx` into an independent
/// `CGImage` (so the backing buffer may drop afterwards).
pub(crate) fn bitmap_context_image(ctx: &CGContext) -> Result<CFRetained<CGImage>, String> {
    // SAFETY: `ctx` is a valid bitmap context.
    let img_ptr = unsafe { CGBitmapContextCreateImage(ctx as *const CGContext) };
    if img_ptr.is_null() {
        return Err("CGBitmapContextCreateImage failed".into());
    }
    // SAFETY: non-null (checked), +1 retained → released on drop.
    Ok(unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(img_ptr)) })
}
