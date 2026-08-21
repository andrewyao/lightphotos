// SPDX-License-Identifier: MIT OR Apache-2.0

//! Per-photo sidecar catalog — the persistence layer for ratings + develop edits.
//!
//! Each photo's rating/adjustments/touchups/rotation lives in its own file,
//! `<photo's directory>/.lightphotos/<photo filename>.xmp`. **The `.xmp`
//! extension here is cosmetic only** — the body is our own compact JSON
//! serialization of [`ImageRecord`], NOT real Adobe XMP/RDF. Do not "fix"
//! this into real XML; the extension was picked because it reads as a
//! familiar sidecar-file convention, nothing more.
//!
//! Sidecars travel with the photos: moving, copying, or sharing a folder
//! carries its `.lightphotos/` subfolder — and thus every rating/edit —
//! along with it, unlike the old single global SQLite catalog, which
//! orphaned everything the moment a folder moved. Originals are never
//! touched, and `.lightphotos` is created lazily (only on the first write
//! for that directory), so browsing a folder read-only never litters it.
//!
//! [`Catalog`] is scoped to one directory at a time (the "active"
//! directory) — there is no cross-folder cache or index. Opening a
//! different folder calls [`Catalog::open_dir`], which reloads the
//! in-memory read cache from that directory's `.lightphotos/*.xmp` files.
//! Individual reads/writes locate their sidecar directly from the photo's
//! own path (not from the active directory), so they stay correct even if
//! called for a path outside it.
//!
//! Legacy state (an older global `catalog.json`) is migrated once via
//! [`migrate_legacy_catalog`], fanning rows out to the per-directory
//! sidecars they belong to. (An even older global SQLite `catalog.db` was
//! also migrated this way for one release; that code — and the `rusqlite`
//! dependency — has since been removed, so a leftover `catalog.db` is no
//! longer picked up.)

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, TouchUp};

/// Hidden per-directory subfolder holding that directory's sidecar files.
const SIDECAR_DIR: &str = ".lightphotos";
/// Sidecar file extension (cosmetic only — see module docs).
const SIDECAR_EXT: &str = "xmp";
/// Legacy global JSON catalog, migrated then retired to `<name>.bak`.
const CATALOG_FILE: &str = "catalog.json";

/// Per-image persisted state: an optional rating plus develop edits. Identity
/// adjustments are skipped on write so unedited (but rated) images stay compact.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ImageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Adjustments::is_identity")]
    pub adjustments: Adjustments,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touchups: Vec<TouchUp>,
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
    /// identity edit, no rotation) — such a record's sidecar is deleted
    /// rather than written, keeping unrated/unedited folders sidecar-free.
    fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.adjustments.is_identity()
            && self.touchups.is_empty()
            && self.rotation == 0
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

/// In-memory catalog of per-image records, backed by per-photo sidecar files
/// under the active directory's `.lightphotos/` subfolder.
///
/// `images` is a read cache mirroring the active directory's sidecars, keyed
/// by filename (the catalog is scoped to one directory at a time — see the
/// module docs). Reads/writes for a path outside the active directory still
/// resolve correctly (they derive their sidecar location from the path
/// itself), they just won't be reflected in this particular cache until
/// [`Catalog::open_dir`] is called for their directory.
pub struct Catalog {
    images: HashMap<OsString, ImageRecord>,
    /// The directory currently active, or `None` before the first
    /// [`Catalog::open_dir`] call — degrades to an empty catalog rather than
    /// panicking.
    dir: Option<PathBuf>,
    /// The most recent persist failure, if any, awaiting delivery to the
    /// user. Set whenever a write fails; drained by [`Catalog::take_error`]
    /// so the UI can surface a toast instead of the change being silently
    /// lost.
    last_error: Option<String>,
}

impl Catalog {
    /// A directory-less catalog with nothing loaded yet. Used at startup,
    /// before any folder/file has been opened.
    pub fn new() -> Catalog {
        Catalog {
            images: HashMap::new(),
            dir: None,
            last_error: None,
        }
    }

