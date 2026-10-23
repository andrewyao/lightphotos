// SPDX-License-Identifier: MIT OR Apache-2.0

//! Sidecar I/O, off the frame.
//!
//! A sidecar write is a `create_dir_all`, a serialize, a temp file and a
//! rename: around 80-120 µs on APFS. Rating a selection of 20 000 photos does
//! that 20 000 times, so doing it inline froze the window for seconds. Every
//! [`FolderStore`](super::FolderStore) mutation now hands the work to this queue and
//! returns.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use crate::develop::Adjustments;
use crate::persist::groups::{Group, GroupId, GroupWrite, Groups};
use crate::sidecar::{ImageRecord, StoredScore};

/// One queued sidecar mutation.
///
/// The payload is a whole-record snapshot rather than a delta, because a
/// `FolderStore` mutation is already a read-modify-write of the entire
/// [`ImageRecord`] or [`Group`]. A later entry for a path therefore supersedes
/// every earlier one, which is what lets [`Writeback::overlay`] answer a read
/// with a single value and lets wasm admit one task per path without losing a
/// field.
///
/// `Patch` is the one delta. It changes one part of whatever the photo's
/// sidecar holds when the write runs. It is for a photo whose record the
/// cache does not hold, such as one in a folder the user has left.
/// [`Writeback::accept`] folds it into an earlier `Put` or `Delete` for the
/// same path, so the newest entry still answers alone.
///
/// `Put`, `Patch` and `Delete` are queued under the photo's own path and
/// derive its sidecar from it. `PutGroup` and `DeleteGroup` are queued under
/// the group sidecar's path, since a group has no photo of its own.
#[derive(Clone)]
pub(super) enum WriteOp {
    Put(Box<ImageRecord>),
    Patch(Box<Patch>),
    Delete,
    PutGroup(Group),
    DeleteGroup,
}

/// The part of a record a [`WriteOp::Patch`] changes. Everything else in
/// the sidecar stays as the write finds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Patch {
    /// A change to the adjustments: see [`AdjustmentsChange`].
    Adjustments(Box<AdjustmentsChange>),
    /// The quality score.
    Score(StoredScore),
}

/// A job's result, as the app's copy of a photo's adjustments before and
/// after it, made at a Unix time. Saving applies only the sliders it
/// changed, onto whatever the photo has saved, so a slider mid-drag on the
/// same photo is neither written half-finished nor lost.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AdjustmentsChange {
    pub(crate) before: Adjustments,
    pub(crate) after: Adjustments,
    pub(crate) at: u64,
}

impl AdjustmentsChange {
    /// `onto` with the sliders this change moved set as it set them.
    pub(crate) fn apply(&self, onto: &Adjustments) -> Adjustments {
        changed_onto(&self.before, &self.after, onto)
    }
}

/// `onto` with every field that differs between `before` and `after` set to
/// `after`'s, compared on the JSON form, where a field left out is its
/// default. Falls back to `after` whole if that form can't be read back.
pub(crate) fn changed_onto(
    before: &Adjustments,
    after: &Adjustments,
    onto: &Adjustments,
) -> Adjustments {
    use serde_json::Value;
    let json = |a: &Adjustments| match serde_json::to_value(a) {
        Ok(Value::Object(fields)) => fields,
        _ => serde_json::Map::new(),
    };
    let (b, a, mut out) = (json(before), json(after), json(onto));
    for key in b.keys().chain(a.keys()) {
        if b.get(key) == a.get(key) {
            continue;
        }
        match a.get(key) {
            Some(v) => {
                out.insert(key.clone(), v.clone());
            }
            None => {
                out.remove(key);
            }
        }
    }
    serde_json::from_value(Value::Object(out)).unwrap_or(*after)
}

impl Patch {
    /// The change from `before` to `after`, stamped now.
    pub(crate) fn adjustments(before: Adjustments, after: Adjustments) -> Patch {
        Patch::Adjustments(Box::new(AdjustmentsChange {
            before,
            after,
            at: crate::sidecar::unix_now(),
        }))
    }

    fn apply(&self, rec: &mut ImageRecord) {
        match self {
            Patch::Adjustments(change) => rec.edit(change.at, |rec| {
                rec.adjustments = change.apply(&rec.adjustments)
            }),
            Patch::Score(score) => rec.score = Some(score.clone()),
        }
    }
}

/// Where a background sidecar load sits in the write order.
///
/// A load reads the disk on another thread, so its snapshot can be older than
/// a write we have already issued. The mark names the point below which every
/// write had reached the disk when the load was requested, so anything we
/// wrote at or after it is a record the load may have missed.
/// [`FolderStore::switch_dir`](super::FolderStore::switch_dir) issues one and
/// [`FolderStore::apply_loaded`](super::FolderStore::apply_loaded) hands it back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LoadMark(u64);

/// The most recent mutation we issued for one photo, and its place in the
/// write order.
struct Authored {
    seq: u64,
    op: WriteOp,
}

/// A finished operation: the path and sequence it was for, and why it failed.
type Done = (PathBuf, u64, Option<String>);

/// Concurrent File System Access writes.
///
/// Not a memory budget, since a record serializes to a few hundred bytes. It
/// caps how many writes the browser has open at once, because the old code
/// detached one task per mutation and a bulk rate could hand the browser
/// 20 000 of them.
#[cfg(target_arch = "wasm32")]
const WEB_WRITE_WINDOW: usize = 32;

