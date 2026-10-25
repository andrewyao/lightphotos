// SPDX-License-Identifier: MIT OR Apache-2.0
//! Live heap bytes for the web benchmark, behind the `heap-stats` feature. The
//! wasm heap never shrinks, so its size is the high-water mark; the gap between
//! that and the peak of live bytes counted here is memory that is free but
//! could not be reused.

use cap::Cap;
use std::alloc::System;
use wasm_bindgen::prelude::*;

#[global_allocator]
static ALLOCATOR: Cap<System> = Cap::new(System, usize::MAX);

/// Puts `window.__lpHeapStats()` on the page. It returns `[live, peak]`: the
/// bytes allocated and not yet freed, and the most that was ever live at once,
/// across every decode worker.
pub(crate) fn register() {
    let Some(window) = web_sys::window() else {
        return;
    };
    let stats = Closure::<dyn Fn() -> js_sys::Array>::new(|| {
        js_sys::Array::of2(
            &JsValue::from_f64(ALLOCATOR.allocated() as f64),
            &JsValue::from_f64(ALLOCATOR.max_allocated() as f64),
        )
    });
    if js_sys::Reflect::set(&window, &"__lpHeapStats".into(), stats.as_ref()).is_ok() {
        stats.forget();
    }
}
