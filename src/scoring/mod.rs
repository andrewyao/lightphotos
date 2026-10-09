// SPDX-License-Identifier: MIT OR Apache-2.0
//! Photo scores: the technical quality score, the judge's penalty curves, face
//! and blink quality, subject masks, and the Apple Vision requests behind them.

pub mod facequality;
pub mod judge;
pub mod quality;
pub mod segmentation;
#[cfg(target_os = "macos")]
pub mod vision;
