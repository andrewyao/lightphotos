// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a GPX file into a [`Track`]. The file comes from wherever the user
//! picked it, so everything in it is checked here and nothing past this
//! module re-checks it.

use super::{ClockPeriod, Track, TrackPoint};

/// The GPX 1.1 namespace. Elements in any other namespace are ignored.
const GPX_NS: &str = "http://www.topografix.com/GPX/1/1";
/// LightRemote's extensions: camera clock measurements and phone time zones.
pub const LR_NS: &str = "https://lightremote.app/gpx/1";

/// A day at one fix a second is about 15 MB of GPX. Past this the file is
/// not a track we can use, and parsing it whole could exhaust memory.
pub const MAX_GPX_BYTES: usize = 64 << 20;

/// Why a GPX file could not be read.
#[derive(Debug, Clone, PartialEq)]
pub enum GpxError {
    TooLarge {
        bytes: usize,
    },
    NotXml(String),
    NotGpx,
    /// An attribute or a timestamp that doesn't parse.
    Malformed(String),
    /// No point has both a position and a time.
    NoPoints,
}

impl std::fmt::Display for GpxError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GpxError::TooLarge { bytes } => {
                write!(
                    f,
                    "the GPX file is {bytes} bytes, over the {MAX_GPX_BYTES}-byte limit"
                )
            }
            GpxError::NotXml(e) => write!(f, "the GPX file is not valid XML: {e}"),
            GpxError::NotGpx => f.write_str("the file is not a GPX track"),
            GpxError::Malformed(e) => write!(f, "the GPX file is malformed: {e}"),
            GpxError::NoPoints => f.write_str("the GPX file has no timed track points"),
        }
    }
}

impl std::error::Error for GpxError {}

impl Track {
    /// Parse GPX bytes. Track points are sorted by time, and of several at
    /// one second the first in the file is kept. A point with no time or
    /// no position is skipped; a time or a LightRemote extension that is
    /// there but doesn't parse is an error.
    pub fn parse(bytes: &[u8]) -> Result<Track, GpxError> {
        if bytes.len() > MAX_GPX_BYTES {
            return Err(GpxError::TooLarge { bytes: bytes.len() });
        }
        let text = std::str::from_utf8(bytes).map_err(|e| GpxError::NotXml(e.to_string()))?;
        let doc = roxmltree::Document::parse(text).map_err(|e| GpxError::NotXml(e.to_string()))?;
        let root = doc.root_element();
        if !is(root, GPX_NS, "gpx") {
            return Err(GpxError::NotGpx);
        }

        let mut points = Vec::new();
        let mut camera_clock = Vec::new();
        let mut utc_offsets = Vec::new();
        for node in root.descendants().filter(roxmltree::Node::is_element) {
            if is(node, GPX_NS, "trkpt") {
                points.extend(point(node)?);
            } else if is(node, LR_NS, "cameraClock") {
                camera_clock.push(ClockPeriod {
                    from: time(attr(node, "measuredAt")?)?,
                    camera_minus_utc_s: offset(int(attr(node, "cameraMinusUtcSeconds")?)?)?,
                });
            } else if is(node, LR_NS, "utcOffset") {
                let minutes = int(attr(node, "minutes")?)?;
                check_zone(minutes)?;
                utc_offsets.push(ClockPeriod {
                    from: time(attr(node, "from")?)?,
                    camera_minus_utc_s: minutes * 60,
                });
            }
        }
        if points.is_empty() {
            return Err(GpxError::NoPoints);
        }
        points.sort_by_key(|p: &TrackPoint| p.utc);
        points.dedup_by_key(|p| p.utc);
        camera_clock.sort_by_key(|c| (c.from, c.camera_minus_utc_s));
        camera_clock.dedup();
        utc_offsets.sort_by_key(|c| (c.from, c.camera_minus_utc_s));
        utc_offsets.dedup();
        Ok(Track {
            points,
            camera_clock,
            utc_offsets,
        })
    }
}

fn is(node: roxmltree::Node, ns: &str, name: &str) -> bool {
    node.tag_name().namespace() == Some(ns) && node.tag_name().name() == name
}

fn attr<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Result<&'a str, GpxError> {
    node.attribute(name).ok_or_else(|| {
        GpxError::Malformed(format!(
            "<{}> has no {name} attribute",
            node.tag_name().name()
        ))
    })
}

fn int(s: &str) -> Result<i64, GpxError> {
    s.trim()
        .parse()
        .map_err(|_| GpxError::Malformed(format!("{s:?} is not a whole number")))
}

/// UTC offsets run from -12:00 to +14:00; a day either side is room for any
/// zone and refuses a number that would overflow later arithmetic.
fn check_zone(minutes: i64) -> Result<(), GpxError> {
    match minutes.abs() <= 24 * 60 {
        true => Ok(()),
        false => Err(GpxError::Malformed(format!(
            "{minutes} minutes is not a UTC offset"
        ))),
    }
}

fn offset(seconds: i64) -> Result<i64, GpxError> {
    match seconds.abs() <= super::MAX_CLOCK_ERROR_S {
        true => Ok(seconds),
        false => Err(GpxError::Malformed(format!(
            "{seconds} s is not a camera clock offset"
        ))),
    }
}

/// Unix seconds for an RFC 3339 time, which must carry its offset: a time
/// with none could be in any zone. Fractions of a second are dropped.
fn time(s: &str) -> Result<i64, GpxError> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .map(|t| t.timestamp())
        .map_err(|_| GpxError::Malformed(format!("{s:?} is not a time with a UTC offset")))
}

fn coordinate(node: roxmltree::Node, name: &str, limit: f64) -> Option<f64> {
    node.attribute(name)?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && v.abs() <= limit)
}

fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| is(*c, GPX_NS, name))
        .and_then(|c| c.text())
}

fn point(node: roxmltree::Node) -> Result<Option<TrackPoint>, GpxError> {
    let Some(t) = child_text(node, "time") else {
        return Ok(None);
    };
    let utc = time(t)?;
    let (Some(lat), Some(lon)) = (
        coordinate(node, "lat", 90.0),
        coordinate(node, "lon", 180.0),
    ) else {
        return Ok(None);
    };
    let ele = child_text(node, "ele")
        .and_then(|e| e.trim().parse::<f64>().ok())
        .filter(|e| e.is_finite());
    Ok(Some(TrackPoint { utc, lat, lon, ele }))
}