/// Sidecar writes and deletes that have left [`FolderStore`](super::FolderStore), plus
/// enough history to tell a background load's snapshot from a newer write of
/// our own.
///
/// # Invariants
///
/// - An entry carries the photo's own path, not just its file name, so a
///   folder switch cannot misroute a queued write into a same-named photo in
///   the new folder. `FolderStore` keys its cache by file name; this queue does
///   not.
/// - Operations on one path complete in submission order. Native gets that
///   from a single FIFO worker, wasm from admitting at most one task per path.
/// - `outstanding` holds every issued sequence that has not reported, so its
///   smallest member is the first write a concurrent load might not see. Every
///   sequence below it reached the disk before the load was requested.
/// - `authored` holds the newest op per path for as long as a load could
///   contradict it. It is trimmed to the still-queued writes whenever no load
///   is in flight, so a session that never leaves its folder keeps nothing
///   beyond the queue itself.
pub(super) struct Writeback {
    /// Queued or in-flight operations per path, counted so a path leaves only
    /// when the last of them reports. Drives [`Writeback::backlog`].
    pending: HashMap<PathBuf, u32>,
    /// Bumped once per enqueue. Orders writes against load requests.
    writes: u64,
    outstanding: std::collections::BTreeSet<u64>,
    authored: HashMap<PathBuf, Authored>,
    /// Loads requested and not yet applied or abandoned.
    loads: usize,
    /// The folder locks this store holds. See [`super::locks`].
    #[cfg(not(target_arch = "wasm32"))]
    locks: super::locks::SharedLocks,
    /// Writes waiting for a folder another program holds, in order. Shared
    /// with the writer thread, so a writer that dies leaves them for the
    /// next one, or for the inline path, to try again.
    #[cfg(not(target_arch = "wasm32"))]
    parked: Parked,
    /// Each queued photo folder's key, as spelled, worked out when a write
    /// for it is first queued, so nothing here touches the disk later. A
    /// folder opened again is worked out afresh
    /// ([`Writeback::forget_folder_key`]); a folder with no key is not kept.
    #[cfg(not(target_arch = "wasm32"))]
    folder_keys: HashMap<PathBuf, crate::sidecar::FolderKey>,
    /// The folder each outstanding write was counted under, so it is
    /// uncounted under the same key whatever the cache says by then.
    #[cfg(not(target_arch = "wasm32"))]
    seq_keys: HashMap<u64, crate::sidecar::FolderKey>,
    /// When the parked writes were last tried with no writer thread.
    #[cfg(not(target_arch = "wasm32"))]
    inline_retry: std::time::Instant,
    /// Queued, running or parked writes per folder. See
    /// [`Writeback::writes_pending_in`].
    #[cfg(not(target_arch = "wasm32"))]
    pending_folders: HashMap<crate::sidecar::FolderKey, u32>,

    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,

    /// `None` until the first write. Browsing a folder spawns no thread.
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<Sender<(PathBuf, u64, WriteOp)>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<std::thread::JoinHandle<()>>,

    /// Accepted but not yet spawned. wasm has no worker thread, so `pump` is
    /// the scheduler.
    #[cfg(target_arch = "wasm32")]
    queue: std::collections::VecDeque<(PathBuf, u64, WriteOp, web_sys::FileSystemDirectoryHandle)>,
    #[cfg(target_arch = "wasm32")]
    in_flight: std::collections::HashSet<PathBuf>,
}

impl Writeback {
    pub(super) fn new(
        #[cfg(not(target_arch = "wasm32"))] locks: super::locks::SharedLocks,
    ) -> Writeback {
        let (done_tx, done_rx) = channel();
        Writeback {
            pending: HashMap::new(),
            writes: 0,
            outstanding: std::collections::BTreeSet::new(),
            authored: HashMap::new(),
            loads: 0,
            #[cfg(not(target_arch = "wasm32"))]
            locks,
            #[cfg(not(target_arch = "wasm32"))]
            parked: Parked::default(),
            #[cfg(not(target_arch = "wasm32"))]
            folder_keys: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            seq_keys: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            inline_retry: std::time::Instant::now(),
            #[cfg(not(target_arch = "wasm32"))]
            pending_folders: HashMap::new(),
            done_tx,
            done_rx,
            #[cfg(not(target_arch = "wasm32"))]
            tx: None,
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
            #[cfg(target_arch = "wasm32")]
            queue: std::collections::VecDeque::new(),
            #[cfg(target_arch = "wasm32")]
            in_flight: std::collections::HashSet::new(),
        }
    }

    /// Photos with sidecar work still outstanding.
    pub(super) fn backlog(&self) -> usize {
        self.pending.len()
    }

    /// Register a background load that is about to read the disk, and return
    /// the mark its result must be applied with.
    ///
    /// The mark is the oldest write that has not reported yet, because that is
    /// the first one the load's snapshot may be taken before. With nothing
    /// queued it is one past the last write, so only writes issued after this
    /// call outrank the load.
    pub(super) fn begin_load(&mut self) -> LoadMark {
        self.loads += 1;
        LoadMark(self.outstanding.first().copied().unwrap_or(self.writes + 1))
    }

    /// Retire a load, whether its result arrived or it was never started.
    pub(super) fn end_load(&mut self) {
        self.loads = self.loads.saturating_sub(1);
        if self.loads == 0 {
            // No surviving load can disagree with the disk about a write that
            // already landed, so only the queue itself is worth remembering.
            self.authored
                .retain(|path, _| self.pending.contains_key(path));
        }
    }

