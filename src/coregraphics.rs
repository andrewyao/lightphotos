// SPDX-License-Identifier: GPL-3.0-or-later

//! macOS-only CoreGraphics helpers shared by `image_decode` and `image_encode`:
//! a file `CFURL` and an sRGB RGBA bitmap context. `objc2-core-graphics` 0.3
//! does not expose the two bitmap-context functions, so they are declared here.

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

/// Build a POSIX-path file `CFURL` for `path`.
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

/// Create an sRGB bitmap context over `data` with premultiplied R,G,B,A byte
/// order, which matches `Rgba8UnormSrgb`.
///
/// # Safety
/// `data` must hold at least `bytes_per_row * height` bytes and stay alive and
/// exclusively owned while the returned context is used.
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

    // SAFETY: the caller guarantees `data` covers `bytes_per_row * height`.
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
    // SAFETY: non-null, and returned +1 retained, so drop releases it.
    Ok(unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(ctx_ptr)) })
}

/// Copy the pixels in `ctx` into an independent `CGImage`, so the context's
/// backing buffer can be dropped afterwards.
pub(crate) fn bitmap_context_image(ctx: &CGContext) -> Result<CFRetained<CGImage>, String> {
    // SAFETY: `ctx` is a valid bitmap context.
    let img_ptr = unsafe { CGBitmapContextCreateImage(ctx as *const CGContext) };
    if img_ptr.is_null() {
        return Err("CGBitmapContextCreateImage failed".into());
    }
    // SAFETY: non-null, and returned +1 retained, so drop releases it.
    Ok(unsafe { CFRetained::from_raw(std::ptr::NonNull::new_unchecked(img_ptr)) })
}
