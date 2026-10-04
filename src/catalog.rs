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
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::develop::{Adjustments, TouchUp};
use crate::groups::{GroupId, GroupWrite, Groups, SavedGroup};

pub(crate) mod group_file;
mod writeback;
pub(crate) use writeback::LoadMark;
use writeback::{WriteOp, Writeback};

/// Hidden subfolder holding a directory's sidecars and thumbnail cache.
pub(crate) const SIDECAR_DIR: &str = ".lightphotos";
/// Sidecar file extension. Cosmetic; see the module docs.
pub(crate) const SIDECAR_EXT: &str = "xmp";
/// Highest star rating. Ratings run `0..=MAX_RATING`, with 0 for unrated.
pub(crate) const MAX_RATING: u8 = 5;

/// Persisted state for one photo. Default fields are skipped on write, so a
/// rated but unedited photo's sidecar stays small.
#[derive(Serialize, Deserialize, Default, Clone)]
pub struct ImageRecord {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
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
    #[serde(default, skip_serializing_if = "is_zero_rot")]
    pub rotation: u8,
}

/// A photo's color label, set with Shift+1..5 in Lightroom's order.
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
    /// The label Shift+`n` sets, for `n` in `1..=5`.
    /// No key sets a color label yet; kept for the label UI still to come.
    #[allow(dead_code)]
    fn from_digit(n: u8) -> Option<ColorLabel> {
        Some(match n {
            1 => ColorLabel::Red,
            2 => ColorLabel::Yellow,
            3 => ColorLabel::Green,
            4 => ColorLabel::Blue,
            5 => ColorLabel::Purple,
            _ => return None,
        })
    }
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
    pub(crate) fn is_empty(&self) -> bool {
        let empty = self.rating.is_none()
            && self.label.is_none()
            && self.adjustments.is_identity()
            && self.touchups.is_empty()
            && self.rotation == 0;
        debug_assert_eq!(
            empty,
            self.serializes_to_nothing(),
            "ImageRecord::is_empty disagrees with what the sidecar would hold; \
             a field was added to the struct but not to is_empty"
        );
        empty
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

/// Records for the active directory, backed by its sidecar files. `images`
/// is a read cache keyed by filename, so it answers only for photos in the
/// active directory. See [`Catalog::cache_key`].
pub struct Catalog {
    images: HashMap<OsString, ImageRecord>,
    /// `None` until a load lands for `dir`, so an unloaded folder never
    /// passes for one without groups.
    groups: Option<Groups>,
    /// Filenames written or removed since [`Catalog::switch_dir`]. A
    /// background load must not overwrite these. It covers removals, which
    /// look the same as "not loaded yet" in `images`.
    dirty: HashSet<OsString>,
    /// The active directory. `None` before the first folder opens.
    dir: Option<PathBuf>,
    /// The latest persist failure, drained by [`Catalog::take_error`] into a
    /// toast.
    last_error: Option<String>,
    /// Sidecar mutations on their way to disk. Every write goes through here,
    /// so no caller blocks a frame on the filesystem.
    writeback: Writeback,

    /// The active folder's File System Access handle. wasm32 has no OS paths,
    /// so sidecar I/O goes through this.
    #[cfg(target_arch = "wasm32")]
    wasm_dir_handle: Option<web_sys::FileSystemDirectoryHandle>,
}

impl Catalog {
    /// An empty catalog with no active directory.
    pub fn new() -> Catalog {
        Catalog {
            images: HashMap::new(),
            groups: None,
            dirty: HashSet::new(),
            dir: None,
            last_error: None,
            writeback: Writeback::new(),
            #[cfg(target_arch = "wasm32")]
            wasm_dir_handle: None,
        }
    }

    /// Retire finished sidecar writes and surface their failures. On wasm this
    /// also starts the next queued writes, so it is the scheduler there. Call
    /// once per frame: it is O(completions) and a no-op with nothing queued.
    pub(crate) fn pump(&mut self) {
        for e in self.writeback.pump() {
            self.note_persist_error(e);
        }
    }

    /// Photos whose sidecar has not caught up with memory yet.
    pub(crate) fn backlog(&self) -> usize {
        self.writeback.backlog()
    }

    /// Block until every queued sidecar write lands, or `timeout` elapses.
    /// The app calls this on exit; `Drop` covers every other path.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn flush_blocking(&mut self, timeout: std::time::Duration) {
        for e in self.writeback.flush_blocking(timeout) {
            self.note_persist_error(e);
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
    #[hotpath::measure]
    pub fn open_dir(&mut self, dir: &Path) {
        let mark = self.switch_dir(dir);
        let loaded = load_sidecars(dir);
        let photos = crate::navigation::Playlist::from_dir(dir);
        self.apply_loaded(dir, mark, loaded, photos.entries());
    }

    /// Make `dir` active and clear the cache without disk I/O, so no lookup
    /// sees the previous directory's records.
    ///
    /// Every load starts here, so this is also where a load takes its place in
    /// the write order. The returned mark must reach [`Catalog::apply_loaded`],
    /// or [`Catalog::abandon_load`] if the load never runs.
    #[must_use]
    pub(crate) fn switch_dir(&mut self, dir: &Path) -> LoadMark {
        self.dir = Some(dir.to_path_buf());
        self.images.clear();
        self.groups = None;
        self.dirty.clear();
        self.writeback.begin_load()
    }

    /// Give back a mark whose load was never started, so the write history it
    /// was holding open can be dropped.
    pub(crate) fn abandon_load(&mut self) {
        self.writeback.end_load();
    }

    /// Whether `dir` is the directory represented by the in-memory cache.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn is_active_dir(&self, dir: &Path) -> bool {
        self.dir.as_deref() == Some(dir)
    }

    /// Merge a background load into the cache if `dir` is still active.
    ///
    /// Two things can make the load's snapshot older than what we know. Keys in
    /// `dirty` were edited during this visit, so they are skipped. Writes from
    /// an earlier visit are not in `dirty`, because `switch_dir` clears it, and
    /// may still have been in flight when this load was requested; `mark` is
    /// what identifies those, and the write-back queue restores them.
    /// Unreadable sidecars are reported even when the load is stale.
    ///
    /// `photos` are the folder's images, the playlist's entries. A group
    /// member that is not one of them is dropped.
    ///
    /// The load retires after the overlay, because retiring the last load
    /// drops the history of writes that landed, and groups have no `dirty`
    /// set to fall back on.
    pub(crate) fn apply_loaded(
        &mut self,
        dir: &Path,
        mark: LoadMark,
        loaded: SidecarLoad,
        photos: &[PathBuf],
    ) {
        if loaded.skipped > 0 {
            self.last_error = Some(skipped_message(loaded.skipped));
        }
        if self.dir.as_deref() == Some(dir) {
            for (name, rec) in loaded.images {
                if !self.dirty.contains(&name) {
                    self.images.insert(name, rec);
                }
            }
            let names: HashSet<&OsStr> = if loaded.groups.is_empty() {
                HashSet::new()
            } else {
                photos.iter().filter_map(|p| p.file_name()).collect()
            };
            let mut groups = Groups::from_loaded(loaded.groups, |n| names.contains(n));
            self.writeback
                .overlay(dir, mark, &mut self.images, &mut groups);
            self.groups = Some(groups);
        }
        self.writeback.end_load();
    }

    pub(crate) fn groups(&self) -> Option<&Groups> {
        self.groups.as_ref()
    }

    pub(crate) fn apply_group_writes(
        &mut self,
        writes: Vec<GroupWrite>,
    ) -> Result<(), GroupWriteRefused> {
        if writes.is_empty() {
            return Ok(());
        }
        let dir = self.dir.clone().ok_or(GroupWriteRefused::NoFolder)?;
        let groups = self.groups.as_mut().ok_or(GroupWriteRefused::LoadPending)?;
        for write in &writes {
            groups.apply(write);
        }
        let repairs = groups.take_repairs();
        for write in writes.into_iter().chain(repairs) {
            let (id, op) = match write {
                GroupWrite::Put(id, group) => (id, WriteOp::PutGroup(group)),
                GroupWrite::Delete(id) => (id, WriteOp::DeleteGroup),
            };
            self.enqueue(&group_file::path(&dir, &id), op);
        }
        Ok(())
    }

    /// Take the pending persist error's cause. Each failure is returned once.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// Log a persist failure and keep it for the UI to show.
    pub(crate) fn note_persist_error(&mut self, e: impl std::fmt::Display) {
        eprintln!("[catalog] Failed to save catalog entry: {e}");
        self.last_error = Some(e.to_string());
    }

    /// The cache key for `path`: its filename, when `path` is in the active
    /// directory. Camera filenames repeat across folders, so a photo elsewhere
    /// must never read or overwrite the active directory's same-named record.
    fn cache_key<'p>(&self, path: &'p Path) -> Option<&'p OsStr> {
        match self.dir.as_deref() {
            Some(dir) if path.parent() == Some(dir) => path.file_name(),
            _ => None,
        }
    }

    fn record(&self, path: &Path) -> Option<&ImageRecord> {
        self.cache_key(path).and_then(|n| self.images.get(n))
    }

    pub fn get(&self, path: &Path) -> Option<u8> {
        self.record(path).and_then(|r| r.rating)
    }

    /// Set the rating, clamped to `0..=MAX_RATING`. `0` clears it.
    pub fn set(&mut self, path: &Path, stars: u8) {
        let stars = stars.min(MAX_RATING);
        let rating = if stars == 0 { None } else { Some(stars) };
        self.update(path, |rec| rec.rating = rating);
    }

    pub fn label(&self, path: &Path) -> Option<ColorLabel> {
        self.record(path).and_then(|r| r.label)
    }

    pub fn set_label(&mut self, path: &Path, label: Option<ColorLabel>) {
        self.update(path, |rec| rec.label = label);
    }

    pub fn adjustments(&self, path: &Path) -> Adjustments {
        self.record(path).map(|r| r.adjustments).unwrap_or_default()
    }

    pub fn set_adjustments(&mut self, path: &Path, adj: &Adjustments) {
        let adj = *adj;
        self.update(path, |rec| rec.adjustments = adj);
    }

    pub fn touchups(&self, path: &Path) -> Vec<TouchUp> {
        self.record(path)
            .map(|r| r.touchups.clone())
            .unwrap_or_default()
    }

    pub fn set_touchups(&mut self, path: &Path, touchups: &[TouchUp]) {
        let touchups = touchups.to_vec();
        self.update(path, |rec| rec.touchups = touchups);
    }

    pub fn rotation(&self, path: &Path) -> u8 {
        self.record(path).map(|r| r.rotation).unwrap_or(0)
    }

    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let rotation = rotation % 4;
        self.update(path, |rec| rec.rotation = rotation);
    }

    /// The one entry point for photos that left the folder, such as trashed
    /// ones. Each loses its record and its sidecar, which skips the Trash
    /// since it is useless apart from its photo, and leaves its group. A
    /// group that loses its representative promotes its first survivor, and
    /// one left with a single photo is deleted with its file.
    ///
    /// While the folder's groups are still loading the group part is skipped.
    /// The load that lands next drops the missing photos and records the
    /// repair, so no group is left naming them.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn forget_photos(&mut self, paths: &[PathBuf]) {
        for path in paths {
            if let Some(name) = self.cache_key(path) {
                self.images.remove(name);
                self.dirty.insert(name.to_os_string());
            }
            self.enqueue(path, WriteOp::Delete);
        }
        self.forget_group_members(paths);
    }

    /// The group half of [`Catalog::forget_photos`]. wasm forgets each
    /// photo's record through a captured handle instead, so it calls only
    /// this. A photo outside the open folder is skipped, since its folder's
    /// groups are not in memory; that folder's next load repairs them.
    pub fn forget_group_members(&mut self, paths: &[PathBuf]) {
        let names: Vec<OsString> = paths
            .iter()
            .filter_map(|p| self.cache_key(p))
            .map(OsStr::to_os_string)
            .collect();
        let Some(groups) = &self.groups else {
            return;
        };
        let writes = groups.forget(&names);
        if let Err(e) = self.apply_group_writes(writes) {
            self.note_persist_error(e);
        }
    }

    /// Apply `mutate` to the record for `path`, then queue its sidecar write,
    /// or its deletion if the record became empty.
    ///
    /// Refused for a photo outside the active directory. The cache holds no
    /// record for it to start from, so writing would replace its sidecar with
    /// only this one field.
    fn update(&mut self, path: &Path, mutate: impl FnOnce(&mut ImageRecord)) {
        let Some(name) = self.cache_key(path).map(OsStr::to_os_string) else {
            self.note_persist_error(format!("{} is not in the open folder", path.display()));
            return;
        };
        self.dirty.insert(name.clone());
        let mut rec = self.images.remove(&name).unwrap_or_default();
        mutate(&mut rec);
        if rec.is_empty() {
            self.enqueue(path, WriteOp::Delete);
        } else {
            self.enqueue(path, WriteOp::Put(rec.clone()));
            self.images.insert(name, rec);
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn enqueue(&mut self, path: &Path, op: WriteOp) {
        self.writeback.enqueue(path, op);
    }

    /// wasm has no OS paths, so the queue entry carries the folder handle the
    /// write must land in. Taking it here, at the moment of the edit, is what
    /// keeps a write that outlives a folder switch pointed at the right folder.
    #[cfg(target_arch = "wasm32")]
    fn enqueue(&mut self, path: &Path, op: WriteOp) {
        match self.wasm_dir_handle.clone() {
            Some(dir_handle) => self.writeback.enqueue(path, op, dir_handle),
            // Without a handle nothing was ever written, so a deletion has
            // nothing to undo, but an edit the user made is being lost.
            None => {
                if matches!(op, WriteOp::Put(_) | WriteOp::PutGroup(_)) {
                    self.note_persist_error("no folder handle for this photo's directory");
                }
            }
        }
    }

    /// Delete a sidecar through a captured handle, for a photo deletion that
    /// finishes after the user navigated to another folder. The cache is left
    /// alone, because it now holds the new folder's records.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn delete_sidecar_with_handle(
        &mut self,
        path: &Path,
        dir_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        self.writeback
            .enqueue(path, WriteOp::Delete, dir_handle.clone());
    }

    /// [`Catalog::remove`] through a captured directory handle, for a photo
    /// deletion that finishes after the user navigated to another folder.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn remove_with_handle(
        &mut self,
        path: &Path,
        dir_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        if let Some(name) = self.cache_key(path) {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GroupWriteRefused {
    NoFolder,
    LoadPending,
}

impl std::fmt::Display for GroupWriteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GroupWriteRefused::NoFolder => "no folder is open for these groups",
            GroupWriteRefused::LoadPending => "the folder's groups are still loading",
        })
    }
}

