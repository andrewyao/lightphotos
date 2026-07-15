// SPDX-License-Identifier: MIT OR Apache-2.0

//! Global edits catalog — the persistence layer for ratings + develop edits.
//!
//! Everything lives in ONE app-managed SQLite database at
//! `~/Library/Application Support/com.lightphotos/catalog.db`. Originals are
//! never touched and nothing is ever written into photo folders.
//!
//! The DB has a single `images(path, rating, adjustments, rotation)` table keyed
//! by the normalized absolute path; `adjustments` holds the serde JSON of a
//! non-identity [`Adjustments`] (NULL for identity edits). An in-memory
//! `HashMap` mirrors the table so reads stay allocation-cheap; writes are
//! single-row UPSERT/DELETE (no whole-file rewrite).
//!
//! Legacy JSON catalogs (`catalog.json`, schema v1 `{ratings}` or v2 `{images}`)
//! are migrated into SQLite on first load and retired to `catalog.json.bak`.
//!
//! Path keys are normalized via `canonicalize()` when it succeeds, else the
//! path is used as-given (so nonexistent / moved files don't panic).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};

use crate::develop::Adjustments;
use crate::paths::normalize;

/// SQLite `user_version` for the current schema (bumped when columns change).
const SQLITE_SCHEMA_VERSION: i64 = 1;
const CATALOG_DB: &str = "catalog.db";
/// Legacy JSON catalog, migrated then renamed to `<CATALOG_FILE>.bak`.
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

/// Legacy JSON shape for schema v2, read only for one-time migration.
#[derive(Deserialize)]
struct CatalogFile {
    #[allow(dead_code)]
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

/// In-memory catalog of per-image records, backed by a single SQLite database.
///
/// The `images` map is a read cache mirroring the DB; `conn` is `None` only when
/// the database could not be opened, in which case the catalog behaves as empty
/// and every write records an error for the UI (rather than panicking).
pub struct Catalog {
    images: HashMap<PathBuf, ImageRecord>,
    /// Directory holding `catalog.db` (created on open).
    dir: PathBuf,
    /// Full path to `catalog.db`.
    db_file: PathBuf,
    /// Open connection, or `None` if the database is unavailable.
    conn: Option<Connection>,
    /// The most recent persist failure, if any, awaiting delivery to the user.
    /// Set whenever a write fails; drained by [`Catalog::take_error`] so the UI
    /// can surface a toast instead of the change being silently lost.
    last_error: Option<String>,
}

impl Catalog {
    /// Load the catalog from the default app-support location.
    ///
    /// A missing database yields an empty catalog (never panics). First migrates
    /// the pre-rename `com.imageviewer` directory if present, so ratings and
    /// edits made under the old name are preserved.
    pub fn load() -> Catalog {
        let dir = default_dir();
        crate::paths::migrate_legacy_dir(&dir, &legacy_dir());
        Catalog::with_dir(dir)
    }

    /// Load the catalog rooted at an explicit directory.
    ///
    /// Same semantics as [`Catalog::load`] but lets tests point at a temp dir so
    /// they never touch the real catalog. Opens (creating if needed) the SQLite
    /// DB, migrates any legacy `catalog.json`, then loads all rows into the read
    /// cache. A DB that cannot be opened degrades to an empty catalog.
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let db_file = dir.join(CATALOG_DB);
        let mut cat = Catalog {
            images: HashMap::new(),
            dir,
            db_file,
            conn: None,
            last_error: None,
        };
        let _ = std::fs::create_dir_all(&cat.dir);
        match cat.open_and_init() {
            Ok(conn) => {
                cat.conn = Some(conn);
                cat.migrate_legacy_json();
                cat.load_into_cache();
            }
            Err(e) => {
                eprintln!(
                    "[catalog] could not open {}: {e} (starting empty)",
                    cat.db_file.display()
                );
            }
        }
        cat
    }

