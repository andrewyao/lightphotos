// SPDX-License-Identifier: MIT OR Apache-2.0

//! Locating photos on a GPX track.
//!
//! A camera stamps `DateTimeOriginal` on its own clock, in local time with no
//! zone, while a GPX track is in UTC. A [`ClockModel`] says how far the
//! camera's clock ran from UTC, and when: measured against the camera by
//! LightRemote, taken from the phone's time zone, or given by the user.
//! [`locate`] turns a photo's wall time into a UTC instant with it and reads
//! the position there off the [`Track`].
//!
//! The rules match LightRemote's `geotag.py`, which hands the same track to
//! exiftool: a photo between two fixes at most [`MAX_INTERPOLATE_S`] apart
//! takes the point on the line between them; otherwise it takes the nearest
//! fix within [`MAX_GAP_S`], or no position. A wall time that two clock
//! periods both place on the track (the hour repeated when a clock goes
//! back) is ambiguous and left alone rather than guessed.
//!
//! The result is [`GeoTag`], which the photo's sidecar keeps. Nothing here
//! touches a photo file.

mod gpx;

pub use gpx::{GpxError, LR_NS, MAX_GPX_BYTES};

use serde::{Deserialize, Serialize};

use crate::decode::image_decode::Gps;

/// The farthest a photo may be from the nearest fix, in seconds.
pub const MAX_GAP_S: i64 = 300;
/// The widest gap between two fixes that a photo is placed along.
pub const MAX_INTERPOLATE_S: i64 = 2 * MAX_GAP_S;
/// Clock measurements within this many seconds of each other are one
/// setting read with noise.
const CLOCK_NOISE_S: i64 = 5;
/// A camera clock offset larger than this, about 300 years, is not a clock
/// setting. Bounding it keeps every sum here far from overflow.
pub const MAX_CLOCK_ERROR_S: i64 = 300 * 366 * 86_400;

/// One fix: Unix seconds, signed degrees and meters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrackPoint {
    pub utc: i64,
    pub lat: f64,
    pub lon: f64,
    pub ele: Option<f64>,
}

/// From `from` (Unix seconds) on, the camera's clock read
/// `camera_minus_utc_s` ahead of UTC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClockPeriod {
    pub from: i64,
    pub camera_minus_utc_s: i64,
}

/// A parsed GPX file: [`Track::parse`] builds it.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// Sorted by time, one per second, never empty.
    points: Vec<TrackPoint>,
    /// LightRemote's camera clock measurements, sorted.
    camera_clock: Vec<ClockPeriod>,
    /// The phone's UTC offset from each change on, sorted.
    utc_offsets: Vec<ClockPeriod>,
}

impl Track {
    /// Whether the file carries any LightRemote clock data, so the user
    /// need not be asked for the camera's offset.
    pub fn knows_the_clock(&self) -> bool {
        !self.camera_clock.is_empty() || !self.utc_offsets.is_empty()
    }

    /// The position at `utc`, by the rules in the module docs.
    pub fn position_at(&self, utc: i64) -> Option<Gps> {
        let i = self.points.partition_point(|p| p.utc < utc);
        let next = self.points.get(i);
        let prev = i.checked_sub(1).and_then(|j| self.points.get(j));
        if let Some(n) = next.filter(|n| n.utc == utc) {
            return Some(fix(n));
        }
        if let (Some(a), Some(b)) = (prev, next) {
            if b.utc.saturating_sub(a.utc) <= MAX_INTERPOLATE_S {
                return Some(between(a, b, utc));
            }
        }
        [prev, next]
            .into_iter()
            .flatten()
            .map(|p| (p.utc.abs_diff(utc), p))
            .filter(|(gap, _)| *gap <= MAX_GAP_S as u64)
            .min_by_key(|(gap, _)| *gap)
            .map(|(_, p)| fix(p))
    }

    fn covers(&self, utc: i64) -> bool {
        self.position_at(utc).is_some()
    }
}

fn fix(p: &TrackPoint) -> Gps {
    Gps {
        lat: p.lat,
        lon: p.lon,
        alt: p.ele,
    }
}

