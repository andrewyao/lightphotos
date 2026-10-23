// SPDX-License-Identifier: MIT OR Apache-2.0
// Non-test code never panics, and every `unsafe` block says why it is sound.
// See CLAUDE.md → Rust rules.
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unimplemented,
    clippy::todo,
    clippy::unreachable,
    clippy::undocumented_unsafe_blocks
)]
// Tests write their fixtures with `std::fs::write`; clippy.toml says why
// non-test code may not.
#![cfg_attr(test, allow(clippy::disallowed_methods))]
//! LightPhotos' decode, encode, develop and scoring layer: everything that
//! turns a file into pixels, an edit, a JPEG or a score without a window. The
//! app in `main.rs` and the probes in `src/bin/` share it, so none of these
//! modules depends on egui, winit or wgpu.

pub mod base64;
pub mod command;
#[cfg(not(target_arch = "wasm32"))]
pub mod control_auth;
pub mod decode;
pub mod develop;
pub mod export;
pub mod hash;
pub mod paths;
pub mod scoring;
pub mod sidecar;
pub mod worker_pool;
