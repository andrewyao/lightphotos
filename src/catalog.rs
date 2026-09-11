// SPDX-License-Identifier: GPL-3.0-or-later

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
//! along with it. Originals are never touched, and `.lightphotos` is
//! created lazily (only on the first write for that directory), so
//! browsing a folder read-only never litters it.
//!
//! [`Catalog`] is scoped to one directory at a time (the "active"
//! directory) — there is no cross-folder cache or index. Opening a
//! different folder calls [`Catalog::open_dir`], which reloads the
//! in-memory read cache from that directory's `.lightphotos/*.xmp` files.
//! Individual reads/writes locate their sidecar directly from the photo's
//! own path (not from the active directory), so they stay correct even if
//! called for a path outside it.
//!
//! (An older global `catalog.json`, and before that a global SQLite
//! `catalog.db`, both predate this per-directory sidecar design. Neither
//! is auto-migrated anymore — that one-time migration path was removed
//! once it was no longer needed.)

use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, TouchUp};

/// Hidden per-directory subfolder holding that directory's sidecar files.
/// `pub(crate)` so `web_catalog_fs.rs` (wasm32's File System Access
/// counterpart to this module's std::fs calls) names the exact same
/// subfolder rather than duplicating the literal.
pub(crate) const SIDECAR_DIR: &str = ".lightphotos";
/// Sidecar file extension (cosmetic only — see module docs).
pub(crate) const SIDECAR_EXT: &str = "xmp";

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
    /// `pub(crate)` so `web_catalog_fs.rs`'s load path can apply the same
    /// "don't cache an empty record" rule `load_sidecars` uses below.
    pub(crate) fn is_empty(&self) -> bool {
        self.rating.is_none()
            && self.adjustments.is_identity()
            && self.touchups.is_empty()
            && self.rotation == 0
    }
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
    /// Filenames written or removed (via `update`/`remove`) since the last
    /// [`Catalog::switch_dir`], so [`Catalog::apply_loaded`] knows which keys
    /// a background load's snapshot must not touch — including a key the
    /// local write *deleted*, which `images` alone can't distinguish from
    /// "never loaded yet" (both are simply absent). Cleared on every
    /// `switch_dir`, since it's scoped to "since the active directory
    /// became active", same as `images` itself.
    dirty: HashSet<OsString>,
    /// The directory currently active, or `None` before the first
    /// [`Catalog::open_dir`] call — degrades to an empty catalog rather than
    /// panicking.
    dir: Option<PathBuf>,
    /// The most recent persist failure, if any, awaiting delivery to the
    /// user. Set whenever a write fails; drained by [`Catalog::take_error`]
    /// so the UI can surface a toast instead of the change being silently
    /// lost.
    last_error: Option<String>,

    /// The active directory's root folder handle — File System Access has
    /// no real OS path for `std::fs` to use, so wasm32's sidecar I/O
    /// (`write_sidecar`/`delete_sidecar` below) needs this instead. Set via
    /// [`Catalog::set_wasm_dir_handle`] (`app/web.rs`'s `poll_folder_pick`
    /// and `apply_web_load_folder`) right after a folder becomes active,
    /// before `open_dir`'s wasm32 counterpart (`app/catalog.rs`'s
    /// `request_catalog_load`) needs it to read `.lightphotos/*.xmp` back.
    #[cfg(target_arch = "wasm32")]
    wasm_dir_handle: Option<web_sys::FileSystemDirectoryHandle>,
    /// Sidecar writes/deletes are fire-and-forget `spawn_local` tasks (see
    /// the wasm32 arms of `write_sidecar`/`delete_sidecar` below) — this is
    /// how a failure gets back to `last_error` despite not being on the call
    /// stack that triggered the write. Drained each frame by
    /// [`Catalog::poll_persist_errors`] (`main.rs`'s `about_to_wait`).
    #[cfg(target_arch = "wasm32")]
    persist_err_tx: std::sync::mpsc::Sender<String>,
    #[cfg(target_arch = "wasm32")]
    persist_err_rx: std::sync::mpsc::Receiver<String>,
}

impl Catalog {
    /// A directory-less catalog with nothing loaded yet. Used at startup,
    /// before any folder/file has been opened.
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

