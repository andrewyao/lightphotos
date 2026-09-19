// SPDX-License-Identifier: GPL-3.0-or-later

//! Per-photo sidecars that persist ratings and develop edits. Each photo's
//! record lives in `<photo dir>/.lightphotos/<photo filename>.xmp`, so edits
//! travel with the folder and originals are never touched. The body is our
//! own JSON, not Adobe XMP; the extension is cosmetic. `.lightphotos/` is
//! created on the first write, so browsing a folder never changes it.
//!
//! [`Catalog`] caches one directory at a time. Reads and writes derive the
//! sidecar path from the photo's own path.

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, TouchUp};

/// Hidden subfolder holding a directory's sidecars and thumbnail cache.
pub(crate) const SIDECAR_DIR: &str = ".lightphotos";
/// Sidecar file extension. Cosmetic; see the module docs.
pub(crate) const SIDECAR_EXT: &str = "xmp";

/// Persisted state for one photo. Default fields are skipped on write, so a
/// rated but unedited photo's sidecar stays small.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ImageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Adjustments::is_identity")]
    pub adjustments: Adjustments,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touchups: Vec<TouchUp>,
    /// Manual rotation in 90° clockwise steps, `0..=3`.
    #[serde(default, skip_serializing_if = "is_zero_rot")]
    pub rotation: u8,
}

fn is_zero_rot(v: &u8) -> bool {
    *v == 0
}

impl ImageRecord {
    /// True when there is nothing to persist. An empty record's sidecar is
    /// deleted instead of written.
    pub(crate) fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.adjustments.is_identity()
            && self.touchups.is_empty()
            && self.rotation == 0
    }
}

/// Records for the active directory, backed by its sidecar files. `images`
/// is a read cache keyed by filename.
pub struct Catalog {
    images: HashMap<OsString, ImageRecord>,
    /// Filenames written or removed since [`Catalog::switch_dir`]. A
    /// background load must not overwrite these. It covers removals, which
    /// look the same as "not loaded yet" in `images`.
    dirty: HashSet<OsString>,
    /// The active directory. `None` before the first folder opens.
    dir: Option<PathBuf>,
    /// The latest persist failure, drained by [`Catalog::take_error`] into a
    /// toast.
    last_error: Option<String>,

    /// The active folder's File System Access handle. wasm32 has no OS paths,
    /// so sidecar I/O goes through this.
    #[cfg(target_arch = "wasm32")]
    wasm_dir_handle: Option<web_sys::FileSystemDirectoryHandle>,
    /// wasm32 sidecar writes run as detached `spawn_local` tasks and report
    /// failures here. [`Catalog::poll_persist_errors`] drains it each frame.
    #[cfg(target_arch = "wasm32")]
    persist_err_tx: std::sync::mpsc::Sender<String>,
    #[cfg(target_arch = "wasm32")]
    persist_err_rx: std::sync::mpsc::Receiver<String>,
}

impl Catalog {
    /// An empty catalog with no active directory.
    pub fn new() -> Catalog {
        #[cfg(target_arch = "wasm32")]
        let (persist_err_tx, persist_err_rx) = std::sync::mpsc::channel();
        Catalog {
            images: HashMap::new(),
            dirty: HashSet::new(),
            dir: None,
            last_error: None,
            #[cfg(target_arch = "wasm32")]
            wasm_dir_handle: None,
            #[cfg(target_arch = "wasm32")]
            persist_err_tx,
            #[cfg(target_arch = "wasm32")]
            persist_err_rx,
        }
    }

