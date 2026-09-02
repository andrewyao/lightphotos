// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: attaches winit's web canvas into the page. winit creates the
//! `<canvas>` element itself on this target but does not insert it into the
//! DOM for you — that's explicitly left to the app, per winit's own
//! `platform::web` docs.
//!
//! ## Pipeline position
//! - Runs once, at startup, before any of the three pipelines exist —
//!   `main.rs`'s wasm32 `resumed()` calls `attach` before constructing
//!   `Renderer::new` (Pipeline 1's final stage), because `Renderer::new`
//!   needs the real viewport size this function determines.
//! - Not called again after startup, and not part of any per-photo pipeline
//!   run.

use winit::dpi::PhysicalSize;
use winit::platform::web::WindowExtWebSys;
use winit::window::Window;

/// Insert `window`'s canvas into `<body>`, sized to fill the viewport, and
/// return the size actually used — the caller needs it for `Renderer::new`
/// (see that function's doc comment for why `window.inner_size()` itself
/// can't be trusted here). Deliberately minimal styling beyond that:
/// winit's own docs warn that `transform`/`border`/`padding` on the canvas
/// throws off its internal size math, so this sticks to
/// `width`/`height`/`display` only.
pub fn attach(window: &Window) -> PhysicalSize<u32> {
    let canvas = window
        .canvas()
        .expect("window has no canvas (not running on web?)");
    canvas.set_id("lightphotos-canvas");

    // Set the canvas's actual backing-store resolution (its width/height
    // ATTRIBUTES, distinct from the CSS size set below) to match the real
    // viewport — a canvas's default drawing-buffer size (300×150) has
    // nothing to do with its CSS-stretched display size, so without this
    // the image would render at the wrong resolution relative to how large
    // it's actually shown. This value is also the one source of truth this
    // module hands back to the caller for `Renderer::new` — see that
    // function's doc comment for why `window.inner_size()` itself can't be
    // used instead.
    //
    // Scaled by devicePixelRatio, not left at CSS/logical pixels: egui
    // already receives the real scale factor (`window.scale_factor()`,
    // passed into `egui_winit::State::new` in main.rs) and computes its
    // scissor rects in PHYSICAL pixels from it — on this HiDPI display that
    // made egui ask for a 4086px-wide scissor rect (2043 logical × 2 DPR)
    // against a surface that was only ever configured at the 2043 logical
    // size, failing WebGPU's "scissor rect must fit the render area"
    // validation. The backing store has to actually be physical-pixel-sized
    // for egui's own math to hold, exactly like every other HiDPI canvas app.
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
    body.append_child(&canvas)
        .expect("append canvas to body");

    PhysicalSize::new(w.max(1), h.max(1))
}
