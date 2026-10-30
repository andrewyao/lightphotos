// SPDX-License-Identifier: MIT OR Apache-2.0
//! Photo scores: the technical quality score, the judge's penalty curves, face
//! and blink quality, subject masks, and the Apple Vision requests behind them.

pub mod detect;
pub mod facequality;
pub mod judge;
pub mod quality;
// Built on macOS only for its tests, which need the `rustface` dev-dependency.
#[cfg(any(not(target_os = "macos"), test))]
pub mod seetaface;
pub mod segmentation;
pub mod sky;
#[cfg(target_os = "macos")]
pub mod vision;