    /// Replace every record in `images`, and every group in `groups`, that
    /// the load carrying `mark` may have read before one of our own writes
    /// reached the disk.
    ///
    /// `FolderStore::switch_dir` clears the cache, so a folder round-trip during a
    /// flush leaves nothing else to correct the load with. Without this the
    /// pre-edit record goes back into the cache and `reconcile_folder_store_mirrors`
    /// drops the user's change from the grid. A record we wrote before the load
    /// was requested is left alone, so an edit made outside the app still wins
    /// on a revisit.
    ///
    /// Keys here are `dir.join(name)` for the same `dir` the load carries, and
    /// `Path` compares by component, so a trailing separator, a doubled one or
    /// a `.` in either spelling cannot make the parent test miss. A spelling
    /// that could, such as `./photos` against `photos`, also fails the
    /// active-directory test in `apply_loaded`, which returns before reaching
    /// here.
    ///
    /// A group write can move photos between groups, so group writes replay
    /// in the order they were issued.
    pub(super) fn overlay(
        &self,
        dir: &Path,
        mark: LoadMark,
        images: &mut HashMap<OsString, ImageRecord>,
        groups: &mut Groups,
    ) {
        let groups_dir = dir
            .join(crate::sidecar::SIDECAR_DIR)
            .join(super::group_file::GROUPS_DIR);
        let mut group_writes = Vec::new();
        for (path, authored) in &self.authored {
            if authored.seq < mark.0 {
                continue;
            }
            match &authored.op {
                WriteOp::Put(_) | WriteOp::Patch(_) | WriteOp::Delete
                    if path.parent() != Some(dir) => {}
                WriteOp::Put(rec) => {
                    if let Some(name) = path.file_name() {
                        images.insert(name.to_os_string(), (**rec).clone());
                    }
                }
                WriteOp::Patch(patch) => {
                    if let Some(name) = path.file_name() {
                        patch.apply(images.entry(name.to_os_string()).or_default());
                    }
                }
                WriteOp::Delete => {
                    if let Some(name) = path.file_name() {
                        images.remove(name);
                    }
                }
                WriteOp::PutGroup(_) | WriteOp::DeleteGroup
                    if path.parent() != Some(groups_dir.as_path()) => {}
                WriteOp::PutGroup(group) => {
                    if let Some(id) = path.file_stem().and_then(GroupId::from_stem) {
                        group_writes.push((authored.seq, GroupWrite::Put(id, group.clone())));
                    }
                }
                WriteOp::DeleteGroup => {
                    if let Some(id) = path.file_stem().and_then(GroupId::from_stem) {
                        group_writes.push((authored.seq, GroupWrite::Delete(id)));
                    }
                }
            }
        }
        group_writes.sort_by_key(|(seq, _)| *seq);
        for (_, write) in &group_writes {
            groups.apply(write);
        }
    }

    /// Take the next sequence number and record the op against `path`. A
    /// `Patch` after a `Put` or `Delete` is recorded as the whole record the
    /// two leave on disk.
    fn accept(&mut self, path: &Path, op: &WriteOp) -> u64 {
        self.writes += 1;
        let seq = self.writes;
        self.outstanding.insert(seq);
        *self.pending.entry(path.to_path_buf()).or_insert(0) += 1;
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(key) = self.folder_key_of(path) {
            *self.pending_folders.entry(key.clone()).or_insert(0) += 1;
            self.seq_keys.insert(seq, key);
        }
        let op = match (op, self.authored.get(path).map(|a| &a.op)) {
            (WriteOp::Patch(patch), Some(WriteOp::Put(rec))) => {
                let mut rec = rec.clone();
                patch.apply(&mut rec);
                WriteOp::Put(rec)
            }
            (WriteOp::Patch(patch), Some(WriteOp::Delete)) => {
                let mut rec = Box::default();
                patch.apply(&mut rec);
                WriteOp::Put(rec)
            }
            (op, _) => op.clone(),
        };
        self.authored
            .insert(path.to_path_buf(), Authored { seq, op });
        seq
    }

    fn retire(&mut self, path: &Path, seq: u64) {
        self.outstanding.remove(&seq);
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(key) = self.seq_keys.remove(&seq) {
            if let Some(n) = self.pending_folders.get_mut(&key) {
                *n = n.saturating_sub(1);
                if *n == 0 {
                    self.pending_folders.remove(&key);
                }
            }
        }
        if let Some(inflight) = self.pending.get_mut(path) {
            *inflight -= 1;
            if *inflight == 0 {
                self.pending.remove(path);
                if self.loads == 0 {
                    self.authored.remove(path);
                }
            }
        }
    }

