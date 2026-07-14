//! Global edits catalog — the persistence layer for ratings + develop edits.
//!
//! Everything lives in ONE app-managed JSON file at
//! `~/Library/Application Support/com.lightphotos/catalog.json`. Originals are
//! never touched and nothing is ever written into photo folders.
//!
//! Schema v2: `{ "version": 2, "images": { "<abs canonical path>": ImageRecord } }`
//! where `ImageRecord` carries an optional rating and (when non-identity) the
//! develop `Adjustments`. v1 files (`{ "version": 1, "ratings": {...} }`) are
//! migrated on load.
//!
//! Path keys are normalized via `canonicalize()` when it succeeds, else the
//! path is used as-given (so nonexistent / moved files don't panic).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::Adjustments;
use crate::paths::normalize;

const CATALOG_VERSION: u32 = 2;
const CATALOG_FILE: &str = "catalog.json";

/// Per-image persisted state: an optional rating plus develop edits. Identity
/// adjustments are skipped on write so unedited (but rated) images stay compact.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ImageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Adjustments::is_identity")]
    pub adjustments: Adjustments,
    /// Manual rotation in 90° clockwise steps (0..=3). Kept separate from the
    /// develop adjustments (it's not a tone/crop edit).
    #[serde(default, skip_serializing_if = "is_zero_rot")]
    pub rotation: u8,
}

fn is_zero_rot(v: &u8) -> bool {
    *v == 0
}

impl ImageRecord {
    /// True when this record carries nothing worth persisting (no rating,
    /// identity edit, no rotation) — such entries are dropped to keep the file
    /// small.
    fn is_empty(&self) -> bool {
        self.rating.is_none() && self.adjustments.is_identity() && self.rotation == 0
    }
}

/// On-disk JSON shape for schema v2. Kept private; `Catalog` is the public API.
#[derive(Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    images: HashMap<PathBuf, ImageRecord>,
}

/// On-disk JSON shape for schema v1 (ratings only). Read only for migration.
#[derive(Deserialize)]
struct CatalogFileV1 {
    #[allow(dead_code)]
    version: u32,
    ratings: HashMap<PathBuf, u8>,
}

/// In-memory catalog of per-image records, backed by a single JSON file.
pub struct Catalog {
    images: HashMap<PathBuf, ImageRecord>,
    /// Directory holding `catalog.json` (created lazily on first write).
    dir: PathBuf,
    /// Full path to `catalog.json`.
    file: PathBuf,
    /// The most recent persist failure, if any, awaiting delivery to the user.
    /// Set whenever a write fails; drained by [`Catalog::take_error`] so the UI
    /// can surface a toast instead of the change being silently lost.
    last_error: Option<String>,
}

impl Catalog {
    /// Load the catalog from the default app-support location.
    ///
    /// Missing or corrupt files yield an empty catalog (never panics). First
    /// migrates the pre-rename `com.imageviewer` directory if present, so ratings
    /// and edits made under the old name are preserved.
    pub fn load() -> Catalog {
        let dir = default_dir();
        crate::paths::migrate_legacy_dir(&dir, &legacy_dir());
        Catalog::with_dir(dir)
    }