    /// `new()` + `open_dir(&dir)` in one step — a convenience mainly used by
    /// tests, which always know their directory up front.
    #[allow(dead_code)] // only called from #[cfg(test)] today
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let mut cat = Catalog::new();
        cat.open_dir(&dir);
        cat
    }

    /// (Re)point the catalog at `dir` as the active directory and rebuild the
    /// read cache from `dir/.lightphotos/*.xmp`. Always reloads from disk,
    /// even if `dir` equals the previously-active directory, so an external
    /// change (another process, a hand-fixed sidecar) is picked up. A
    /// missing `.lightphotos` directory is not an error — it's just an empty
    /// catalog; the directory itself is never eagerly created here.
    pub fn open_dir(&mut self, dir: &Path) {
        self.dir = Some(dir.to_path_buf());
        self.images.clear();
        self.load_into_cache();
    }

    /// Populate the in-memory read cache from the active directory's sidecars.
    fn load_into_cache(&mut self) {
        let Some(dir) = self.dir.clone() else {
            return;
        };
        let entries = match std::fs::read_dir(dir.join(SIDECAR_DIR)) {
            Ok(rd) => rd,
            Err(_) => return, // no .lightphotos yet: empty catalog, not an error
        };

        let mut skipped = 0usize;
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            if !path.is_file() {
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) != Some(SIDECAR_EXT) {
                continue;
            }
            // file_stem() strips exactly the trailing ".xmp", correctly
            // preserving a name like "PHOTO1.ARW" which itself contains a dot.
            let Some(stem) = path.file_stem() else {
                continue;
            };
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<ImageRecord>(&bytes) {
                    Ok(rec) if !rec.is_empty() => {
                        self.images.insert(stem.to_os_string(), rec);
                    }
                    Ok(_) => {} // an empty record on disk: nothing to cache
                    Err(e) => {
                        eprintln!("[catalog] unreadable sidecar {}: {e}", path.display());
                        skipped += 1;
                    }
                },
                Err(e) => {
                    eprintln!("[catalog] could not read {}: {e}", path.display());
                    skipped += 1;
                }
            }
        }

        if skipped > 0 {
            self.last_error = Some(format!(
                "{skipped} catalog entr{} could not be read and {} skipped.",
                if skipped == 1 { "y" } else { "ies" },
                if skipped == 1 { "was" } else { "were" },
            ));
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
        let msg = format!("Failed to save catalog entry: {e}");
        eprintln!("[catalog] {msg}");
        self.last_error = Some(msg);
    }

    /// Rating for `path`, if any.
    pub fn get(&self, path: &Path) -> Option<u8> {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .and_then(|r| r.rating)
    }

    /// Set the rating for `path`, clamped to `0..=5`. A rating of `0` removes
    /// the rating. If the record ends up empty (no rating + identity edit)
    /// its sidecar is deleted. Persisted atomically.
    pub fn set(&mut self, path: &Path, stars: u8) {
        let stars = stars.min(5);
        let rating = if stars == 0 { None } else { Some(stars) };
        self.update(path, |rec| rec.rating = rating);
    }

    /// Develop adjustments for `path` (identity when unset).
    pub fn adjustments(&self, path: &Path) -> Adjustments {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .map(|r| r.adjustments)
            .unwrap_or_default()
    }

    /// Store develop adjustments for `path`. If the record ends up empty (no
    /// rating + identity edit) its sidecar is deleted. Persisted atomically.
    pub fn set_adjustments(&mut self, path: &Path, adj: &Adjustments) {
        let adj = *adj;
        self.update(path, |rec| rec.adjustments = adj);
    }

    pub fn touchups(&self, path: &Path) -> Vec<TouchUp> {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .map(|r| r.touchups.clone())
            .unwrap_or_default()
    }

    pub fn set_touchups(&mut self, path: &Path, touchups: &[TouchUp]) {
        let touchups = touchups.to_vec();
        self.update(path, |rec| rec.touchups = touchups);
    }

    /// Manual rotation (90° CW steps, 0..=3) for `path`.
    pub fn rotation(&self, path: &Path) -> u8 {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .map(|r| r.rotation)
            .unwrap_or(0)
    }

    /// Store the manual rotation for `path` (0..=3). Persisted atomically.
    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let rotation = rotation % 4;
        self.update(path, |rec| rec.rotation = rotation);
    }

    /// Forget any record for `path` (rating + adjustments + touchups +
    /// rotation) by deleting its sidecar. Used when a photo is deleted from
    /// disk. A no-op when nothing was stored. The sidecar is deleted outright
    /// (not moved to Trash) — it has no meaningful pairing to its photo once
    /// separated from `.lightphotos/`, and this matches the old catalog's
    /// unconditional delete-on-remove behavior.
    pub fn remove(&mut self, path: &Path) {
        if let Some(name) = path.file_name() {
            self.images.remove(name);
        }
        if let Err(e) = self.delete_sidecar(path) {
            self.note_persist_error(e);
        }
    }

    /// Apply `mutate` to the record for `path` (creating it if needed), drop
    /// its sidecar if it became empty, then persist just that one file.
    fn update(&mut self, path: &Path, mutate: impl FnOnce(&mut ImageRecord)) {
        let Some(name) = path.file_name().map(|n| n.to_os_string()) else {
            return;
        };
        let mut rec = self.images.remove(&name).unwrap_or_default();
        mutate(&mut rec);
        let empty = rec.is_empty();
        let result = if empty {
            self.delete_sidecar(path)
        } else {
            self.write_sidecar(path, &rec)
        };
        if !empty {
            self.images.insert(name, rec);
        }
        if let Err(e) = result {
            self.note_persist_error(e);
        }
    }

    fn write_sidecar(&self, path: &Path, rec: &ImageRecord) -> Result<(), String> {
        let sidecar =
            sidecar_path(path).ok_or_else(|| "cannot determine sidecar path".to_string())?;
        write_sidecar_file(&sidecar, rec)
    }

    fn delete_sidecar(&self, path: &Path) -> Result<(), String> {
        let Some(sidecar) = sidecar_path(path) else {
            return Ok(());
        };
        match std::fs::remove_file(&sidecar) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e.to_string()),
        }
    }
}

