// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-photo sidecars that persist ratings and develop edits. Each photo's
//! record lives in `<photo dir>/.lightphotos/<photo filename>.xmp`, so edits
//! travel with the folder and originals are never touched. The body is our
//! own JSON, not Adobe XMP; the extension is cosmetic. Natively, opening a
//! folder creates `.lightphotos/` for its lock file and nothing else (see
//! [`FolderLock`]); a photo's sidecar appears on its first edit.
//!
//! This module owns the record, its path and its single-file read and write.
//! The app's folder store caches a folder of them and queues the writes.

#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use crate::develop::{Adjustments, Detections, Mask, MaskShape, TouchUp, MAX_MASKS};
use crate::scoring::quality::QualityScore;

#[cfg(not(target_arch = "wasm32"))]
mod lock;
#[cfg(not(target_arch = "wasm32"))]
pub use lock::{
    probe, FolderKey, FolderLock, Holder, LockError, Writer, LOCK_FILE, LOCK_WAIT, WHO_FILE,
};

/// Hidden subfolder holding a directory's sidecars and thumbnail cache.
pub const SIDECAR_DIR: &str = ".lightphotos";
/// Sidecar file extension. Cosmetic; see the module docs.
pub const SIDECAR_EXT: &str = "xmp";
/// A real record is a few kilobytes even with every touch-up spot. A bigger
/// file is not ours, and reading it whole could exhaust memory.
pub const MAX_SIDECAR_BYTES: usize = 1 << 20;

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

/// Persisted state for one photo. Default fields are skipped on write, so a
/// rated but unedited photo's sidecar stays small.
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ImageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flag: Option<Flag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<ColorLabel>,
    #[serde(default, skip_serializing_if = "Adjustments::is_identity")]
    pub adjustments: Adjustments,
    #[serde(
        default,
        skip_serializing_if = "Vec::is_empty",
        deserialize_with = "at_most_max_touchups"
    )]
    pub touchups: Vec<TouchUp>,
    #[serde(default, skip_serializing_if = "MaskList::is_empty")]
    pub masks: MaskList,
    /// Which coverage file answers each detected-mask target. Beside
    /// `masks`, not inside them, so a pasted or undone mask finds the photo's
    /// own detection by target. A fact about the photo, not an edit: it is
    /// written outside the undo history.
    #[serde(default, skip_serializing_if = "Detections::is_empty")]
    pub detections: Detections,
    /// Manual rotation in 90° clockwise steps, `0..=3`.
    #[serde(
        default,
        skip_serializing_if = "is_zero_rot",
        deserialize_with = "quarter_turns"
    )]
    pub rotation: u8,
    /// Kept here rather than in `signals.json` because it is measured on the
    /// edited photo, so it travels with the edits it describes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<StoredScore>,
    /// Unix seconds when the edits last changed, for the Grid's Edit Time
    /// sort. `None` while the photo has no edits.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edited: Option<u64>,
}

/// A quality score and the edits it was measured on.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct StoredScore {
    #[serde(flatten)]
    pub score: QualityScore,
    /// [`ImageRecord::edit_signature`] when scored. A different signature
    /// now means the photo was edited since, and the score is stale.
    pub edits: u64,
}

/// A sidecar comes from whoever supplied the folder, and past
/// [`crate::develop::MAX_TOUCHUPS`] the renderer's buffer overflows, so extra
/// spots are dropped on load.
fn at_most_max_touchups<'de, D>(deserializer: D) -> Result<Vec<TouchUp>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let mut touchups = Vec::<TouchUp>::deserialize(deserializer)?;
    touchups.truncate(crate::develop::MAX_TOUCHUPS);
    Ok(touchups)
}

/// A photo's masks as its sidecar holds them: the ones this build reads, and
/// any whose `kind` it doesn't know, kept as the JSON they came as and
/// written back where they stood. A folder shared with a later build keeps
/// that build's masks through every write of this one. Foreign entries
/// render nothing and the panel never lists them; `Deref` hands out only the
/// known masks.
///
/// A sidecar's masks come from whoever supplied the folder. An entry of a
/// known kind that isn't a mask, or holds a number that isn't one, is dropped
/// rather than failing the whole record and its rating; the rest are clamped
/// into range, and past [`MAX_MASKS`] the renderer's buffer overflows.
#[derive(Clone, Debug, Default)]
pub struct MaskList {
    known: Vec<Mask>,
    /// Each with its index in the list as read.
    foreign: Vec<(usize, Box<RawValue>)>,
}

impl MaskList {
    pub fn is_empty(&self) -> bool {
        self.known.is_empty() && self.foreign.is_empty()
    }

