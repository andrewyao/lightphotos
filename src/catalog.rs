//! Global edits catalog — the persistence layer for ratings + develop edits.
//!
//! Everything lives in ONE app-managed JSON file at
//! `~/Library/Application Support/com.imageviewer/catalog.json`. Originals are
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
}

impl ImageRecord {
    /// True when this record carries nothing worth persisting (no rating and an
    /// identity edit) — such entries are dropped to keep the file small.
    fn is_empty(&self) -> bool {
        self.rating.is_none() && self.adjustments.is_identity()
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
}

impl Catalog {
    /// Load the catalog from the default app-support location.
    ///
    /// Missing or corrupt files yield an empty catalog (never panics).
    pub fn load() -> Catalog {
        Catalog::with_dir(default_dir())
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
        Catalog { images, dir, file }
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

    /// Apply `mutate` to the record for `key` (creating it if needed), drop the
    /// entry if it became empty, then persist.
    fn update(&mut self, key: PathBuf, mutate: impl FnOnce(&mut ImageRecord)) {
        let mut rec = self.images.remove(&key).unwrap_or_default();
        mutate(&mut rec);
        if !rec.is_empty() {
            self.images.insert(key, rec);
        }
        if let Err(e) = self.persist() {
            eprintln!(
                "[catalog] failed to persist catalog at {}: {e}",
                self.file.display()
            );
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
                    },
                )
            })
            .collect();
        Ok(images)
    }
}

/// Default catalog directory: `$HOME/Library/Application Support/com.imageviewer/`.
fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library")
        .join("Application Support")
        .join("com.imageviewer")
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