    /// Open the SQLite database and ensure the schema exists.
    fn open_and_init(&self) -> rusqlite::Result<Connection> {
        let conn = Connection::open(&self.db_file)?;
        conn.pragma_update(None, "user_version", SQLITE_SCHEMA_VERSION)?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS images (
                 path       TEXT PRIMARY KEY,
                 rating     INTEGER,
                 adjustments TEXT,
                 rotation   INTEGER NOT NULL DEFAULT 0
             )",
            [],
        )?;
        Ok(conn)
    }

    /// One-time import of a legacy `catalog.json` (v1 or v2) into SQLite. On
    /// success the JSON is renamed to `catalog.json.bak`; corrupt JSON is left
    /// in place and ignored so the catalog simply starts empty.
    fn migrate_legacy_json(&mut self) {
        let json = self.dir.join(CATALOG_FILE);
        if !json.exists() {
            return;
        }
        let Ok(bytes) = std::fs::read(&json) else { return };
        match parse_catalog(&bytes) {
            Ok(images) => {
                if let Err(e) = self.insert_all(&images) {
                    eprintln!("[catalog] JSON migration failed: {e}");
                    return; // leave the JSON in place for a retry next launch
                }
                let bak = json.with_extension("json.bak");
                if let Err(e) = std::fs::rename(&json, &bak) {
                    eprintln!("[catalog] could not retire catalog.json: {e}");
                }
            }
            Err(e) => eprintln!("[catalog] ignoring corrupt catalog.json: {e}"),
        }
    }

    /// Insert many records in a single transaction (used for migration).
    fn insert_all(&self, images: &HashMap<PathBuf, ImageRecord>) -> rusqlite::Result<()> {
        let Some(conn) = self.conn.as_ref() else {
            return Err(rusqlite::Error::InvalidQuery);
        };
        let tx = conn.unchecked_transaction()?;
        for (path, rec) in images {
            if !rec.is_empty() {
                upsert_row(&tx, path, rec)?;
            }
        }
        tx.commit()
    }

    /// Populate the in-memory read cache from the DB.
    fn load_into_cache(&mut self) {
        let Some(conn) = self.conn.as_ref() else { return };
        let mut stmt = match conn.prepare("SELECT path, rating, adjustments, rotation FROM images") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[catalog] could not read rows: {e}");
                return;
            }
        };
        let rows = stmt.query_map([], |row| {
            let path: String = row.get(0)?;
            let rating: Option<u8> = row.get(1)?;
            let adj_json: Option<String> = row.get(2)?;
            let rotation: u8 = row.get(3)?;
            let adjustments = adj_json
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default();
            Ok((PathBuf::from(path), ImageRecord { rating, adjustments, rotation }))
        });
        match rows {
            Ok(iter) => {
                for r in iter.flatten() {
                    self.images.insert(r.0, r.1);
                }
            }
            Err(e) => eprintln!("[catalog] could not map rows: {e}"),
        }
    }

    /// Take the pending persist error, if any. Returns `Some(message)` exactly
    /// once per failure so callers can show a single toast; subsequent calls
    /// return `None` until the next failed write.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Record a persist failure: log it and stash it for the UI to surface.
    fn note_persist_error(&mut self, e: impl std::fmt::Display) {
        let msg = format!("Failed to save catalog to {}: {e}", self.db_file.display());
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
    /// deleted from disk. A no-op when nothing was stored.
    pub fn remove(&mut self, path: &Path) {
        let key = normalize(path);
        if self.images.remove(&key).is_some() {
            if let Err(e) = self.delete_row(&key) {
                self.note_persist_error(e);
            }
        }
    }

    /// Apply `mutate` to the record for `key` (creating it if needed), drop the
    /// entry if it became empty, then persist just that row.
    fn update(&mut self, key: PathBuf, mutate: impl FnOnce(&mut ImageRecord)) {
        let mut rec = self.images.remove(&key).unwrap_or_default();
        mutate(&mut rec);
        let result = if rec.is_empty() {
            self.delete_row(&key)
        } else {
            self.write_row(&key, &rec)
        };
        if !rec.is_empty() {
            self.images.insert(key, rec);
        }
        if let Err(e) = result {
            self.note_persist_error(e);
        }
    }

    /// UPSERT a single record's row into the DB.
    fn write_row(&self, key: &Path, rec: &ImageRecord) -> rusqlite::Result<()> {
        let conn = self.conn.as_ref().ok_or(rusqlite::Error::InvalidQuery)?;
        upsert_row(conn, key, rec)
    }

    /// DELETE a single row (no-op if absent).
    fn delete_row(&self, key: &Path) -> rusqlite::Result<()> {
        let conn = self.conn.as_ref().ok_or(rusqlite::Error::InvalidQuery)?;
        conn.execute("DELETE FROM images WHERE path = ?1", [path_key(key)])?;
        Ok(())
    }
}