    /// Replaces the masks this build reads; foreign entries stay.
    pub fn set(&mut self, known: Vec<Mask>) {
        self.known = known;
    }

    /// A mask entry of a kind this build doesn't read, or a detected one
    /// whose target it doesn't know, to keep as it came. Anything else that
    /// failed to parse is garbage, and dropped.
    fn is_foreign(raw: &RawValue) -> bool {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(raw.get()) else {
            return false;
        };
        let Some(shape) = v.get("shape") else {
            return false;
        };
        let Some(kind) = shape.get("kind").and_then(serde_json::Value::as_str) else {
            return false;
        };
        if !MaskShape::KINDS.contains(&kind) {
            return true;
        }
        kind == "detected"
            && shape
                .get("target")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|target| !crate::develop::DetectTarget::NAMES.contains(&target))
    }
}

impl From<Vec<Mask>> for MaskList {
    fn from(known: Vec<Mask>) -> Self {
        MaskList {
            known,
            foreign: Vec::new(),
        }
    }
}

impl std::ops::Deref for MaskList {
    type Target = [Mask];
    fn deref(&self) -> &[Mask] {
        &self.known
    }
}

impl PartialEq for MaskList {
    fn eq(&self, other: &Self) -> bool {
        self.known == other.known
            && self.foreign.len() == other.foreign.len()
            && self
                .foreign
                .iter()
                .zip(&other.foreign)
                .all(|((i, a), (j, b))| i == j && a.get() == b.get())
    }
}

impl Serialize for MaskList {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        #[serde(untagged)]
        enum Entry<'a> {
            Mask(&'a Mask),
            Foreign(&'a RawValue),
        }
        let mut out = Vec::with_capacity(self.known.len() + self.foreign.len());
        let mut known = self.known.iter();
        let mut foreign = self.foreign.iter().peekable();
        loop {
            match foreign.peek() {
                Some((at, raw)) if *at == out.len() => {
                    out.push(Entry::Foreign(raw));
                    foreign.next();
                }
                _ => match known.next() {
                    Some(m) => out.push(Entry::Mask(m)),
                    None => break,
                },
            }
        }
        // A foreign entry past the end, after the masks before it were
        // deleted, keeps its order after the rest.
        out.extend(foreign.map(|(_, raw)| Entry::Foreign(raw)));
        out.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for MaskList {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut list = MaskList::default();
        let mut points_left = crate::develop::mask::MAX_BRUSH_POINTS;
        for (at, raw) in Vec::<Box<RawValue>>::deserialize(deserializer)?
            .into_iter()
            .enumerate()
        {
            match serde_json::from_str::<Mask>(raw.get()) {
                Ok(m) => {
                    if list.known.len() < MAX_MASKS {
                        list.known.extend(m.sanitized_within(&mut points_left));
                    }
                }
                Err(_) if MaskList::is_foreign(&raw) && list.foreign.len() < MAX_MASKS => {
                    list.foreign.push((at, raw));
                }
                Err(_) => {}
            }
        }
        Ok(list)
    }
}

/// A sidecar's rotation is whatever u8 its writer put there; only `0..=3`
/// is a rotation, so the rest wraps on load.
fn quarter_turns<'de, D>(deserializer: D) -> Result<u8, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(u8::deserialize(deserializer)? % 4)
}

fn is_zero_rot(v: &u8) -> bool {
    *v == 0
}

impl ImageRecord {
    /// True when there is nothing to persist. An empty record's sidecar is
    /// deleted instead of written.
    ///
    /// This restates what the `skip_serializing_if` attributes already say,
    /// because it runs on every rating keystroke and serializing there would
    /// cost a `Map` allocation per photo. The debug assertion keeps the two
    /// honest: a field added to the struct but not to this conjunction fails
    /// the moment any test sets it.
    pub fn is_empty(&self) -> bool {
        let empty = self.rating.is_none()
            && self.flag.is_none()
            && self.label.is_none()
            && !self.has_edits()
            && self.masks.is_empty()
            && self.detections.is_empty()
            && self.score.is_none()
            && self.edited.is_none();
        debug_assert_eq!(
            empty,
            self.serializes_to_nothing(),
            "ImageRecord::is_empty disagrees with what the sidecar would hold; \
             a field was added to the struct but not to is_empty"
        );
        empty
    }

    /// A hash of the edits that change how the photo renders.
    pub fn edit_signature(&self) -> u64 {
        crate::develop::edit_signature_with_touchups(
            &self.adjustments,
            &self.touchups,
            &self.masks,
            &self.detections,
            self.rotation,
        )
    }

