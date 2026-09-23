// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32 Web Worker binary: decodes images and bakes exports off the main
//! thread. `web_worker_pool.rs` runs several copies, each with its own wasm
//! memory, so no `SharedArrayBuffer` or nightly Rust is needed.
//!
//! It is a separate binary because a worker has no `window` and cannot run
//! winit, wgpu, or egui. There is no lib target, so shared modules come in
//! through `#[path]` at the crate root, where their `crate::` paths resolve
//! the same as in the main binary.
#![allow(dead_code)]

#[cfg(target_arch = "wasm32")]
#[path = "../develop.rs"]
mod develop;
#[cfg(target_arch = "wasm32")]
#[path = "../export.rs"]
mod export;
#[cfg(target_arch = "wasm32")]
#[path = "../hash.rs"]
mod hash;
#[cfg(target_arch = "wasm32")]
#[path = "../image_decode.rs"]
mod image_decode;
#[cfg(target_arch = "wasm32")]
#[path = "../image_encode.rs"]
mod image_encode;
#[cfg(target_arch = "wasm32")]
#[path = "../image_ops.rs"]
mod image_ops;
#[cfg(target_arch = "wasm32")]
#[path = "../paths.rs"]
mod paths;
#[cfg(target_arch = "wasm32")]
#[path = "../raw/preview.rs"]
mod raw_preview;
#[cfg(target_arch = "wasm32")]
#[path = "../thumbnail.rs"]
mod thumbnail;

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    // Native `cargo build` still builds every `[[bin]]`. This stub lets it
    // succeed without the wasm-only modules above.
}

#[cfg(target_arch = "wasm32")]
fn main() {
    wasm::run();
}

#[cfg(target_arch = "wasm32")]
mod wasm {
    use crate::image_decode::{self, DecodedImage, PixelFormat};
    use crate::image_encode;
    use crate::raw_preview;
    use crate::thumbnail;
    use js_sys::{Array, Object, Reflect, Uint8Array};
    use std::sync::Arc;
    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;
    use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

