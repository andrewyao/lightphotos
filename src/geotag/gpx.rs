// SPDX-License-Identifier: MIT OR Apache-2.0

//! Reading a GPX file into a [`Track`]. The file comes from wherever the user
//! picked it, so everything in it is checked here and nothing past this
//! module re-checks it.

use super::{ClockPeriod, Fix, Track, TrackPoint};

/// The GPX 1.1 and 1.0 namespaces, which lay a track out the same way. A
/// file's points are in its root's namespace; elements in any other are
/// ignored.
const GPX_NS: [&str; 2] = [
    "http://www.topografix.com/GPX/1/1",
    "http://www.topografix.com/GPX/1/0",
];
/// LightRemote's extensions. Only its camera clock measurements are read;
/// its phone time zones say nothing about the camera's clock.
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
    /// Parse the GPX file `file_name`, which names the file in each
    /// location it gives. Track points are sorted by time, and of several
    /// at one second the first in the file is kept. A point with no time or
    /// no position is skipped; a time or a LightRemote extension that is
    /// there but doesn't parse is an error.
    pub fn parse(file_name: &str, bytes: &[u8]) -> Result<Track, GpxError> {
        if bytes.len() > MAX_GPX_BYTES {
            return Err(GpxError::TooLarge { bytes: bytes.len() });
        }
        let text = std::str::from_utf8(bytes).map_err(|e| GpxError::NotXml(e.to_string()))?;
        let doc = roxmltree::Document::parse(text).map_err(|e| GpxError::NotXml(e.to_string()))?;
        let root = doc.root_element();
        let Some(ns) = GPX_NS.into_iter().find(|ns| is(root, ns, "gpx")) else {
            return Err(GpxError::NotGpx);
        };

        let mut points = Vec::new();
        let mut camera_clock = Vec::new();
        for node in root.descendants().filter(roxmltree::Node::is_element) {
            if is(node, ns, "trkpt") {
                points.extend(point(node, ns)?.map(|point| Fix { point, file: 0 }));
            } else if is(node, LR_NS, "cameraClock") {
                camera_clock.push(ClockPeriod {
                    from: time(attr(node, "measuredAt")?)?,
                    camera_minus_utc_s: offset(int(attr(node, "cameraMinusUtcSeconds")?)?)?,
                });
            }
        }
        if points.is_empty() {
            return Err(GpxError::NoPoints);
        }
        let mut track = Track {
            points,
            files: vec![file_name.to_string()],
            camera_clock,
        };
        track.normalize();
        Ok(track)
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

fn child_text<'a>(node: roxmltree::Node<'a, '_>, ns: &str, name: &str) -> Option<&'a str> {
    node.children()
        .find(|c| is(*c, ns, name))
        .and_then(|c| c.text())
}

fn point(node: roxmltree::Node, ns: &str) -> Result<Option<TrackPoint>, GpxError> {
    let Some(t) = child_text(node, ns, "time") else {
        return Ok(None);
    };
    let utc = time(t)?;
    let (Some(lat), Some(lon)) = (
        coordinate(node, "lat", 90.0),
        coordinate(node, "lon", 180.0),
    ) else {
        return Ok(None);
    };
    let ele = child_text(node, ns, "ele")
        .and_then(|e| e.trim().parse::<f64>().ok())
        .filter(|e| e.is_finite());
    Ok(Some(TrackPoint { utc, lat, lon, ele }))
}
