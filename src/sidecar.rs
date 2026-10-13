// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-photo sidecars that persist ratings and develop edits. Each photo's
//! record lives in `<photo dir>/.lightphotos/<photo filename>.xmp`, so edits
//! travel with the folder and originals are never touched. The body is our
//! own JSON, not Adobe XMP; the extension is cosmetic. `.lightphotos/` is
//! created on the first write, so browsing a folder never changes it.
//!
//! This module owns the record, its path and its single-file read and write.
//! The app's folder store caches a folder of them and queues the writes.

#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, TouchUp};
use crate::scoring::quality::QualityScore;

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
            self.rotation,
        )
    }

    fn has_edits(&self) -> bool {
        !self.adjustments.is_identity() || !self.touchups.is_empty() || self.rotation != 0
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
            6,
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
