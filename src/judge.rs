// SPDX-License-Identifier: GPL-3.0-or-later

//! Rendered pixels in, a quality score out. Shared by the app's scoring
//! workers and `src/bin/score_probe.rs`, which includes it by `#[path]`, so
//! it reaches other modules only through ones the probe also includes.

use crate::quality::{self, QualityScore};

/// Score opaque sRGB8 RGBA pixels.
pub fn judge(rgba: &[u8], width: u32, height: u32) -> QualityScore {
    quality::score(&quality::technical(rgba, width, height), None, None)
}
