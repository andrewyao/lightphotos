// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser shell: the canvas, File System Access folders and exports,
//! worker-side decode, the thumbnail cache, and page analytics.

#[cfg(any(target_arch = "wasm32", test))]
pub(crate) mod analytics;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_canvas;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_catalog_fs;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_decode;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_export_fs;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_exports;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_fs;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web_thumb_cache;
