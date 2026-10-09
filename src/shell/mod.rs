// SPDX-License-Identifier: MIT OR Apache-2.0
//! The platform shell around `App`: the macOS menu bar and Finder open
//! handler, native dialogs, the saved window rect, the system trash, and the
//! `--drive` and `--profile` headless runners.

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod dialog;
pub(crate) mod display;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod drive;
pub(crate) mod macos_delegate;
#[cfg(target_os = "macos")]
pub(crate) mod menu;
// Profiling drives the real `navigation`, `catalog`, `Loader` and `export`
// code, none of which the browser build has, and the driver runs from the
// native `main`. Gating the module the same way keeps `--features hotpath`
// building for wasm32 instead of failing on APIs that target cannot have.
#[cfg(all(feature = "hotpath", not(target_arch = "wasm32")))]
pub(crate) mod profile;
pub(crate) mod trash;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod window_rect;