    /// Point wasm32's sidecar I/O at `handle` (the active folder's root) —
    /// called on every folder switch, before the catalog load it also
    /// triggers needs it. See `wasm_dir_handle`'s doc comment.
    ///
    /// Takes an `Option` and is called unconditionally, so a folder with no
    /// handle *clears* this rather than leaving the previous folder's in
    /// place. Sidecar writes then fail loudly (`write_sidecar` returns an
    /// error the toast path surfaces) instead of quietly saving one folder's
    /// ratings and develop edits into another folder's `.lightphotos/`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn set_wasm_dir_handle(
        &mut self,
        handle: Option<web_sys::FileSystemDirectoryHandle>,
    ) {
        self.wasm_dir_handle = handle;
    }

    /// Drain persist failures that landed asynchronously since the last
    /// poll (writes/deletes are fire-and-forget on wasm32 — see
    /// `write_sidecar`/`delete_sidecar`'s wasm32 arms) into `last_error`,
    /// same one-shot-per-frame convention as every other `poll_*` in this
    /// codebase.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn poll_persist_errors(&mut self) {
        while let Ok(e) = self.persist_err_rx.try_recv() {
            self.note_persist_error(e);
        }
    }

    /// `new()` + `open_dir(&dir)` in one step — a convenience mainly used by
    /// tests, which always know their directory up front. Native/test-only —
    /// see `open_dir`'s doc comment on why wasm32 has no synchronous
    /// counterpart at all.
    #[allow(dead_code)] // only called from #[cfg(test)] today
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_dir(dir: PathBuf) -> Catalog {
        let mut cat = Catalog::new();
        cat.open_dir(&dir);
        cat
    }

    /// (Re)point the catalog at `dir` as the active directory and rebuild the
    /// read cache from `dir/.lightphotos/*.xmp`, synchronously. Always
    /// reloads from disk, even if `dir` equals the previously-active
    /// directory, so an external change (another process, a hand-fixed
    /// sidecar) is picked up. A missing `.lightphotos` directory is not an
    /// error — it's just an empty catalog; the directory itself is never
    /// eagerly created here.
    ///
    /// This blocks on disk I/O proportional to `dir`'s sidecar count — for
    /// the async equivalent used by the live app (so opening a heavily
    /// rated/edited directory never stalls first paint), see
    /// [`Catalog::switch_dir`] + [`Catalog::apply_loaded`], composed exactly
    /// as this function does but with `load_sidecars` run on a background
    /// thread in between. Native-only: File System Access has no
    /// synchronous read at all (everything is a Promise), so wasm32 has no
    /// equivalent of this function — `app/catalog.rs`'s `request_catalog_load`
    /// is the only path there, always async.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn open_dir(&mut self, dir: &Path) {
        self.switch_dir(dir);
        let loaded = load_sidecars(dir);
        self.apply_loaded(dir, loaded);
    }

    /// Point the catalog at `dir` and clear the read cache immediately —
    /// no disk I/O. This alone is what prevents cross-directory leakage
    /// (see `open_dir_switches_active_directory_without_cross_directory_leakage`):
    /// a lookup for the new directory's photos returns nothing (correctly
    /// "not yet known") rather than a stale entry from whatever directory
    /// was active before, until [`Catalog::apply_loaded`] populates it.
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

    /// Merge a [`SidecarLoad`] (typically produced by `load_sidecars` on a
    /// background thread) into the cache, if `dir` is still the active
    /// directory — a load whose directory was since switched away from is
    /// silently discarded, same as `poll_selection_mask`'s stale-result
    /// handling.
    ///
    /// Skips any key in `dirty` rather than replacing `images` outright:
    /// `switch_dir` clears the cache immediately, so a `set`/
    /// `set_adjustments`/`remove`/etc. call arriving after switch but before
    /// this load lands mutates the (now-empty) cache directly — a full
    /// replace would clobber that local write (or, for `remove`, resurrect a
    /// record the user just deleted, since an absent key can't otherwise be
    /// told apart from "not loaded yet") with `loaded`'s necessarily-older
    /// disk snapshot. The sidecar on disk is unaffected either way; this
    /// only protects the in-memory read cache from momentarily
    /// reverting/resurrecting.
    ///
    /// `skipped` (corrupt/unreadable sidecars found during the scan) is
    /// folded into `last_error` unconditionally, even for a directory the
    /// user has since navigated away from — the old synchronous `open_dir`
    /// always surfaced this, and a real read failure on disk doesn't stop
    /// being true just because it's no longer the active directory.
    pub(crate) fn apply_loaded(&mut self, dir: &Path, loaded: SidecarLoad) {
        if loaded.skipped > 0 {
            self.last_error = Some(skipped_message(loaded.skipped));
        }
        if self.dir.as_deref() != Some(dir) {
            return; // stale: the active directory has since changed
        }
        for (name, rec) in loaded.images {
            if !self.dirty.contains(&name) {
                self.images.insert(name, rec);
            }
        }
    }

    /// Take the pending persist error, if any. Returns `Some(message)` exactly
    /// once per failure so callers can show a single toast; subsequent calls
    /// return `None` until the next failed write.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Record a persist failure: log it and stash it for the UI to surface.
    /// `pub(crate)` so callers outside this module (e.g. `App` failing to
    /// even spawn a background load thread) can report through the same
    /// toast mechanism as an on-disk write failure, rather than that
    /// failure going silently unreported.
    pub(crate) fn note_persist_error(&mut self, e: impl std::fmt::Display) {
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
            self.dirty.insert(name.to_os_string());
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

    /// File System Access has no synchronous write — this fires the actual
    /// disk write as a background `spawn_local` task and returns
    /// immediately (always `Ok(())`, since there's no synchronous result to
    /// report). `update`'s caller already applied the change to the
    /// in-memory `images` cache regardless of persist outcome, same as
    /// native; a failure here surfaces later, asynchronously, via
    /// `persist_err_tx` → [`Catalog::poll_persist_errors`] → `last_error`,
    /// instead of being available on this call's return value.
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

    /// Same fire-and-forget shape as the wasm32 `write_sidecar` above.
    #[cfg(target_arch = "wasm32")]
    fn delete_sidecar(&self, path: &Path) -> Result<(), String> {
        let Some(name) = path.file_name() else {
            return Ok(());
        };
        // No handle means nothing was ever written for this directory
        // either — matches native's "missing sidecar is fine" NotFound arm.
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

    /// Delete a sidecar through an explicitly captured directory handle. This
    /// is used when a photo deletion completes after navigation changed the
    /// catalog's active handle.
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

    /// Remove the cached record and delete its sidecar through an explicitly
    /// captured directory handle.
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

/// Result of scanning one directory's `.lightphotos/*.xmp` sidecars —
/// [`load_sidecars`]'s return type. Free-standing (no `&Catalog` needed) so
/// it can run on a background thread; `Catalog::apply_loaded` folds it in.
pub(crate) struct SidecarLoad {
    pub images: HashMap<OsString, ImageRecord>,
    pub skipped: usize,
}

/// Scan `dir/.lightphotos/*.xmp` and parse every sidecar into a
/// [`SidecarLoad`]. Pure disk I/O, independent of any `Catalog` instance —
/// the same scan `Catalog::open_dir` used to do inline against `&mut self`,
/// extracted so it can run on a background thread (see `Catalog::switch_dir`
/// / `Catalog::apply_loaded`) as well as synchronously (`Catalog::open_dir`).
/// A missing `.lightphotos` directory is not an error — just an empty result.
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
        // file_stem() strips exactly the trailing ".xmp", correctly
        // preserving a name like "PHOTO1.ARW" which itself contains a dot.
        let Some(stem) = path.file_stem() else {
            continue;
        };
        match std::fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<ImageRecord>(&bytes) {
                Ok(rec) if !rec.is_empty() => {
                    images.insert(stem.to_os_string(), rec);
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

    SidecarLoad { images, skipped }
}

/// Human-readable "N entries could not be read" message for `last_error`.
fn skipped_message(skipped: usize) -> String {
    format!(
        "{skipped} catalog entr{} could not be read and {} skipped.",
        if skipped == 1 { "y" } else { "ies" },
        if skipped == 1 { "was" } else { "were" },
    )
}

/// The sidecar path for `path`: `<path's directory>/.lightphotos/<filename>.xmp`.
/// `None` when `path` has no parent or no filename (e.g. `/` or `..`).
/// Built via `OsString` concatenation (not a lossy `to_string_lossy` round
/// trip) so non-UTF8 filenames stay exact.
#[cfg(not(target_arch = "wasm32"))]
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

    // --- async load (switch_dir / apply_loaded / load_sidecars) ------

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

        // The stale `a` load now lands — it must be ignored, not merged in.
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
        // A write lands after switch_dir but before the background load
        // (spawned at switch time, snapshotting the pre-write disk state)
        // has returned.
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
