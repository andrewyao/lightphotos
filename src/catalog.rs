#![allow(dead_code)] // TODO: remove once wired into App (T5)

//! Global ratings catalog — the persistence layer for 1–5 star ratings.
//!
//! Ratings live in ONE app-managed JSON file at
//! `~/Library/Application Support/com.imageviewer/catalog.json`. Originals are
//! never touched and nothing is ever written into photo folders.
//!
//! Schema: `{ "version": 1, "ratings": { "<abs canonical path>": 1..=5 } }`.
//!
//! Path keys are normalized via `canonicalize()` when it succeeds, else the
//! path is used as-given (so nonexistent / moved files don't panic).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const CATALOG_VERSION: u32 = 1;
const CATALOG_FILE: &str = "catalog.json";

/// On-disk JSON shape. Kept private; `Catalog` is the public API.
#[derive(Serialize, Deserialize)]
struct CatalogFile {
    version: u32,
    ratings: HashMap<PathBuf, u8>,
}

/// In-memory ratings catalog, backed by a single JSON file.
pub struct Catalog {
    ratings: HashMap<PathBuf, u8>,
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
    /// so they never touch the real catalog.
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let file = dir.join(CATALOG_FILE);
        let ratings = match std::fs::read(&file) {
            Ok(bytes) => match serde_json::from_slice::<CatalogFile>(&bytes) {
                Ok(parsed) => parsed.ratings,
                Err(e) => {
                    eprintln!(
                        "[catalog] ignoring corrupt catalog at {}: {e}",
                        file.display()
                    );
                    HashMap::new()
                }
            },
            // Missing file (or any read error) → start empty.
            Err(_) => HashMap::new(),
        };
        Catalog { ratings, dir, file }
    }

    /// Rating for `path`, if any. Path is normalized the same way as `set`.
    pub fn get(&self, path: &Path) -> Option<u8> {
        self.ratings.get(&normalize(path)).copied()
    }

    /// Set the rating for `path`, clamped to `0..=5`. A rating of `0` removes
    /// the entry. The whole catalog is then persisted atomically (temp file in
    /// the same dir + `fs::rename`). IO errors are logged, never panic.
    pub fn set(&mut self, path: &Path, stars: u8) {
        let key = normalize(path);
        let stars = stars.min(5);
        if stars == 0 {
            self.ratings.remove(&key);
        } else {
            self.ratings.insert(key, stars);
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
            ratings: self.ratings.clone(),
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

/// Default catalog directory: `$HOME/Library/Application Support/com.imageviewer/`.
fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    home.join("Library")
        .join("Application Support")
        .join("com.imageviewer")
}

/// Normalize a path for use as a catalog key: prefer the canonical absolute
/// path, but fall back to the path as-given when canonicalization fails (e.g.
/// the file doesn't exist or was moved). Never panics.
fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
}
