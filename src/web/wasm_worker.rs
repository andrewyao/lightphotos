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
//!
//! ## Pipeline position
//! - This binary IS wasm32's decode step for both Pipeline 1 (Loupe) and
//!   Pipeline 2 (Grid/filmstrip) — it never runs on macOS or native
//!   Linux/Windows.
//! - `web_worker_pool.rs` (main thread) posts a job here; this file's
//!   `decode()` tries the cheap embedded-preview extractors first
//!   (`thumbnail::embedded_preview_from_bytes`, then
//!   `thumbnail::rawler_full_image_from_bytes` for RAF/CR3), falling back to
//!   `raw_fast_preview`'s `Fast`/`Quality` tiers for everything else.
//! - The result posts back to the main thread, which routes it into
//!   `loader.rs`'s caches via `insert_*_external` (see that file's own
//!   doc comment).
//! - See `ARCHITECTURE.md`.
#![allow(dead_code)]

#[cfg(target_arch = "wasm32")]
#[path = "../image_decode.rs"]
mod image_decode;
#[cfg(target_arch = "wasm32")]
#[path = "../thumbnail.rs"]
mod thumbnail;
#[cfg(target_arch = "wasm32")]
#[path = "../raw/fast_preview.rs"]
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
// Pulled in for `denoise_linear_rgb_buffer`, which `raw_fast_preview`'s
// `Quality`-tier code now calls (see raw/fast_preview.rs).
#[cfg(target_arch = "wasm32")]
#[path = "../develop.rs"]
mod develop;

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
    use crate::image_decode::{self, DecodedImage, PixelFormat};
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
    /// uses for quick previews); if that container can't even be opened this
    /// way (CR3's ISO-BMFF wrapper, RAF's proprietary header — neither is
    /// TIFF/JPEG at byte 0), tries `thumbnail::rawler_full_image_from_bytes`
    /// next — RAF/CR3-only by design (see that function's own doc comment):
    /// several other formats treated as RAW here (CR2/NEF/ARW/DNG/RW2/PEF)
    /// *also* have a `full_image()` override, but their containers already
    /// open fine above, so letting this ask them too silently swapped the
    /// camera's own embedded JPEG in for the real linear-RAW demosaic on
    /// every one of those, confirmed on a real Sony ARW — a different
    /// picture, not a subtly-off tonemap; only then falls back to
    /// `raw_fast_preview`. This isn't only about
    /// speed: `raw_fast_preview`'s `catch_unwind` guards around `rawler`'s
    /// parser are almost certainly *ineffective* on wasm32-unknown-unknown
    /// (no real stack unwinding without nightly + explicit exception-handling
    /// support, which this build doesn't use) — a panic there traps the
    /// whole wasm instance, permanently killing this worker with no console
    /// output at all. That matched an observed symptom exactly: grid
    /// population getting stuck after a small, fixed number of thumbnails
    /// (workers dying off one at a time as each hit some RAW file that
    /// panicked rawler's parser), independent of the separate
    /// `NotReadableError` read-concurrency issue `app/web.rs` handles.
    /// Routing the common case (a grid thumbnail, or a RAF/CR3 Loupe open)
    /// through one of the two embedded-preview extractors instead — far less
    /// panic-prone code than `raw_fast_preview`'s demosaic path — should make
    /// that far rarer, though a genuine fix still wants real
    /// exception-handling support or an audited panic-free `rawler` call
    /// path.
    ///
    /// `quality`, set by `web_worker_pool.rs`'s `submit()` from the job's
    /// `JobKind` (never decided here), picks which `raw_fast_preview` entry
    /// point services the fallback: `false` (Grid/`Thumb`) →
    /// `decode_raw_fast_from_bytes` (quarter-res Bayer bin, sRGB8 output,
    /// unchanged); `true` (Loupe/`Preview`) → `decode_raw_quality_from_bytes`
    /// (full PPG demosaic, `PixelFormat::LinearF16` output — the renderer
    /// tonemaps this on the GPU via `raw_shader.wgsl` instead of expecting it
    /// pre-baked). Meaningless for the non-RAW branch below.
    fn decode(bytes: &[u8], max_px: u32, is_raw: bool, quality: bool) -> Result<DecodedImage, String> {
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
            // rawler's `full_image()` is the camera's own full-resolution
            // embedded JPEG, not a small baseline thumbnail — no "too small"
            // check needed, it's plenty big for Grid and Loupe alike.
            if let Some(preview) = thumbnail::rawler_full_image_from_bytes(bytes, max_px) {
                return Ok(preview);
            }
            return if quality {
                raw_fast_preview::decode_raw_quality_from_bytes(bytes, max_px)
            } else {
                raw_fast_preview::decode_raw_fast_from_bytes(bytes, max_px)
            };
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
            let quality = get_bool(&data, "quality");
            let bytes_val = Reflect::get(&data, &JsValue::from_str("bytes"))
                .unwrap_or(JsValue::UNDEFINED);
            let bytes = Uint8Array::new(&bytes_val).to_vec();

            let result = Object::new();
            let _ = Reflect::set(&result, &JsValue::from_str("id"), &JsValue::from_f64(id));
            match decode(&bytes, max_px, is_raw, quality) {
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
                    let _ = Reflect::set(
                        &result,
                        &JsValue::from_str("linear"),
                        &JsValue::from_bool(img.pixel_format == PixelFormat::LinearF16),
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