    /// Set the active folder's handle on every folder switch. `None` clears
    /// it, so writes fail loudly instead of landing in the previous folder.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn set_wasm_dir_handle(
        &mut self,
        handle: Option<web_sys::FileSystemDirectoryHandle>,
    ) {
        self.wasm_dir_handle = handle;
    }

    /// Move async persist failures into `last_error`. Call once per frame.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn poll_persist_errors(&mut self) {
        while let Ok(e) = self.persist_err_rx.try_recv() {
            self.note_persist_error(e);
        }
    }

    /// `new()` plus `open_dir(&dir)`.
    #[allow(dead_code)] // only called from #[cfg(test)] today
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let mut cat = Catalog::new();
        cat.open_dir(&dir);
        cat
    }

    /// Make `dir` active and reload its sidecars from disk, blocking. The app
    /// instead calls [`Catalog::switch_dir`], runs `load_sidecars` on a
    /// background thread, then calls [`Catalog::apply_loaded`]. Native only,
    /// because File System Access has no synchronous reads.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_dir(&mut self, dir: &Path) {
        self.switch_dir(dir);
        let loaded = load_sidecars(dir);
        self.apply_loaded(dir, loaded);
    }

    /// Make `dir` active and clear the cache without disk I/O, so no lookup
    /// sees the previous directory's records.
    pub(crate) fn switch_dir(&mut self, dir: &Path) {
        self.dir = Some(dir.to_path_buf());
        self.images.clear();
        self.dirty.clear();
    }

    /// Whether `dir` is the directory represented by the in-memory cache.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn is_active_dir(&self, dir: &Path) -> bool {
        self.dir.as_deref() == Some(dir)
    }

    /// Merge a background load into the cache if `dir` is still active.
    /// Keys in `dirty` are skipped, because the load's snapshot is older than
    /// any edit made after `switch_dir`. Unreadable sidecars are reported
    /// even when the load is stale.
    pub(crate) fn apply_loaded(&mut self, dir: &Path, loaded: SidecarLoad) {
        if loaded.skipped > 0 {
            self.last_error = Some(skipped_message(loaded.skipped));
        }
        if self.dir.as_deref() != Some(dir) {
            return;
        }
        for (name, rec) in loaded.images {
            if !self.dirty.contains(&name) {
                self.images.insert(name, rec);
            }
        }
    }

    /// Take the pending persist error. Each failure is returned once.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Log a persist failure and keep it for the UI to show.
    pub(crate) fn note_persist_error(&mut self, e: impl std::fmt::Display) {
        let msg = format!("Failed to save catalog entry: {e}");
        eprintln!("[catalog] {msg}");
        self.last_error = Some(msg);
    }

    pub fn get(&self, path: &Path) -> Option<u8> {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .and_then(|r| r.rating)
    }

    /// Set the rating, clamped to `0..=5`. `0` clears it.
    pub fn set(&mut self, path: &Path, stars: u8) {
        let stars = stars.min(5);
        let rating = if stars == 0 { None } else { Some(stars) };
        self.update(path, |rec| rec.rating = rating);
    }

    pub fn adjustments(&self, path: &Path) -> Adjustments {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .map(|r| r.adjustments)
            .unwrap_or_default()
    }

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

    pub fn rotation(&self, path: &Path) -> u8 {
        path.file_name()
            .and_then(|n| self.images.get(n))
            .map(|r| r.rotation)
            .unwrap_or(0)
    }

    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let rotation = rotation % 4;
        self.update(path, |rec| rec.rotation = rotation);
    }

    /// Delete the record and sidecar for `path`, when its photo is deleted.
    /// The sidecar skips the Trash, since it is useless apart from its photo.
    pub fn remove(&mut self, path: &Path) {
        if let Some(name) = path.file_name() {
            self.images.remove(name);
            self.dirty.insert(name.to_os_string());
        }
        if let Err(e) = self.delete_sidecar(path) {
            self.note_persist_error(e);
        }
    }

    /// Apply `mutate` to the record for `path`, then write its sidecar, or
    /// delete it if the record became empty. Native writes are atomic.
    fn update(&mut self, path: &Path, mutate: impl FnOnce(&mut ImageRecord)) {
        let Some(name) = path.file_name().map(|n| n.to_os_string()) else {
            return;
        };
        self.dirty.insert(name.clone());
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

    #[cfg(not(target_arch = "wasm32"))]
    fn write_sidecar(&self, path: &Path, rec: &ImageRecord) -> Result<(), String> {
        let sidecar =
            sidecar_path(path).ok_or_else(|| "cannot determine sidecar path".to_string())?;
        write_sidecar_file(&sidecar, rec)
    }

    #[cfg(not(target_arch = "wasm32"))]
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

    /// File System Access has no synchronous write, so this starts a
    /// `spawn_local` task. Only a missing folder handle fails here. Write
    /// failures arrive later through `persist_err_tx`.
    #[cfg(target_arch = "wasm32")]
    fn write_sidecar(&self, path: &Path, rec: &ImageRecord) -> Result<(), String> {
        let Some(name) = path.file_name() else {
            return Ok(());
        };
        let Some(dir_handle) = self.wasm_dir_handle.clone() else {
            return Err("no folder handle for this photo's directory".to_string());
        };
        let bytes = serde_json::to_vec_pretty(rec).map_err(|e| e.to_string())?;
        let name = name.to_os_string();
        let tx = self.persist_err_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::web_catalog_fs::write_sidecar(&dir_handle, &name, &bytes).await {
                let _ = tx.send(format!("could not save {}: {e}", name.to_string_lossy()));
            }
        });
        Ok(())
    }

    #[cfg(target_arch = "wasm32")]
    fn delete_sidecar(&self, path: &Path) -> Result<(), String> {
        let Some(name) = path.file_name() else {
            return Ok(());
        };
        // Without a handle nothing was written, so there is nothing to delete.
        let Some(dir_handle) = self.wasm_dir_handle.clone() else {
            return Ok(());
        };
        let name = name.to_os_string();
        let tx = self.persist_err_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::web_catalog_fs::delete_sidecar(&dir_handle, &name).await {
                let _ = tx.send(format!("could not delete {}: {e}", name.to_string_lossy()));
            }
        });
        Ok(())
    }

    /// Delete a sidecar through a captured handle, for a photo deletion that
    /// finishes after the user navigated to another folder.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn delete_sidecar_with_handle(
        &self,
        path: &Path,
        dir_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        let Some(name) = path.file_name() else {
            return;
        };
        let name = name.to_os_string();
        let dir_handle = dir_handle.clone();
        let tx = self.persist_err_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::web_catalog_fs::delete_sidecar(&dir_handle, &name).await {
                let _ = tx.send(format!("could not delete {}: {e}", name.to_string_lossy()));
            }
        });
    }

    /// [`Catalog::remove`] through a captured directory handle.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn remove_with_handle(
        &mut self,
        path: &Path,
        dir_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        if let Some(name) = path.file_name() {
            self.images.remove(name);
            self.dirty.insert(name.to_os_string());
        }
        self.delete_sidecar_with_handle(path, dir_handle);
    }
}

