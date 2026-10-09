// SPDX-License-Identifier: MIT OR Apache-2.0
//! LightPhotos' decode, encode, develop and scoring layer: everything that
//! turns a file into pixels, an edit, a JPEG or a score without a window. The
//! app in `main.rs` and the probes in `src/bin/` share it, so none of these
//! modules depends on egui, winit or wgpu.

pub mod decode;
pub mod develop;
pub mod export;
pub mod hash;
pub mod paths;
pub mod scoring;
pub mod worker_pool;