    /// The masks with their coverage read from beside `photo`, for a bake.
    /// A recorded coverage that is missing or corrupt is an error, never a
    /// silently unmasked render.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn mask_set(&self, photo: &Path) -> Result<crate::develop::MaskSet, String> {
        crate::develop::MaskSet::read(photo, &self.masks, &self.detections).map_err(|e| {
            format!(
                "{}: {e}",
                photo.file_name().map_or_else(
                    || photo.display().to_string(),
                    |n| n.to_string_lossy().into_owned()
                )
            )
        })
    }

    /// Whether an edit this build renders is set. A foreign mask is kept
    /// but renders nothing, so it is not one; `is_empty` still counts it.
    fn has_edits(&self) -> bool {
        !self.adjustments.is_identity()
            || !self.touchups.is_empty()
            || !self.masks.known.is_empty()
            || self.rotation != 0
    }

    /// Run `edit`, then stamp `edited` with `now` if it changed how the
    /// photo renders, or clear it if no edit is left.
    pub fn edit(&mut self, now: u64, edit: impl FnOnce(&mut ImageRecord)) {
        let before = self.edit_signature();
        edit(self);
        if !self.has_edits() {
            self.edited = None;
        } else if self.edit_signature() != before {
            self.edited = Some(now);
        }
    }

    /// Bring a record read from disk in line with its edits: no edits, no
    /// edit time. A sidecar written before edit times were kept takes
    /// `saved_at`, when the file was last written.
    pub fn settle_edited(&mut self, saved_at: impl FnOnce() -> Option<u64>) {
        if !self.has_edits() {
            self.edited = None;
        } else if self.edited.is_none() {
            self.edited = saved_at();
        }
    }

    /// Whether this record's sidecar would be `{}`, read off the serde
    /// attributes rather than a second hand-written list.
    fn serializes_to_nothing(&self) -> bool {
        matches!(
            serde_json::to_value(self),
            Ok(serde_json::Value::Object(fields)) if fields.is_empty()
        )
    }
}

/// Unix seconds now, the time [`ImageRecord::edit`] stamps.
pub fn unix_now() -> u64 {
    web_time::SystemTime::now()
        .duration_since(web_time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Parse a sidecar's bytes, and settle its edit time with `saved_at`, the
/// file's modification time.
pub fn parse_record(
    bytes: &[u8],
    saved_at: impl FnOnce() -> Option<u64>,
) -> Result<ImageRecord, String> {
    if bytes.len() > MAX_SIDECAR_BYTES {
        return Err(format!(
            "sidecar is {} bytes, over the {MAX_SIDECAR_BYTES}-byte limit",
            bytes.len()
        ));
    }
    let mut rec: ImageRecord = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    rec.settle_edited(saved_at);
    Ok(rec)
}

/// `<dir>/.lightphotos/<filename>.xmp`, or `None` for a path like `/` or `..`.
/// Built from `OsString` so non-UTF-8 names stay exact.
#[cfg(not(target_arch = "wasm32"))]
pub fn sidecar_path(photo: &Path) -> Option<PathBuf> {
    let dir = photo.parent()?;
    let name = photo.file_name()?;
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".");
    sidecar_name.push(SIDECAR_EXT);
    Some(dir.join(SIDECAR_DIR).join(sidecar_name))
}

/// Read and parse one sidecar file. A file over [`MAX_SIDECAR_BYTES`] is
/// refused without reading it whole.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_sidecar_file(sidecar: &Path) -> Result<ImageRecord, String> {
    read_open_sidecar(std::fs::File::open(sidecar).map_err(|e| e.to_string())?)
}

#[cfg(not(target_arch = "wasm32"))]
fn read_open_sidecar(file: std::fs::File) -> Result<ImageRecord, String> {
    use std::io::Read;
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_SIDECAR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    parse_record(&bytes, || {
        let saved = file.metadata().ok()?.modified().ok()?;
        Some(saved.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
    })
}

/// The record saved for `photo`, or an empty one when it has no sidecar.
/// A sidecar that is there but cannot be opened is an error, so a caller
/// never writes over a record it could not read.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_record(photo: &Path) -> Result<ImageRecord, String> {
    let sidecar = sidecar_path(photo).ok_or("cannot determine sidecar path")?;
    match std::fs::File::open(&sidecar) {
        Ok(file) => read_open_sidecar(file),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ImageRecord::default()),
        Err(e) => Err(format!("{}: {e}", sidecar.display())),
    }
}

/// Why a sidecar couldn't be read.
#[derive(Debug)]
pub enum ReadError {
    /// Opening or reading the file failed. `lasting` when it won't pass
    /// (no permission, not a file).
    Io { why: String, lasting: bool },
    /// It read, but isn't a record this build understands: it doesn't parse,
    /// or is over [`MAX_SIDECAR_BYTES`].
    Unreadable(String),
}

