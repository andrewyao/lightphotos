// SPDX-License-Identifier: MIT OR Apache-2.0
//! How many of the window's drawn pixels reach the panel. In a macOS scaled
//! mode, such as "looks like 3360x1890" on a 3840x2160 panel, the system draws
//! the window at twice the looks-like size (6720x3780) and then shrinks the
//! result onto the panel. Detail finer than a panel pixel is lost in that
//! shrink, so the Loupe uses this ratio to decide when a full-resolution
//! decode can show more than the preview.

use winit::window::Window;

/// Panel pixels per drawn pixel on the display that shows `window`. Less than
/// 1.0 in a macOS scaled mode, else 1.0.
#[cfg(target_os = "macos")]
pub(crate) fn panel_scale(window: &Window) -> f32 {
    use objc2_core_graphics::{
        CGDisplayCopyAllDisplayModes, CGDisplayCopyDisplayMode, CGDisplayMode,
    };
    use winit::platform::macos::MonitorHandleExtMacOS;

    // IOKit's kDisplayModeNativeFlag: the mode that matches the panel's pixels.
    const NATIVE_FLAG: u32 = 0x0200_0000;

    let Some(id) = window.current_monitor().map(|m| m.native_id()) else {
        return 1.0;
    };
    let Some(current) = CGDisplayCopyDisplayMode(id) else {
        return 1.0;
    };
    let drawn = CGDisplayMode::pixel_width(Some(&current));
    // SAFETY: `None` options are allowed.
    let Some(modes) = (unsafe { CGDisplayCopyAllDisplayModes(id, None) }) else {
        return 1.0;
    };
    // SAFETY: CoreGraphics documents the array as holding `CGDisplayMode`s.
    let modes = unsafe { modes.cast_unchecked::<CGDisplayMode>() };
    let native = modes
        .iter()
        .filter(|m| CGDisplayMode::io_flags(Some(m)) & NATIVE_FLAG != 0)
        .map(|m| CGDisplayMode::pixel_width(Some(&m)))
        .max();
    match native {
        Some(native) if drawn > 0 => scale_of(native, drawn),
        _ => 1.0,
    }
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn panel_scale(_window: &Window) -> f32 {
    1.0
}

/// `native / drawn`, never above 1.0: a mode drawn with fewer pixels than the
/// panel has is stretched up, which adds no detail.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn scale_of(native: usize, drawn: usize) -> f32 {
    (native as f32 / drawn as f32).min(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scaled_4k_mode_shows_about_four_sevenths_of_its_drawn_pixels() {
        // "Looks like 3360x1890" draws 6720 wide onto a 3840 panel.
        assert!((scale_of(3840, 6720) - 0.5714).abs() < 1e-3);
    }

    #[test]
    fn a_native_or_lower_mode_keeps_every_drawn_pixel() {
        assert_eq!(scale_of(3840, 3840), 1.0);
        assert_eq!(scale_of(3840, 2560), 1.0);
    }
}
