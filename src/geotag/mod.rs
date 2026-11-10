// SPDX-License-Identifier: MIT OR Apache-2.0

//! Locating photos on a GPX track.
//!
//! A camera stamps `DateTimeOriginal` on its own clock, in local time with no
//! zone, while a GPX track is in UTC. A [`ClockModel`] says how far the
//! camera's clock ran from UTC, and when: measured against the camera by
//! LightRemote, else the camera's time zone setting as the user gave it
//! ([`CameraZone`]). [`locate`] turns a photo's wall time into a UTC instant
//! with it and reads the position there off the [`Track`].
//!
//! Nothing but its owner changes a camera's clock, so a measurement holds
//! until the next one, across files and days. The first one also reaches
//! back to photos taken before it.
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
mod zone;

pub use gpx::{GpxError, LR_NS, MAX_GPX_BYTES};
pub use zone::{disagreement, CameraZone, DISAGREEMENT_S, ZONES};

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

/// One or more parsed GPX files: [`Track::parse`] builds one and
/// [`Track::merge`] joins several.
#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// Sorted by time, one per second, never empty.
    points: Vec<Fix>,
    /// The files the points came from, by name; [`Fix::file`] indexes it.
    files: Vec<String>,
    /// LightRemote's camera clock measurements, sorted.
    camera_clock: Vec<ClockPeriod>,
}

/// A track point and the file it came from.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fix {
    point: TrackPoint,
    file: usize,
}

impl Track {
    /// Several files' tracks as one: every fix and clock measurement, in
    /// time order. Where two files have a fix at the same second,
    /// the earlier file's is kept. `None` for no tracks.
    pub fn merge(tracks: impl IntoIterator<Item = Track>) -> Option<Track> {
        let mut merged = Track {
            points: Vec::new(),
            files: Vec::new(),
            camera_clock: Vec::new(),
        };
        for track in tracks {
            let base = merged.files.len();
            merged.files.extend(track.files);
            merged.points.extend(track.points.into_iter().map(|f| Fix {
                file: f.file.saturating_add(base),
                ..f
            }));
            merged.camera_clock.extend(track.camera_clock);
        }
        if merged.points.is_empty() {
            return None;
        }
        merged.normalize();
        Some(merged)
    }

    /// Sort everything by time; of several fixes at one second keep the
    /// first, and drop repeated clock entries.
    fn normalize(&mut self) {
        self.points.sort_by_key(|f| f.point.utc);
        self.points.dedup_by_key(|f| f.point.utc);
        self.camera_clock
            .sort_by_key(|c| (c.from, c.camera_minus_utc_s));
        self.camera_clock.dedup();
    }

    /// The names of the files this track joins, in the order given.
    pub fn file_names(&self) -> &[String] {
        &self.files
    }

    /// The position at `utc`, by the rules in the module docs, and the
    /// name of the file holding the nearest fix it used.
    fn position_at(&self, utc: i64) -> Option<(Gps, &str)> {
        let i = self.points.partition_point(|f| f.point.utc < utc);
        let next = self.points.get(i);
        let prev = i.checked_sub(1).and_then(|j| self.points.get(j));
        let gap = |f: &Fix| f.point.utc.abs_diff(utc);
        let (gps, nearest) = if let Some(n) = next.filter(|n| n.point.utc == utc) {
            (fix(&n.point), n)
        } else if let Some((a, b)) = prev
            .zip(next)
            .filter(|(a, b)| b.point.utc.saturating_sub(a.point.utc) <= MAX_INTERPOLATE_S)
        {
            let nearest = if gap(b) < gap(a) { b } else { a };
            (between(&a.point, &b.point, utc), nearest)
        } else {
            let nearest = [prev, next]
                .into_iter()
                .flatten()
                .filter(|f| gap(f) <= MAX_GAP_S as u64)
                .min_by_key(|f| gap(f))?;
            (fix(&nearest.point), nearest)
        };
        let file = self.files.get(nearest.file).map_or("", String::as_str);
        Some((gps, file))
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
    /// The camera's time zone setting, as the user gave it: one offset for
    /// the whole track.
    Setting { camera_minus_utc_s: i64 },
}

impl ClockModel {
    /// LightRemote's measurements when `track` has any, else `setting`.
    /// `None` when it has neither. An out-of-range `setting` counts as none.
    pub fn select(track: &Track, setting: Option<i64>) -> Option<ClockModel> {
        if !track.camera_clock.is_empty() {
            return Some(ClockModel::Measured(clock_periods(&track.camera_clock)));
        }
        setting
            .filter(|s| s.abs() <= MAX_CLOCK_ERROR_S)
            .map(|camera_minus_utc_s| ClockModel::Setting { camera_minus_utc_s })
    }