    /// Load the catalog rooted at an explicit directory.
    ///
    /// Same semantics as [`Catalog::load`] but lets tests point at a temp dir
    /// so they never touch the real catalog. Handles absent, v1, and v2 files;
    /// anything malformed degrades to an empty catalog.
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let file = dir.join(CATALOG_FILE);
        let images = match std::fs::read(&file) {
            Ok(bytes) => parse_catalog(&bytes).unwrap_or_else(|e| {
                eprintln!(
                    "[catalog] ignoring corrupt catalog at {}: {e}",
                    file.display()
                );
                HashMap::new()
            }),
            // Missing file (or any read error) → start empty.
            Err(_) => HashMap::new(),
        };
        Catalog { images, dir, file, last_error: None }
    }

    /// Take the pending persist error, if any. Returns `Some(message)` exactly
    /// once per failure so callers can show a single toast; subsequent calls
    /// return `None` until the next failed write.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Record a persist failure: log it and stash it for the UI to surface.
    fn note_persist_error(&mut self, e: std::io::Error) {
        let msg = format!("Failed to save catalog to {}: {e}", self.file.display());
        eprintln!("[catalog] {msg}");
        self.last_error = Some(msg);
    }

    /// Rating for `path`, if any. Path is normalized the same way as `set`.
    pub fn get(&self, path: &Path) -> Option<u8> {
        self.images.get(&normalize(path)).and_then(|r| r.rating)
    }

    /// Set the rating for `path`, clamped to `0..=5`. A rating of `0` removes
    /// the rating. If the record ends up empty (no rating + identity edit) the
    /// whole entry is dropped. The catalog is then persisted atomically.
    pub fn set(&mut self, path: &Path, stars: u8) {
        let key = normalize(path);
        let stars = stars.min(5);
        let rating = if stars == 0 { None } else { Some(stars) };
        self.update(key, |rec| rec.rating = rating);
    }

    /// Develop adjustments for `path` (identity when unset).
    pub fn adjustments(&self, path: &Path) -> Adjustments {
        self.images
            .get(&normalize(path))
            .map(|r| r.adjustments)
            .unwrap_or_default()
    }

    /// Store develop adjustments for `path`. If the record ends up empty (no
    /// rating + identity edit) the entry is dropped. Persisted atomically.
    pub fn set_adjustments(&mut self, path: &Path, adj: &Adjustments) {
        let key = normalize(path);
        let adj = *adj;
        self.update(key, |rec| rec.adjustments = adj);
    }

    /// Manual rotation (90° CW steps, 0..=3) for `path`.
    pub fn rotation(&self, path: &Path) -> u8 {
        self.images
            .get(&normalize(path))
            .map(|r| r.rotation)
            .unwrap_or(0)
    }

    /// Store the manual rotation for `path` (0..=3). Persisted atomically.
    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let key = normalize(path);
        let rotation = rotation % 4;
        self.update(key, |rec| rec.rotation = rotation);
    }

    /// Forget any record for `path` (rating + adjustments). Used when a photo is
    /// deleted from disk. Persisted atomically; a no-op when nothing was stored.
    pub fn remove(&mut self, path: &Path) {
        if self.images.remove(&normalize(path)).is_some() {
            if let Err(e) = self.persist() {
                self.note_persist_error(e);
            }
        }
    }

    /// Apply `mutate` to the record for `key` (creating it if needed), drop the
    /// entry if it became empty, then persist.
    fn update(&mut self, key: PathBuf, mutate: impl FnOnce(&mut ImageRecord)) {
        let mut rec = self.images.remove(&key).unwrap_or_default();
        mutate(&mut rec);
        if !rec.is_empty() {
            self.images.insert(key, rec);
        }
        if let Err(e) = self.persist() {
            self.note_persist_error(e);
        }
    }

    /// Serialize the catalog and write it atomically.
    fn persist(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;

        let snapshot = CatalogFile {
            version: CATALOG_VERSION,
            images: self.images.clone(),
        };
        let json = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;

        // Unique temp name in the same dir so the final rename is atomic.
        let tmp = self.dir.join(format!(
            ".{CATALOG_FILE}.tmp.{}",
            std::process::id()
        ));
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &self.file)?;
        Ok(())
    }
}

/// Parse catalog bytes, migrating v1 → v2 as needed. Returns the in-memory
/// `images` map, or an error if the bytes are not valid catalog JSON.
fn parse_catalog(bytes: &[u8]) -> serde_json::Result<HashMap<PathBuf, ImageRecord>> {
    // Peek at `version` to decide how to interpret the rest.
    let value: serde_json::Value = serde_json::from_slice(bytes)?;
    let version = value.get("version").and_then(|v| v.as_u64()).unwrap_or(0);

    if version >= 2 {
        let parsed: CatalogFile = serde_json::from_value(value)?;
        Ok(parsed.images)
    } else {
        // v1 (or version-less with a `ratings` map) → migrate ratings into records.
        let parsed: CatalogFileV1 = serde_json::from_value(value)?;
        let images = parsed
            .ratings
            .into_iter()
            .map(|(path, stars)| {
                (
                    path,
                    ImageRecord {
                        rating: Some(stars),
                        adjustments: Adjustments::default(),
                        rotation: 0,
                    },
                )
            })
            .collect();
        Ok(images)
    }
}

/// Default catalog directory: `$HOME/Library/Application Support/com.lightphotos/`.
fn default_dir() -> PathBuf {
    app_support().join("com.lightphotos")
}