/// The point `utc` reaches on the straight line from `a` to `b`, the short
/// way round the antimeridian. `a` is before `b`, so the span is not zero.
fn between(a: &TrackPoint, b: &TrackPoint, utc: i64) -> Gps {
    let f = (utc - a.utc) as f64 / (b.utc - a.utc) as f64;
    let mut dlon = b.lon - a.lon;
    if dlon > 180.0 {
        dlon -= 360.0;
    } else if dlon < -180.0 {
        dlon += 360.0;
    }
    let mut lon = a.lon + f * dlon;
    if lon > 180.0 {
        lon -= 360.0;
    } else if lon < -180.0 {
        lon += 360.0;
    }
    Gps {
        lat: a.lat + f * (b.lat - a.lat),
        lon,
        alt: a.ele.zip(b.ele).map(|(x, y)| x + f * (y - x)),
    }
}

/// How the camera's clock relates to UTC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClockModel {
    /// LightRemote compared the camera's clock with UTC. Covers its zone,
    /// its DST setting and its drift together.
    Measured(Vec<ClockPeriod>),
    /// The phone's time zone, assuming the camera was set to it.
    PhoneZone(Vec<ClockPeriod>),
    /// One offset the user gave, for the whole track.
    Manual { camera_minus_utc_s: i64 },
}

impl ClockModel {
    /// The best model `track` supports: measured, else the phone's zone,
    /// else `manual`. `None` when the track has no clock data and no
    /// offset was given. An out-of-range `manual` counts as none.
    pub fn select(track: &Track, manual: Option<i64>) -> Option<ClockModel> {
        if !track.camera_clock.is_empty() {
            return Some(ClockModel::Measured(clock_periods(&track.camera_clock)));
        }
        if !track.utc_offsets.is_empty() {
            return Some(ClockModel::PhoneZone(track.utc_offsets.clone()));
        }
        manual
            .filter(|s| s.abs() <= MAX_CLOCK_ERROR_S)
            .map(|camera_minus_utc_s| ClockModel::Manual { camera_minus_utc_s })
    }

    /// The one camera offset that puts wall time `wall` (camera clock
    /// seconds, read as if UTC) in its own clock period and on the track.
    /// The first period reaches back before its start, so photos taken
    /// before the first measurement still match where the track has them.
    fn resolve(&self, track: &Track, wall: i64) -> Resolution {
        let manual;
        let periods: &[ClockPeriod] = match self {
            ClockModel::Measured(p) | ClockModel::PhoneZone(p) => p,
            ClockModel::Manual { camera_minus_utc_s } => {
                manual = ClockPeriod {
                    from: i64::MIN,
                    camera_minus_utc_s: *camera_minus_utc_s,
                };
                std::slice::from_ref(&manual)
            }
        };
        let mut found = std::collections::BTreeSet::new();
        for (i, p) in periods.iter().enumerate() {
            let Some(utc) = wall.checked_sub(p.camera_minus_utc_s) else {
                continue;
            };
            if i > 0 && utc < p.from {
                continue;
            }
            if periods.get(i + 1).is_some_and(|next| utc >= next.from) {
                continue;
            }
            if track.covers(utc) {
                found.insert(p.camera_minus_utc_s);
            }
        }
        let mut found = found.into_iter();
        match (found.next(), found.next()) {
            (Some(offset), None) => Resolution::Offset(offset),
            (Some(_), Some(_)) => Resolution::Ambiguous,
            (None, _) => Resolution::Outside,
        }
    }
}

enum Resolution {
    Offset(i64),
    Ambiguous,
    Outside,
}

