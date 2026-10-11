// SPDX-License-Identifier: MIT OR Apache-2.0

//! The parts of a photo's sidecar record that the command layer names: its
//! rating range, flag and color label.

use serde::{Deserialize, Serialize};

/// Highest star rating. Ratings run `0..=MAX_RATING`, with 0 for unrated.
pub const MAX_RATING: u8 = 5;

/// A culling decision on a photo, apart from its stars. Unflagged is `None`.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum Flag {
    Pick,
    Reject,
}

/// A photo's color label. 6-9 set the first four, as in Lightroom.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ColorLabel {
    Red,
    Yellow,
    Green,
    Blue,
    Purple,
}

impl ColorLabel {
    /// The label digit key `n` sets: 6 red, 7 yellow, 8 green, 9 blue.
    /// Purple has no key, as in Lightroom.
    pub fn from_digit(n: u8) -> Option<ColorLabel> {
        Some(match n {
            6 => ColorLabel::Red,
            7 => ColorLabel::Yellow,
            8 => ColorLabel::Green,
            9 => ColorLabel::Blue,
            _ => return None,
        })
    }
}
