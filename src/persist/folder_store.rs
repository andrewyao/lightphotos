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
use crate::sidecar::{ColorLabel, Flag, ImageRecord, StoredScore, MAX_RATING};
#[cfg(not(target_arch = "wasm32"))]
use crate::sidecar::{SIDECAR_DIR, SIDECAR_EXT};

pub(crate) mod group_file;
#[cfg(not(target_arch = "wasm32"))]
mod locks;
mod writeback;
pub(crate) use writeback::{changed_onto, LoadMark, Patch};
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
    /// The latest notice from the writes that isn't a failure (a sidecar
    /// kept aside, a lock still being tried): [`FolderStore::take_notice`].
    last_notice: Option<String>,
    /// See [`FolderStore::sort_revision`].
    sort_revision: u64,
    /// See [`FolderStore::marks_revision`].
    marks_revision: u64,
    /// Sidecar mutations on their way to disk. Every write goes through here,
    /// so no caller blocks a frame on the filesystem.
    writeback: Writeback,
    /// The folder locks this store holds. See [`locks`].
    #[cfg(not(target_arch = "wasm32"))]
    locks: locks::Locks,
    /// What the open folder's visit may write.
    #[cfg(not(target_arch = "wasm32"))]
    visit: locks::Visit,
    /// The open folder's key, for the read-only test on each write.
    #[cfg(not(target_arch = "wasm32"))]
    open_key: Option<crate::sidecar::FolderKey>,
    /// This visit put the open folder in `read_only_folders`, so leaving it
    /// undecided takes it out again. A folder decided read-only earlier in
    /// the session stays.
    #[cfg(not(target_arch = "wasm32"))]
    quiet_added: bool,
    /// The visit changed since the app last asked. See
    /// [`FolderStore::take_visit_changed`].
    #[cfg(not(target_arch = "wasm32"))]
    visit_changed: bool,
    /// Every folder this session opened read-only and has not opened
    /// writable since: no derived file is written into them, even by work
    /// still running after the user moved on.
    #[cfg(not(target_arch = "wasm32"))]
    read_only_folders: HashSet<crate::sidecar::FolderKey>,
    /// See [`FolderStore::take_lockless_notice`].
    lockless_notice: bool,
    /// The browser couldn't reach the folder: see `fail_open`.
    #[cfg(target_arch = "wasm32")]
    no_access: bool,
    /// See [`FolderStore::take_refused`].
    refused: Vec<PathBuf>,

    /// The active folder's File System Access handle. wasm32 has no OS paths,
    /// so sidecar I/O goes through this.
    #[cfg(target_arch = "wasm32")]
    wasm_dir_handle: Option<web_sys::FileSystemDirectoryHandle>,
}

