// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32 Web Worker decode entry point — the wasm port plan's M4 (real
//! threading). `app/web.rs`'s `request_web_thumbs`/`request_web_preview`
//! used to decode inline on the main thread via `spawn_local`, serially; this
//! binary is what those requests get dispatched to instead, N copies of it
//! running in independent `web_sys::Worker`s (`web_worker_pool.rs`, the
//! main-thread counterpart), each with its own separate wasm linear memory —
//! no `SharedArrayBuffer`/atomics, no nightly, stays on stable Rust (decided
//! over `wasm-bindgen-rayon` specifically to avoid both).
//!
//! This has to be a **separate binary**, not a branch inside `main.rs`'s
//! existing `fn main()`: a Worker context has no `window` (only
//! `DedicatedWorkerGlobalScope`), so it can't run winit/wgpu/egui at all, and
//! trunk's `data-type="worker"` link (`index.html`) needs its own compiled
//! bin target to point at — see the Trunk asset-pipeline docs
//! (`rel="rust"`/`data-type`) for how a second `[[bin]]` gets loaded as a
//! worker with a stable (non-content-hashed) output filename
//! (`wasm_worker.js`/`wasm_worker_bg.wasm`), unlike the main app's own
//! trunk-hashed output — that stability is exactly what lets
//! `web_worker_pool.rs` construct the worker's script URL without knowing
//! any build hash.
//!
//! The modules are pulled in by `#[path]` rather than through a `use
//! lightphotos::...`, because lightphotos has no lib target — same pattern
//! `face_probe.rs`/`seg_probe.rs` already use, and for the same reason:
//! `src/main.rs` is the crate root, so there is nothing for a second binary
//! to `use`. Declared at crate root (not nested in a module) so `crate::`
//! paths inside those files resolve exactly as they do in the main binary.
#![allow(dead_code)]

#[cfg(target_arch = "wasm32")]
#[path = "../image_decode.rs"]
mod image_decode;
#[cfg(target_arch = "wasm32")]
#[path = "../thumbnail.rs"]
mod thumbnail;
#[cfg(target_arch = "wasm32")]
#[path = "../raw_fast_preview.rs"]
mod raw_fast_preview;
// thumbnail.rs's on-disk `ThumbCache` (unused here — this worker only ever
// calls its bytes-based `embedded_preview_from_bytes`) still pulls these two
// in at compile time; re-declared for the same reason the three above are.
#[cfg(target_arch = "wasm32")]
#[path = "../paths.rs"]
mod paths;
#[cfg(target_arch = "wasm32")]
#[path = "../hash.rs"]
mod hash;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Never actually invoked — trunk only builds/loads this bin for
    // wasm32 — but `[[bin]]` targets are still considered by a native
    // `cargo build`/`cargo test`, and the modules above (`web-sys`-backed)
    // don't compile there at all. An empty stub keeps native builds clean
    // without gating this bin out of the manifest entirely.
}