    fn drain_completions(&mut self, errors: &mut Vec<String>) {
        while let Ok((path, seq, err)) = self.done_rx.try_recv() {
            #[cfg(target_arch = "wasm32")]
            self.in_flight.remove(&path);
            self.retire(&path, seq);
            if let Some(e) = err {
                errors.push(e);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Writeback {
    pub(super) fn enqueue(&mut self, path: &Path, op: WriteOp) {
        let seq = self.accept(path, &op);
        match self.sender() {
            Some(tx) => {
                // A worker that died hands the op back. Retire it and write
                // inline, and the next enqueue starts a fresh worker.
                if let Err(std::sync::mpsc::SendError((path, seq, op))) =
                    tx.send((path.to_path_buf(), seq, op))
                {
                    self.tx = None;
                    if let Some(handle) = self.worker.take() {
                        let _ = handle.join();
                    }
                    self.perform_inline(&path, seq, &op);
                }
            }
            // Without a worker the write still has to happen, or the edit is
            // lost with no way for the user to find out.
            None => self.perform_inline(path, seq, &op),
        }
    }

    /// Without a worker thread the write runs here, under the same lock
    /// rules: one that would park joins the parked writes, behind any for
    /// its folder, and is tried again on the next write or flush.
    fn perform_inline(&self, path: &Path, seq: u64, op: &WriteOp) {
        let mut locks = super::locks::SaveLocks::new(self.locks.clone());
        let mut parked = lock_parked(&self.parked);
        retry_parked(&mut parked, &mut locks, &self.done_tx);
        if !behind_parked(&parked, path, op) {
            match perform_locked(&mut locks, path, op) {
                Saved::Done(result) => {
                    let _ = self.done_tx.send((path.to_path_buf(), seq, result.err()));
                }
                Saved::Parked => parked.push_back((path.to_path_buf(), seq, op.clone())),
            }
        } else {
            parked.push_back((path.to_path_buf(), seq, op.clone()));
        }
        drop(parked);
        locks.release_except(None);
    }

    /// Try the parked writes again here, for when no writer thread runs.
    fn retry_parked_inline(&self) {
        let mut locks = super::locks::SaveLocks::new(self.locks.clone());
        retry_parked(&mut lock_parked(&self.parked), &mut locks, &self.done_tx);
        locks.release_except(None);
    }

    /// The key of the photo folder `path` writes into, worked out on its
    /// first write and then kept.
    fn folder_key_of(&mut self, path: &Path) -> Option<crate::sidecar::FolderKey> {
        let dir = photo_folder(path)?;
        self.key_of_dir(dir)
    }

    /// The key of the folder `op` writes into, as [`Self::folder_key_of`].
    pub(super) fn op_folder_key(
        &mut self,
        path: &Path,
        op: &WriteOp,
    ) -> Option<crate::sidecar::FolderKey> {
        let dir = op_folder(path, op)?;
        self.key_of_dir(dir)
    }

    fn key_of_dir(&mut self, dir: &Path) -> Option<crate::sidecar::FolderKey> {
        if let Some(key) = self.folder_keys.get(dir) {
            return Some(key.clone());
        }
        let key = crate::sidecar::FolderKey::of(dir)?;
        self.folder_keys.insert(dir.to_path_buf(), key.clone());
        Some(key)
    }

    /// `dir` is being opened: its kept key may be from another volume
    /// mounted there before, so it is worked out again.
    pub(super) fn forget_folder_key(&mut self, dir: &Path) {
        self.folder_keys.remove(dir);
        self.folder_keys.remove(&spelled_as_parent(dir));
    }

    /// The key `dir` was just found to have, at open.
    pub(super) fn note_folder_key(&mut self, dir: &Path, key: crate::sidecar::FolderKey) {
        self.folder_keys.insert(spelled_as_parent(dir), key.clone());
        self.folder_keys.insert(dir.to_path_buf(), key);
    }

    fn sender(&mut self) -> Option<&Sender<(PathBuf, u64, WriteOp)>> {
        if self.tx.is_none() {
            let (tx, rx) = channel::<(PathBuf, u64, WriteOp)>();
            let done_tx = self.done_tx.clone();
            let locks = super::locks::SaveLocks::new(self.locks.clone());
            let parked = self.parked.clone();
            let spawned = std::thread::Builder::new()
                .name("folder-store-write".into())
                .spawn(move || write_loop(&rx, &done_tx, locks, &parked));
            match spawned {
                Ok(handle) => {
                    self.worker = Some(handle);
                    self.tx = Some(tx);
                }
                Err(e) => {
                    eprintln!("[folder store] could not start the sidecar writer: {e}");
                    return None;
                }
            }
        }
        self.tx.as_ref()
    }

    /// Drain finished writes, returning the failures for the caller to report.
    pub(super) fn pump(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.drain_completions(&mut errors);
        // With no writer thread, parked writes are tried here, as often as
        // the writer would.
        if self.tx.is_none() && self.inline_retry.elapsed() >= PARKED_RETRY {
            self.inline_retry = std::time::Instant::now();
            if !lock_parked(&self.parked).is_empty() {
                self.retry_parked_inline();
            }
            self.drain_completions(&mut errors);
        }
        errors
    }

    /// Wait for the queue to empty, or for `timeout` to elapse. Writes still
    /// queued at the deadline are reported as one error, since on quit they
    /// are lost.
    pub(super) fn flush_blocking(&mut self, timeout: std::time::Duration) -> Vec<String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut errors = Vec::new();
        self.drain_completions(&mut errors);
        while !self.pending.is_empty() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            // Without a writer thread, parked writes are tried here.
            let left = if self.tx.is_none() {
                self.retry_parked_inline();
                left.min(std::time::Duration::from_millis(100))
            } else {
                left
            };
            match self.done_rx.recv_timeout(left) {
                Ok((path, seq, err)) => {
                    self.retire(&path, seq);
                    if let Some(e) = err {
                        errors.push(e);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) if self.tx.is_none() => {}
                Err(_) => break,
            }
        }
        if !self.pending.is_empty() {
            errors.push(format!(
                "{} photos' sidecar writes did not finish within {} s",
                self.pending.len(),
                timeout.as_secs()
            ));
        }
        errors
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Writeback {
    fn drop(&mut self) {
        // Closing the channel makes the worker finish what is queued and then
        // exit, so dropping a `FolderStore` never loses an edit. The join is
        // deliberately unbounded, unlike the one on quit: an edit is worth more
        // than a prompt shutdown. The cost is that on an unresponsive volume
        // this hangs with the window already gone and nothing on screen to say
        // why.
        self.tx = None;
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

/// How often the writer tries again a write parked behind another
/// program's lock, while nothing else arrives.
#[cfg(not(target_arch = "wasm32"))]
const PARKED_RETRY: std::time::Duration = std::time::Duration::from_millis(500);

/// The writer thread: runs each write in order, under its folder's lock.
/// A write into a folder another program holds is parked with every later
/// write for that folder behind it, so the order stays, and all of them
/// are tried again whenever a write arrives, every [`PARKED_RETRY`], and
/// once more when the queue closes at quit. None is dropped while the app
/// runs (`docs/design/folder-lock.md`, rule 1).
#[cfg(not(target_arch = "wasm32"))]
fn write_loop(
    rx: &std::sync::mpsc::Receiver<(PathBuf, u64, WriteOp)>,
    done_tx: &Sender<Done>,
    mut locks: super::locks::SaveLocks,
    shared: &Parked,
) {
    use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
    loop {
        let waiting = !lock_parked(shared).is_empty();
        let next = match rx.try_recv() {
            Ok(next) => Some(next),
            Err(TryRecvError::Disconnected) => None,
            Err(TryRecvError::Empty) => {
                // Idle: a left folder's lock goes now, so the other
                // program can get in.
                locks.release_except(None);
                if waiting {
                    match rx.recv_timeout(PARKED_RETRY) {
                        Ok(next) => Some(next),
                        Err(RecvTimeoutError::Timeout) => {
                            retry_parked(&mut lock_parked(shared), &mut locks, done_tx);
                            continue;
                        }
                        Err(RecvTimeoutError::Disconnected) => None,
                    }
                } else {
                    rx.recv().ok()
                }
            }
        };
        let mut parked = lock_parked(shared);
        retry_parked(&mut parked, &mut locks, done_tx);
        let Some((path, seq, op)) = next else {
            drop(parked);
            locks.release_except(None);
            return;
        };
        if behind_parked(&parked, &path, &op) {
            parked.push_back((path, seq, op));
            continue;
        }
        match perform_locked(&mut locks, &path, &op) {
            Saved::Done(result) => {
                let _ = done_tx.send((path, seq, result.err()));
            }
            Saved::Parked => parked.push_back((path, seq, op)),
        }
    }
}

/// Writes waiting for a folder another program holds, shared by the
/// writer thread and the inline path.
#[cfg(not(target_arch = "wasm32"))]
type Parked = std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(PathBuf, u64, WriteOp)>>>;

#[cfg(not(target_arch = "wasm32"))]
fn lock_parked(
    parked: &Parked,
) -> std::sync::MutexGuard<'_, std::collections::VecDeque<(PathBuf, u64, WriteOp)>> {
    parked
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Whether a write for `path` must wait behind a parked one for its folder,
/// so writes to one folder keep their order.
#[cfg(not(target_arch = "wasm32"))]
fn behind_parked(
    parked: &std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    path: &Path,
    op: &WriteOp,
) -> bool {
    if parked.is_empty() {
        return false;
    }
    op_key(path, op).is_some_and(|key| {
        parked
            .iter()
            .any(|(p, _, o)| op_key(p, o).as_ref() == Some(&key))
    })
}

/// Try the parked writes again, in order; a folder still held keeps the
/// rest of its writes parked behind the first.
#[cfg(not(target_arch = "wasm32"))]
fn retry_parked(
    parked: &mut std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    locks: &mut super::locks::SaveLocks,
    done_tx: &Sender<Done>,
) {
    retry_parked_with(parked, done_tx, |path, op| perform_locked(locks, path, op));
}

/// [`retry_parked`] with the write given. Each write leaves the queue only
/// once it is done, so a panic in one loses none of the others.
#[cfg(not(target_arch = "wasm32"))]
fn retry_parked_with(
    parked: &mut std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    done_tx: &Sender<Done>,
    mut write: impl FnMut(&Path, &WriteOp) -> Saved,
) {
    let mut held: Vec<crate::sidecar::FolderKey> = Vec::new();
    let mut i = 0;
    while let Some((path, seq, op)) = parked.get(i) {
        let key = op_key(path, op);
        if key.as_ref().is_some_and(|k| held.contains(k)) {
            i += 1;
            continue;
        }
        match write(path, op) {
            Saved::Done(result) => {
                let _ = done_tx.send((path.clone(), *seq, result.err()));
                parked.remove(i);
            }
            Saved::Parked => {
                held.extend(key);
                i += 1;
            }
        }
    }
}

/// The photo folder `op` writes into: the photo's own, or, for a group
/// file under `<folder>/.lightphotos/groups/`, the folder it groups.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn op_folder<'p>(path: &'p Path, op: &WriteOp) -> Option<&'p Path> {
    match op {
        WriteOp::Put(_) | WriteOp::Patch(_) | WriteOp::Delete => path.parent(),
        WriteOp::PutGroup(_) | WriteOp::DeleteGroup => path.parent()?.parent()?.parent(),
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn op_key(path: &Path, op: &WriteOp) -> Option<crate::sidecar::FolderKey> {
    crate::sidecar::FolderKey::of(op_folder(path, op)?)
}

/// Run `op` under its folder's lock: see [`super::locks::SaveLocks`]. A
/// delete where nothing was ever saved locks nothing, and makes no
/// `.lightphotos/` to lock it; a folder that is gone isn't made again.
#[cfg(not(target_arch = "wasm32"))]
fn perform_locked(locks: &mut super::locks::SaveLocks, path: &Path, op: &WriteOp) -> Saved {
    let Some(dir) = op_folder(path, op) else {
        return Saved::Done(perform(path, op));
    };
    let deletes = matches!(op, WriteOp::Delete | WriteOp::DeleteGroup);
    if deletes && !dir.join(crate::sidecar::SIDECAR_DIR).is_dir() {
        return Saved::Done(perform(path, op));
    }
    let Some(key) = crate::sidecar::FolderKey::of(dir) else {
        return Saved::Done(Err(format!(
            "could not save {}: its folder is gone",
            path.display()
        )));
    };
    locks.write_in(&key, || perform(path, op))
}

#[cfg(not(target_arch = "wasm32"))]
impl Writeback {
    /// Whether a write into the folder `key`, a photo's or one of its
    /// groups', is still queued, running or parked.
    ///
    /// It reads only the counts kept as writes are queued and retired, so
    /// it never touches the disk.
    pub(super) fn writes_pending_in(&self, key: &crate::sidecar::FolderKey) -> bool {
        self.pending_folders.contains_key(key)
    }
}

/// `dir` as a photo path's parent spells it: no trailing separator and
/// no `.` parts.
#[cfg(not(target_arch = "wasm32"))]
fn spelled_as_parent(dir: &Path) -> PathBuf {
    dir.components().collect()
}

/// The photo folder a queued path writes into: a photo's parent, or for a
/// group file the folder above its `.lightphotos/groups/`.
#[cfg(not(target_arch = "wasm32"))]
fn photo_folder(path: &Path) -> Option<&Path> {
    let parent = path.parent()?;
    let groups = parent.file_name() == Some(std::ffi::OsStr::new(super::group_file::GROUPS_DIR))
        && parent.parent()?.file_name() == Some(std::ffi::OsStr::new(crate::sidecar::SIDECAR_DIR));
    if groups {
        parent.parent()?.parent()
    } else {
        Some(parent)
    }
}

#[cfg(not(target_arch = "wasm32"))]
use super::locks::Saved;

#[cfg(not(target_arch = "wasm32"))]
fn perform(path: &Path, op: &WriteOp) -> Result<(), String> {
    use crate::paths::{remove_if_present, write_atomic_in_dir};
    match op {
        WriteOp::Put(rec) => crate::sidecar::write_record(path, rec),
        WriteOp::Patch(patch) => {
            let mut rec = crate::sidecar::read_record(path)?;
            patch.apply(&mut rec);
            crate::sidecar::write_record(path, &rec)
        }
        WriteOp::Delete => crate::sidecar::delete_record(path),
        WriteOp::PutGroup(group) => write_atomic_in_dir(path, &super::group_file::to_bytes(group)?)
            .map_err(|e| e.to_string()),
        WriteOp::DeleteGroup => remove_if_present(path).map_err(|e| e.to_string()),
    }
}

#[cfg(target_arch = "wasm32")]
impl Writeback {
    /// Queue `op` against the folder handle it must land in. Capturing the
    /// handle here, rather than reading the folder store's current one at write
    /// time, is what lets a write outlive a folder switch.
    pub(super) fn enqueue(
        &mut self,
        path: &Path,
        op: WriteOp,
        dir_handle: web_sys::FileSystemDirectoryHandle,
    ) {
        let seq = self.accept(path, &op);
        self.queue
            .push_back((path.to_path_buf(), seq, op, dir_handle));
        self.admit();
    }

    /// Drain finished writes and start as many queued ones as the window
    /// allows, returning the failures for the caller to report.
    pub(super) fn pump(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.drain_completions(&mut errors);
        self.admit();
        errors
    }

    /// Spawn from the head of the queue while the window has room. A path
    /// already in flight stops the scan instead of being skipped, which is
    /// what keeps two writes to one photo in submission order.
    fn admit(&mut self) {
        while self.in_flight.len() < WEB_WRITE_WINDOW {
            let Some((path, _, _, _)) = self.queue.front() else {
                return;
            };
            if self.in_flight.contains(path) {
                return;
            }
            let Some((path, seq, op, dir_handle)) = self.queue.pop_front() else {
                return;
            };
            let Some(name) = path.file_name().map(std::ffi::OsString::from) else {
                let _ = self
                    .done_tx
                    .send((path, seq, Some("path has no file name".to_string())));
                continue;
            };
            // A photo's path names its sidecar; a group's path is already
            // the group file's own.
            let is_group = matches!(op, WriteOp::PutGroup(_) | WriteOp::DeleteGroup);
            let body = match &op {
                WriteOp::Put(rec) => serde_json::to_vec_pretty(rec)
                    .map(Some)
                    .map_err(|e| e.to_string()),
                WriteOp::PutGroup(group) => super::group_file::to_bytes(group).map(Some),
                WriteOp::Patch(_) | WriteOp::Delete | WriteOp::DeleteGroup => Ok(None),
            };
            let body = match body {
                Ok(body) => body,
                Err(e) => {
                    let _ = self.done_tx.send((path, seq, Some(e)));
                    continue;
                }
            };
            self.in_flight.insert(path.clone());
            let done_tx = self.done_tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                use crate::web::web_folder_store_fs as fs;
                let result = match (&op, is_group, body) {
                    (WriteOp::Patch(patch), _, _) => {
                        fs::update_sidecar(&dir_handle, &name, |rec| patch.apply(rec)).await
                    }
                    (_, false, Some(bytes)) => fs::write_sidecar(&dir_handle, &name, &bytes).await,
                    (_, false, None) => fs::delete_sidecar(&dir_handle, &name).await,
                    (_, true, Some(bytes)) => fs::write_group(&dir_handle, &name, &bytes).await,
                    (_, true, None) => fs::delete_group(&dir_handle, &name).await,
                };
                let err = result
                    .err()
                    .map(|e| format!("could not save {}: {e}", name.to_string_lossy()));
                let _ = done_tx.send((path, seq, err));
            });
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    fn new_wb() -> Writeback {
        Writeback::new(super::super::locks::SharedLocks::default())
    }

    #[test]
    fn a_later_write_to_one_path_supersedes_an_earlier_one_in_the_overlay() {
        let mut wb = new_wb();
        let mark = wb.begin_load();
        let path = PathBuf::from("/photos/a.jpg");
        wb.enqueue(
            &path,
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(1),
                ..Default::default()
            })),
        );
        wb.enqueue(
            &path,
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(4),
                ..Default::default()
            })),
        );

        let mut images = HashMap::new();
        wb.overlay(
            Path::new("/photos"),
            mark,
            &mut images,
            &mut Groups::default(),
        );
        assert_eq!(
            images
                .get(std::ffi::OsStr::new("a.jpg"))
                .and_then(|r| r.rating),
            Some(4),
            "the overlay must serve the newest queued snapshot, not the first"
        );
    }

    /// A `Patch` changes only the adjustments, whether it lands on a
    /// queued `Put` or on the record the load read.
    #[test]
    fn an_adjust_in_the_overlay_keeps_the_rest_of_the_record() {
        let exposed = Adjustments {
            exposure: 1.0,
            ..Default::default()
        };
        let mut wb = new_wb();
        let mark = wb.begin_load();
        let queued = PathBuf::from("/photos/a.jpg");
        wb.enqueue(
            &queued,
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(1),
                ..Default::default()
            })),
        );
        let patch = || {
            WriteOp::Patch(Box::new(Patch::Adjustments(Box::new(AdjustmentsChange {
                before: Adjustments::default(),
                after: exposed,
                at: 7,
            }))))
        };
        wb.enqueue(&queued, patch());
        wb.enqueue(Path::new("/photos/b.jpg"), patch());