impl Default for Catalog {
    fn default() -> Catalog {
        Catalog::new()
    }
}

/// The sidecars read from one directory, plus a count of unreadable ones.
pub(crate) struct SidecarLoad {
    pub images: HashMap<OsString, ImageRecord>,
    pub skipped: usize,
}

/// Read every sidecar in `dir/.lightphotos/`. Needs no `Catalog`, so it can
/// run on a background thread. A missing folder gives an empty result.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_sidecars(dir: &Path) -> SidecarLoad {
    let mut images = HashMap::new();
    let mut skipped = 0usize;

    let entries = match std::fs::read_dir(dir.join(SIDECAR_DIR)) {
        Ok(rd) => rd,
        Err(_) => return SidecarLoad { images, skipped },
    };

    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some(SIDECAR_EXT) {
            continue;
        }
        // `file_stem` strips only ".xmp", so "PHOTO1.ARW" keeps its dot.
        let Some(stem) = path.file_stem() else {
            continue;
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ImageRecord>(&bytes) {
                Ok(rec) if !rec.is_empty() => {
                    images.insert(stem.to_os_string(), rec);
                }
                Ok(_) => {}
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

    SidecarLoad { images, skipped }
}

fn skipped_message(skipped: usize) -> String {
    format!(
        "{skipped} catalog entr{} could not be read and {} skipped.",
        if skipped == 1 { "y" } else { "ies" },
        if skipped == 1 { "was" } else { "were" },
    )
}

/// `<dir>/.lightphotos/<filename>.xmp`, or `None` for a path like `/` or `..`.
/// Built from `OsString` so non-UTF-8 names stay exact.
#[cfg(not(target_arch = "wasm32"))]
fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let name = path.file_name()?;
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".");
    sidecar_name.push(SIDECAR_EXT);
    Some(dir.join(SIDECAR_DIR).join(sidecar_name))
}