impl FolderStore {
    /// An empty folder store with no active directory.
    pub fn new() -> FolderStore {
        #[cfg(not(target_arch = "wasm32"))]
        let locks = locks::Locks::default();
        FolderStore {
            images: HashMap::new(),
            groups: None,
            dirty: HashSet::new(),
            dir: None,
            last_error: None,
            last_notice: None,
            sort_revision: 0,
            marks_revision: 0,
            #[cfg(not(target_arch = "wasm32"))]
            writeback: Writeback::new(locks.shared()),
            #[cfg(target_arch = "wasm32")]
            writeback: Writeback::new(),
            #[cfg(not(target_arch = "wasm32"))]
            locks,
            #[cfg(not(target_arch = "wasm32"))]
            visit: locks::Visit::Unlocked,
            #[cfg(not(target_arch = "wasm32"))]
            open_key: None,
            #[cfg(not(target_arch = "wasm32"))]
            quiet_added: false,
            #[cfg(not(target_arch = "wasm32"))]
            visit_changed: false,
            #[cfg(not(target_arch = "wasm32"))]
            read_only_folders: HashSet::new(),
            lockless_notice: false,
            #[cfg(target_arch = "wasm32")]
            no_access: false,
            refused: Vec::new(),
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
        self.take_write_notices();
        #[cfg(not(target_arch = "wasm32"))]
        {
            if self.visit == locks::Visit::Opening {
                if let Some(visit) = self.locks.retry() {
                    self.set_visit(visit);
                }
            }
            let writeback = &self.writeback;
            self.locks.settle(|key| writeback.writes_pending_in(key));
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
        self.take_write_notices();
    }

    /// Every notice the writes left, as one line: all of them when there are
    /// few, else the first ones and how many more.
    fn take_write_notices(&mut self) {
        let notices = self.writeback.take_notices();
        for n in &notices {
            eprintln!("[folder store] {n}");
        }
        let line = match notices.as_slice() {
            [] => return,
            [one] => one.clone(),
            [a, b] => format!("{a}; {b}"),
            [a, b, rest @ ..] => format!("{a}; {b}; and {} more", rest.len()),
        };
        self.last_notice = Some(match self.last_notice.take() {
            Some(earlier) => format!("{earlier}; {line}"),
            None => line,
        });
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
        let photos = crate::navigation::Playlist::from_dir(dir);
        let mark = self.switch_dir(dir, !photos.entries().is_empty());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while self.visit == locks::Visit::Opening && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
            self.pump();
        }
        let loaded = load_sidecars(dir);
        self.apply_loaded(dir, mark, loaded, photos.entries());
    }

    /// Make `dir` active and clear the cache without disk I/O, so no lookup
    /// sees the previous directory's records.
    ///
    /// Every load starts here, so this is also where a load takes its place in
    /// the write order. The returned mark must reach [`FolderStore::apply_loaded`],
    /// or [`FolderStore::abandon_load`] if the load never runs.
    ///
    /// Natively it tries `dir`'s lock once, with no wait, when `dir` has
    /// photos. A lock another program holds makes the whole visit
    /// read-only; opening the folder again tries again. A lock busy for a
    /// moment makes it `Opening`, and [`FolderStore::pump`] tries again each
    /// frame. See `docs/design/folder-lock.md`.
    #[must_use]
    pub(crate) fn switch_dir(
        &mut self,
        dir: &Path,
        #[cfg_attr(
            target_arch = "wasm32",
            allow(unused_variables, reason = "the browser build takes no lock")
        )]
        has_photos: bool,
    ) -> LoadMark {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.forget_undecided();
            // A folder opened again may be on another volume now (an SD
            // card mounted at the same path), so its key is worked out afresh.
            self.writeback.forget_folder_key(dir);
            let (key, visit) = self.locks.open(dir, has_photos);
            if let Some(key) = &key {
                self.writeback.note_folder_key(dir, key.clone());
            }
            self.open_key = key;
            self.set_visit(visit);
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.no_access = false;
        }
        self.dir = Some(dir.to_path_buf());
        self.images.clear();
        self.groups = None;
        self.dirty.clear();
        self.writeback.begin_load()
    }

    /// Take in the open folder's visit: the derived-file writers' set and
    /// the lockless notice.
    #[cfg(not(target_arch = "wasm32"))]
    fn set_visit(&mut self, visit: locks::Visit) {
        if visit == (locks::Visit::Lockless { new: true }) {
            self.lockless_notice = true;
        }
        let quiet = matches!(visit, locks::Visit::ReadOnly(_) | locks::Visit::Opening);
        if let Some(key) = &self.open_key {
            if quiet {
                if self.read_only_folders.insert(key.clone()) {
                    self.quiet_added = true;
                }
            } else {
                self.read_only_folders.remove(key);
                self.quiet_added = false;
            }
        }
        self.visit = visit;
        self.visit_changed = true;
    }

    /// The open folder is being left: if its lock was never decided, it
    /// leaves the derived-file writers' read-only set. Only a folder decided
    /// read-only stays in it.
    #[cfg(not(target_arch = "wasm32"))]
    fn forget_undecided(&mut self) {
        let added = std::mem::take(&mut self.quiet_added);
        if self.visit != locks::Visit::Opening || !added {
            return;
        }
        if let Some(key) = &self.open_key {
            self.read_only_folders.remove(key);
        }
    }

    /// The load thread could not start, so nothing is known of the folder's
    /// sidecars: the visit is read-only, as for a lock that can't be made.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn fail_open(&mut self) {
        self.locks.leave();
        self.set_visit(locks::Visit::ReadOnly(HeldBy::NoAccess));
    }

    /// The folder's sidecars can't be loaded (in the browser, no handle for
    /// the folder): the visit is decided, read-only, and says so.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn fail_open(&mut self) {
        self.no_access = true;
    }

    /// Whether no derived file is written into the open folder now: its
    /// visit is read-only, or its lock is not decided yet.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn derived_quiet(&self) -> bool {
        matches!(
            self.visit,
            locks::Visit::ReadOnly(_) | locks::Visit::Opening
        )
    }

    /// Whether the visit may sweep derived files: decided and writable.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn may_sweep(&self) -> bool {
        !matches!(
            self.visit,
            locks::Visit::ReadOnly(_) | locks::Visit::Opening
        )
    }

    /// Whether the open folder's visit changed since the last call, so the
    /// app tells its derived-file writers.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn take_visit_changed(&mut self) -> bool {
        std::mem::take(&mut self.visit_changed)
    }

    /// Whether the open folder takes edits: its lock is decided writable
    /// (held, lockless, or a folder with no photos) and its sidecars have
    /// loaded. Before both, an edit could only build a record from an empty
    /// cache and replace the photo's saved sidecar with it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn editable(&self) -> bool {
        self.open_key.is_some()
            && matches!(
                self.visit,
                locks::Visit::Held | locks::Visit::Lockless { .. } | locks::Visit::Unlocked
            )
            && self.groups.is_some()
    }

    /// The browser has no locks: the folder takes edits once its sidecars
    /// have loaded.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn editable(&self) -> bool {
        self.dir.is_some() && self.groups.is_some() && !self.no_access
    }

    /// Whether the open folder's lock is still being tried, once a frame.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn lock_undecided(&self) -> bool {
        self.visit == locks::Visit::Opening
    }

    /// Whether the folder showing is still opening: not read-only, and not
    /// editable yet (its lock undecided, or its sidecars still loading).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn opening(&self) -> bool {
        self.open_key.is_some() && self.read_only().is_none() && !self.editable()
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn opening(&self) -> bool {
        self.dir.is_some() && self.groups.is_none() && !self.no_access
    }

    /// Every folder opened read-only this session and not since writable.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn read_only_folders(&self) -> &HashSet<crate::sidecar::FolderKey> {
        &self.read_only_folders
    }

    /// At quit, after the flush: let go of every folder whose writes have
    /// all landed, so a relaunch need not wait for the rest of the quit (the
    /// catalog and signal flushes). A folder with a write still queued or
    /// parked keeps its lock until the app ends.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn release_locks(&mut self) {
        self.locks.leave();
        let writeback = &self.writeback;
        self.locks.settle(|key| writeback.writes_pending_in(key));
        if self.writeback.backlog() == 0 {
            self.locks.release_writer();
        }
    }

    /// Who holds the open folder, when the visit is read-only.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn read_only(&self) -> Option<HeldBy> {
        match &self.visit {
            locks::Visit::ReadOnly(by) => Some(*by),
            _ => None,
        }
    }

    /// The browser build has no locks; only a folder it can't read is
    /// read-only.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn read_only(&self) -> Option<HeldBy> {
        self.no_access.then_some(HeldBy::NoAccess)
    }

    /// Whether the folder just opened can't be locked, so a headless MCP
    /// there is unsafe. True once per session per folder.
    pub(crate) fn take_lockless_notice(&mut self) -> bool {
        std::mem::take(&mut self.lockless_notice)
    }

    /// The open folder is left, by a close. Its lock goes once its queued
    /// writes have landed.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn leave_dir(&mut self) {
        self.forget_undecided();
        self.locks.leave();
        // Before the visit, so a read-only folder left stays in the set.
        self.open_key = None;
        self.set_visit(locks::Visit::Unlocked);
    }

    /// The app is quitting: the folders it holds say so in their holder
    /// line, so a relaunch on one waits for it a moment.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn begin_quit(&mut self) {
        self.locks.mark_quitting();
    }

    /// Photos whose edit was refused because the folder is read-only,
    /// since the last call, so the app can put its own copies back.
    pub(crate) fn take_refused(&mut self) -> Vec<PathBuf> {
        std::mem::take(&mut self.refused)
    }

    /// Refuse an edit to `path` in the open folder while it takes none
    /// (read-only, or still opening or loading), before anything changes.
    /// True when refused.
    fn refuse(&mut self, path: &Path) -> bool {
        if self.editable() || !self.in_open_folder(path) {
            return false;
        }
        self.refused.push(path.to_path_buf());
        true
    }

    /// Whether `path`'s folder is the open one, however either is spelled.
    #[cfg(not(target_arch = "wasm32"))]
    fn in_open_folder(&self, path: &Path) -> bool {
        if self.cache_key(path).is_some() {
            return true;
        }
        let Some(open) = &self.open_key else {
            return false;
        };
        crate::sidecar::FolderKey::of_photo(path).as_ref() == Some(open)
    }

    #[cfg(target_arch = "wasm32")]
    fn in_open_folder(&self, path: &Path) -> bool {
        self.cache_key(path).is_some()
    }

    /// Give back a mark whose load was never started, so the write history it
    /// was holding open can be dropped.
    pub(crate) fn abandon_load(&mut self, mark: LoadMark) {
        self.writeback.end_load(mark);
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
        self.writeback.end_load(mark);
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
        if let Some(by) = self.read_only() {
            return Err(GroupWriteRefused::ReadOnly(by));
        }
        // Groups change in memory first, so not until the visit is decided.
        if self.opening() {
            return Err(GroupWriteRefused::LoadPending);
        }
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

    /// Take the pending notice. Each is returned once.
    pub(crate) fn take_notice(&mut self) -> Option<String> {
        self.last_notice.take()
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
        self.update(path, Patch::Rating(rating));
    }

    pub fn flag(&self, path: &Path) -> Option<Flag> {
        self.record(path).and_then(|r| r.flag)
    }

    pub fn set_flag(&mut self, path: &Path, flag: Option<Flag>) {
        self.update(path, Patch::Flag(flag));
    }

    pub fn label(&self, path: &Path) -> Option<ColorLabel> {
        self.record(path).and_then(|r| r.label)
    }

    pub fn set_label(&mut self, path: &Path, label: Option<ColorLabel>) {
        self.update(path, Patch::Label(label));
    }

    pub fn adjustments(&self, path: &Path) -> Adjustments {
        self.record(path).map(|r| r.adjustments).unwrap_or_default()
    }

    /// Set the adjustments. The write changes only the sliders that differ
    /// from the cache's copy, onto what the sidecar holds.
    pub fn set_adjustments(&mut self, path: &Path, adj: &Adjustments) {
        let before = self.adjustments(path);
        self.update(path, Patch::adjustments(before, *adj));
    }

    /// Change one part of a photo's record whose record the cache does not
    /// hold, such as one in a folder the user has left. The write reads the
    /// photo's sidecar when it runs and changes only that part, so its other
    /// fields survive and a same-named photo in the active directory is never
    /// touched.
    ///
    /// A job result for the open folder before its visit takes edits (still
    /// opening, or read-only) comes here too. Like a write into a left
    /// folder it is parked until the app holds the folder's lock, and lands
    /// under it; it never lands while another program holds the folder.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn patch_uncached(&mut self, path: &Path, patch: Patch) {
        self.writeback.enqueue(path, WriteOp::patch(patch, None));
    }

    /// As on native, through the handle of the photo's own folder.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn patch_uncached(
        &mut self,
        path: &Path,
        patch: Patch,
        dir_handle: &web_sys::FileSystemDirectoryHandle,
    ) {
        self.writeback
            .enqueue(path, WriteOp::patch(patch, None), dir_handle.clone());
    }

    pub fn touchups(&self, path: &Path) -> Vec<TouchUp> {
        self.record(path)
            .map(|r| r.touchups.clone())
            .unwrap_or_default()
    }

    pub fn set_touchups(&mut self, path: &Path, touchups: &[TouchUp]) {
        let touchups = touchups.to_vec();
        let at = crate::sidecar::unix_now();
        self.update(path, Patch::Touchups { touchups, at });
    }

    pub fn masks(&self, path: &Path) -> Vec<Mask> {
        self.record(path)
            .map(|r| r.masks.to_vec())
            .unwrap_or_default()
    }

    pub fn set_masks(&mut self, path: &Path, masks: &[Mask]) {
        let masks = masks.to_vec();
        let at = crate::sidecar::unix_now();
        self.update(path, Patch::Masks { masks, at });
    }

    pub fn detections(&self, path: &Path) -> crate::develop::Detections {
        self.record(path)
            .map(|r| r.detections.clone())
            .unwrap_or_default()
    }

    pub fn rotation(&self, path: &Path) -> u8 {
        self.record(path).map(|r| r.rotation).unwrap_or(0)
    }

    pub fn set_rotation(&mut self, path: &Path, rotation: u8) {
        let rotation = rotation % 4;
        let at = crate::sidecar::unix_now();
        self.update(path, Patch::Rotation { rotation, at });
    }

    /// Unix seconds when the photo's edits last changed; `None` if it has none.
    pub fn edited(&self, path: &Path) -> Option<u64> {
        self.record(path).and_then(|r| r.edited)
    }

    #[cfg(test)]
    pub(crate) fn set_edited(&mut self, path: &Path, secs: u64) {
        self.update(path, Patch::Edited(secs));
    }

    /// The stored score and whether the photo was edited after it was taken.
    pub fn score(&self, path: &Path) -> Option<(&StoredScore, bool)> {
        let rec = self.record(path)?;
        let stored = rec.score.as_ref()?;
        Some((stored, stored.edits != rec.edit_signature()))
    }

    /// The one entry point for photos that left the folder, such as trashed
    /// ones. A delete the user starts is refused in a read-only visit before
    /// it gets here (`App::start_delete`); one a job finishes later, as a
    /// bulk delete does, is parked like any job result until the app holds
    /// the folder.
    /// Each loses its record and its sidecar, which skips the Trash
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
            if let Some(name) = self.cache_key(path).map(OsStr::to_os_string) {
                self.images.remove(&name);
                self.dirty.insert(name);
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
        // Not while the visit is undecided or read-only: a group file is
        // written whole, from this visit's cache. The next writable visit's
        // load drops the missing photos and writes the repair.
        if self.opening() || self.read_only().is_some() {
            return;
        }
        let writes = groups.forget(&names);
        if let Err(e) = self.apply_group_writes(writes) {
            self.note_persist_error(e);
        }
    }

    /// Apply a job's change to a photo the cache holds ([`FolderStore::holds`]),
    /// as any edit is applied.
    /// Apply `patch` to the cache's record for `path`, and queue the same
    /// change for its sidecar, where it is applied to the record on disk
    /// when it runs. The cache is changed from the change, never the other
    /// way round, so a field the cache doesn't know survives.
    ///
    /// Refused for a photo outside the active directory, and, before
    /// anything changes, while the open folder takes no edit: read-only, or
    /// not loaded yet (on every target).
    pub(crate) fn update(&mut self, path: &Path, patch: Patch) {
        if self.refuse(path) {
            return;
        }
        self.apply_change(path, patch);
    }

    /// Whether a job's result for `path` goes through the cache: the photo
    /// is the open folder's, its sidecars have loaded, and the visit is not
    /// read-only. Its lock may still be undecided: the write then parks
    /// until the app holds the folder.
    pub(crate) fn takes_result(&self, path: &Path) -> bool {
        self.cache_key(path).is_some() && self.groups.is_some() && self.visit_open()
    }

    /// A job's result for a photo [`FolderStore::takes_result`] is true for:
    /// into the cache and queued, as an edit, whatever the lock's state.
    pub(crate) fn apply_result(&mut self, path: &Path, patch: Patch) {
        if self.takes_result(path) {
            self.apply_change(path, patch);
        }
    }

    /// Whether a folder is open in this visit and not read-only.
    #[cfg(not(target_arch = "wasm32"))]
    fn visit_open(&self) -> bool {
        self.open_key.is_some() && self.read_only().is_none()
    }

    #[cfg(target_arch = "wasm32")]
    fn visit_open(&self) -> bool {
        self.dir.is_some() && self.read_only().is_none()
    }

    fn apply_change(&mut self, path: &Path, patch: Patch) {
        let Some(name) = self.cache_key(path).map(OsStr::to_os_string) else {
            self.note_persist_error(format!("{} is not in the open folder", path.display()));
            return;
        };
        self.dirty.insert(name.clone());
        let before = self.images.remove(&name).unwrap_or_default();
        let mut rec = before.clone();
        let sorted_by = (rec.rating, rec.edited);
        let marked = PhotoMarks::of(&rec);
        if let Err(e) = patch.apply(&mut rec) {
            self.note_persist_error(format!("could not change {}: {e}", path.display()));
            self.dirty.remove(&name);
            if !before.is_empty() {
                self.images.insert(name, before);
            }
            return;
        }
        if (rec.rating, rec.edited) != sorted_by {
            self.sort_revision += 1;
        }
        if PhotoMarks::of(&rec) != marked {
            self.marks_revision += 1;
        }
        if !rec.is_empty() {
            self.images.insert(name, rec);
        }
        // The cache's copy before the change goes along, for a sidecar on
        // disk that turns out unreadable (see `writeback::perform`).
        self.enqueue(path, WriteOp::patch(patch, Some(before)));
    }

    /// Every write goes to the writeback, which lands it under its folder's
    /// lock, or parks it while another program holds that folder. Edits from
    /// the user never get here for a folder that takes none (`refuse`).
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
                if matches!(op, WriteOp::Patch(_) | WriteOp::PutGroup(_)) {
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
    /// The folder is read-only for this visit.
    ReadOnly(HeldBy),
}