impl Default for Catalog {
    fn default() -> Catalog {
        Catalog::new()
    }
}

/// The sidecar path for `path`: `<path's directory>/.lightphotos/<filename>.xmp`.
/// `None` when `path` has no parent or no filename (e.g. `/` or `..`).
/// Built via `OsString` concatenation (not a lossy `to_string_lossy` round
/// trip) so non-UTF8 filenames stay exact.
fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let name = path.file_name()?;
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".");
    sidecar_name.push(SIDECAR_EXT);
    Some(dir.join(SIDECAR_DIR).join(sidecar_name))
}

/// Write `rec` as pretty-printed JSON to `sidecar`, creating its parent
/// `.lightphotos` directory as needed. Atomic: writes to a `.tmp` sibling
/// then renames over the target, matching the existing convention in
/// `export.rs`/`thumbnail.rs`.
fn write_sidecar_file(sidecar: &Path, rec: &ImageRecord) -> Result<(), String> {
    let parent = sidecar
        .parent()
        .ok_or_else(|| "sidecar path has no parent".to_string())?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let bytes = serde_json::to_vec_pretty(rec).map_err(|e| e.to_string())?;
    let tmp = sidecar.with_extension(format!("{SIDECAR_EXT}.tmp"));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, sidecar) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

/// Outcome of a [`migrate_legacy_catalog`] pass, mainly useful for tests and
/// diagnostics; `App::new()` currently discards it (failures are non-fatal
/// and retried on the next launch).
#[allow(dead_code)] // fields read from #[cfg(test)] today; App::new() discards the value
pub struct MigrationSummary {
    /// Rows successfully written as sidecars (or already empty/migrated).
    pub migrated: usize,
    /// Rows that could not be migrated this pass (missing/unwritable target
    /// directory) — retried on the next call since the source is left intact.
    pub skipped: usize,
    /// True when there was nothing to migrate (no legacy file found).
    pub already_done: bool,
}

/// One-time, directory-independent migration of the old global
/// `catalog.json` into per-photo sidecars. Safe to call on every launch: a
/// fast `Path::exists()` check makes it a no-op once fully migrated, and a
/// partial pass (some rows' target directories missing/unwritable) is
/// safely retriable — the legacy file is only retired once every row in it
/// resolved with zero skips.
pub fn migrate_legacy_catalog() -> MigrationSummary {
    let dir = default_dir();
    crate::paths::migrate_legacy_dir(&dir, &legacy_dir());

    let json_file = dir.join(CATALOG_FILE);
    if json_file.exists() {
        return migrate_json(&json_file);
    }
    MigrationSummary {
        migrated: 0,
        skipped: 0,
        already_done: true,
    }
}