/// The TEXT primary-key form of a (normalized) path.
fn path_key(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// UPSERT `rec` for `path` using any connection or transaction. Identity
/// adjustments are stored as SQL NULL to keep rows compact.
fn upsert_row(conn: &Connection, path: &Path, rec: &ImageRecord) -> rusqlite::Result<()> {
    let adj_json = if rec.adjustments.is_identity() {
        None
    } else {
        Some(
            serde_json::to_string(&rec.adjustments)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?,
        )
    };
    conn.execute(
        "INSERT INTO images (path, rating, adjustments, rotation) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(path) DO UPDATE SET rating = ?2, adjustments = ?3, rotation = ?4",
        rusqlite::params![path_key(path), rec.rating, adj_json, rec.rotation],
    )?;
    Ok(())
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
    fn migrates_v1_json_to_sqlite() {
        let dir = unique_tmp_dir();
        let json = dir.join("catalog.json");
        let p = dir.join("photo.jpg");
        let key = normalize(&p);

        // Hand-write a v1 JSON file.
        let v1 = serde_json::json!({
            "version": 1,
            "ratings": { key.to_str().unwrap(): 4u8 },
        });
        std::fs::write(&json, serde_json::to_vec_pretty(&v1).unwrap()).unwrap();

        // Loading migrates the rating into SQLite and retires the JSON file.
        {
            let cat = Catalog::with_dir(dir.clone());
            assert_eq!(cat.get(&p), Some(4));
        }
        assert!(
            !json.exists(),
            "catalog.json should be retired after migration"
        );
        assert!(
            dir.join("catalog.json.bak").exists(),
            "migrated JSON should be preserved as catalog.json.bak"
        );
        assert!(dir.join("catalog.db").exists(), "SQLite DB should exist");

        // Reload reads purely from SQLite (JSON is gone).
        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(4));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrates_v2_json_to_sqlite() {
        let dir = unique_tmp_dir();
        let json = dir.join("catalog.json");
        let p = dir.join("photo.jpg");
        let key = normalize(&p);

        let mut adj = Adjustments::default();
        adj.exposure = 1.25;

        // Hand-write a v2 JSON file with a rating + non-identity adjustments.
        let v2 = serde_json::json!({
            "version": 2,
            "images": { key.to_str().unwrap(): {
                "rating": 5u8,
                "adjustments": adj,
            }},
        });
        std::fs::write(&json, serde_json::to_vec_pretty(&v2).unwrap()).unwrap();

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(5));
        assert_eq!(reloaded.adjustments(&p), adj);
        assert!(!json.exists());
        assert!(dir.join("catalog.json.bak").exists());

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

    /// Read the raw `adjustments` column for `path` straight from the DB, so we
    /// can assert identity edits are stored as SQL NULL (not a wasteful blob).
    fn raw_adjustments_column(dir: &Path, path: &Path) -> Option<String> {
        let key = normalize(path);
        let conn = rusqlite::Connection::open(dir.join("catalog.db")).unwrap();
        conn.query_row(
            "SELECT adjustments FROM images WHERE path = ?1",
            [key.to_str().unwrap()],
            |row| row.get::<_, Option<String>>(0),
        )
        .unwrap()
    }

    #[test]
    fn identity_adjustments_stored_as_null() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        // A rated image with identity edits: rating row present, adjustments NULL.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 2);
        }
        assert_eq!(
            raw_adjustments_column(&dir, &p),
            None,
            "identity adjustments must be stored as NULL"
        );

        // A non-identity edit stores a JSON blob in the column.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            let mut adj = Adjustments::default();
            adj.shadows = 40.0;
            cat.set_adjustments(&p, &adj);
        }
        assert!(
            raw_adjustments_column(&dir, &p).is_some(),
            "non-identity adjustments must be stored"
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