impl std::fmt::Display for GroupWriteRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            GroupWriteRefused::NoFolder => "no folder is open for these groups",
            GroupWriteRefused::LoadPending => "the folder's groups are still loading",
            GroupWriteRefused::ReadOnly(by) => return f.write_str(&by.read_only_text()),
        })
    }
}

/// Why a folder opened read-only: who holds it, or that it can't be locked.
#[cfg_attr(
    target_arch = "wasm32",
    allow(
        dead_code,
        reason = "the browser build has no folder locks, so only NoAccess is made there"
    )
)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeldBy {
    /// Another LightPhotos window or instance, by pid.
    App(u32),
    /// A headless `lightphotos-mcp`, by pid.
    Mcp(u32),
    /// A holder the lock file doesn't name.
    Other,
    /// No one: the lock couldn't be made (a read-only volume, no
    /// permission), so nothing would stop another writer.
    Unlockable,
    /// The folder's sidecars couldn't be read at open (the browser has no
    /// access to it, or the load couldn't start).
    NoAccess,
}

impl HeldBy {
    /// The read-only line and every refusal, in the UI's language.
    pub(crate) fn read_only_text(self) -> String {
        let t = crate::i18n::t();
        let who = match self {
            HeldBy::App(pid) => (t.held_by_app)(pid),
            HeldBy::Mcp(pid) => (t.held_by_mcp)(pid),
            HeldBy::Other => t.held_by_other.to_string(),
            HeldBy::Unlockable => return t.folder_cant_lock.to_string(),
            HeldBy::NoAccess => return t.folder_no_access.to_string(),
        };
        (t.folder_read_only)(&who)
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
    use crate::scoring::quality::QualityScore;

    /// Sidecar writes are queued, so a test that reads the disk waits here
    /// first. A timeout rather than an unbounded wait, so a stuck writer
    /// fails the assertion instead of hanging the suite.
    fn flush(cat: &mut FolderStore) {
        cat.flush_blocking(std::time::Duration::from_secs(10));
    }

    fn sidecar_for(dir: &Path, filename: &str) -> PathBuf {
        dir.join(SIDECAR_DIR).join(format!("{filename}.xmp"))
    }

    /// Design rows 9 and 11: a write for a photo of the open folder,
    /// spelled relative to the working directory, lands under the app's
    /// own lock; the writer never turns away its own writes.
    #[test]
    fn a_relative_photo_path_in_the_open_folder_is_saved() {
        let dir = temp_folder("relative-photo");
        std::fs::write(dir.join("a.jpg"), b"").unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in cwd.components().skip(1) {
            rel.push("..");
        }
        let photo = rel
            .join(dir.strip_prefix("/").unwrap_or(&dir))
            .join("a.jpg");
        let patch = Patch::Score(crate::sidecar::StoredScore {
            score: crate::scoring::quality::QualityScore {
                value: 50,
                basis: crate::scoring::quality::Basis::TechnicalOnly,
                base: None,
                deductions: Vec::new(),
            },
            edits: 0,
        });
        cat.patch_uncached(&photo, patch);
        flush(&mut cat);
        let error = cat.take_error();
        let saved = crate::sidecar::read_record(&dir.join("a.jpg")).unwrap();
        drop(cat);
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(error, None);
        assert_eq!(saved.score.map(|s| s.score.value), Some(50));
    }

    /// Design rows 1 and 29: a write into a folder another program holds
    /// is parked, not dropped: it lands once the folder is free, and a
    /// quit before then reports it.
    #[test]
    fn a_write_into_a_held_folder_waits_and_lands_once_free() {
        use crate::sidecar::{FolderLock, Writer};
        let dir = temp_folder("parked-write");
        let photo = dir.join("a.jpg");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut cat = FolderStore::new();
        cat.patch_uncached(
            &photo,
            Patch::adjustments(
                crate::develop::Adjustments::default(),
                crate::develop::Adjustments {
                    exposure: 1.0,
                    ..Default::default()
                },
            ),
        );
        cat.flush_blocking(std::time::Duration::from_millis(200));
        let waiting = cat.take_error();
        let before = crate::sidecar::read_record(&photo).unwrap().adjustments;
        drop(other);
        cat.flush_blocking(std::time::Duration::from_secs(10));
        let after = crate::sidecar::read_record(&photo).unwrap().adjustments;
        drop(cat);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(waiting.is_some_and(|e| e.contains("did not finish")));
        assert_eq!(before.exposure, 0.0, "nothing written while held");
        assert_eq!(after.exposure, 1.0, "and it lands once free");
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
            cat.set_masks(&p, &[m.clone()]);
            assert_eq!(cat.masks(&p), vec![m.clone()]);
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

    /// Review 3.7 round 5 #7: how long rating 20 000 photos takes to reach
    /// the disk, fresh and over existing sidecars. Run with
    /// `cargo test --release drain_20k -- --ignored --nocapture`.
    #[test]
    #[ignore = "a timing, not a check"]
    fn drain_20k() {
        let dir = temp_folder("folder-store-bench");
        std::fs::write(dir.join("photo.jpg"), b"").unwrap();
        let photos: Vec<PathBuf> = (0..20_000)
            .map(|i| dir.join(format!("p{i:05}.jpg")))
            .collect();
        for (label, stars) in [("fresh", 3u8), ("over existing", 4)] {
            let mut cat = FolderStore::with_dir(dir.clone());
            let t = std::time::Instant::now();
            for p in &photos {
                cat.set(p, stars);
            }
            let queued = t.elapsed();
            let errors = cat
                .writeback
                .flush_blocking(std::time::Duration::from_secs(120));
            let drained = t.elapsed();
            assert!(errors.is_empty(), "{errors:?}");
            eprintln!("drain_20k {label}: queued in {queued:?}, on disk after {drained:?}");
        }
        // 20 000 changes to one photo in a row, as a slider scrub sends.
        let mut cat = FolderStore::with_dir(dir.clone());
        let t = std::time::Instant::now();
        for i in 0..20_000u32 {
            cat.set(&photos[0], u8::try_from(i % 5).unwrap_or(0) + 1);
        }
        let errors = cat
            .writeback
            .flush_blocking(std::time::Duration::from_secs(120));
        assert!(errors.is_empty(), "{errors:?}");
        eprintln!("drain_20k one photo: on disk after {:?}", t.elapsed());
        let check = crate::sidecar::read_record(&photos[19_999]).unwrap().rating;
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(check, Some(4));
    }

    /// Review 3.7 round 4 (the root cause): every write is a field change
    /// applied to the record on disk. A field the cache doesn't know (here
    /// written to the sidecar behind its back, as a parked job result or a
    /// write from a left visit lands) survives an edit of another field.
    #[test]
    fn a_stale_cache_never_overwrites_a_field_on_disk() {
        let dir = temp_folder("folder-store-test");
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, b"").unwrap();
        {
            let mut seed = FolderStore::with_dir(dir.clone());
            seed.set(&photo, 3);
            flush(&mut seed);
        }
        let mut cat = FolderStore::with_dir(dir.clone());
        let mut behind = crate::sidecar::read_record(&photo).unwrap();
        behind.flag = Some(Flag::Pick);
        behind.adjustments.exposure = 1.0;
        crate::sidecar::write_record(&photo, &behind).unwrap();
        cat.set_label(&photo, Some(ColorLabel::Red));
        cat.set_rotation(&photo, 1);
        flush(&mut cat);
        let on_disk = crate::sidecar::read_record(&photo).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(on_disk.rating, Some(3));
        assert_eq!(
            on_disk.flag,
            Some(Flag::Pick),
            "the cache never knew the flag"
        );
        assert_eq!(on_disk.adjustments.exposure, 1.0);
        assert_eq!(on_disk.label, Some(ColorLabel::Red));
        assert_eq!(on_disk.rotation, 1);
    }

    /// Review 3.7 round 5 #4: a delete a job finishes in a read-only visit
    /// (a bulk delete started while the folder was writable) is parked like
    /// any job result, and lands once the app can hold the folder. Only a
    /// delete the user starts is refused there (`App::start_delete`).
    #[test]
    fn a_job_delete_in_a_read_only_visit_parks_and_lands_once_free() {
        use crate::sidecar::{FolderLock, Writer};
        let dir = temp_folder("folder-store-test");
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, b"").unwrap();
        {
            let mut seed = FolderStore::with_dir(dir.clone());
            seed.set(&photo, 3);
            flush(&mut seed);
        }
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        let read_only = cat.read_only().is_some();
        cat.forget_photos(std::slice::from_ref(&photo));
        cat.writeback
            .flush_blocking(std::time::Duration::from_millis(300));
        let while_held = crate::sidecar::read_record(&photo).unwrap().rating;
        let queued = cat.backlog();
        drop(other);
        flush(&mut cat);
        let after = crate::sidecar::read_record(&photo).unwrap().rating;
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(read_only);
        assert_eq!((while_held, queued), (Some(3), 1), "parked while held");
        assert_eq!(after, None, "lands once free");
    }