#[cfg(not(target_arch = "wasm32"))]
fn io_error(sidecar: &Path, e: &std::io::Error) -> ReadError {
    ReadError::Io {
        why: format!("{}: {e}", sidecar.display()),
        lasting: lock::lasting(e.kind()),
    }
}

/// [`read_record`], telling an I/O failure from a file that read but isn't
/// a record.
#[cfg(not(target_arch = "wasm32"))]
pub fn read_record_checked(photo: &Path) -> Result<ImageRecord, ReadError> {
    use std::io::Read;
    let sidecar = sidecar_path(photo).ok_or_else(|| ReadError::Io {
        why: "cannot determine sidecar path".into(),
        lasting: true,
    })?;
    let file = match std::fs::File::open(&sidecar) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(ImageRecord::default()),
        Err(e) => return Err(io_error(&sidecar, &e)),
    };
    let mut bytes = Vec::new();
    (&file)
        .take(MAX_SIDECAR_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| io_error(&sidecar, &e))?;
    parse_record(&bytes, || {
        let saved = file.metadata().ok()?.modified().ok()?;
        Some(saved.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs())
    })
    .map_err(ReadError::Unreadable)
}

/// Save `rec` as `photo`'s sidecar, atomically, creating `.lightphotos/` if
/// needed. An empty record deletes the sidecar instead.
#[cfg(not(target_arch = "wasm32"))]
pub fn write_record(photo: &Path, rec: &ImageRecord) -> Result<(), String> {
    if rec.is_empty() {
        return delete_record(photo);
    }
    let sidecar = sidecar_path(photo).ok_or("cannot determine sidecar path")?;
    let bytes = serde_json::to_vec_pretty(rec).map_err(|e| e.to_string())?;
    crate::paths::write_atomic_in_dir(&sidecar, &bytes).map_err(|e| e.to_string())
}

