// SPDX-License-Identifier: MIT OR Apache-2.0

//! The open folder's sidecars, cached. The record format, its path and its
//! single-file read and write live in [`crate::sidecar`].
//!
//! [`FolderStore`] caches one directory at a time. Reads and writes derive the
//! sidecar path from the photo's own path.

use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::develop::{Adjustments, Mask, TouchUp};
use crate::persist::catalog::{DirMarks, PhotoMarks};
use crate::persist::groups::{GroupId, GroupWrite, Groups, SavedGroup};
use crate::scoring::quality::QualityScore;
use crate::sidecar::{ColorLabel, Flag, ImageRecord, StoredScore, MAX_RATING};
#[cfg(not(target_arch = "wasm32"))]
use crate::sidecar::{SIDECAR_DIR, SIDECAR_EXT};

pub(crate) mod group_file;
mod writeback;
pub(crate) use writeback::LoadMark;
use writeback::{WriteOp, Writeback};

/// Records for the active directory, backed by its sidecar files. `images`
/// is a read cache keyed by filename, so it answers only for photos in the
/// active directory. See [`FolderStore::cache_key`].
pub struct FolderStore {
    images: HashMap<OsString, ImageRecord>,
    /// `None` until a load lands for `dir`, so an unloaded folder never
    /// passes for one without groups.
    groups: Option<Groups>,
    /// Filenames written or removed since [`FolderStore::switch_dir`]. A
    /// background load must not overwrite these. It covers removals, which
    /// look the same as "not loaded yet" in `images`.
    dirty: HashSet<OsString>,
    /// The active directory. `None` before the first folder opens.
    dir: Option<PathBuf>,
    /// The latest persist failure, drained by [`FolderStore::take_error`] into a
    /// toast.
    last_error: Option<String>,
    /// See [`FolderStore::sort_revision`].
    sort_revision: u64,
    /// See [`FolderStore::marks_revision`].
    marks_revision: u64,
    /// Sidecar mutations on their way to disk. Every write goes through here,
    /// so no caller blocks a frame on the filesystem.
    writeback: Writeback,

    /// The active folder's File System Access handle. wasm32 has no OS paths,
    /// so sidecar I/O goes through this.
    #[cfg(target_arch = "wasm32")]
    wasm_dir_handle: Option<web_sys::FileSystemDirectoryHandle>,
}