    /// Review 3.7 round 5 #2: a folder whose lock can never be made (here
    /// `.lightphotos` is a file) fails the write at once, with the reason,
    /// once; nothing waits.
    #[test]
    fn a_write_whose_lock_can_never_be_made_fails_at_once() {
        let dir = temp_folder("folder-store-test");
        std::fs::write(dir.join(SIDECAR_DIR), b"not a dir").unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        assert!(cat.take_error().is_none(), "no error before any write");
        cat.set(&dir.join("photo.jpg"), 3);
        let started = std::time::Instant::now();
        flush(&mut cat);
        let took = started.elapsed();
        let msg = cat.take_error().expect("the failed save is reported");
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(msg.contains("os error"), "the OS cause, got: {msg}");
        assert!(cat.take_error().is_none(), "reported once");
        assert_eq!(cat.backlog(), 0, "not parked");
        assert!(
            took < std::time::Duration::from_secs(2),
            "no wait: {took:?}"
        );
    }

    /// Review 3.7 round 5 #1: a sidecar that reads but isn't a record (bad
    /// JSON, or over the size cap) is moved aside, never deleted, and the
    /// edit goes onto the cache's copy, which is saved; the user hears once.
    #[test]
    fn an_unreadable_sidecar_is_moved_aside_and_the_edit_saved() {
        for (tag, bytes) in [
            ("bad-json", b"{not json".to_vec()),
            (
                "too-big",
                vec![b' '; crate::sidecar::MAX_SIDECAR_BYTES + 10],
            ),
        ] {
            let dir = temp_folder("folder-store-test");
            let photo = dir.join("photo.jpg");
            std::fs::write(&photo, b"").unwrap();
            {
                let mut seed = FolderStore::with_dir(dir.clone());
                seed.set(&photo, 3);
                flush(&mut seed);
            }
            let mut cat = FolderStore::with_dir(dir.clone());
            let sidecar = crate::sidecar::sidecar_path(&photo).unwrap();
            std::fs::write(&sidecar, &bytes).unwrap();
            cat.set_label(&photo, Some(ColorLabel::Red));
            flush(&mut cat);
            cat.pump();
            let msg = cat.take_notice().unwrap_or_default();
            let failed = cat.take_error();
            let mut aside = sidecar.as_os_str().to_os_string();
            aside.push(".unreadable-1");
            let kept = std::fs::read(&aside).unwrap_or_default();
            let saved = crate::sidecar::read_record(&photo).unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            assert_eq!(kept, bytes, "{tag}: moved aside whole");
            assert_eq!(saved.rating, Some(3), "{tag}: onto the cache's copy");
            assert_eq!(saved.label, Some(ColorLabel::Red), "{tag}: the edit saved");
            assert!(
                msg.contains("photo.jpg"),
                "{tag}: told, naming the photo: {msg}"
            );
            assert!(failed.is_none(), "{tag}: not a failure: {failed:?}");
        }
    }