/// The pre-rename catalog directory (`com.imageviewer`), migrated on first load.
fn legacy_dir() -> PathBuf {
    app_support().join("com.imageviewer")
}

fn app_support() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library").join("Application Support")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp dir under `std::env::temp_dir()` (no tempfile crate).
    fn unique_tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "imageviewer-catalog-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_persists_across_reload() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg"); // nonexistent — exercises the fallback.

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 3);
            assert_eq!(cat.get(&p), Some(3));
        } // drop

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(3));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zero_removes_entry_and_persists_removal() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 4);
            assert_eq!(cat.get(&p), Some(4));
            cat.set(&p, 0);
            assert_eq!(cat.get(&p), None);
        }

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rating_is_clamped_to_five() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 9);
        assert_eq!(cat.get(&p), Some(5));

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(5));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_v1_ratings_to_v2() {
        let dir = unique_tmp_dir();
        let file = dir.join(CATALOG_FILE);
        let p = dir.join("photo.jpg");
        let key = normalize(&p);

        // Hand-write a v1 file.
        let v1 = serde_json::json!({
            "version": 1,
            "ratings": { key.to_str().unwrap(): 4u8 },
        });
        std::fs::write(&file, serde_json::to_vec_pretty(&v1).unwrap()).unwrap();

        // Load migrates the rating; a mutation rewrites the file as v2.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            assert_eq!(cat.get(&p), Some(4));
            cat.set(&p, 4); // trigger a save in v2 shape
        }

        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(
            on_disk.contains("\"version\": 2"),
            "expected version 2 after save, got: {on_disk}"
        );
        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(4));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adjustments_persist_across_reload() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        let mut adj = Adjustments::default();
        adj.exposure = 1.5;
        adj.contrast = 25.0;

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set_adjustments(&p, &adj);
            assert_eq!(cat.adjustments(&p), adj);
        }

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.adjustments(&p), adj);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_persists_across_reload() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set_rotation(&p, 3);
            assert_eq!(cat.rotation(&p), 3);
            // Wraps mod 4.
            cat.set_rotation(&p, 5);
            assert_eq!(cat.rotation(&p), 1);
        }

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.rotation(&p), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_persist_is_reported_once_via_take_error() {
        // Point the catalog at a directory that can't be created because a
        // *file* sits where a parent dir would need to be, so `create_dir_all`
        // (and thus persist) fails deterministically.
        let base = unique_tmp_dir();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, b"not a dir").unwrap();
        let bad_dir = blocker.join("catalog"); // parent is a file → mkdir fails

        let mut cat = Catalog::with_dir(bad_dir);
        assert!(cat.take_error().is_none(), "no error before any write");

        cat.set(&base.join("photo.jpg"), 3);
        // The write failed, so the error is available exactly once...
        assert!(cat.take_error().is_some(), "failed save should report an error");
        // ...and is drained (not re-delivered) on the next check.
        assert!(cat.take_error().is_none(), "error should be taken only once");

        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn identity_adjustments_not_serialized() {
        let dir = unique_tmp_dir();
        let file = dir.join(CATALOG_FILE);
        let p = dir.join("photo.jpg");

        // A rated image with identity edits: must serialize the rating but no
        // `adjustments` key.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 2);
        }
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(on_disk.contains("\"rating\""), "rating should serialize");
        assert!(
            !on_disk.contains("adjustments"),
            "identity adjustments must not serialize, got: {on_disk}"
        );

        // Now give it a non-identity edit: `adjustments` should appear.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            let mut adj = Adjustments::default();
            adj.shadows = 40.0;
            cat.set_adjustments(&p, &adj);
        }
        let on_disk = std::fs::read_to_string(&file).unwrap();
        assert!(
            on_disk.contains("adjustments"),
            "non-identity adjustments must serialize, got: {on_disk}"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rating_and_adjustments_coexist() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        let mut adj = Adjustments::default();
        adj.temp = -30.0;
        adj.whites = 15.0;

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 5);
            cat.set_adjustments(&p, &adj);
            assert_eq!(cat.get(&p), Some(5));
            assert_eq!(cat.adjustments(&p), adj);
        }

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(5));
        assert_eq!(reloaded.adjustments(&p), adj);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