/// Fan `images` out to sidecar files, one per row. Returns `(migrated,
/// skipped)`. A row is "migrated" if it's empty (nothing to write), its
/// sidecar already exists (idempotent re-run), or the write succeeds.
/// "Skipped" covers a missing/unwritable target directory or a write
/// failure — always retriable on a later pass.
fn fan_out(images: &HashMap<PathBuf, ImageRecord>) -> (usize, usize) {
    let mut migrated = 0usize;
    let mut skipped = 0usize;
    for (path, rec) in images {
        if rec.is_empty() {
            migrated += 1;
            continue;
        }
        let Some(sidecar) = sidecar_path(path) else {
            skipped += 1;
            continue;
        };
        if sidecar.exists() {
            migrated += 1;
            continue;
        }
        match path.parent() {
            Some(parent) if parent.exists() => match write_sidecar_file(&sidecar, rec) {
                Ok(()) => migrated += 1,
                Err(e) => {
                    eprintln!("[catalog] migration: could not write {}: {e}", sidecar.display());
                    skipped += 1;
                }
            },
            _ => skipped += 1, // parent directory missing/unmounted: retry later
        }
    }
    (migrated, skipped)
}

fn migrate_json(json_file: &Path) -> MigrationSummary {
    let images = match std::fs::read(json_file).map(|b| parse_catalog(&b)) {
        Ok(Ok(m)) => m,
        Ok(Err(e)) => {
            eprintln!("[catalog] ignoring corrupt {}: {e}", json_file.display());
            return MigrationSummary {
                migrated: 0,
                skipped: 0,
                already_done: false,
            };
        }
        Err(e) => {
            eprintln!("[catalog] migration: could not read {}: {e}", json_file.display());
            return MigrationSummary {
                migrated: 0,
                skipped: 0,
                already_done: false,
            };
        }
    };
    let (migrated, skipped) = fan_out(&images);
    if skipped == 0 {
        retire(json_file, "json.bak");
    }
    eprintln!("[catalog] migration (json): {migrated} migrated, {skipped} skipped");
    MigrationSummary {
        migrated,
        skipped,
        already_done: false,
    }
}