/// Write `rec` as JSON to `sidecar` through a `.tmp` sibling and rename, so a
/// crash never leaves a partial file.
#[cfg(not(target_arch = "wasm32"))]
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// A unique temp dir under `std::env::temp_dir()`.
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
        // A file named .lightphotos makes create_dir_all, and so every
        // write, fail.
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
        // Removing again is a no-op.
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

    #[test]
    fn apply_loaded_is_discarded_for_a_directory_no_longer_active() {
        let a = unique_tmp_dir();
        let b = unique_tmp_dir();
        let pa = a.join("photo.jpg");

        let mut cat = Catalog::with_dir(a.clone());
        cat.set(&pa, 5);
        // A background load for `a` was started, but before it lands the
        // active directory switches to `b`.
        cat.switch_dir(&b);
        assert_eq!(cat.get(&pa), None, "switching clears the cache immediately");

        // The stale `a` load lands and must be ignored.
        let stale = load_sidecars(&a);
        cat.apply_loaded(&a, stale);
        assert_eq!(
            cat.get(&pa),
            None,
            "a load for a directory that's no longer active must be discarded"
        );

        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    #[test]
    fn apply_loaded_does_not_clobber_a_local_write_made_after_switch() {
        let dir = unique_tmp_dir();
        let written = dir.join("written.jpg");
        let other = dir.join("other.jpg");

        // `other.jpg` already has a rating on disk from a previous session.
        let mut seed = Catalog::with_dir(dir.clone());
        seed.set(&other, 3);
        drop(seed);

        let mut cat = Catalog::new();
        cat.switch_dir(&dir);
        // A write lands after switch_dir but before the background load,
        // whose snapshot predates the write, returns.
        cat.set(&written, 5);
        let loaded = load_sidecars(&dir); // snapshot predates `written`'s sidecar...
        cat.apply_loaded(&dir, loaded);

        assert_eq!(
            cat.get(&written),
            Some(5),
            "a local write made while a load was in flight must survive apply_loaded"
        );
        assert_eq!(
            cat.get(&other),
            Some(3),
            "entries only present in the loaded snapshot must still be merged in"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_loaded_does_not_resurrect_a_locally_removed_record() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        // A rating exists on disk from a previous session.
        Catalog::with_dir(dir.clone()).set(&p, 4);

        let mut cat = Catalog::new();
        cat.switch_dir(&dir);
        let loaded = load_sidecars(&dir); // snapshot still carries the rating
        cat.remove(&p); // the user clears it before the load lands
        cat.apply_loaded(&dir, loaded);

        assert_eq!(
            cat.get(&p),
            None,
            "a local removal made while a load was in flight must not be \
             resurrected by a stale snapshot taken before it"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_loaded_still_surfaces_skipped_count_for_a_stale_directory() {
        let a = unique_tmp_dir();
        let b = unique_tmp_dir();
        std::fs::create_dir_all(a.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&a, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = Catalog::new();
        cat.switch_dir(&a);
        let loaded = load_sidecars(&a); // has skipped == 1
        cat.switch_dir(&b); // the user already left `a` before the load lands
        cat.apply_loaded(&a, loaded);

        assert!(
            cat.take_error().is_some(),
            "a corrupt sidecar found by a stale (directory-since-changed) load \
             must still be surfaced, not silently dropped"
        );

        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    #[test]
    fn load_sidecars_reads_a_directory_without_mutating_a_catalog() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        Catalog::with_dir(dir.clone()).set(&p, 4);

        let loaded = load_sidecars(&dir);
        assert_eq!(loaded.skipped, 0);
        assert_eq!(
            loaded
                .images
                .get(std::ffi::OsStr::new("photo.jpg"))
                .and_then(|r| r.rating),
            Some(4)
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
