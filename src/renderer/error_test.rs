// SPDX-License-Identifier: MIT OR Apache-2.0

//! REND-01: a wgpu error must not panic the UI thread.
//!
//! Before the uncaptured-error handler was installed, an oversized texture
//! request through `set_image` reached wgpu's default handler, which panics
//! the calling thread (`panic!("wgpu error: ...")`). These tests provoke that
//! error through the real `set_image` path and prove the renderer now records
//! it and keeps running.

use super::Renderer;
use crate::decode::image_decode::{DecodedImage, DecodedImageFields, PixelFormat};

/// A headless renderer, or `None` to skip where no GPU adapter exists (the
/// same policy as `match_test::headless`: fail on CI's macOS runner, skip
/// elsewhere).
fn headless(w: u32, h: u32) -> Option<Renderer> {
    match Renderer::new_headless(w, h) {
        Ok(r) => Some(r),
        Err(e) => {
            assert!(
                !(cfg!(target_os = "macos") && std::env::var_os("CI").is_some()),
                "error_test: no GPU adapter on CI: {e}"
            );
            eprintln!("error_test: skipped, no GPU adapter: {e}");
            None
        }
    }
}

/// A texture one texel wider than the adapter allows fails wgpu validation.
/// The buffer stays tiny (one row), so the test is cheap. `set_image` is the
/// exact path `App::upload_shown` takes.
fn oversized(max_dim: u32) -> DecodedImage {
    let w = max_dim.saturating_add(1);
    DecodedImage::new_tracked(DecodedImageFields {
        width: w,
        height: 1,
        rgba: vec![0u8; w as usize * 4],
        pixel_format: PixelFormat::Srgb8,
    })
}

#[test]
fn an_oversized_image_records_an_error_instead_of_panicking() {
    let Some(mut r) = headless(64, 64) else {
        return;
    };
    let img = oversized(r.max_dim);
    // Without the handler this call panics on the UI thread via wgpu's
    // default handler. With it, the error is captured.
    r.set_image(&img);
    let err = r.take_error();
    assert!(
        err.is_some(),
        "an oversized texture should record a GpuError, got None"
    );
    // A second take sees nothing: the fault is drained once.
    assert_eq!(r.take_error(), None, "take_error should clear the fault");
}

#[test]
fn a_clean_renderer_reports_no_error() {
    let Some(r) = headless(64, 64) else {
        return;
    };
    assert_eq!(
        r.take_error(),
        None,
        "a freshly built renderer has no GPU fault"
    );
}

#[test]
fn a_valid_image_renders_without_an_error_after_a_caught_fault() {
    let Some(mut r) = headless(64, 64) else {
        return;
    };
    // Provoke and drain a fault.
    r.set_image(&oversized(r.max_dim));
    let _ = r.take_error();
    // A normal image then loads and renders cleanly: the device survived.
    let good = DecodedImage::new_tracked(DecodedImageFields {
        width: 8,
        height: 8,
        rgba: vec![128u8; 8 * 8 * 4],
        pixel_format: PixelFormat::Srgb8,
    });
    r.set_image(&good);
    r.render(None, None, None);
    assert_eq!(
        r.take_error(),
        None,
        "a valid image after a caught fault must not record a new error"
    );
}

/// REND-02: `set_image` validates its caller's dimensions and buffer length
/// before reaching wgpu. A short buffer would make `write_texture` read past
/// the slice, so it is refused, the image dropped, and a fault recorded.
#[test]
fn a_short_buffer_drops_the_image_and_records_a_fault() {
    let Some(mut r) = headless(64, 64) else {
        return;
    };
    // Load a valid image first so there is something to drop.
    let good = DecodedImage::new_tracked(DecodedImageFields {
        width: 8,
        height: 8,
        rgba: vec![128u8; 8 * 8 * 4],
        pixel_format: PixelFormat::Srgb8,
    });
    r.set_image(&good);
    assert_eq!(r.image_size, (8, 8));

    // An 8x8 image needs 256 bytes; give it 100.
    let short = DecodedImage::new_tracked(DecodedImageFields {
        width: 8,
        height: 8,
        rgba: vec![0u8; 100],
        pixel_format: PixelFormat::Srgb8,
    });
    r.set_image(&short);
    assert_eq!(r.image_size, (0, 0), "the image is dropped, not uploaded");
    assert!(r.take_error().is_some(), "a short buffer records a fault");
}

/// REND-02: `upload_thumb` returns `None` rather than reaching wgpu when the
/// size is past the adapter limit or the buffer is the wrong length.
#[test]
fn upload_thumb_rejects_oversize_and_short_buffers() {
    let Some(mut r) = headless(64, 64) else {
        return;
    };
    let max = r.max_dim;
    // One texel too wide: a valid-length buffer can't be built cheaply, so
    // pass a tiny one; the dimension check rejects it first.
    assert_eq!(r.upload_thumb(max.saturating_add(1), 1, &[0u8; 4]), None);
    // Right dimensions, short buffer.
    assert_eq!(r.upload_thumb(4, 4, &[0u8; 10]), None);
    // Zero side.
    assert_eq!(r.upload_thumb(0, 4, &[]), None);
    // A good thumb still uploads.
    assert!(r.upload_thumb(4, 4, &[200u8; 4 * 4 * 4]).is_some());
    assert_eq!(r.take_error(), None, "no fault reached wgpu");
}

/// REND-02: `set_touchups` caps its slice to `MAX_TOUCHUPS`, so a longer one
/// can't overrun the fixed GPU buffer.
#[test]
fn set_touchups_past_the_cap_does_not_overrun_the_buffer() {
    use crate::develop::{GpuTouchUp, MAX_TOUCHUPS};
    use bytemuck::Zeroable;
    let Some(mut r) = headless(64, 64) else {
        return;
    };
    let too_many = vec![GpuTouchUp::zeroed(); MAX_TOUCHUPS + 16];
    r.set_touchups(&too_many);
    r.render(None, None, None);
    assert_eq!(
        r.take_error(),
        None,
        "capping to MAX_TOUCHUPS keeps the write inside the buffer"
    );
}