        let mut images = HashMap::from([(
            OsString::from("b.jpg"),
            ImageRecord {
                rating: Some(2),
                ..Default::default()
            },
        )]);
        wb.overlay(
            Path::new("/photos"),
            mark,
            &mut images,
            &mut Groups::default(),
        );
        for (name, rating) in [("a.jpg", 1), ("b.jpg", 2)] {
            let rec = images.get(std::ffi::OsStr::new(name));
            assert_eq!(rec.and_then(|r| r.rating), Some(rating), "{name}");
            assert_eq!(rec.map(|r| r.adjustments), Some(exposed), "{name}");
        }
    }

    #[test]
    fn the_overlay_leaves_other_folders_alone() {
        let mut wb = new_wb();
        let mark = wb.begin_load();
        wb.enqueue(
            Path::new("/photos/a/x.jpg"),
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(5),
                ..Default::default()
            })),
        );

        let mut images = HashMap::new();
        wb.overlay(
            Path::new("/photos/b"),
            mark,
            &mut images,
            &mut Groups::default(),
        );
        assert!(
            images.is_empty(),
            "a queued write for one folder must not appear in another folder's cache"
        );
    }

    #[test]
    fn a_queued_delete_removes_a_loaded_record_from_the_overlay() {
        let mut wb = new_wb();
        let mark = wb.begin_load();
        wb.enqueue(Path::new("/photos/a.jpg"), WriteOp::Delete);

        let mut images = HashMap::new();
        images.insert(
            OsString::from("a.jpg"),
            ImageRecord {
                rating: Some(3),
                ..Default::default()
            },
        );
        wb.overlay(
            Path::new("/photos"),
            mark,
            &mut images,
            &mut Groups::default(),
        );
        assert!(
            images.is_empty(),
            "a record whose deletion is still queued must not come back from a stale load"
        );
    }

    /// The mark is what separates a load whose snapshot predates one of our
    /// writes from a later load whose snapshot already carries it. Both sides
    /// matter: shield too little and an edit is reverted, shield too much and
    /// a sidecar changed outside the app is ignored forever.
    #[test]
    fn the_mark_decides_which_loads_a_landed_write_shields() {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-writeback-test-{}", std::process::id()));
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        // An unapplied load keeps the write history alive for both marks.
        let older = wb.begin_load();
        wb.enqueue(
            &path,
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(2),
                ..Default::default()
            })),
        );
        wb.flush_blocking(std::time::Duration::from_secs(10));
        let newer = wb.begin_load();

        let mut images = HashMap::new();
        wb.overlay(&dir, newer, &mut images, &mut Groups::default());
        assert!(
            images.is_empty(),
            "a load requested after the write landed reads it off the disk, so \
             the overlay must stand aside and let an outside edit win"
        );

        let mut images = HashMap::new();
        wb.overlay(&dir, older, &mut images, &mut Groups::default());
        assert_eq!(
            images
                .get(std::ffi::OsStr::new("a.jpg"))
                .and_then(|r| r.rating),
            Some(2),
            "a load requested before the write landed may have missed it"
        );

        // The same directory spelled with a trailing separator, which is what
        // the parent test in `overlay` has to see through.
        let mut images = HashMap::new();
        wb.overlay(&dir.join(""), older, &mut images, &mut Groups::default());
        assert_eq!(
            images
                .get(std::ffi::OsStr::new("a.jpg"))
                .and_then(|r| r.rating),
            Some(2),
            "a trailing separator must not silently skip the overlay"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A flush that gives up with writes still queued must say so: on quit
    /// those edits are lost.
    #[test]
    fn a_flush_that_times_out_reports_the_unwritten_photos() {
        let mut wb = new_wb();
        // A worker that never reads, so the write stays queued.
        let (tx, _parked) = channel();
        wb.tx = Some(tx);
        wb.enqueue(Path::new("/photos/a.jpg"), WriteOp::Delete);
        let errors = wb.flush_blocking(std::time::Duration::ZERO);

        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("1 photos'"), "{errors:?}");
    }

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lightphotos-wb-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn rated(stars: u8) -> WriteOp {
        WriteOp::Put(Box::new(ImageRecord {
            rating: Some(stars),
            ..Default::default()
        }))
    }

    /// Review 3.7 #3: with no writer thread, a write into a folder another
    /// program holds is parked like any other, not written unlocked, and
    /// lands once the folder is free.
    #[test]
    fn an_inline_write_into_a_held_folder_parks_too() {
        use crate::sidecar::{FolderLock, Writer};
        let dir = temp("inline-park");
        let path = dir.join("a.jpg");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut wb = new_wb();
        let (tx, rx) = channel();
        drop(rx);
        wb.tx = Some(tx);
        wb.enqueue(&path, rated(3));
        let while_held = crate::sidecar::read_record(&path).unwrap().rating;
        drop(other);
        let errors = wb.flush_blocking(std::time::Duration::from_secs(5));
        let after = crate::sidecar::read_record(&path).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(while_held, None, "nothing written while held");
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(after, Some(3));
        assert_eq!(wb.backlog(), 0);
    }

    /// Review 3.7 #7: parked writes live outside the writer thread, so a
    /// writer that is gone leaves them for the inline path, and none is
    /// lost or left pending.
    #[test]
    fn parked_writes_outlive_their_writer() {
        let dir = temp("parked-outlive");
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        // As a writer thread leaves it when it stops: one write parked.
        let seq = wb.accept(&path, &rated(4));
        lock_parked(&wb.parked).push_back((path.clone(), seq, rated(4)));
        let errors = wb.flush_blocking(std::time::Duration::from_secs(5));
        let after = crate::sidecar::read_record(&path).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(after, Some(4));
        assert_eq!(wb.backlog(), 0);
    }

    /// Review 3.7 #8: a folder's pending count is kept as writes are queued,
    /// so it still holds when the folder can no longer be looked up (here
    /// renamed away), and settle never touches the disk.
    #[test]
    fn a_folders_pending_writes_are_counted_without_the_disk() {
        use crate::sidecar::{FolderKey, FolderLock, Writer};
        let dir = temp("pending-count");
        let key = FolderKey::of(&dir).unwrap();
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut wb = new_wb();
        wb.enqueue(&dir.join("a.jpg"), rated(2));
        let moved = dir.with_extension("moved");
        let _ = std::fs::remove_dir_all(&moved);
        std::fs::rename(&dir, &moved).unwrap();
        let pending = wb.writes_pending_in(&key);
        drop(other);
        let _ = std::fs::remove_dir_all(&moved);
        assert!(pending, "the parked write still counts for its folder");
    }

    /// Review 3.7 round 2 #5: a folder opened again is keyed afresh, so a
    /// volume mounted again at the same path (here the folder made again,
    /// which gives it a new inode) is not counted under the old key, and a
    /// folder with no key yet is looked up again rather than kept as none.
    #[test]
    fn a_folder_opened_again_is_keyed_afresh() {
        use crate::sidecar::FolderKey;
        let dir = temp("key-afresh");
        let photo = dir.join("a.jpg");
        let mut wb = new_wb();
        let first = wb.op_folder_key(&photo, &rated(1));
        std::fs::remove_dir_all(&dir).unwrap();
        let gone = wb.op_folder_key(&dir.with_extension("none").join("a.jpg"), &rated(1));
        std::fs::create_dir_all(&dir).unwrap();
        // Hold the old inode's slot so the new folder can't reuse it.
        let keep = temp("key-afresh-keep");
        wb.forget_folder_key(&dir);
        let again = wb.op_folder_key(&photo, &rated(1));
        let fresh = FolderKey::of(&dir);
        let missing = dir.with_extension("later");
        let _ = std::fs::remove_dir_all(&missing);
        let none_yet = wb.op_folder_key(&missing.join("a.jpg"), &rated(1));
        std::fs::create_dir_all(&missing).unwrap();
        let found = wb.op_folder_key(&missing.join("a.jpg"), &rated(1));
        for d in [&dir, &keep, &missing] {
            let _ = std::fs::remove_dir_all(d);
        }
        assert!(first.is_some() && gone.is_none());
        assert_eq!(again, fresh, "keyed afresh after the folder opened again");
        assert!(none_yet.is_none());
        assert!(found.is_some(), "a folder with no key is not kept as none");
    }

    /// Review 3.7 round 2 #5: a write is uncounted under the key it was
    /// counted under, even when the folder's key changed in between.
    #[test]
    fn a_write_is_uncounted_under_the_key_it_was_counted_under() {
        use crate::sidecar::FolderKey;
        let dir = temp("key-count");
        let photo = dir.join("a.jpg");
        let mut wb = new_wb();
        let old = FolderKey::of(&dir).unwrap();
        let seq = wb.accept(&photo, &rated(1));
        std::fs::remove_dir_all(&dir).unwrap();
        let keep = temp("key-count-keep");
        std::fs::create_dir_all(&dir).unwrap();
        wb.forget_folder_key(&dir);
        let _ = wb.op_folder_key(&photo, &rated(1));
        wb.retire(&photo, seq);
        let still = wb.writes_pending_in(&old);
        for d in [&dir, &keep] {
            let _ = std::fs::remove_dir_all(d);
        }
        assert!(!still, "the old key's count went back to zero");
    }

    /// Review 3.7 round 2 #7: with no writer thread, `pump` tries the
    /// parked writes again on the writer's schedule, not only on the next
    /// write or at quit.
    #[test]
    fn pump_retries_inline_parked_writes() {
        use crate::sidecar::{FolderLock, Writer};
        let dir = temp("inline-pump");
        let path = dir.join("a.jpg");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut wb = new_wb();
        let (tx, rx) = channel();
        drop(rx);
        wb.tx = Some(tx);
        wb.enqueue(&path, rated(3));
        drop(other);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while wb.backlog() > 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = wb.pump();
        }
        let after = crate::sidecar::read_record(&path).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(after, Some(3), "pump wrote the parked write");
        assert_eq!(wb.backlog(), 0);
    }

    /// Review 3.7 round 2 #8: a write that panics while the parked writes
    /// are tried again loses none of the others.
    #[test]
    fn a_panic_while_retrying_parked_writes_loses_none() {
        let dir = temp("parked-panic");
        let mut parked = std::collections::VecDeque::new();
        for (seq, name) in (1u64..).zip(["a.jpg", "b.jpg", "c.jpg"]) {
            parked.push_back((dir.join(name), seq, rated(1)));
        }
        let (done_tx, done_rx) = channel();
        let shared: Parked = std::sync::Arc::new(std::sync::Mutex::new(parked));
        let for_panic = shared.clone();
        let unwound = std::panic::catch_unwind(move || {
            let mut parked = lock_parked(&for_panic);
            retry_parked_with(&mut parked, &done_tx, |path, _| {
                if path.ends_with("b.jpg") {
                    panic!("a write that panics");
                }
                Saved::Done(Ok(()))
            });
        });
        let left: Vec<u64> = lock_parked(&shared)
            .iter()
            .map(|(_, seq, _)| *seq)
            .collect();
        let done: Vec<u64> = done_rx.try_iter().map(|(_, seq, _)| seq).collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(unwound.is_err());
        assert_eq!(done, vec![1], "the write before the panic is done");
        assert_eq!(
            left,
            vec![2, 3],
            "the panicking write and the rest stay parked"
        );
    }

    /// A worker that has died must not swallow the edits sent to it.
    #[test]
    fn a_write_to_a_dead_worker_still_reaches_the_disk() {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-writeback-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        let (tx, rx) = channel();
        drop(rx);
        wb.tx = Some(tx);
        wb.enqueue(
            &path,
            WriteOp::Put(Box::new(ImageRecord {
                rating: Some(3),
                ..Default::default()
            })),
        );
        let errors = wb.flush_blocking(std::time::Duration::from_secs(1));

        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(wb.backlog(), 0, "the write must retire its pending entry");
        assert_eq!(
            crate::sidecar::read_record(&path)
                .ok()
                .and_then(|r| r.rating),
            Some(3)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
