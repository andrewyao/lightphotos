// SPDX-License-Identifier: MIT OR Apache-2.0
//! File to pixels and pixels to file: decode on every platform, JPEG encode,
//! and every rawler call (in `rawler/`, non-mac only).

#[cfg(target_os = "macos")]
pub mod coregraphics;
pub mod decode_budget;
pub mod image_decode;
pub mod image_encode;
// rawler is LGPL-2.1 and must stay out of the macOS build.
#[cfg(not(target_os = "macos"))]
pub mod rawler;