impl FolderStore {
    /// An empty folder store with no active directory.
    pub fn new() -> FolderStore {
        FolderStore {
            images: HashMap::new(),
            groups: None,
            dirty: HashSet::new(),
            dir: None,
            last_error: None,
            sort_revision: 0,
            marks_revision: 0,
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
    #[cfg(test)]
    #[cfg(not(target_arch = "wasm32"))]
    pub fn with_dir(dir: PathBuf) -> FolderStore {
        let mut cat = FolderStore::new();
        cat.open_dir(&dir);
        cat
    }

    /// Make `dir` active and reload its sidecars from disk, blocking. The app
    /// instead calls [`FolderStore::switch_dir`], runs `load_sidecars` on a
    /// background thread, then calls [`FolderStore::apply_loaded`]. Native only,
    /// because File System Access has no synchronous reads.
    #[cfg(test)]
    #[cfg(not(target_arch = "wasm32"))]
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
    /// the write order. The returned mark must reach [`FolderStore::apply_loaded`],
    /// or [`FolderStore::abandon_load`] if the load never runs.
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
            self.sort_revision += 1;
            self.marks_revision += 1;
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
        eprintln!("[folder store] Failed to save folder store entry: {e}");
        self.last_error = Some(e.to_string());
    }

    /// Counts changes to the stars and edit times, the folder store's inputs to the
    /// Grid's order, so the Grid knows when to sort again.
    pub(crate) fn sort_revision(&self) -> u64 {
        self.sort_revision
    }

    /// Counts changes to any photo's rating, flag or label, and each load
    /// of the active directory, so the catalog knows when its copy of the
    /// marks is behind. Edits to adjustments don't move it: a slider drag
    /// updates every frame.
    pub(crate) fn marks_revision(&self) -> u64 {
        self.marks_revision
    }

    /// The active directory and its marked photos, as the catalog's copy
    /// should read. `None` until the directory's load has landed, so a mark
    /// made while loading never projects a partial directory.
    pub(crate) fn dir_marks(&self) -> Option<(&Path, DirMarks)> {
        let dir = self.dir.as_deref()?;
        self.groups.as_ref()?;
        let marks = self
            .images
            .iter()
            .filter_map(|(name, rec)| {
                let marks = PhotoMarks::of(rec);
                if marks.is_empty() {
                    return None;
                }
                // A name that isn't UTF-8 can't be a JSON key that names
                // the file again, and the copy is disposable.
                Some((name.to_str()?.to_string(), marks))
            })
            .collect();
        Some((dir, marks))
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

    pub fn flag(&self, path: &Path) -> Option<Flag> {
        self.record(path).and_then(|r| r.flag)
    }

    pub fn set_flag(&mut self, path: &Path, flag: Option<Flag>) {
        self.update(path, |rec| rec.flag = flag);
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
        self.update_edits(path, |rec| rec.adjustments = adj);
    }

    pub fn touchups(&self, path: &Path) -> Vec<TouchUp> {
        self.record(path)
            .map(|r| r.touchups.clone())
            .unwrap_or_default()
    }

    pub fn set_touchups(&mut self, path: &Path, touchups: &[TouchUp]) {
        let touchups = touchups.to_vec();
        self.update_edits(path, |rec| rec.touchups = touchups);
    }

    pub fn masks(&self, path: &Path) -> Vec<Mask> {
        self.record(path)
            .map(|r| r.masks.to_vec())
            .unwrap_or_default()
    }

    pub fn set_masks(&mut self, path: &Path, masks: &[Mask]) {
        let masks = masks.to_vec();
        self.update_edits(path, |rec| rec.masks.set(masks));
    }

    pub fn detections(&self, path: &Path) -> crate::develop::Detections {
        self.record(path)
            .map(|r| r.detections.clone())
            .unwrap_or_default()
    }

    /// Records which coverage answers `target` for `path`. Through
    /// `update_edits`, since a mask reading it now renders differently.
    pub fn set_detection(
        &mut self,
        path: &Path,
        target: crate::develop::DetectTarget,
        id: Option<crate::develop::RasterId>,
    ) {
        self.update_edits(path, |rec| rec.detections.set(target, id));
    }

    pub fn rotation(&self, path: &Path) -> u8 {
        self.record(path).map(|r| r.rotation).unwrap_or(0)
    }

    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let rotation = rotation % 4;
        self.update_edits(path, |rec| rec.rotation = rotation);
    }

    /// Unix seconds when the photo's edits last changed; `None` if it has none.
    pub fn edited(&self, path: &Path) -> Option<u64> {
        self.record(path).and_then(|r| r.edited)
    }

    #[cfg(test)]
    pub(crate) fn set_edited(&mut self, path: &Path, secs: u64) {
        self.update(path, |rec| rec.edited = Some(secs));
    }

    /// The stored score and whether the photo was edited after it was taken.
    pub fn score(&self, path: &Path) -> Option<(&StoredScore, bool)> {
        let rec = self.record(path)?;
        let stored = rec.score.as_ref()?;
        Some((stored, stored.edits != rec.edit_signature()))
    }

    /// Store `score` as measured on the photo's edits as of `edits`.
    pub fn set_score(&mut self, path: &Path, score: QualityScore, edits: u64) {
        self.update(path, |rec| rec.score = Some(StoredScore { score, edits }));
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
                self.marks_revision += 1;
            }
            self.enqueue(path, WriteOp::Delete);
        }
        self.forget_group_members(paths);
    }

    /// The group half of [`FolderStore::forget_photos`]. wasm forgets each
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

    fn update_edits(&mut self, path: &Path, edit: impl FnOnce(&mut ImageRecord)) {
        let now = crate::sidecar::unix_now();
        self.update(path, |rec| rec.edit(now, edit));
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
        let sorted_by = (rec.rating, rec.edited);
        let marked = PhotoMarks::of(&rec);
        mutate(&mut rec);
        if (rec.rating, rec.edited) != sorted_by {
            self.sort_revision += 1;
        }
        if PhotoMarks::of(&rec) != marked {
            self.marks_revision += 1;
        }
        if rec.is_empty() {
            self.enqueue(path, WriteOp::Delete);
        } else {
            self.enqueue(path, WriteOp::Put(Box::new(rec.clone())));
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

    /// [`FolderStore::remove`] through a captured directory handle, for a photo
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
            self.marks_revision += 1;
        }
        self.delete_sidecar_with_handle(path, dir_handle);
    }
}

