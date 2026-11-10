// SPDX-License-Identifier: MIT OR Apache-2.0

//! A camera's time zone setting, as a camera's own menu takes it: a UTC
//! offset and a daylight saving switch. Cameras such as Sony's don't change
//! either on their own, so the setting is one fixed offset, not a named zone
//! that follows DST.

use serde::{Deserialize, Serialize};

/// The UTC offsets a camera's menu offers, in minutes, each with a city or
/// two that keep it as standard time.
pub const ZONES: &[(i32, &str)] = &[
    (-720, "Baker Island"),
    (-660, "Pago Pago"),
    (-600, "Honolulu"),
    (-570, "Marquesas"),
    (-540, "Anchorage"),
    (-480, "Los Angeles, Vancouver"),
    (-420, "Denver, Phoenix"),
    (-360, "Chicago, Mexico City"),
    (-300, "New York, Toronto"),
    (-240, "Halifax, Santiago"),
    (-210, "St. John's"),
    (-180, "São Paulo, Buenos Aires"),
    (-120, "South Georgia"),
    (-60, "Azores, Cape Verde"),
    (0, "London, Lisbon"),
    (60, "Paris, Berlin"),
    (120, "Athens, Cairo"),
    (180, "Moscow, Istanbul"),
    (210, "Tehran"),
    (240, "Dubai, Baku"),
    (270, "Kabul"),
    (300, "Karachi, Tashkent"),
    (330, "Delhi, Colombo"),
    (345, "Kathmandu"),
    (360, "Dhaka, Almaty"),
    (390, "Yangon"),
    (420, "Bangkok, Jakarta"),
    (480, "Beijing, Singapore"),
    (525, "Eucla"),
    (540, "Tokyo, Seoul"),
    (570, "Adelaide, Darwin"),
    (600, "Sydney, Guam"),
    (630, "Lord Howe Island"),
    (660, "Nouméa, Solomon Islands"),
    (720, "Auckland, Fiji"),
    (765, "Chatham Islands"),
    (780, "Tonga, Samoa"),
    (840, "Kiritimati"),
];

/// A camera's time zone setting.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct CameraZone {
    /// One of [`ZONES`]' offsets, in minutes east of UTC.
    pub utc_offset_min: i32,
    /// The camera's daylight saving switch, which sets its clock an hour
    /// ahead.
    pub dst: bool,
}

impl CameraZone {
    /// The setting, or `None` for an offset the menu doesn't offer, as a
    /// hostile prefs file could hold.
    pub fn new(utc_offset_min: i32, dst: bool) -> Option<CameraZone> {
        ZONES
            .iter()
            .any(|(m, _)| *m == utc_offset_min)
            .then_some(CameraZone {
                utc_offset_min,
                dst,
            })
    }

    /// The menu's offset nearest `seconds` east of UTC, DST off.
    pub fn nearest(seconds: i64) -> CameraZone {
        let minutes = seconds / 60;
        let utc_offset_min = ZONES
            .iter()
            .map(|(m, _)| *m)
            .min_by_key(|m| (i64::from(*m) - minutes).unsigned_abs())
            .unwrap_or(0);
        CameraZone {
            utc_offset_min,
            dst: false,
        }
    }

    /// How far the camera's clock runs ahead of UTC.
    pub fn camera_minus_utc_s(self) -> i64 {
        (i64::from(self.utc_offset_min) + if self.dst { 60 } else { 0 }) * 60
    }

    /// Whether the menu offers this offset.
    pub fn is_valid(self) -> bool {
        CameraZone::new(self.utc_offset_min, self.dst).is_some()
    }
}

/// A measured clock offset at least this far from the camera's setting
/// means the setting is likely wrong, such as its DST switch.
pub const DISAGREEMENT_S: i64 = 30 * 60;

/// How far LightRemote's measurement, `measured`, is from the setting the
/// user gave, when it is far enough to tell them. Both are camera-minus-UTC
/// seconds.
pub fn disagreement(measured: i64, setting: i64) -> Option<i64> {
    let diff = measured.saturating_sub(setting);
    (diff.unsigned_abs() >= DISAGREEMENT_S as u64).then_some(diff)
}
