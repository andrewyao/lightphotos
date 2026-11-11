// SPDX-License-Identifier: MIT OR Apache-2.0

//! Which camera took a photo, and how its clock is set: the time zone
//! setting, and whether LightRemote was paired with it and measured it. A
//! camera's clock keeps whatever zone its owner set, so this is asked once
//! per camera and kept in the app's prefs.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::decode::image_decode::ImageMetadata;
use crate::geotag::CameraZone;

/// The prefs key holding every camera's setting, as JSON.
const PREF: &str = "camera_time_zones";
/// The most cameras remembered, so a hostile prefs file can't grow the map
/// without bound.
const MAX_CAMERAS: usize = 256;
/// The longest model or serial kept, in chars.
const MAX_FIELD_CHARS: usize = 64;

/// A camera: its EXIF Model and, when the file has one, its body serial.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct CameraKey {
    pub(crate) model: String,
    pub(crate) serial: Option<String>,
}

impl CameraKey {
    pub(crate) fn of(meta: &ImageMetadata) -> CameraKey {
        CameraKey {
            model: meta.camera_model.as_deref().map(clean).unwrap_or_default(),
            serial: meta
                .camera_serial
                .as_deref()
                .map(clean)
                .filter(|s| !s.is_empty()),
        }
    }

    /// The key in the prefs map: the model, then `#` and the serial.
    pub(crate) fn pref_key(&self) -> String {
        match &self.serial {
            Some(serial) => format!("{}#{serial}", self.model),
            None => self.model.clone(),
        }
    }

    /// How the dialog names the camera. `None` for a photo with no model.
    pub(crate) fn label(&self) -> Option<String> {
        if self.model.is_empty() {
            return None;
        }
        Some(match &self.serial {
            Some(serial) => format!("{} · {serial}", self.model),
            None => self.model.clone(),
        })
    }
}

/// Give each photo with no serial the serial of its model's one numbered
/// body, as a body's JPEGs can lack the serial its RAWs carry. A model
/// with several numbered bodies is left alone: which one is unknown.
pub(crate) fn fold_serials<'a>(cameras: impl IntoIterator<Item = &'a mut CameraKey>) {
    let mut cameras: Vec<&mut CameraKey> = cameras.into_iter().collect();
    let mut serials: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for c in &cameras {
        if let Some(serial) = &c.serial {
            serials
                .entry(c.model.clone())
                .or_default()
                .insert(serial.clone());
        }
    }
    for c in cameras.iter_mut().filter(|c| c.serial.is_none()) {
        let only = serials.get(&c.model).filter(|s| s.len() == 1);
        c.serial = only.and_then(BTreeSet::first).cloned();
    }
}

/// How a camera's clock is set, as the import dialog asks it.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CameraClock {
    #[serde(flatten)]
    pub(crate) zone: CameraZone,
    /// LightRemote was paired with this camera, so the clock measurements
    /// in its tracks are this camera's.
    #[serde(default)]
    pub(crate) paired: bool,
}

/// Trimmed, without control characters, and capped.
fn clean(s: &str) -> String {
    s.trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_FIELD_CHARS)
        .collect()
}

/// Each camera's remembered clock, by [`CameraKey::pref_key`].
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct CameraClocks(BTreeMap<String, CameraClock>);

impl CameraClocks {
    pub(crate) fn load() -> CameraClocks {
        crate::persist::prefs::load(PREF)
            .map(|json| CameraClocks::parse(&json))
            .unwrap_or_default()
    }

    /// The settings in `json`. Entries that don't read, or name an offset
    /// a camera's menu doesn't offer, are dropped; JSON that doesn't read
    /// at all gives none.
    pub(crate) fn parse(json: &str) -> CameraClocks {
        let Ok(raw) = serde_json::from_str::<BTreeMap<String, serde_json::Value>>(json) else {
            return CameraClocks::default();
        };
        CameraClocks(
            raw.into_iter()
                .filter_map(|(k, v)| {
                    let clock: CameraClock = serde_json::from_value(v).ok()?;
                    clock.zone.is_valid().then_some((k, clock))
                })
                .take(MAX_CAMERAS)
                .collect(),
        )
    }

    pub(crate) fn to_json(&self) -> String {
        serde_json::to_string(&self.0).unwrap_or_else(|_| "{}".into())
    }

    /// `camera`'s remembered clock. One with no serial that was never asked
    /// about takes its model's one remembered numbered body's: a body's
    /// JPEGs can lack the serial its RAWs carry, and a run that reads a
    /// batch at a time may not see the RAWs.
    pub(crate) fn get(&self, camera: &CameraKey) -> Option<CameraClock> {
        if let Some(clock) = self.0.get(&camera.pref_key()) {
            return Some(*clock);
        }
        if camera.serial.is_some() {
            return None;
        }
        let prefix = format!("{}#", camera.model);
        let mut bodies = self
            .0
            .range(prefix.clone()..)
            .take_while(|(key, _)| key.starts_with(&prefix));
        match (bodies.next(), bodies.next()) {
            (Some((_, clock)), None) => Some(*clock),
            _ => None,
        }
    }

    /// Remember `clock` for `camera`. Past [`MAX_CAMERAS`], a new camera is
    /// not remembered.
    pub(crate) fn set(&mut self, camera: &CameraKey, clock: CameraClock) {
        let key = camera.pref_key();
        if self.0.len() < MAX_CAMERAS || self.0.contains_key(&key) {
            self.0.insert(key, clock);
        }
    }

    pub(crate) fn save(&self) {
        if let Err(e) = crate::persist::prefs::save(PREF, &self.to_json()) {
            eprintln!("[lightphotos] couldn't save the camera clocks: {e}");
        }
    }
}