/// The sidecars read from one directory, plus a count of unreadable ones.
pub(crate) struct SidecarLoad {
    pub images: HashMap<OsString, ImageRecord>,
    pub groups: Vec<(GroupId, SavedGroup)>,
    pub skipped: usize,
}

/// Read every sidecar in `dir/.lightphotos/` and its `groups/`. Needs no
/// `Catalog`, so it can run on a background thread. A missing folder gives
/// an empty result.
#[cfg(not(target_arch = "wasm32"))]
#[hotpath::measure]
pub(crate) fn load_sidecars(dir: &Path) -> SidecarLoad {
    let mut images = HashMap::new();
    let mut skipped = 0usize;

    let entries = match std::fs::read_dir(dir.join(SIDECAR_DIR)) {
        Ok(rd) => rd,
        Err(_) => {
            return SidecarLoad {
                images,
                groups: Vec::new(),
                skipped,
            }
        }
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

    let groups = group_file::load(dir, &mut skipped);
    SidecarLoad {
        images,
        groups,
        skipped,
    }
}

fn skipped_message(skipped: usize) -> String {
    format!(
        "{skipped} catalog entr{} could not be read and {} skipped.",
        if skipped == 1 { "y" } else { "ies" },
        if skipped == 1 { "was" } else { "were" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::groups::{Group, GroupId};
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

    /// Sidecar writes are queued, so a test that reads the disk waits here
    /// first. A timeout rather than an unbounded wait, so a stuck writer
    /// fails the assertion instead of hanging the suite.
    fn flush(cat: &mut Catalog) {
        cat.flush_blocking(std::time::Duration::from_secs(10));
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

        flush(&mut cat);
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
        flush(&mut reloaded);
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
        flush(&mut cat);
        let msg = cat
            .take_error()
            .expect("failed save should report an error");
        assert!(
            msg.contains("os error"),
            "error should carry the OS cause, got: {msg}"
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
        flush(&mut cat);
        assert_eq!(cat.get(&dir.join("good.jpg")), Some(4));

        // Bad sidecar is untouched by the unrelated write, and only replaced
        // once the user makes an edit for that exact photo.
        assert_eq!(
            std::fs::read(sidecar_for(&dir, "bad.jpg")).unwrap(),
            b"{not valid json"
        );
        cat.set(&dir.join("bad.jpg"), 5);
        assert_eq!(cat.get(&dir.join("bad.jpg")), Some(5));

        flush(&mut cat);
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
        flush(&mut cat);
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
        flush(&mut cat);
        assert!(
            dir.join(SIDECAR_DIR).exists(),
            ".lightphotos should appear after the first write"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The defect this guards is a new `ImageRecord` field that `is_empty`
    /// forgets, which makes a cleared photo keep writing a sidecar instead of
    /// deleting it. Every field is set through the serialized form, so the
    /// check is the serde attributes rather than a second hand-written list.
    #[test]
    fn every_persisted_field_on_its_own_makes_a_record_non_empty() {
        let mut populated = ImageRecord::default();
        populated.rating = Some(3);
        populated.label = Some(ColorLabel::Red);
        populated.adjustments.exposure = 0.5;
        populated.touchups.push(TouchUp {
            center: [0.4, 0.5],
            radius: 0.02,
            source: [0.6, 0.5],
            feather: 0.5,
            delta: [0.01, -0.02, 0.0],
        });
        populated.rotation = 1;

        let serde_json::Value::Object(fields) = serde_json::to_value(&populated).unwrap() else {
            panic!("a record serializes to an object");
        };
        assert_eq!(
            fields.len(),
            5,
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
    fn empty_record_deletes_the_sidecar_file() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 3);
        flush(&mut cat);
        assert!(sidecar_for(&dir, "photo.jpg").exists());
        cat.set(&p, 0);
        flush(&mut cat);
        assert!(
            !sidecar_for(&dir, "photo.jpg").exists(),
            "clearing the rating on an otherwise-identity record should delete the sidecar"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn forgetting_a_photo_deletes_its_sidecar_file() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");
        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 4);
        flush(&mut cat);
        assert!(sidecar_for(&dir, "photo.jpg").exists());
        cat.forget_photos(std::slice::from_ref(&p));
        flush(&mut cat);
        assert_eq!(cat.get(&p), None);
        assert!(!sidecar_for(&dir, "photo.jpg").exists());
        // Removing again is a no-op.
        cat.forget_photos(std::slice::from_ref(&p));

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

        flush(&mut cat);
        cat.open_dir(&b);
        assert_eq!(
            cat.get(&pb),
            None,
            "directory b's same-named photo must not see directory a's rating"
        );
        cat.set(&pb, 2);
        assert_eq!(cat.get(&pb), Some(2));

        flush(&mut cat);
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
    fn a_photo_outside_the_active_directory_never_sees_its_records() {
        let a = unique_tmp_dir();
        let b = unique_tmp_dir();
        let pa = a.join("IMG_0001.JPG");
        let pb = b.join("IMG_0001.JPG");

        let mut cat = Catalog::with_dir(a.clone());
        cat.set(&pa, 4);
        cat.set_rotation(&pa, 1);
        cat.set_touchups(
            &pa,
            &[TouchUp {
                center: [0.5, 0.5],
                radius: 0.1,
                source: [0.2, 0.2],
                feather: 0.5,
                delta: [0.0; 3],
            }],
        );
        let adj = Adjustments {
            exposure: 1.0,
            ..Default::default()
        };
        cat.set_adjustments(&pa, &adj);

        assert_eq!(cat.get(&pb), None);
        assert_eq!(cat.label(&pb), None);
        assert_eq!(cat.rotation(&pb), 0);
        assert!(cat.touchups(&pb).is_empty());
        assert_eq!(cat.adjustments(&pb), Adjustments::default());

        cat.set(&pb, 1);
        assert!(
            cat.take_error().is_some(),
            "an off-folder write is reported"
        );
        cat.forget_photos(std::slice::from_ref(&pb));
        assert_eq!(cat.get(&pa), Some(4), "a's record survives b's writes");
        assert_eq!(cat.adjustments(&pa), adj);

        flush(&mut cat);
        assert!(!sidecar_for(&b, "IMG_0001.JPG").exists());
        assert_eq!(Catalog::with_dir(a.clone()).get(&pa), Some(4));

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
        let mark = cat.switch_dir(&b);
        assert_eq!(cat.get(&pa), None, "switching clears the cache immediately");

        // The stale `a` load lands and must be ignored.
        let stale = load_sidecars(&a);
        cat.apply_loaded(&a, mark, stale, &images_in(&a));
        assert_eq!(
            cat.get(&pa),
            None,
            "a load for a directory that's no longer active must be discarded"
        );

        flush(&mut cat);
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
        let mark = cat.switch_dir(&dir);
        // A write lands after switch_dir but before the background load,
        // whose snapshot predates the write, returns.
        cat.set(&written, 5);
        let loaded = load_sidecars(&dir); // snapshot predates `written`'s sidecar...
        cat.apply_loaded(&dir, mark, loaded, &images_in(&dir));

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

        flush(&mut cat);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_loaded_does_not_resurrect_a_locally_removed_record() {
        let dir = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        // A rating exists on disk from a previous session.
        Catalog::with_dir(dir.clone()).set(&p, 4);

        let mut cat = Catalog::new();
        let mark = cat.switch_dir(&dir);
        let loaded = load_sidecars(&dir); // snapshot still carries the rating
        cat.forget_photos(std::slice::from_ref(&p));
        cat.apply_loaded(&dir, mark, loaded, &images_in(&dir));

        assert_eq!(
            cat.get(&p),
            None,
            "a local removal made while a load was in flight must not be \
             resurrected by a stale snapshot taken before it"
        );

        flush(&mut cat);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_loaded_still_surfaces_skipped_count_for_a_stale_directory() {
        let a = unique_tmp_dir();
        let b = unique_tmp_dir();
        std::fs::create_dir_all(a.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&a, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = Catalog::new();
        let mark = cat.switch_dir(&a);
        let loaded = load_sidecars(&a); // has skipped == 1
        let _ = cat.switch_dir(&b); // the user already left `a` before the load lands
        cat.apply_loaded(&a, mark, loaded, &images_in(&a));

        assert!(
            cat.take_error().is_some(),
            "a corrupt sidecar found by a stale (directory-since-changed) load \
             must still be surfaced, not silently dropped"
        );

        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    /// Every sidecar write used to happen inline, so a bulk rate paid the
    /// filesystem once per photo and froze the window for seconds.
    #[test]
    fn rating_a_whole_folder_does_not_block_on_the_filesystem() {
        let dir = unique_tmp_dir();
        let paths: Vec<PathBuf> = (0..20_000)
            .map(|i| dir.join(format!("photo{i:05}.jpg")))
            .collect();

        let mut cat = Catalog::with_dir(dir.clone());
        let started = std::time::Instant::now();
        for p in &paths {
            cat.set(p, 3);
        }
        let elapsed = started.elapsed();

        assert!(
            cat.backlog() > 0,
            "the writes must still be outstanding, not already paid for inline"
        );
        // 20000 inline temp-then-rename sidecar writes measure about 2s on an
        // idle APFS volume, so this bound still catches the regression with
        // room to spare on a machine that is busy doing something else.
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "rating 20000 photos took {elapsed:?}; it must not wait on 20000 sidecar writes"
        );

        flush(&mut cat);
        assert_eq!(cat.backlog(), 0);
        assert_eq!(
            Catalog::with_dir(dir.clone()).get(&paths[19_999]),
            Some(3),
            "every queued write must still reach the disk"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_round_trip_during_a_flush_does_not_revert_the_edit() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        // A previous session left this photo at three stars.
        Catalog::with_dir(dir.clone()).set(&p, 3);

        let mut cat = Catalog::with_dir(dir.clone());
        // The snapshot a background load would carry: taken before the edit.
        let stale = load_sidecars(&dir);
        cat.set(&p, 5);
        // The user leaves and comes back while that write is still queued,
        // which clears both the cache and `dirty`.
        let _ = cat.switch_dir(&elsewhere);
        let mark = cat.switch_dir(&dir);
        cat.apply_loaded(&dir, mark, stale, &images_in(&dir));

        assert_eq!(
            cat.get(&p),
            Some(5),
            "a load that predates a still-queued write must not put the old \
             rating back into the cache"
        );

        flush(&mut cat);
        assert_eq!(
            Catalog::with_dir(dir.clone()).get(&p),
            Some(5),
            "the queued write must still land in its own folder after the round trip"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    /// The same round trip, but the queued write lands before the stale load
    /// is applied. Nothing the write left behind at apply time can correct the
    /// load, so the mark has to: the write was still outstanding when the load
    /// was requested, so the load's snapshot may be older than it.
    ///
    /// This is the ordinary case for a large batch rather than a narrow race.
    /// The frame loop drains completions before it applies loads, and a 20 000
    /// photo queue empties in about 2.5 s while a directory scan of 20 000
    /// sidecars is still running.
    #[test]
    fn a_completed_write_is_not_reverted_by_a_load_that_predates_it() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        Catalog::with_dir(dir.clone()).set(&p, 3);

        let mut cat = Catalog::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        cat.set(&p, 5);
        let _ = cat.switch_dir(&elsewhere);
        let mark = cat.switch_dir(&dir);
        flush(&mut cat);
        cat.apply_loaded(&dir, mark, stale, &images_in(&dir));

        assert_eq!(
            cat.get(&p),
            Some(5),
            "a load that predates a completed write must not put the old rating back"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    /// The same, with the intervening folder's load abandoned, so this load is
    /// the last one outstanding and retiring it would drop the landed write's
    /// history before the overlay could use it.
    #[test]
    fn a_completed_write_survives_the_last_outstanding_load() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        Catalog::with_dir(dir.clone()).set(&p, 3);

        let mut cat = Catalog::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        cat.set(&p, 5);
        let _ = cat.switch_dir(&elsewhere);
        cat.abandon_load();
        let mark = cat.switch_dir(&dir);
        flush(&mut cat);
        cat.apply_loaded(&dir, mark, stale, &images_in(&dir));

        assert_eq!(cat.get(&p), Some(5));

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    /// The mark must not shield a record forever: a sidecar changed outside the
    /// app between visits has to win on the revisit, which is what
    /// `reloading_a_folder_drops_values_its_sidecars_no_longer_have` relies on.
    #[test]
    fn a_load_requested_after_a_write_landed_still_wins() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        let p = dir.join("photo.jpg");

        let mut cat = Catalog::with_dir(dir.clone());
        cat.set(&p, 5);
        flush(&mut cat);

        // Another tool rewrites the sidecar while the user is elsewhere.
        Catalog::with_dir(dir.clone()).set(&p, 1);

        let _ = cat.switch_dir(&elsewhere);
        let mark = cat.switch_dir(&dir);
        let loaded = load_sidecars(&dir);
        cat.apply_loaded(&dir, mark, loaded, &images_in(&dir));

        assert_eq!(
            cat.get(&p),
            Some(1),
            "a snapshot taken after our write landed is the newer truth"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
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

    fn images_in(dir: &Path) -> Vec<PathBuf> {
        crate::navigation::Playlist::from_dir(dir)
            .entries()
            .to_vec()
    }

    fn names(ns: &[&str]) -> Vec<OsString> {
        ns.iter().map(OsString::from).collect()
    }

    fn photos(dir: &Path, names: &[&str]) -> Vec<OsString> {
        for n in names {
            std::fs::write(dir.join(n), b"jpeg").unwrap();
        }
        names.iter().map(OsString::from).collect()
    }

    fn groups_dir(dir: &Path) -> PathBuf {
        dir.join(SIDECAR_DIR).join(group_file::GROUPS_DIR)
    }

    fn write_group_file(dir: &Path, stem: &str, body: &str) -> PathBuf {
        std::fs::create_dir_all(groups_dir(dir)).unwrap();
        let path = groups_dir(dir).join(format!("{stem}.json"));
        std::fs::write(&path, body).unwrap();
        path
    }

    fn only_group(cat: &Catalog) -> (GroupId, Group) {
        let all: Vec<_> = cat.groups().unwrap().iter().collect();
        assert_eq!(all.len(), 1, "expected one group, got {all:?}");
        (all[0].0.clone(), all[0].1.clone())
    }

    #[test]
    fn a_group_round_trips_through_writeback_and_reload() {
        let dir = unique_tmp_dir();
        let members = photos(&dir, &["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG"]);
        let group = Group::new(members, "IMG_0002.JPG".into()).unwrap();

        let mut cat = Catalog::with_dir(dir.clone());
        let writes = cat
            .groups()
            .unwrap()
            .create(group.clone(), std::time::SystemTime::now());
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);
        assert_eq!(cat.take_error(), None);
        let (id, _) = only_group(&cat);

        let file = group_file::path(&dir, &id);
        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({
                "v": 1,
                "members": ["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG"],
                "representative": "IMG_0002.JPG",
            })
        );
        assert_eq!(
            only_group(&Catalog::with_dir(dir.clone())),
            (id.clone(), group)
        );

        let writes = cat.groups().unwrap().dissolve(&id);
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);
        assert!(!file.exists(), "dissolving deletes the sidecar");
        assert!(Catalog::with_dir(dir.clone()).groups().unwrap().is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_load_without_groups_creates_no_directory() {
        let dir = unique_tmp_dir();
        photos(&dir, &["a.jpg", "b.jpg"]);
        let cat = Catalog::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        assert!(
            !dir.join(SIDECAR_DIR).exists(),
            "loading a pristine folder must not create .lightphotos"
        );

        Catalog::with_dir(dir.clone()).set(&dir.join("a.jpg"), 3);
        let cat = Catalog::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        assert!(
            !groups_dir(&dir).exists(),
            "loading a folder with photo sidecars must not create groups/"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn loading_repairs_groups_in_memory_and_leaves_their_files_alone() {
        let dir = unique_tmp_dir();
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
        let missing =
            r#"{"v":1,"members":["gone.jpg","a.jpg","b.jpg"],"representative":"gone.jpg"}"#;
        let missing_file = write_group_file(&dir, "g-2", missing);
        let rival = r#"{"v":1,"members":["b.jpg","c.jpg"],"representative":"b.jpg"}"#;
        let rival_file = write_group_file(&dir, "g-1", rival);

        let mut cat = Catalog::with_dir(dir.clone());
        assert_eq!(cat.take_error(), None);
        let (id, group) = only_group(&cat);
        assert_eq!(id.to_string(), "g-2");
        assert_eq!(group.members(), ["a.jpg", "b.jpg"]);
        assert_eq!(
            group.rep(),
            "a.jpg",
            "a missing representative moves to the first member"
        );
        flush(&mut cat);
        assert_eq!(std::fs::read_to_string(&missing_file).unwrap(), missing);
        assert_eq!(std::fs::read_to_string(&rival_file).unwrap(), rival);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_group_is_counted_and_the_rest_still_load() {
        let dir = unique_tmp_dir();
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg", "d.jpg", "notes.txt"]);
        write_group_file(&dir, "g-bad", "{not json");
        write_group_file(
            &dir,
            "My Group",
            r#"{"v":1,"members":["a.jpg","b.jpg"],"representative":"a.jpg"}"#,
        );
        write_group_file(
            &dir,
            "g-txt",
            r#"{"v":1,"members":["d.jpg","notes.txt"],"representative":"d.jpg"}"#,
        );
        write_group_file(
            &dir,
            "g-one",
            r#"{"v":1,"members":["c.jpg"],"representative":"c.jpg"}"#,
        );
        write_group_file(
            &dir,
            "g-v2",
            r#"{"v":2,"members":["a.jpg","c.jpg"],"representative":"a.jpg"}"#,
        );
        write_group_file(
            &dir,
            "g-ok",
            r#"{"v":1,"members":["a.jpg","b.jpg"],"representative":"b.jpg"}"#,
        );

        let loaded = load_sidecars(&dir);
        assert_eq!(
            loaded.skipped, 3,
            "bad JSON, an unknown version, and a name that is not a group id"
        );

        let mut cat = Catalog::with_dir(dir.clone());
        assert!(cat.take_error().is_some());
        let (id, _) = only_group(&cat);
        assert_eq!(
            id.to_string(),
            "g-ok",
            "a one-member file and a group kept up only by a file that is not a photo are dropped"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn group_ids_on_disk(dir: &Path) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(groups_dir(dir))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        ids.sort();
        ids
    }

    #[test]
    fn ungrouping_the_winner_does_not_bring_the_loser_back() {
        let dir = unique_tmp_dir();
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
        write_group_file(
            &dir,
            "g-000000000001-000000",
            r#"{"v":1,"members":["a.jpg","b.jpg","c.jpg"],"representative":"a.jpg"}"#,
        );
        write_group_file(
            &dir,
            "g-000000000002-000000",
            r#"{"v":1,"members":["b.jpg","c.jpg"],"representative":"b.jpg"}"#,
        );

        let mut cat = Catalog::with_dir(dir.clone());
        let (winner, _) = only_group(&cat);
        assert_eq!(winner.to_string(), "g-000000000002-000000");
        let writes = cat.groups().unwrap().dissolve(&winner);
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);

        assert!(Catalog::with_dir(dir.clone()).groups().unwrap().is_empty());
        assert_eq!(group_ids_on_disk(&dir), Vec::<String>::new());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_group_dropped_for_a_missing_photo_stays_dropped_when_it_returns() {
        let dir = unique_tmp_dir();
        photos(&dir, &["a.jpg", "c.jpg", "d.jpg"]);
        let dropped = write_group_file(
            &dir,
            "g-1",
            r#"{"v":1,"members":["a.jpg","b.jpg"],"representative":"a.jpg"}"#,
        );

        let mut cat = Catalog::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        flush(&mut cat);
        assert!(dropped.exists(), "loading never writes");

        let unrelated = Group::new(names(&["c.jpg", "d.jpg"]), "c.jpg".into()).unwrap();
        let writes = cat
            .groups()
            .unwrap()
            .create(unrelated, std::time::SystemTime::now());
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);
        assert!(
            !dropped.exists(),
            "the first mutation deletes the dropped file"
        );

        photos(&dir, &["b.jpg"]);
        let cat = Catalog::with_dir(dir.clone());
        assert_eq!(
            only_group(&cat).1.members(),
            ["c.jpg", "d.jpg"],
            "g-1 must not come back with b.jpg"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_load_that_predates_a_group_write_keeps_the_group() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        let members = photos(&dir, &["a.jpg", "b.jpg"]);

        let mut cat = Catalog::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        let group = Group::new(members, "b.jpg".into()).unwrap();
        let writes = cat
            .groups()
            .unwrap()
            .create(group.clone(), std::time::SystemTime::now());
        cat.apply_group_writes(writes).unwrap();
        let _ = cat.switch_dir(&elsewhere);
        cat.abandon_load();
        let mark = cat.switch_dir(&dir);
        flush(&mut cat);
        cat.apply_loaded(&dir, mark, stale, &images_in(&dir));

        assert_eq!(only_group(&cat).1, group);

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }

    #[test]
    fn a_group_write_during_a_pending_load_is_refused_and_changes_nothing() {
        let dir = unique_tmp_dir();
        let members = photos(&dir, &["a.jpg", "b.jpg"]);
        let group = Group::new(members, "a.jpg".into()).unwrap();
        let writes = Groups::default().create(group, std::time::SystemTime::now());

        let mut cat = Catalog::new();
        let mark = cat.switch_dir(&dir);
        assert_eq!(
            cat.apply_group_writes(writes.clone()),
            Err(GroupWriteRefused::LoadPending)
        );
        assert!(cat.groups().is_none());
        assert_eq!(cat.backlog(), 0, "nothing was queued");

        let loaded = load_sidecars(&dir);
        cat.apply_loaded(&dir, mark, loaded, &images_in(&dir));
        assert!(cat.groups().unwrap().is_empty());
        assert!(!groups_dir(&dir).exists());

        let _ = cat.switch_dir(&dir);
        cat.abandon_load();
        assert_eq!(
            cat.apply_group_writes(writes),
            Err(GroupWriteRefused::LoadPending),
            "an abandoned load leaves the groups unknown"
        );
        assert_eq!(cat.backlog(), 0);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_load_replays_group_writes_in_the_order_they_were_issued() {
        let dir = unique_tmp_dir();
        let elsewhere = unique_tmp_dir();
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
        let (first, second) = (
            GroupId::from_stem("g-1".as_ref()).unwrap(),
            GroupId::from_stem("g-2".as_ref()).unwrap(),
        );
        let second_group = Group::new(names(&["b.jpg", "c.jpg"]), "c.jpg".into()).unwrap();

        let mut cat = Catalog::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        cat.enqueue(
            &group_file::path(&dir, &second),
            WriteOp::PutGroup(Group::new(names(&["a.jpg", "b.jpg"]), "a.jpg".into()).unwrap()),
        );
        cat.enqueue(
            &group_file::path(&dir, &first),
            WriteOp::PutGroup(second_group.clone()),
        );
        let _ = cat.switch_dir(&elsewhere);
        cat.abandon_load();
        let mark = cat.switch_dir(&dir);
        flush(&mut cat);
        cat.apply_loaded(&dir, mark, stale, &images_in(&dir));

        assert_eq!(
            only_group(&cat),
            (first, second_group),
            "the later write takes b.jpg from the earlier one"
        );

        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&elsewhere).unwrap();
    }
}
