// SPDX-License-Identifier: MIT OR Apache-2.0
//! LightPhotos' decode, encode, develop and scoring layer: everything that
//! turns a file into pixels, an edit, a JPEG or a score without a window. The
//! app in `main.rs` and the probes in `src/bin/` share it, so none of these
//! modules depends on egui, winit or wgpu.

pub mod chroma;
pub mod curve;
pub mod decode;
pub mod develop;
pub mod export;
pub mod facequality;
pub mod hash;
pub mod image_ops;
#[cfg(not(target_arch = "wasm32"))]
pub mod immich;
pub mod judge;
pub mod paths;
pub mod quality;
pub mod segmentation;
#[cfg(target_os = "macos")]
pub mod vision;
pub mod worker_pool;