    /// Decode one job's bytes. RAW files try the embedded EXIF preview,
    /// then rawler's embedded full image (RAF/CR3 only), then a real
    /// decode. The embedded paths are faster and avoid rawler's decode
    /// path, where a panic aborts this worker on wasm32.
    ///
    /// `quality` picks the RAW decode: `false` is the fast quarter-res sRGB8
    /// grid decode, `true` is the full demosaic in `LinearF16`, which the
    /// renderer tonemaps on the GPU. It is ignored for non-RAW files.
    fn decode(
        bytes: &[u8],
        max_px: u32,
        is_raw: bool,
        quality: bool,
    ) -> Result<DecodedImage, String> {
        if is_raw {
            if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
                // Use the same minimum resolution as native thumbnails.
                // Tiny EXIF previews must not enter the shared cache.
                if thumbnail::preview_is_large_enough(preview.width, preview.height, max_px) {
                    return Ok(preview);
                }
            }
            // No size gate here. This is the camera's full-resolution JPEG,
            // and Loupe jobs ask for 8192px on WebGPU, so a gate would reject
            // a 4000px embedded JPEG and force a full demosaic.
            if let Some(preview) = thumbnail::rawler_full_image_from_bytes(bytes, max_px) {
                return Ok(preview);
            }
            return if quality {
                raw_preview::decode_raw_quality_from_bytes(bytes, max_px)
            } else {
                raw_preview::decode_raw_fast_from_bytes(bytes, max_px)
            };
        }
        if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
            // Loupe jobs need more pixels than a small EXIF preview has.
            if thumbnail::preview_is_large_enough(preview.width, preview.height, max_px) {
                return Ok(preview);
            }
        }
        image_decode::decode_nonraw_from_bytes(bytes, max_px)
    }

    /// Bake one export and post `{ id, ok, jpeg }`, with the JPEG buffer
    /// transferred, or `{ id, ok: false, error }`.
    fn handle_export(
        scope: &DedicatedWorkerGlobalScope,
        result: &Object,
        data: &JsValue,
        bytes: Vec<u8>,
        is_raw: bool,
        max_px: u32,
    ) {
        let adj_json = Reflect::get(data, &JsValue::from_str("adjustments"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        let touchups_json = Reflect::get(data, &JsValue::from_str("touchups"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_default();
        let rot = get_f64(data, "rot") as u8;

        let baked = (|| -> Result<Vec<u8>, String> {
            let adj: crate::develop::Adjustments = serde_json::from_str(&adj_json)
                .map_err(|e| format!("bad adjustments json: {e}"))?;
            let touchups: Vec<crate::develop::TouchUp> = serde_json::from_str(&touchups_json)
                .map_err(|e| format!("bad touchups json: {e}"))?;
            crate::export::bake_jpeg_from_shared_vec(
                Arc::new(bytes),
                is_raw,
                &adj,
                &touchups,
                rot,
                max_px,
            )
        })();

        match baked {
            Ok(jpeg) => {
                let arr = Uint8Array::from(jpeg.as_slice());
                let _ = Reflect::set(result, &JsValue::from_str("ok"), &JsValue::TRUE);
                let _ = Reflect::set(result, &JsValue::from_str("jpeg"), &arr.buffer());
                let transfer = Array::new();
                transfer.push(&arr.buffer());
                let _ = scope.post_message_with_transfer(result, &transfer.into());
            }
            Err(e) => {
                let _ = Reflect::set(result, &JsValue::from_str("ok"), &JsValue::FALSE);
                let _ = Reflect::set(result, &JsValue::from_str("error"), &JsValue::from_str(&e));
                let _ = scope.post_message(result);
            }
        }
    }

    /// Panics on a missing or non-numeric field. That means a protocol bug in
    /// `web_worker_pool.rs`, so it should fail loudly.
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
            // Set for a thumbnail that missed the disk cache. The cache JPEG
            // is encoded here to keep the work off the main thread.
            let encode_jpeg = get_bool(&data, "encodeJpeg");
            let bytes_val =
                Reflect::get(&data, &JsValue::from_str("bytes")).unwrap_or(JsValue::UNDEFINED);
            let bytes = Uint8Array::new(&bytes_val).to_vec();

            let result = Object::new();
            let _ = Reflect::set(&result, &JsValue::from_str("id"), &JsValue::from_f64(id));

            if get_bool(&data, "export") {
                handle_export(&scope_for_closure, &result, &data, bytes, is_raw, max_px);
                return;
            }

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
                    // JPEG cannot hold alpha or LinearF16, so those results
                    // are not cached.
                    if encode_jpeg && crate::thumbnail::jpeg_cacheable(&img) {
                        if let Ok(bytes) =
                            image_encode::encode_jpeg_to_vec(img.width, img.height, &img.rgba)
                        {
                            let jpeg = Uint8Array::from(bytes.as_slice());
                            let _ =
                                Reflect::set(&result, &JsValue::from_str("jpeg"), &jpeg.buffer());
                            transfer.push(&jpeg.buffer());
                        }
                    }
                    let _ = scope_for_closure.post_message_with_transfer(&result, &transfer.into());
                }
                Err(e) => {
                    let _ = Reflect::set(&result, &JsValue::from_str("ok"), &JsValue::FALSE);
                    let _ =
                        Reflect::set(&result, &JsValue::from_str("error"), &JsValue::from_str(&e));
                    let _ = scope_for_closure.post_message(&result);
                }
            }
        }) as Box<dyn Fn(MessageEvent)>);
        scope.set_onmessage(Some(onmessage.as_ref().unchecked_ref()));
        onmessage.forget();

        // A worker drops messages sent before its script first yields to the
        // event loop. The pool holds jobs until it receives this.
        let ready = Object::new();
        let _ = Reflect::set(&ready, &JsValue::from_str("ready"), &JsValue::TRUE);
        let _ = scope.post_message(&ready);
    }
}