impl Default for FolderStore {
    fn default() -> FolderStore {
        FolderStore::new()
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
/// `FolderStore`, so it can run on a background thread. A missing folder gives
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
        match crate::sidecar::read_sidecar_file(&path) {
            Ok(rec) => {
                if !rec.is_empty() {
                    images.insert(stem.to_os_string(), rec);
                }
            }
            Err(e) => {
                eprintln!("[folder store] unreadable sidecar {}: {e}", path.display());
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
        "{skipped} sidecar file{} could not be read and {} skipped.",
        if skipped == 1 { "" } else { "s" },
        if skipped == 1 { "was" } else { "were" },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;
    use crate::persist::groups::{Group, GroupId};

    /// Sidecar writes are queued, so a test that reads the disk waits here
    /// first. A timeout rather than an unbounded wait, so a stuck writer
    /// fails the assertion instead of hanging the suite.
    fn flush(cat: &mut FolderStore) {
        cat.flush_blocking(std::time::Duration::from_secs(10));
    }

    fn sidecar_for(dir: &Path, filename: &str) -> PathBuf {
        dir.join(SIDECAR_DIR).join(format!("{filename}.xmp"))
    }

    #[test]
    fn round_trip_persists_across_reload() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg"); // nonexistent — exercises the fallback.

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set(&p, 3);
            assert_eq!(cat.get(&p), Some(3));
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(3));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn zero_removes_entry_and_persists_removal() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set(&p, 4);
            assert_eq!(cat.get(&p), Some(4));
            cat.set(&p, 0);
            assert_eq!(cat.get(&p), None);
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rating_is_clamped_to_five() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&p, 9);
        assert_eq!(cat.get(&p), Some(5));

        flush(&mut cat);
        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(5));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_flag_persists_and_clearing_it_deletes_an_otherwise_empty_sidecar() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set_flag(&p, Some(Flag::Reject));
        flush(&mut cat);
        let sidecar = std::fs::read_to_string(sidecar_for(&dir, "photo.jpg")).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&sidecar).unwrap(),
            serde_json::json!({"flag": "reject"})
        );
        let mut reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.flag(&p), Some(Flag::Reject));

        reloaded.set_flag(&p, None);
        flush(&mut reloaded);
        assert!(
            !sidecar_for(&dir, "photo.jpg").exists(),
            "an unflagged, unrated photo has no sidecar"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn adjustments_persist_across_reload() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let adj = Adjustments {
            exposure: 1.5,
            contrast: 25.0,
            ..Default::default()
        };

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set_adjustments(&p, &adj);
            assert_eq!(cat.adjustments(&p), adj);
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.adjustments(&p), adj);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn denoise_persists_across_reload() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let adj = Adjustments {
            denoise: 40.0,
            ..Default::default()
        };

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set_adjustments(&p, &adj);
            assert_eq!(cat.adjustments(&p), adj);
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.adjustments(&p), adj);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rotation_persists_across_reload() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set_rotation(&p, 3);
            assert_eq!(cat.rotation(&p), 3);
            // Wraps mod 4.
            cat.set_rotation(&p, 5);
            assert_eq!(cat.rotation(&p), 1);
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.rotation(&p), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn touchups_persist_across_reload_and_can_be_removed() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let t = TouchUp {
            center: [0.4, 0.5],
            radius: 0.02,
            source: [0.6, 0.5],
            feather: 0.5,
            delta: [0.01, -0.02, 0.0],
            opacity: 1.0,
        };
        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set_touchups(&p, &[t]);
            assert_eq!(cat.touchups(&p), vec![t]);
        }
        let mut reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.touchups(&p), vec![t]);
        reloaded.set_touchups(&p, &[]);
        flush(&mut reloaded);
        assert!(FolderStore::with_dir(dir.clone()).touchups(&p).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn masks_persist_across_reload_and_can_be_removed() {
        let dir = temp_folder("folder-store-masks");
        let p = dir.join("photo.jpg");
        let mut m = Mask::radial([0.3, 0.6], [0.2, 0.1]);
        m.amounts.exposure = 1.0;
        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set_masks(&p, &[m]);
            assert_eq!(cat.masks(&p), vec![m]);
        }
        let mut reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.masks(&p), vec![m]);
        reloaded.set_masks(&p, &[]);
        flush(&mut reloaded);
        assert!(FolderStore::with_dir(dir.clone()).masks(&p).is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rating_and_adjustments_coexist() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let adj = Adjustments {
            temp: -30.0,
            whites: 15.0,
            ..Default::default()
        };

        {
            let mut cat = FolderStore::with_dir(dir.clone());
            cat.set(&p, 5);
            cat.set_adjustments(&p, &adj);
            assert_eq!(cat.get(&p), Some(5));
            assert_eq!(cat.adjustments(&p), adj);
        }

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.get(&p), Some(5));
        assert_eq!(reloaded.adjustments(&p), adj);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn identity_adjustments_omitted_from_sidecar_json() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        // A rated image with identity edits: sidecar has no "adjustments" key.
        {
            let mut cat = FolderStore::with_dir(dir.clone());
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
            let mut cat = FolderStore::with_dir(dir.clone());
            let adj = Adjustments {
                shadows: 40.0,
                ..Default::default()
            };
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
        let dir = temp_folder("folder-store-test");
        std::fs::write(dir.join(SIDECAR_DIR), b"not a dir").unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&dir, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&dir, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        let raw = dir.join("PHOTO1.ARW");
        let jpg = dir.join("PHOTO1.JPG");

        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&raw, 5);
        cat.set(&jpg, 2);
        flush(&mut cat);
        assert_eq!(cat.get(&raw), Some(5));
        assert_eq!(cat.get(&jpg), Some(2));
        assert!(sidecar_for(&dir, "PHOTO1.ARW").exists());
        assert!(sidecar_for(&dir, "PHOTO1.JPG").exists());

        let reloaded = FolderStore::with_dir(dir.clone());
        assert_eq!(reloaded.get(&raw), Some(5));
        assert_eq!(reloaded.get(&jpg), Some(2));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn lightphotos_dir_created_lazily_only_on_first_write() {
        let dir = temp_folder("folder-store-test");
        let mut cat = FolderStore::with_dir(dir.clone());
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

    fn sample_score() -> QualityScore {
        QualityScore {
            value: 73,
            basis: crate::scoring::quality::Basis::WithAesthetics,
            base: Some(80),
            deductions: vec![crate::scoring::quality::Deduction {
                penalty: crate::scoring::quality::Penalty::Noisy,
                points: 7,
            }],
        }
    }

    #[test]
    fn a_score_alone_keeps_the_sidecar_and_round_trips() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let mut cat = FolderStore::with_dir(dir.clone());
        let edits = ImageRecord::default().edit_signature();
        cat.set_score(&p, sample_score(), edits);
        flush(&mut cat);
        assert!(sidecar_for(&dir, "photo.jpg").exists());

        let reloaded = FolderStore::with_dir(dir.clone());
        let (stored, stale) = reloaded.score(&p).expect("the score survives a reload");
        assert_eq!(stored.score, sample_score());
        assert!(!stale, "nothing was edited since");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edit_after_scoring_marks_the_score_stale_and_undoing_it_clears_that() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set_score(&p, sample_score(), ImageRecord::default().edit_signature());
        cat.set(&p, 4);
        assert_eq!(
            cat.score(&p).map(|(_, stale)| stale),
            Some(false),
            "a rating is not an edit"
        );

        let adj = Adjustments {
            exposure: 1.0,
            ..Adjustments::default()
        };
        cat.set_adjustments(&p, &adj);
        assert_eq!(cat.score(&p).map(|(_, stale)| stale), Some(true));
        cat.set_adjustments(&p, &Adjustments::default());
        assert_eq!(cat.score(&p).map(|(_, stale)| stale), Some(false));

        cat.set_rotation(&p, 1);
        assert_eq!(cat.score(&p).map(|(_, stale)| stale), Some(true));

        flush(&mut cat);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_record_deletes_the_sidecar_file() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let mut cat = FolderStore::with_dir(dir.clone());
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
        let a = temp_folder("folder-store-test");
        let b = temp_folder("folder-store-test");
        let pa = a.join("photo.jpg");
        let pb = b.join("photo.jpg"); // same filename, different directory

        let mut cat = FolderStore::with_dir(a.clone());
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
    fn a_photo_outside_the_active_directory_never_sees_its_records() {
        let a = temp_folder("folder-store-test");
        let b = temp_folder("folder-store-test");
        let pa = a.join("IMG_0001.JPG");
        let pb = b.join("IMG_0001.JPG");

        let mut cat = FolderStore::with_dir(a.clone());
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
                opacity: 1.0,
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
        assert_eq!(FolderStore::with_dir(a.clone()).get(&pa), Some(4));

        std::fs::remove_dir_all(&a).unwrap();
        std::fs::remove_dir_all(&b).unwrap();
    }

    #[test]
    fn apply_loaded_is_discarded_for_a_directory_no_longer_active() {
        let a = temp_folder("folder-store-test");
        let b = temp_folder("folder-store-test");
        let pa = a.join("photo.jpg");

        let mut cat = FolderStore::with_dir(a.clone());
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
        let dir = temp_folder("folder-store-test");
        let written = dir.join("written.jpg");
        let other = dir.join("other.jpg");

        // `other.jpg` already has a rating on disk from a previous session.
        let mut seed = FolderStore::with_dir(dir.clone());
        seed.set(&other, 3);
        drop(seed);

        let mut cat = FolderStore::new();
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
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        // A rating exists on disk from a previous session.
        FolderStore::with_dir(dir.clone()).set(&p, 4);

        let mut cat = FolderStore::new();
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
        let a = temp_folder("folder-store-test");
        let b = temp_folder("folder-store-test");
        std::fs::create_dir_all(a.join(SIDECAR_DIR)).unwrap();
        std::fs::write(sidecar_for(&a, "bad.jpg"), b"{not valid json").unwrap();

        let mut cat = FolderStore::new();
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
        let dir = temp_folder("folder-store-test");
        let paths: Vec<PathBuf> = (0..20_000)
            .map(|i| dir.join(format!("photo{i:05}.jpg")))
            .collect();

        let mut cat = FolderStore::with_dir(dir.clone());
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
            FolderStore::with_dir(dir.clone()).get(&paths[19_999]),
            Some(3),
            "every queued write must still reach the disk"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_folder_round_trip_during_a_flush_does_not_revert_the_edit() {
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        // A previous session left this photo at three stars.
        FolderStore::with_dir(dir.clone()).set(&p, 3);

        let mut cat = FolderStore::with_dir(dir.clone());
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
            FolderStore::with_dir(dir.clone()).get(&p),
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
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        FolderStore::with_dir(dir.clone()).set(&p, 3);

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        FolderStore::with_dir(dir.clone()).set(&p, 3);

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");

        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&p, 5);
        flush(&mut cat);

        // Another tool rewrites the sidecar while the user is elsewhere.
        FolderStore::with_dir(dir.clone()).set(&p, 1);

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

    fn fully_populated() -> ImageRecord {
        let mut rec = ImageRecord {
            rating: Some(3),
            flag: Some(Flag::Reject),
            label: Some(ColorLabel::Green),
            score: Some(StoredScore {
                score: sample_score(),
                edits: 9,
            }),
            ..ImageRecord::default()
        };
        rec.edit(1_700_000_000, |r| {
            r.adjustments.exposure = -0.7;
            r.rotation = 2;
            r.touchups = vec![TouchUp {
                center: [0.4, 0.5],
                radius: 0.02,
                source: [0.6, 0.5],
                feather: 0.5,
                delta: [0.01, -0.02, 0.0],
                opacity: 1.0,
            }];
        });
        rec
    }

    #[test]
    fn a_record_the_lib_writes_is_what_the_folder_store_loads() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let rec = fully_populated();
        crate::sidecar::write_record(&p, &rec).unwrap();

        let cat = FolderStore::with_dir(dir.clone());
        assert_eq!(cat.record(&p), Some(&rec));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_record_the_folder_store_writes_is_what_the_lib_reads() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let rec = fully_populated();
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&p, 3);
        cat.set_flag(&p, rec.flag);
        cat.set_label(&p, rec.label);
        cat.set_adjustments(&p, &rec.adjustments);
        cat.set_touchups(&p, &rec.touchups);
        cat.set_rotation(&p, rec.rotation);
        cat.set_score(&p, sample_score(), 9);
        flush(&mut cat);

        let read = crate::sidecar::read_record(&p).unwrap();
        assert_eq!(Some(&read), cat.record(&p));
        assert_eq!(
            ImageRecord {
                edited: rec.edited,
                ..read
            },
            rec,
            "every field but the edit time matches the lib's own record"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn load_sidecars_reads_a_directory_without_mutating_a_folder_store() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        FolderStore::with_dir(dir.clone()).set(&p, 4);

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

    #[test]
    fn ratings_flags_and_labels_leave_the_edit_time_alone() {
        let dir = temp_folder("folder-store-test");
        let p = dir.join("photo.jpg");
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&p, 2);
        cat.set_flag(&p, Some(Flag::Pick));
        cat.set_label(&p, Some(ColorLabel::Red));
        assert_eq!(cat.edited(&p), None);
        cat.set_rotation(&p, 1);
        let stamped = cat.edited(&p);
        assert!(stamped.is_some());
        cat.set(&p, 4);
        assert_eq!(cat.edited(&p), stamped);
        cat.flush_blocking(std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edited_sidecar_without_an_edit_time_takes_the_file_time() {
        let dir = temp_folder("folder-store-test");
        let side = dir.join(SIDECAR_DIR);
        std::fs::create_dir_all(&side).unwrap();
        std::fs::write(side.join("old.jpg.xmp"), r#"{"rotation":1}"#).unwrap();
        std::fs::write(side.join("odd.jpg.xmp"), r#"{"rating":2,"edited":5}"#).unwrap();
        std::fs::write(side.join("bare.jpg.xmp"), r#"{"edited":5}"#).unwrap();
        let file_time = std::fs::metadata(side.join("old.jpg.xmp"))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let loaded = load_sidecars(&dir);
        let edited = |n: &str| loaded.images.get(std::ffi::OsStr::new(n)).unwrap().edited;
        assert_eq!(edited("old.jpg"), Some(file_time));
        assert_eq!(edited("odd.jpg"), None, "no edits, no edit time");
        assert!(
            !loaded.images.contains_key(std::ffi::OsStr::new("bare.jpg")),
            "a sidecar holding only an edit time is empty"
        );
        assert_eq!(loaded.skipped, 0);
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

    fn only_group(cat: &FolderStore) -> (GroupId, Group) {
        let all: Vec<_> = cat.groups().unwrap().iter().collect();
        assert_eq!(all.len(), 1, "expected one group, got {all:?}");
        (all[0].0.clone(), all[0].1.clone())
    }

    #[test]
    fn a_group_round_trips_through_writeback_and_reload() {
        let dir = temp_folder("folder-store-test");
        let members = photos(&dir, &["IMG_0001.JPG", "IMG_0002.JPG", "IMG_0003.JPG"]);
        let group = Group::new(members, Some("IMG_0002.JPG".into())).unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
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
            only_group(&FolderStore::with_dir(dir.clone())),
            (id.clone(), group)
        );

        let writes = cat.groups().unwrap().dissolve(&id);
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);
        assert!(!file.exists(), "dissolving deletes the sidecar");
        assert!(FolderStore::with_dir(dir.clone())
            .groups()
            .unwrap()
            .is_empty());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_group_with_no_representative_writes_none_and_reloads_without_one() {
        let dir = temp_folder("folder-store-test");
        let members = photos(&dir, &["a.jpg", "b.jpg"]);
        let group = Group::new(members, None).unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
        let writes = cat
            .groups()
            .unwrap()
            .create(group.clone(), std::time::SystemTime::now());
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);
        let (id, _) = only_group(&cat);

        let body: serde_json::Value =
            serde_json::from_slice(&std::fs::read(group_file::path(&dir, &id)).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"v": 1, "members": ["a.jpg", "b.jpg"]})
        );
        assert_eq!(only_group(&FolderStore::with_dir(dir.clone())), (id, group));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_load_without_groups_creates_no_directory() {
        let dir = temp_folder("folder-store-test");
        photos(&dir, &["a.jpg", "b.jpg"]);
        let cat = FolderStore::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        assert!(
            !dir.join(SIDECAR_DIR).exists(),
            "loading a pristine folder must not create .lightphotos"
        );

        FolderStore::with_dir(dir.clone()).set(&dir.join("a.jpg"), 3);
        let cat = FolderStore::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        assert!(
            !groups_dir(&dir).exists(),
            "loading a folder with photo sidecars must not create groups/"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn loading_repairs_groups_in_memory_and_leaves_their_files_alone() {
        let dir = temp_folder("folder-store-test");
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
        let missing =
            r#"{"v":1,"members":["gone.jpg","a.jpg","b.jpg"],"representative":"gone.jpg"}"#;
        let missing_file = write_group_file(&dir, "g-2", missing);
        let rival = r#"{"v":1,"members":["b.jpg","c.jpg"],"representative":"b.jpg"}"#;
        let rival_file = write_group_file(&dir, "g-1", rival);

        let mut cat = FolderStore::with_dir(dir.clone());
        assert_eq!(cat.take_error(), None);
        let (id, group) = only_group(&cat);
        assert_eq!(id.to_string(), "g-2");
        assert_eq!(group.members(), ["a.jpg", "b.jpg"]);
        assert_eq!(group.rep(), None, "a missing representative is cleared");
        flush(&mut cat);
        assert_eq!(std::fs::read_to_string(&missing_file).unwrap(), missing);
        assert_eq!(std::fs::read_to_string(&rival_file).unwrap(), rival);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_unreadable_group_is_counted_and_the_rest_still_load() {
        let dir = temp_folder("folder-store-test");
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

        let mut cat = FolderStore::with_dir(dir.clone());
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
        let dir = temp_folder("folder-store-test");
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

        let mut cat = FolderStore::with_dir(dir.clone());
        let (winner, _) = only_group(&cat);
        assert_eq!(winner.to_string(), "g-000000000002-000000");
        let writes = cat.groups().unwrap().dissolve(&winner);
        cat.apply_group_writes(writes).unwrap();
        flush(&mut cat);

        assert!(FolderStore::with_dir(dir.clone())
            .groups()
            .unwrap()
            .is_empty());
        assert_eq!(group_ids_on_disk(&dir), Vec::<String>::new());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_group_dropped_for_a_missing_photo_stays_dropped_when_it_returns() {
        let dir = temp_folder("folder-store-test");
        photos(&dir, &["a.jpg", "c.jpg", "d.jpg"]);
        let dropped = write_group_file(
            &dir,
            "g-1",
            r#"{"v":1,"members":["a.jpg","b.jpg"],"representative":"a.jpg"}"#,
        );

        let mut cat = FolderStore::with_dir(dir.clone());
        assert!(cat.groups().unwrap().is_empty());
        flush(&mut cat);
        assert!(dropped.exists(), "loading never writes");

        let unrelated = Group::new(names(&["c.jpg", "d.jpg"]), Some("c.jpg".into())).unwrap();
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
        let cat = FolderStore::with_dir(dir.clone());
        assert_eq!(
            only_group(&cat).1.members(),
            ["c.jpg", "d.jpg"],
            "g-1 must not come back with b.jpg"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_load_that_predates_a_group_write_keeps_the_group() {
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        let members = photos(&dir, &["a.jpg", "b.jpg"]);

        let mut cat = FolderStore::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        let group = Group::new(members, Some("b.jpg".into())).unwrap();
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
        let dir = temp_folder("folder-store-test");
        let members = photos(&dir, &["a.jpg", "b.jpg"]);
        let group = Group::new(members, Some("a.jpg".into())).unwrap();
        let writes = Groups::default().create(group, std::time::SystemTime::now());

        let mut cat = FolderStore::new();
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
        let dir = temp_folder("folder-store-test");
        let elsewhere = temp_folder("folder-store-test");
        photos(&dir, &["a.jpg", "b.jpg", "c.jpg"]);
        let (first, second) = (
            GroupId::from_stem("g-1".as_ref()).unwrap(),
            GroupId::from_stem("g-2".as_ref()).unwrap(),
        );
        let second_group = Group::new(names(&["b.jpg", "c.jpg"]), Some("c.jpg".into())).unwrap();

        let mut cat = FolderStore::with_dir(dir.clone());
        let stale = load_sidecars(&dir);
        cat.enqueue(
            &group_file::path(&dir, &second),
            WriteOp::PutGroup(
                Group::new(names(&["a.jpg", "b.jpg"]), Some("a.jpg".into())).unwrap(),
            ),
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