/// Rename `file` to `<file>.<new_ext>` (e.g. `catalog.json` → `catalog.json.bak`),
/// leaving it in place if a backup already exists there (don't clobber an
/// earlier one) or the rename fails.
fn retire(file: &Path, new_ext: &str) {
    let bak = file.with_extension(new_ext);
    if bak.exists() {
        eprintln!(
            "[catalog] {} already exists; leaving {} in place",
            bak.display(),
            file.display()
        );
        return;
    }
    if let Err(e) = std::fs::rename(file, &bak) {
        eprintln!("[catalog] could not retire {}: {e}", file.display());
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
                        touchups: Vec::new(),
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
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
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
            "lightphotos-catalog-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn sidecar_for(dir: &Path, filename: &str) -> PathBuf {
        dir.join(SIDECAR_DIR).join(format!("{filename}.xmp"))
    }

    #[test]
    fn round_trip_persists_across_reload() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg"); // nonexistent — exercises the fallback.

        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 3);
            assert_eq!(cat.get(&p), Some(3));
        }

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
    fn denoise_persists_across_reload() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        let mut adj = Adjustments::default();
        adj.denoise = 40.0;

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
    fn touchups_persist_across_reload_and_can_be_removed() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        let t = TouchUp {
            center: [0.4, 0.5],
            radius: 0.02,
            source: [0.6, 0.5],
            feather: 0.5,
            delta: [0.01, -0.02, 0.0],
        };
        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set_touchups(&p, &[t]);
            assert_eq!(cat.touchups(&p), vec![t]);
        }
        let mut reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.touchups(&p), vec![t]);
        reloaded.set_touchups(&p, &[]);
        assert!(Catalog::with_dir(dir.clone()).touchups(&p).is_empty());
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

    #[test]
    fn identity_adjustments_omitted_from_sidecar_json() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        // A rated image with identity edits: sidecar has no "adjustments" key.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            cat.set(&p, 2);
        }
        let bytes = std::fs::read(sidecar_for(&dir, "photo.jpg")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.get("adjustments").is_none(),
            "identity adjustments must be omitted, not stored as an empty object"
        );

        // A non-identity edit stores an "adjustments" object.
        {
            let mut cat = Catalog::with_dir(dir.clone());
            let mut adj = Adjustments::default();
            adj.shadows = 40.0;
            cat.set_adjustments(&p, &adj);
        }
        let bytes = std::fs::read(sidecar_for(&dir, "photo.jpg")).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(
            value.get("adjustments").is_some(),
            "non-identity adjustments must be stored"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn failed_persist_is_reported_once_via_take_error() {
        // Point the catalog at a directory whose .lightphotos can't be
        // created because a *file* sits where it would need to go, so
        // create_dir_all (and thus persist) fails deterministically.
        let dir = unique_tmp_dir();
        std::fs::write(dir.join(SIDECAR_DIR), b"not a dir").unwrap();

        let mut cat = Catalog::with_dir(dir.clone());
        assert!(cat.take_error().is_none(), "no error before any write");

        cat.set(&dir.join("photo.jpg"), 3);
        let msg = cat
            .take_error()
            .expect("failed save should report an error");
        assert!(
            msg.contains("Failed to save"),
            "error should describe the failed save, got: {msg}"
        );
        assert!(
            cat.take_error().is_none(),
            "error should be taken only once"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unreadable_sidecar_is_reported_not_silently_dropped() {
        let dir = unique_tmp_dir();
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&dir, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = Catalog::with_dir(dir.clone());
        assert_eq!(
            cat.get(&dir.join("bad.jpg")),
            None,
            "an unreadable sidecar is not loaded"
        );
        assert!(
            cat.take_error().is_some(),
            "an unreadable sidecar should be surfaced, not silently skipped"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn corrupt_sidecar_does_not_block_other_photos_in_same_directory() {
        let dir = unique_tmp_dir();
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&dir, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = Catalog::with_dir(dir.clone());
        let _ = cat.take_error();
        cat.set(&dir.join("good.jpg"), 4);
        assert_eq!(cat.get(&dir.join("good.jpg")), Some(4));

        // Bad sidecar is untouched by the unrelated write, and only replaced
        // once the user makes an edit for that exact photo.
        assert_eq!(
            std::fs::read(sidecar_for(&dir, "bad.jpg")).unwrap(),
            b"{not valid json"
        );
        cat.set(&dir.join("bad.jpg"), 5);
        assert_eq!(cat.get(&dir.join("bad.jpg")), Some(5));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn same_stem_raw_and_jpeg_get_independent_sidecars() {
        let dir = unique_tmp_dir();
        let raw = dir.join("PHOTO1.ARW");
        let jpg = dir.join("PHOTO1.JPG");

        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&raw, 5);
        cat.set(&jpg, 2);
        assert_eq!(cat.get(&raw), Some(5));
        assert_eq!(cat.get(&jpg), Some(2));
        assert!(sidecar_for(&dir, "PHOTO1.ARW").exists());
        assert!(sidecar_for(&dir, "PHOTO1.JPG").exists());

        let reloaded = Catalog::with_dir(dir.clone());
        assert_eq!(reloaded.get(&raw), Some(5));
        assert_eq!(reloaded.get(&jpg), Some(2));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lightphotos_dir_created_lazily_only_on_first_write() {
        let dir = unique_tmp_dir();
        let mut cat = Catalog::with_dir(dir.clone());
        assert!(
            !dir.join(SIDECAR_DIR).exists(),
            "opening/reading a directory must not create .lightphotos"
        );
        cat.set(&dir.join("photo.jpg"), 3);
        assert!(
            dir.join(SIDECAR_DIR).exists(),
            ".lightphotos should appear after the first write"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_record_deletes_the_sidecar_file() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 3);
        assert!(sidecar_for(&dir, "photo.jpg").exists());
        cat.set(&p, 0);
        assert!(
            !sidecar_for(&dir, "photo.jpg").exists(),
            "clearing the rating on an otherwise-identity record should delete the sidecar"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn remove_deletes_sidecar_file() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 4);
        assert!(sidecar_for(&dir, "photo.jpg").exists());
        cat.remove(&p);
        assert_eq!(cat.get(&p), None);
        assert!(!sidecar_for(&dir, "photo.jpg").exists());
        // Removing again (already gone) is a harmless no-op.
        cat.remove(&p);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn open_dir_switches_active_directory_without_cross_directory_leakage() {
        let a = unique_tmp_dir();
        let b = unique_tmp_dir();
        let pa = a.join("photo.jpg");
        let pb = b.join("photo.jpg"); // same filename, different directory

        let mut cat = Catalog::with_dir(a.clone());
        cat.set(&pa, 5);

        cat.open_dir(&b);
        assert_eq!(
            cat.get(&pb),
            None,
            "directory b's same-named photo must not see directory a's rating"
        );
        cat.set(&pb, 2);
        assert_eq!(cat.get(&pb), Some(2));

        cat.open_dir(&a);
        assert_eq!(
            cat.get(&pa),
            Some(5),
            "switching back to a must restore a's persisted rating"
        );

        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    // --- migration ---------------------------------------------------

    fn write_legacy_json(dir: &Path, rows: &[(&Path, Option<u8>, Option<&Adjustments>)]) {
        let images: HashMap<PathBuf, ImageRecord> = rows
            .iter()
            .map(|(path, rating, adj)| {
                (
                    path.to_path_buf(),
                    ImageRecord {
                        rating: *rating,
                        adjustments: adj.copied().unwrap_or_default(),
                        touchups: Vec::new(),
                        rotation: 0,
                    },
                )
            })
            .collect();
        let contents = serde_json::json!({ "version": 2, "images": images });
        std::fs::write(
            dir.join(CATALOG_FILE),
            serde_json::to_vec(&contents).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn migrate_no_op_when_nothing_legacy_present() {
        // migrate_legacy_catalog() always targets the real app-support dir,
        // so this just checks the reported shape holds for the "nothing to
        // do" case using the fan_out/migrate_sqlite building blocks directly.
        let dir = unique_tmp_dir();
        let images: HashMap<PathBuf, ImageRecord> = HashMap::new();
        let (migrated, skipped) = fan_out(&images);
        assert_eq!((migrated, skipped), (0, 0));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn migrate_fans_rows_out_to_correct_directories_and_retires_json() {
        let app_dir = unique_tmp_dir();
        let photo_dir_1 = unique_tmp_dir();
        let photo_dir_2 = unique_tmp_dir();

        let p1 = photo_dir_1.join("a.jpg");
        let p2 = photo_dir_2.join("b.jpg");
        write_legacy_json(&app_dir, &[(&p1, Some(4), None), (&p2, Some(2), None)]);

        let summary = migrate_json(&app_dir.join(CATALOG_FILE));
        assert_eq!(summary.migrated, 2);
        assert_eq!(summary.skipped, 0);
        assert!(!summary.already_done);

        assert!(sidecar_for(&photo_dir_1, "a.jpg").exists());
        assert!(sidecar_for(&photo_dir_2, "b.jpg").exists());
        assert!(
            app_dir.join("catalog.json.bak").exists(),
            "fully-resolved migration should retire catalog.json"
        );
        assert!(!app_dir.join(CATALOG_FILE).exists());

        std::fs::remove_dir_all(&app_dir).unwrap();
        std::fs::remove_dir_all(&photo_dir_1).unwrap();
        std::fs::remove_dir_all(&photo_dir_2).unwrap();
    }

    #[test]
    fn migrate_skips_missing_directory_and_retries_next_pass() {
        let app_dir = unique_tmp_dir();
        let missing = app_dir.join("does-not-exist-anywhere");
        let p = missing.join("a.jpg");
        write_legacy_json(&app_dir, &[(&p, Some(4), None)]);

        let summary = migrate_json(&app_dir.join(CATALOG_FILE));
        assert_eq!(summary.migrated, 0);
        assert_eq!(summary.skipped, 1);
        assert!(
            app_dir.join(CATALOG_FILE).exists(),
            "a partial migration must NOT retire catalog.json"
        );

        // Now the target becomes available and a re-run resolves it,
        // demonstrating idempotent retry.
        std::fs::create_dir_all(&missing).unwrap();
        let summary2 = migrate_json(&app_dir.join(CATALOG_FILE));
        assert_eq!(summary2.migrated, 1);
        assert_eq!(summary2.skipped, 0);
        assert!(app_dir.join("catalog.json.bak").exists());

        // `missing` is nested under `app_dir` — removing app_dir takes it too.
        std::fs::remove_dir_all(&app_dir).unwrap();
    }

    #[test]
    fn migrate_is_idempotent_on_already_written_sidecar() {
        let app_dir = unique_tmp_dir();
        let photo_dir = unique_tmp_dir();
        let p = photo_dir.join("a.jpg");
        write_legacy_json(&app_dir, &[(&p, Some(3), None)]);

        // Pre-seed the sidecar as if a previous partial pass (or the live
        // app) already wrote it, with a DIFFERENT rating.
        write_sidecar_file(
            &sidecar_for(&photo_dir, "a.jpg"),
            &ImageRecord {
                rating: Some(1),
                ..Default::default()
            },
        )
        .unwrap();

        let summary = migrate_json(&app_dir.join(CATALOG_FILE));
        assert_eq!(summary.migrated, 1);
        assert_eq!(summary.skipped, 0);

        let cat = Catalog::with_dir(photo_dir.clone());
        assert_eq!(
            cat.get(&p),
            Some(1),
            "an already-existing sidecar must not be clobbered by migration"
        );

        std::fs::remove_dir_all(&app_dir).unwrap();
        std::fs::remove_dir_all(&photo_dir).unwrap();
    }
}