/// Delete `photo`'s sidecar. A photo with none is already done.
#[cfg(not(target_arch = "wasm32"))]
pub fn delete_record(photo: &Path) -> Result<(), String> {
    match sidecar_path(photo) {
        Some(sidecar) => crate::paths::remove_if_present(&sidecar).map_err(|e| e.to_string()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scoring::quality::{Basis, Deduction, Penalty};

    fn sample_score() -> QualityScore {
        QualityScore {
            value: 73,
            basis: Basis::WithAesthetics,
            base: Some(80),
            deductions: vec![Deduction {
                penalty: Penalty::Noisy,
                points: 7,
            }],
        }
    }

    #[test]
    fn a_saved_rotation_past_three_quarter_turns_wraps_on_load() {
        let rec = parse_record(br#"{"rotation":255}"#, || None).unwrap();
        assert_eq!(rec.rotation, 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_sidecar_that_cannot_be_opened_is_an_error_not_an_empty_record() {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-sidecar-eloop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        let photo = dir.join("a.jpg");
        assert_eq!(
            read_record(&photo),
            Ok(ImageRecord::default()),
            "no sidecar"
        );

        let sidecar = sidecar_path(&photo).unwrap();
        std::os::unix::fs::symlink(&sidecar, &sidecar).unwrap();
        let read = read_record(&photo);
        assert!(read.is_err(), "a looping sidecar link: {read:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The defect this guards is a new `ImageRecord` field that `is_empty`
    /// forgets, which makes a cleared photo keep writing a sidecar instead of
    /// deleting it. Every field is set through the serialized form, so the
    /// check is the serde attributes rather than a second hand-written list.
    #[test]
    fn every_persisted_field_on_its_own_makes_a_record_non_empty() {
        let populated = ImageRecord {
            rating: Some(3),
            label: Some(ColorLabel::Red),
            adjustments: Adjustments {
                exposure: 0.5,
                ..Default::default()
            },
            touchups: vec![TouchUp {
                center: [0.4, 0.5],
                radius: 0.02,
                source: [0.6, 0.5],
                feather: 0.5,
                delta: [0.01, -0.02, 0.0],
                opacity: 1.0,
            }],
            masks: vec![Mask::linear([0.5, 0.0], [0.5, 1.0])].into(),
            detections: {
                let mut d = Detections::default();
                d.set(
                    crate::develop::DetectTarget::Subject,
                    "0123456789abcdef".parse().ok(),
                );
                d
            },
            rotation: 1,
            score: Some(StoredScore {
                score: sample_score(),
                edits: 7,
            }),
            ..Default::default()
        };

        let serde_json::Value::Object(fields) = serde_json::to_value(&populated).unwrap() else {
            panic!("a record serializes to an object");
        };
        assert_eq!(
            fields.len(),
            8,
            "set every field of ImageRecord here, got {fields:?}"
        );

        for name in fields.keys() {
            let mut only = serde_json::Map::new();
            only.insert(name.clone(), fields[name].clone());
            let rec: ImageRecord = serde_json::from_value(serde_json::Value::Object(only)).unwrap();
            assert!(
                !rec.is_empty(),
                "a record holding only {name} must still be written"
            );
        }

        assert!(ImageRecord::default().is_empty());
    }

    #[test]
    fn a_sidecar_loads_at_most_max_touchups() {
        let spot =
            r#"{"center":[0.5,0.5],"radius":0.1,"source":[0.2,0.2],"feather":0.5,"delta":[0,0,0]}"#;
        let spots = vec![spot; crate::develop::MAX_TOUCHUPS + 36].join(",");
        let rec: ImageRecord =
            serde_json::from_str(&format!(r#"{{"rating":2,"touchups":[{spots}]}}"#)).unwrap();
        assert_eq!(rec.touchups.len(), crate::develop::MAX_TOUCHUPS);
        assert_eq!(rec.rating, Some(2));
    }

    #[test]
    fn a_sidecar_keeps_its_good_masks_and_drops_or_clamps_the_rest() {
        let good =
            r#"{"shape":{"kind":"linear","start":[0.5,0],"end":[0.5,1]},"amounts":{"exposure":1}}"#;
        let body = format!(
            r#"{{"rating":2,"masks":[
                {good},
                {{"shape":{{"kind":"lasso","points":[]}}}},
                {{"shape":{{"kind":"radial","center":[0.5,0.5],"radii":[-1,0.2]}},"amounts":{{"temp":900}}}},
                {{"shape":{{"kind":"linear","start":[1e39,0],"end":[0.5,1]}}}},
                "not a mask",
                {{"shape":{{"kind":"radial","center":[0.5],"radii":[0.1,0.1]}}}}
            ]}}"#
        );
        let rec = parse_record(body.as_bytes(), || None).unwrap();
        assert_eq!(rec.rating, Some(2), "a bad mask doesn't cost the rating");
        assert_eq!(rec.masks.len(), 2, "{:?}", rec.masks);
        assert_eq!(rec.masks[0].amounts.exposure, 1.0);
        let crate::develop::MaskShape::Radial { radii, .. } = rec.masks[1].shape else {
            panic!("the radial mask survives clamped: {:?}", rec.masks[1]);
        };
        assert_eq!(radii, [1e-4, 0.2]);
        assert_eq!(rec.masks[1].amounts.temp, 100.0);
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains(r#""kind":"lasso""#),
            "kept as foreign: {json}"
        );
        assert!(!json.contains("not a mask"), "garbage is dropped: {json}");
        assert!(
            !json.contains("1e39"),
            "a bad known kind is dropped: {json}"
        );
    }

    /// The defect: a detected mask for a target a later build added was
    /// dropped as garbage, since its kind is known, and the next write lost
    /// it. One with a target that isn't even a name is still garbage.
    #[test]
    fn a_detected_mask_of_an_unknown_target_is_kept_as_foreign() {
        let person = r#"{"shape":{"kind":"detected","target":"person"},"amounts":{"exposure":1}}"#;
        let body = format!(
            r#"{{"masks":[{person},{{"shape":{{"kind":"detected","target":5}}}},{{"shape":{{"kind":"detected","target":"sky"}}}}]}}"#
        );
        let rec = parse_record(body.as_bytes(), || None).unwrap();
        assert_eq!(rec.masks.len(), 1, "only the sky mask is this build's");
        let written = serde_json::to_string(&rec).unwrap();
        assert!(written.contains(person), "{written}");
        assert!(!written.contains(r#""target":5"#), "{written}");
        assert_eq!(parse_record(written.as_bytes(), || None).unwrap(), rec);
    }

    /// The defect: a build that doesn't know a mask kind drops it on load and
    /// the next write of that sidecar loses it for the build that does.
    #[test]
    fn a_mask_of_an_unknown_kind_survives_a_write_in_place_byte_for_byte() {
        let foreign = r#"{"shape": {"kind":"lasso",  "points":[[0.1,0.2],[0.3,0.4]]}, "amounts": {"exposure": 0.5}, "future": true}"#;
        let known = r#"{"shape":{"kind":"radial","center":[0.5,0.5],"radii":[0.1,0.1]}}"#;
        let body = format!(r#"{{"masks":[{known},{foreign},{known}],"rating":1}}"#);
        let mut rec = parse_record(body.as_bytes(), || None).unwrap();
        assert_eq!(rec.masks.len(), 2);
        assert!(!rec.masks.is_empty());

        let written = serde_json::to_string(&rec).unwrap();
        let back: serde_json::Value = serde_json::from_str(&written).unwrap();
        let entries = back["masks"].as_array().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1]["shape"]["kind"], "lasso", "in place: {written}");
        assert!(written.contains(foreign), "verbatim: {written}");

        let reread = parse_record(written.as_bytes(), || None).unwrap();
        assert_eq!(reread, rec, "a second build round-trips it again");

        rec.masks.set(Vec::new());
        assert!(!rec.masks.is_empty(), "the foreign mask still persists");
        rec.rating = None;
        assert!(!rec.is_empty(), "so the sidecar is written, not deleted");
        let written = serde_json::to_string(&rec).unwrap();
        assert!(written.contains(foreign), "{written}");
        let only_foreign = parse_record(written.as_bytes(), || None).unwrap();
        assert_eq!(only_foreign.masks.len(), 0);
        assert_eq!(
            only_foreign.edit_signature(),
            ImageRecord::default().edit_signature(),
            "a foreign mask renders nothing, so it signs as nothing"
        );
    }

    #[test]
    fn a_brush_mask_round_trips_and_a_malformed_one_is_dropped() {
        let brush = r#"{"shape":{"kind":"brush","strokes":[{"radius":0.05,"feather":0.5,"flow":0.8,"points":[[0.1,0.2],[0.3,0.4]]},{"radius":0.02,"feather":0,"flow":1,"erase":true,"points":[[0.2,0.3]]}]},"amounts":{"exposure":1}}"#;
        let body = format!(
            r#"{{"rating":3,"masks":[
                {brush},
                {{"shape":{{"kind":"brush","strokes":"lots"}}}},
                {{"shape":{{"kind":"brush","strokes":[{{"radius":0.1}}]}}}},
                {{"shape":{{"kind":"brush"}}}}
            ]}}"#
        );
        let rec = parse_record(body.as_bytes(), || None).unwrap();
        assert_eq!(rec.rating, Some(3));
        assert_eq!(rec.masks.len(), 1, "{:?}", rec.masks);
        let strokes = rec.masks[0].strokes().unwrap();
        assert_eq!(strokes.len(), 2);
        assert!(strokes[1].erase);
        let written = serde_json::to_string(&rec).unwrap();
        assert!(!written.contains("lots"), "garbage is dropped: {written}");
        assert_eq!(parse_record(written.as_bytes(), || None).unwrap(), rec);
    }

    #[test]
    fn a_sidecar_over_the_brush_point_cap_loads_truncated() {
        use crate::develop::mask::{MAX_BRUSH_POINTS, STROKE_POINTS};
        let points = vec!["[0.5,0.5]"; MAX_BRUSH_POINTS / 2 + 10].join(",");
        let mask = format!(
            r#"{{"shape":{{"kind":"brush","strokes":[{{"radius":0.05,"feather":0.5,"flow":1,"points":[{points}]}}]}}}}"#
        );
        let inverted = mask.replacen(r#"{"shape""#, r#"{"invert":true,"shape""#, 1);
        let body = format!(r#"{{"masks":[{mask},{mask},{inverted}]}}"#);
        let rec = parse_record(body.as_bytes(), || None).unwrap();
        let total: usize = rec.masks.iter().map(Mask::brush_points).sum();
        assert_eq!(total, MAX_BRUSH_POINTS);
        assert_eq!(
            rec.masks[0].brush_points(),
            MAX_BRUSH_POINTS / 2 + 10 + STROKE_POINTS
        );
        // Kept with no strokes, the inverted mask would cover the photo.
        assert_eq!(rec.masks.len(), 2, "a brush cut to nothing is dropped");
    }

    /// The defect: the point cap once let a photo's strokes write a sidecar
    /// past `MAX_SIDECAR_BYTES`, which the next read refuses whole, rating
    /// and all. Both ways to spend the cap must still read back.
    #[test]
    fn a_sidecar_at_the_brush_cap_writes_under_the_size_limit() {
        use crate::develop::mask::{MAX_BRUSH_POINTS, STROKE_POINTS};
        use crate::develop::{MaskShape, Stroke};
        let stroke = |n: usize| Stroke {
            radius: 0.12345,
            feather: 0.12345,
            flow: 0.12345,
            erase: true,
            points: vec![[-9.12345, -9.12345]; n],
        };
        let brush = |strokes: Vec<Stroke>| {
            let mut m = Mask::brush();
            m.shape = MaskShape::Brush {
                strokes: strokes.into(),
            };
            m.amounts.exposure = -1.2345;
            m
        };
        let singles = MAX_BRUSH_POINTS / (1 + STROKE_POINTS);
        for masks in [
            vec![brush(vec![stroke(MAX_BRUSH_POINTS - STROKE_POINTS)])],
            (0..MAX_MASKS)
                .map(|_| brush(vec![stroke(1); singles / MAX_MASKS]))
                .collect(),
        ] {
            let rec = ImageRecord {
                rating: Some(5),
                masks: masks.into(),
                ..ImageRecord::default()
            };
            let total: usize = rec.masks.iter().map(Mask::brush_points).sum();
            assert!(total <= MAX_BRUSH_POINTS);
            let bytes = serde_json::to_vec_pretty(&rec).unwrap();
            assert!(
                bytes.len() < MAX_SIDECAR_BYTES * 3 / 4,
                "{} bytes",
                bytes.len()
            );
            assert_eq!(parse_record(&bytes, || None).unwrap().masks, rec.masks);
        }
    }

    #[test]
    fn a_sidecar_loads_at_most_max_masks() {
        let mask = r#"{"shape":{"kind":"radial","center":[0.5,0.5],"radii":[0.1,0.1]}}"#;
        let masks = vec![mask; 10_000].join(",");
        let rec = parse_record(format!(r#"{{"masks":[{masks}]}}"#).as_bytes(), || None).unwrap();
        assert_eq!(rec.masks.len(), crate::develop::MAX_MASKS);
    }

    #[test]
    fn a_sidecar_from_before_masks_loads_and_signs_the_same() {
        let rec = parse_record(br#"{"adjustments":{"exposure":0.5}}"#, || None).unwrap();
        assert!(rec.masks.is_empty());
        assert_eq!(
            rec.edit_signature(),
            crate::develop::edit_signature_with_touchups(
                &rec.adjustments,
                &[],
                &[],
                &Detections::default(),
                0
            )
        );
        let mut masked = rec.clone();
        masked.masks.set(vec![Mask::radial([0.5, 0.5], [0.1, 0.1])]);
        assert_ne!(rec.edit_signature(), masked.edit_signature());
    }

    /// A detection changes the render only through a mask that reads it:
    /// recording one for an unused target must not re-bake every thumbnail,
    /// and a gradient-only photo must sign as it did before detections.
    #[test]
    fn a_detection_signs_only_through_the_masks_that_read_it() {
        use crate::develop::DetectTarget;
        let gradient = parse_record(
            br#"{"masks":[{"shape":{"kind":"radial","center":[0.5,0.5],"radii":[0.1,0.1]}}]}"#,
            || None,
        )
        .unwrap();
        let id = "5be1c0a39d2f7e14".parse().ok();
        let mut with_sky = gradient.clone();
        with_sky.detections.set(DetectTarget::Sky, id);
        assert_eq!(gradient.edit_signature(), with_sky.edit_signature());
        assert_eq!(
            gradient.edit_signature(),
            crate::develop::edit_signature_with_touchups(
                &gradient.adjustments,
                &[],
                &gradient.masks,
                &Detections::default(),
                0
            )
        );

        let mut sky_mask = gradient.clone();
        sky_mask.masks.set(vec![
            Mask::detected(DetectTarget::Sky),
            gradient.masks[0].clone(),
        ]);
        let pending = sky_mask.edit_signature();
        assert_ne!(pending, gradient.edit_signature());
        sky_mask.detections.set(DetectTarget::Sky, id);
        let landed = sky_mask.edit_signature();
        assert_ne!(landed, pending, "landing a detection re-bakes");
        sky_mask.detections.set(DetectTarget::Subject, id);
        assert_eq!(
            landed,
            sky_mask.edit_signature(),
            "an unused target doesn't"
        );
    }

    /// The defect: a `detections` value of the wrong shape failed the whole
    /// record, and the photo lost its rating and edits.
    #[test]
    fn a_hostile_detections_value_costs_only_the_detection_not_the_record() {
        for (body, subject) in [
            (r#"{"rating":4,"detections":{"subject":42}}"#, None),
            (
                r#"{"rating":4,"detections":{"subject":null,"sky":[1]}}"#,
                None,
            ),
            (r#"{"rating":4,"detections":"x"}"#, None),
            (r#"{"rating":4,"detections":[]}"#, None),
            (r#"{"rating":4,"detections":null}"#, None),
            (r#"{"rating":4,"detections":7}"#, None),
            (
                r#"{"rating":4,"detections":{"subject":"0123456789abcdef","sky":{"id":1},"extra":true}}"#,
                "0123456789abcdef".parse().ok(),
            ),
        ] {
            let rec =
                parse_record(body.as_bytes(), || None).unwrap_or_else(|e| panic!("{body}: {e}"));
            assert_eq!(rec.rating, Some(4), "{body}");
            assert_eq!(rec.detections.subject, subject, "{body}");
            assert_eq!(rec.detections.sky, None, "{body}");
        }
    }

    #[test]
    fn detections_round_trip_through_the_sidecar_and_a_bad_id_loads_as_none() {
        let body = br#"{"masks":[{"shape":{"kind":"detected","target":"subject"}}],
            "detections":{"subject":"0123456789abcdef","sky":"not-an-id"}}"#;
        let rec = parse_record(body, || None).unwrap();
        assert_eq!(rec.masks.len(), 1);
        assert_eq!(rec.detections.subject, "0123456789abcdef".parse().ok());
        assert_eq!(rec.detections.sky, None);
        assert!(!rec.is_empty());
        let json = serde_json::to_string(&rec).unwrap();
        assert!(
            json.contains(r#""detections":{"subject":"0123456789abcdef"}"#),
            "{json}"
        );
        assert_eq!(parse_record(json.as_bytes(), || None).unwrap(), rec);

        let only = ImageRecord {
            detections: rec.detections,
            ..ImageRecord::default()
        };
        assert!(!only.is_empty(), "a detection alone still persists");
        assert_eq!(
            only.edit_signature(),
            ImageRecord::default().edit_signature(),
            "but is not an edit"
        );
    }

    #[test]
    fn an_edit_stamps_its_time_and_undoing_every_edit_clears_it() {
        let mut rec = ImageRecord {
            rating: Some(3),
            ..ImageRecord::default()
        };
        rec.edit(100, |r| r.adjustments.exposure = 0.5);
        assert_eq!(rec.edited, Some(100));
        rec.edit(200, |r| r.adjustments.exposure = 0.5);
        assert_eq!(
            rec.edited,
            Some(100),
            "an edit that changes nothing keeps the time"
        );
        rec.edit(300, |r| r.rotation = 1);
        assert_eq!(rec.edited, Some(300));
        rec.edit(400, |r| {
            r.adjustments = Adjustments::default();
            r.rotation = 0;
        });
        assert_eq!(rec.edited, None);
        rec.rating = None;
        assert!(rec.is_empty(), "an undone edit leaves no sidecar behind");
    }

    #[test]
    fn the_edit_time_does_not_count_as_an_edit() {
        let mut rec = ImageRecord::default();
        rec.adjustments.exposure = 0.5;
        let signature = rec.edit_signature();
        rec.edited = Some(1);
        assert_eq!(rec.edit_signature(), signature);
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod file_tests {
    use super::*;

    fn temp_folder(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-sidecar-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_written_record_reads_back_from_the_sidecar_beside_the_photo() {
        let dir = temp_folder("round-trip");
        let photo = dir.join("IMG_1.ARW");
        let mut rec = ImageRecord {
            rating: Some(4),
            flag: Some(Flag::Pick),
            ..ImageRecord::default()
        };
        rec.edit(1_700_000_000, |r| r.adjustments.exposure = 0.5);

        assert!(
            !dir.join(SIDECAR_DIR).exists(),
            "nothing is created before the first write"
        );
        write_record(&photo, &rec).unwrap();
        assert!(dir.join(SIDECAR_DIR).join("IMG_1.ARW.xmp").is_file());
        assert_eq!(read_record(&photo).unwrap(), rec);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_photo_without_a_sidecar_reads_as_an_empty_record() {
        let dir = temp_folder("missing");
        assert_eq!(
            read_record(&dir.join("none.jpg")).unwrap(),
            ImageRecord::default()
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writing_an_empty_record_deletes_the_sidecar() {
        let dir = temp_folder("empty");
        let photo = dir.join("a.jpg");
        let rated = ImageRecord {
            rating: Some(2),
            ..ImageRecord::default()
        };
        write_record(&photo, &rated).unwrap();
        write_record(&photo, &ImageRecord::default()).unwrap();
        assert!(sidecar_path(&photo).is_some_and(|p| !p.exists()));
        delete_record(&photo).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_garbage_truncated_or_huge_sidecar_is_an_error() {
        let dir = temp_folder("bad");
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        let huge = format!(
            "{{\"rating\":1,\"pad\":\"{}\"}}",
            "x".repeat(MAX_SIDECAR_BYTES)
        );
        let cases: [(&str, &[u8]); 4] = [
            ("garbage.jpg", b"\x00\xff not json"),
            ("truncated.jpg", br#"{"rating":3,"adjustments":{"expo"#),
            ("wrong-type.jpg", br#"{"rating":"five"}"#),
            ("huge.jpg", huge.as_bytes()),
        ];
        for (name, body) in cases {
            let photo = dir.join(name);
            std::fs::write(sidecar_path(&photo).unwrap(), body).unwrap();
            assert!(read_record(&photo).is_err(), "{name} must not parse");
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