    /// Review 3.7 round 6 #3: a sidecar that can't be read and won't be
    /// (here mode 000) fails that one write at once, with the reason,
    /// touches nothing, and another photo's rating in the folder still lands.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_sidecar_fails_its_own_write_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_folder("folder-store-test");
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        std::fs::write(&a, b"").unwrap();
        std::fs::write(&b, b"").unwrap();
        {
            let mut seed = FolderStore::with_dir(dir.clone());
            seed.set(&a, 2);
            flush(&mut seed);
        }
        let sidecar = crate::sidecar::sidecar_path(&a).unwrap();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o000)).unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.update(&a, Patch::Rating(Some(5)));
        cat.set(&b, 4);
        let started = std::time::Instant::now();
        flush(&mut cat);
        let took = started.elapsed();
        let error = cat.take_error();
        std::fs::set_permissions(&sidecar, std::fs::Permissions::from_mode(0o644)).unwrap();
        let a_on_disk = crate::sidecar::read_record(&a).unwrap().rating;
        let b_on_disk = crate::sidecar::read_record(&b).unwrap().rating;
        let mut aside = sidecar.as_os_str().to_os_string();
        aside.push(".unreadable-1");
        let moved = std::path::Path::new(&aside).exists();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            error.is_some_and(|e| e.contains("a.jpg")),
            "the reason, naming the photo"
        );
        assert_eq!(a_on_disk, Some(2), "a's sidecar untouched");
        assert!(!moved, "nothing moved aside");
        assert_eq!(b_on_disk, Some(4), "b's rating lands");
        assert!(
            took < std::time::Duration::from_secs(2),
            "no wait: {took:?}"
        );
    }

    /// Review 3.7 round 6 #2: a sidecar that isn't a record, for a change
    /// that carries no cache copy (a job's result for a photo the cache
    /// doesn't hold), is left as it is and the write fails, naming it.
    #[test]
    fn an_unreadable_sidecar_without_a_cache_copy_is_left_alone() {
        let dir = temp_folder("folder-store-test");
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, b"").unwrap();
        let cat_dir = temp_folder("folder-store-test");
        let mut cat = FolderStore::with_dir(cat_dir.clone());
        let sidecar = crate::sidecar::sidecar_path(&photo).unwrap();
        std::fs::create_dir_all(sidecar.parent().unwrap()).unwrap();
        std::fs::write(&sidecar, b"{not json").unwrap();
        cat.patch_uncached(&photo, Patch::Rating(Some(3)));
        flush(&mut cat);
        let error = cat.take_error();
        let kept = std::fs::read(&sidecar).unwrap();
        let mut aside = sidecar.as_os_str().to_os_string();
        aside.push(".unreadable-1");
        let moved = std::path::Path::new(&aside).exists();
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&cat_dir).unwrap();
        assert!(error.is_some_and(|e| e.contains("photo.jpg")));
        assert_eq!(kept, b"{not json", "left as it is");
        assert!(!moved);
    }

    /// Review 3.7 round 6 #2: a change that can't apply to a record that
    /// read fine fails the write; nothing is moved aside.
    #[test]
    fn a_change_that_cant_apply_moves_nothing_aside() {
        let dir = temp_folder("folder-store-test");
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, b"").unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&photo, 2);
        flush(&mut cat);
        cat.patch_uncached(&photo, Patch::Fail);
        flush(&mut cat);
        let sidecar = crate::sidecar::sidecar_path(&photo).unwrap();
        let mut aside = sidecar.as_os_str().to_os_string();
        aside.push(".unreadable-1");
        let moved = std::path::Path::new(&aside).exists();
        let error = cat.take_error();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(error.is_some());
        assert!(!moved, "a good record is never moved aside");
    }

    /// Review 3.7 round 5 #6: a change that can't apply fails its write
    /// with an error, and nothing stale goes over the sidecar.
    #[test]
    fn a_change_that_cant_apply_fails_and_writes_nothing() {
        let dir = temp_folder("folder-store-test");
        let photo = dir.join("photo.jpg");
        std::fs::write(&photo, b"").unwrap();
        let mut cat = FolderStore::with_dir(dir.clone());
        cat.set(&photo, 2);
        flush(&mut cat);
        cat.update(&photo, Patch::Fail);
        let cache_error = cat.take_error();
        cat.patch_uncached(&photo, Patch::Fail);
        flush(&mut cat);
        let write_error = cat.take_error();
        let saved = crate::sidecar::read_record(&photo).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(cache_error.is_some() && write_error.is_some());
        assert_eq!(cat.get(&photo), Some(2), "the cache is as it was");
        assert_eq!(saved.rating, Some(2), "the sidecar is as it was");
    }

    /// Review 3.7 round 5 #3: a folder whose sidecars can't be loaded is a
    /// decided, read-only visit that says so, not one opening for ever.
    #[test]
    fn a_folder_that_cant_load_is_read_only_not_opening() {
        let dir = temp_folder("folder-store-test");
        std::fs::write(dir.join("photo.jpg"), b"").unwrap();
        let mut cat = FolderStore::new();
        let mark = cat.switch_dir(&dir, true);
        cat.fail_open();
        cat.abandon_load(mark);
        let opening = cat.opening();
        let by = cat.read_only();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(!opening);
        assert_eq!(by, Some(HeldBy::NoAccess));
        assert!(HeldBy::NoAccess.read_only_text().contains("no access"));
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
        cat.update(
            &p,
            crate::persist::folder_store::Patch::Score(crate::sidecar::StoredScore {
                score: sample_score(),
                edits,
            }),
        );
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
        cat.update(
            &p,
            crate::persist::folder_store::Patch::Score(crate::sidecar::StoredScore {
                score: sample_score(),
                edits: ImageRecord::default().edit_signature(),
            }),
        );
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
        flush(&mut cat);

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
        let mark = cat.switch_dir(&b, true);
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
    fn an_edit_before_the_load_lands_is_refused_not_written_over_the_sidecar() {
        let dir = temp_folder("folder-store-test");
        let written = dir.join("written.jpg");
        let other = dir.join("other.jpg");

        // Both already have marks on disk from a previous session.
        let mut seed = FolderStore::with_dir(dir.clone());
        seed.set(&other, 3);
        seed.set(&written, 2);
        seed.set_flag(&written, Some(Flag::Pick));
        drop(seed);

        let mut cat = FolderStore::new();
        let mark = cat.switch_dir(&dir, true);
        // An edit after switch_dir, before the background load returns,
        // would build a record from nothing and replace the sidecar.
        cat.set(&written, 5);
        let refused = cat.take_refused();
        let loaded = load_sidecars(&dir);
        cat.apply_loaded(&dir, mark, loaded, &images_in(&dir));
        flush(&mut cat);

        assert_eq!(refused, std::slice::from_ref(&written));
        assert_eq!(cat.get(&written), Some(2), "the edit was refused");
        let on_disk = crate::sidecar::read_record(&written).unwrap();
        assert_eq!((on_disk.rating, on_disk.flag), (Some(2), Some(Flag::Pick)));
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
        let mark = cat.switch_dir(&dir, true);
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
        let mark = cat.switch_dir(&a, true);
        let loaded = load_sidecars(&a); // has skipped == 1
        let _ = cat.switch_dir(&b, true); // the user already left `a` before the load lands
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

        // 20000 writes can outlast `flush`'s 10s on a loaded CI runner while
        // the rest of the suite runs alongside, so this one waits longer.
        cat.flush_blocking(std::time::Duration::from_secs(120));
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
        let _ = cat.switch_dir(&elsewhere, true);
        let mark = cat.switch_dir(&dir, true);
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
        let _ = cat.switch_dir(&elsewhere, true);
        let mark = cat.switch_dir(&dir, true);
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
        let away = cat.switch_dir(&elsewhere, true);
        cat.abandon_load(away);
        let mark = cat.switch_dir(&dir, true);
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

        // The writer keeps its folder lock until it idles, just after it
        // reports the write done; a write by the other tool would park behind
        // it and be dropped with its store. Wait the lock out first.
        drop(
            crate::sidecar::FolderLock::acquire(
                &dir,
                crate::sidecar::Writer::Mcp,
                std::time::Duration::from_secs(5),
            )
            .unwrap(),
        );

        // Another tool rewrites the sidecar while the user is elsewhere.
        FolderStore::with_dir(dir.clone()).set(&p, 1);

        let _ = cat.switch_dir(&elsewhere, true);
        let mark = cat.switch_dir(&dir, true);
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
        cat.update(
            &p,
            crate::persist::folder_store::Patch::Score(crate::sidecar::StoredScore {
                score: sample_score(),
                edits: 9,
            }),
        );
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
        let made: Vec<String> = std::fs::read_dir(dir.join(SIDECAR_DIR))
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n != crate::sidecar::LOCK_FILE && n != crate::sidecar::WHO_FILE)
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(
            made,
            Vec::<String>::new(),
            "loading a pristine folder writes nothing but its lock"
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
        let away = cat.switch_dir(&elsewhere, true);
        cat.abandon_load(away);
        let mark = cat.switch_dir(&dir, true);
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
        let mark = cat.switch_dir(&dir, true);
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

        let unused = cat.switch_dir(&dir, true);
        cat.abandon_load(unused);
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
        let away = cat.switch_dir(&elsewhere, true);
        cat.abandon_load(away);
        let mark = cat.switch_dir(&dir, true);
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