#[cfg(target_arch = "wasm32")]
fn main() {
    wasm::run();
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use crate::image_decode::{self, DecodedImage};
    use crate::raw_fast_preview;
    use crate::thumbnail;
    use js_sys::{Array, Object, Reflect, Uint8Array};
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

    /// Decode one job's bytes to RGBA. For RAW, tries the file's own
    /// embedded EXIF baseline thumbnail first (`thumbnail::
    /// embedded_preview_from_bytes` — already-decoded by the camera, just a
    /// small JPEG decode, same trick Photopea and every fast RAW browser
    /// uses for quick previews), falling back to the quarter-res Bayer-
    /// demosaic path (`raw_fast_preview`) only when that's missing or too
    /// small for what was asked. This isn't only about speed: `raw_fast_
    /// preview`'s `catch_unwind` guards around `rawler`'s parser are almost
    /// certainly *ineffective* on wasm32-unknown-unknown (no real stack
    /// unwinding without nightly + explicit exception-handling support,
    /// which this build doesn't use) — a panic there traps the whole wasm
    /// instance, permanently killing this worker with no console output at
    /// all. That matched an observed symptom exactly: grid population
    /// getting stuck after a small, fixed number of thumbnails (workers
    /// dying off one at a time as each hit some RAW file that panicked
    /// rawler's parser), independent of the separate `NotReadableError`
    /// read-concurrency issue `app/web.rs` handles. Routing the common case
    /// (a grid thumbnail) through the JPEG decoder instead — far less
    /// panic-prone code — should make that far rarer, though a genuine
    /// fix still wants real exception-handling support or an audited
    /// panic-free `rawler` call path.
    fn decode(bytes: &[u8], max_px: u32, is_raw: bool) -> Result<DecodedImage, String> {
        if is_raw {
            if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
                // "Too small for what was asked" — the embedded baseline
                // thumbnail is typically ~160x120 (see its own doc
                // comment), fine for a grid cell but not the Loupe. Half
                // the requested size is a rough-but-workable cutoff, not a
                // precise one.
                if preview.width.max(preview.height) * 2 >= max_px {
                    return Ok(preview);
                }
            }
            return raw_fast_preview::decode_raw_fast_from_bytes(bytes, max_px);
        }
        if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
            return Ok(preview);
        }
        image_decode::decode_jpeg_png_tiff_from_bytes(bytes, max_px)
    }

    /// Read a numeric field off a job/result object via `Reflect`, panicking
    /// with a clear message on a malformed message rather than silently
    /// coercing `NaN` to `0` — a malformed job means a real protocol bug in
    /// `web_worker_pool.rs`, worth surfacing loudly during development.
    fn get_f64(obj: &JsValue, key: &str) -> f64 {
        Reflect::get(obj, &JsValue::from_str(key))
            .unwrap_or(JsValue::UNDEFINED)
            .as_f64()
            .unwrap_or_else(|| panic!("wasm_worker: job field `{key}` missing or not a number"))
    }

    fn get_bool(obj: &JsValue, key: &str) -> bool {
        Reflect::get(obj, &JsValue::from_str(key))
            .unwrap_or(JsValue::UNDEFINED)
            .as_bool()
            .unwrap_or(false)
    }

    pub fn run() {
        console_error_panic_hook::set_once();

        let scope = DedicatedWorkerGlobalScope::from(JsValue::from(js_sys::global()));
        let scope_for_closure = scope.clone();

        let onmessage = Closure::wrap(Box::new(move |msg: MessageEvent| {
            let data = msg.data();
            let id = get_f64(&data, "id");
            let max_px = get_f64(&data, "maxPx") as u32;
            let is_raw = get_bool(&data, "isRaw");
            let bytes_val = Reflect::get(&data, &JsValue::from_str("bytes"))
                .unwrap_or(JsValue::UNDEFINED);
            let bytes = Uint8Array::new(&bytes_val).to_vec();

            let result = Object::new();
            let _ = Reflect::set(&result, &JsValue::from_str("id"), &JsValue::from_f64(id));
            match decode(&bytes, max_px, is_raw) {
                Ok(img) => {
                    let rgba = Uint8Array::from(img.rgba.as_slice());
                    let _ = Reflect::set(&result, &JsValue::from_str("ok"), &JsValue::TRUE);
                    let _ = Reflect::set(
                        &result,
                        &JsValue::from_str("width"),
                        &JsValue::from_f64(img.width as f64),
                    );
                    let _ = Reflect::set(
                        &result,
                        &JsValue::from_str("height"),
                        &JsValue::from_f64(img.height as f64),
                    );
                    let _ = Reflect::set(&result, &JsValue::from_str("rgba"), &rgba.buffer());
                    let transfer = Array::new();
                    transfer.push(&rgba.buffer());
                    let _ = scope_for_closure
                        .post_message_with_transfer(&result, &transfer.into());
                }
                Err(e) => {
                    let _ = Reflect::set(&result, &JsValue::from_str("ok"), &JsValue::FALSE);
                    let _ = Reflect::set(
                        &result,
                        &JsValue::from_str("error"),
                        &JsValue::from_str(&e),
                    );
                    let _ = scope_for_closure.post_message(&result);
                }
            }
        }) as Box<dyn Fn(MessageEvent)>);
        scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        // Readiness handshake — mirrors trunk's own webworker example: a
        // worker only starts processing MessageEvents once its script first
        // yields to the JS event loop, so any job the pool sends before this
        // fires would be silently dropped. `web_worker_pool.rs` queues jobs
        // for a worker until it sees this.
        let ready = Object::new();
        let _ = Reflect::set(&ready, &JsValue::from_str("ready"), &JsValue::TRUE);
        let _ = scope.post_message(&ready);
    }
}