    pub fn source(&self) -> ClockSource {
        match self {
            ClockModel::Measured(_) => ClockSource::Measured,
            ClockModel::Setting { .. } => ClockSource::Setting,
        }
    }

    /// The one camera offset that puts wall time `wall` (camera clock
    /// seconds, read as if UTC) in its own clock period and on the track.
    /// The first period reaches back before its start, so photos taken
    /// before the first measurement still match where the track has them.
    fn resolve(&self, track: &Track, wall: i64) -> Resolution {
        let setting;
        let periods: &[ClockPeriod] = match self {
            ClockModel::Measured(p) => p,
            ClockModel::Setting { camera_minus_utc_s } => {
                setting = ClockPeriod {
                    from: i64::MIN,
                    camera_minus_utc_s: *camera_minus_utc_s,
                };
                std::slice::from_ref(&setting)
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
        /// Where that offset came from. Records written before this was
        /// kept have none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        clock: Option<ClockSource>,
    },
}

/// Where the camera clock offset a match used came from.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ClockSource {
    /// LightRemote's measurement of the camera's clock.
    Measured,
    /// The camera's time zone setting the user gave.
    Setting,
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
    /// The track has no clock measurement and the camera no time zone
    /// setting.
    NoClock,
    /// The file has its own GPS, which wins.
    AlreadyHasGps,
}

/// One photo against `track`, with its camera's clock, if known.
/// `capture_local` is its `DateTimeOriginal`, `YYYY:MM:DD HH:MM:SS` on the
/// camera's clock. The location's source names the file holding the nearest
/// fix used and the clock offset.
pub fn locate(
    track: &Track,
    clock: Option<&ClockModel>,
    has_gps: bool,
    capture_local: Option<&str>,
) -> MatchOutcome {
    if has_gps {
        return MatchOutcome::AlreadyHasGps;
    }
    let Some(wall) = capture_local.and_then(wall_seconds) else {
        return MatchOutcome::NoCaptureTime;
    };
    let Some(clock) = clock else {
        return MatchOutcome::NoClock;
    };
    let offset = match clock.resolve(track, wall) {
        Resolution::Offset(o) => o,
        Resolution::Ambiguous => return MatchOutcome::Ambiguous,
        Resolution::Outside => return MatchOutcome::OutsideTrack,
    };
    let Some((gps, file_name)) = wall
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
            file_name: file_name.to_string(),
            camera_minus_utc_s: offset,
            clock: Some(clock.source()),
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

/// A UTC offset as camera-minus-UTC seconds, from text a person or EXIF
/// wrote: `+09:00`, `-5:30`, `+0530`, `9` or `-8`. Within a day either way.
pub fn parse_utc_offset(text: &str) -> Option<i64> {
    let text = text.trim();
    let (sign, rest) = match text
        .strip_prefix('-')
        .or_else(|| text.strip_prefix('\u{2212}'))
    {
        Some(rest) => (-1, rest),
        None => (1, text.strip_prefix('+').unwrap_or(text)),
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) if digits(h) && m.len() == 2 && digits(m) => (h, m),
        Some(_) => return None,
        None if rest.len() == 4 && rest.bytes().all(|b| b.is_ascii_digit()) => {
            (rest.get(..2)?, rest.get(2..)?)
        }
        None if digits(rest) => (rest, "0"),
        None => return None,
    };
    let (h, m): (i64, i64) = (h.parse().ok()?, m.parse().ok()?);
    (h <= 24 && m < 60).then_some(sign * (h * 3600 + m * 60))
}

/// `seconds` east of UTC as `+HH:MM`, rounded down to the minute.
pub fn format_utc_offset(seconds: i64) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.unsigned_abs() / 60;
    format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests;
