// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: puts winit's `<canvas>` into the page. winit creates the
//! canvas on web but leaves inserting it into the DOM to the app.

use winit::dpi::PhysicalSize;
use winit::platform::web::WindowExtWebSys;
use winit::window::Window;

/// Append `window`'s canvas to `<body>`, sized to fill the viewport, and
/// return its size in physical pixels. Pass that size to `Renderer::new`;
/// `window.inner_size()` is not reliable yet at this point. Styling sets only
/// `width`/`height`/`display`, because winit's size math breaks with
/// `transform`, `border`, or `padding` on the canvas.
pub fn attach(window: &Window) -> PhysicalSize<u32> {
    let canvas = window
        .canvas()
        .expect("window has no canvas (not running on web?)");
    canvas.set_id("lightphotos-canvas");

    // The backing store (the width/height attributes, not the CSS size)
    // defaults to 300x150, so set it to the viewport in physical pixels.
    // egui computes scissor rects in physical pixels from the scale factor,
    // and WebGPU rejects a scissor rect larger than the surface.
    let browser_window = web_sys::window().expect("no window");
    let dpr = browser_window.device_pixel_ratio();
    let dpr = if dpr > 0.0 { dpr } else { 1.0 };
    let logical_w = browser_window
        .inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(1100.0);
    let logical_h = browser_window
        .inner_height()
        .ok()
        .and_then(|v| v.as_f64())
        .unwrap_or(800.0);
    let w = (logical_w * dpr) as u32;
    let h = (logical_h * dpr) as u32;
    canvas.set_width(w.max(1));
    canvas.set_height(h.max(1));

    if let Ok(style) = js_sys::Reflect::get(&canvas, &"style".into()) {
        let _ = js_sys::Reflect::set(&style, &"width".into(), &"100vw".into());
        let _ = js_sys::Reflect::set(&style, &"height".into(), &"100vh".into());
        let _ = js_sys::Reflect::set(&style, &"display".into(), &"block".into());
    }

    let document = web_sys::window()
        .and_then(|w| w.document())
        .expect("no document");
    let body = document.body().expect("document has no body");
    body.append_child(&canvas).expect("append canvas to body");

    PhysicalSize::new(w.max(1), h.max(1))
}