/// Measurements collapsed into clock settings: a reading within
/// [`CLOCK_NOISE_S`] of the current setting joins it, and the setting takes
/// the upper median of its readings, keeping its first reading's time.
/// `entries` is sorted.
fn clock_periods(entries: &[ClockPeriod]) -> Vec<ClockPeriod> {
    let mut periods: Vec<ClockPeriod> = Vec::new();
    let mut samples: Vec<i64> = Vec::new();
    for e in entries {
        match periods.last_mut() {
            Some(last)
                if (e.camera_minus_utc_s - last.camera_minus_utc_s).abs() <= CLOCK_NOISE_S =>
            {
                samples.push(e.camera_minus_utc_s);
                samples.sort_unstable();
                if let Some(median) = samples.get(samples.len() / 2) {
                    last.camera_minus_utc_s = *median;
                }
            }
            _ => {
                samples = vec![e.camera_minus_utc_s];
                periods.push(*e);
            }
        }
    }
    periods
}

/// A location LightPhotos found for a photo, kept in its sidecar.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct GeoTag {
    pub lat: f64,
    pub lon: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alt: Option<f64>,
    pub source: GeoSource,
}

/// Where a [`GeoTag`] came from. Tagged, so later sources can join.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GeoSource {
    /// Matched on a GPX track by capture time.
    Gpx {
        /// The GPX file's name, without its folder.
        file_name: String,
        /// The camera clock offset the match used.
        camera_minus_utc_s: i64,
    },
}

impl GeoTag {
    /// The position, or `None` for one off the globe, as a hostile sidecar
    /// could hold.
    pub fn gps(&self) -> Option<Gps> {
        let alt = self.alt.filter(|a| a.is_finite());
        (self.lat.is_finite()
            && self.lon.is_finite()
            && self.lat.abs() <= 90.0
            && self.lon.abs() <= 180.0)
            .then_some(Gps {
                lat: self.lat,
                lon: self.lon,
                alt,
            })
    }
}

/// What geotagging found for one photo.
#[derive(Clone, Debug, PartialEq)]
pub enum MatchOutcome {
    Located(GeoTag),
    /// Two clock periods both place the photo on the track.
    Ambiguous,
    OutsideTrack,
    NoCaptureTime,
    /// The file has its own GPS, which wins.
    AlreadyHasGps,
}

/// One photo against `track`. `capture_local` is its `DateTimeOriginal`,
/// `YYYY:MM:DD HH:MM:SS` on the camera's clock; `gpx_name` is recorded as
/// the location's source.
pub fn locate(
    track: &Track,
    clock: &ClockModel,
    gpx_name: &str,
    has_gps: bool,
    capture_local: Option<&str>,
) -> MatchOutcome {
    if has_gps {
        return MatchOutcome::AlreadyHasGps;
    }
    let Some(wall) = capture_local.and_then(wall_seconds) else {
        return MatchOutcome::NoCaptureTime;
    };
    let offset = match clock.resolve(track, wall) {
        Resolution::Offset(o) => o,
        Resolution::Ambiguous => return MatchOutcome::Ambiguous,
        Resolution::Outside => return MatchOutcome::OutsideTrack,
    };
    let Some(gps) = wall
        .checked_sub(offset)
        .and_then(|utc| track.position_at(utc))
    else {
        return MatchOutcome::OutsideTrack;
    };
    MatchOutcome::Located(GeoTag {
        lat: gps.lat,
        lon: gps.lon,
        alt: gps.alt,
        source: GeoSource::Gpx {
            file_name: gpx_name.to_string(),
            camera_minus_utc_s: offset,
        },
    })
}

/// An EXIF wall time as seconds since the epoch, read as if it were UTC.
/// Sub-second digits and anything past the seconds are ignored.
fn wall_seconds(local: &str) -> Option<i64> {
    let head = local.trim().get(..19)?;
    chrono::NaiveDateTime::parse_from_str(head, "%Y:%m:%d %H:%M:%S")
        .ok()
        .map(|t| t.and_utc().timestamp())
}

/// A UTC offset as camera-minus-UTC seconds, from an EXIF offset such as
/// `+09:00`. For prefilling the offset the user is asked for.
pub fn offset_seconds(exif_offset: &str) -> Option<i64> {
    exif_offset
        .trim()
        .parse::<chrono::FixedOffset>()
        .ok()
        .map(|o| i64::from(o.local_minus_utc()))
}

#[cfg(test)]
mod tests;
