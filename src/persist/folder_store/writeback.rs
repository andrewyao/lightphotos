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

use crate::develop::{Adjustments, DetectTarget, Mask, RasterId, TouchUp};
use crate::persist::groups::{Group, GroupId, GroupWrite, Groups};
use crate::sidecar::{ColorLabel, Flag, ImageRecord, StoredScore};

/// One queued sidecar mutation.
///
/// Every photo write is a field change ([`Patch`]): it names the fields it
/// changes and is applied to the record read from disk when it runs, under
/// the folder's lock. No write carries a whole record, so a field the cache
/// doesn't know of (one a job result or another program wrote) survives
/// every edit. See `docs/design/folder-lock.md`, "Every write is a field
/// change".
///
/// `Patch` and `Delete` are queued under the photo's own path and derive its
/// sidecar from it. `PutGroup` and `DeleteGroup` are queued under the group
/// sidecar's path, since a group has no photo of its own; a group file is
/// the app's alone and is written whole.
#[derive(Clone)]
pub(super) enum WriteOp {
    Patch(Box<PatchOp>),
    Delete,
    PutGroup(Group),
    DeleteGroup,
}

/// A field change on its way to a photo's sidecar.
#[derive(Clone)]
pub(super) struct PatchOp {
    pub(super) patch: Patch,
    /// The cache's copy of the record before the change, if the cache held
    /// it: what the change goes onto if the sidecar on disk turns out not
    /// to be a record we can read (it is moved aside, never deleted).
    pub(super) fallback: Option<ImageRecord>,
}

impl WriteOp {
    pub(super) fn patch(patch: Patch, fallback: Option<ImageRecord>) -> WriteOp {
        WriteOp::Patch(Box::new(PatchOp { patch, fallback }))
    }
}

/// The fields of a record one write changes. Everything else in the
/// sidecar stays as the write finds it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Patch {
    /// The star rating; `None` clears it.
    Rating(Option<u8>),
    /// The pick or reject flag.
    Flag(Option<Flag>),
    /// The color label.
    Label(Option<ColorLabel>),
    /// A change to the adjustments: see [`AdjustmentsChange`].
    Adjustments(Box<AdjustmentsChange>),
    /// The spot removals, made at a Unix time.
    Touchups { touchups: Vec<TouchUp>, at: u64 },
    /// The masks this build reads, made at a Unix time; foreign ones stay.
    Masks { masks: Vec<Mask>, at: u64 },
    /// Which coverage answers one detected-mask target.
    Detection {
        target: DetectTarget,
        id: Option<RasterId>,
        at: u64,
    },
    /// The manual rotation, in quarter turns.
    Rotation { rotation: u8, at: u64 },
    /// The quality score.
    Score(StoredScore),
    /// The edit time, as tests set it.
    #[cfg(test)]
    Edited(u64),
    /// Tests: a change that can't apply.
    #[cfg(test)]
    Fail,
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
    pub(crate) fn apply(&self, onto: &Adjustments) -> Result<Adjustments, String> {
        changed_onto(&self.before, &self.after, onto)
    }
}

/// `onto` with every field that differs between `before` and `after` set to
/// `after`'s, compared on the JSON form, where a field left out is its
/// default. An error if that form can't be read back: writing `after` whole
/// instead would put stale sliders over newer ones.
pub(crate) fn changed_onto(
    before: &Adjustments,
    after: &Adjustments,
    onto: &Adjustments,
) -> Result<Adjustments, String> {
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
    serde_json::from_value(Value::Object(out))
        .map_err(|e| format!("the adjustments could not be merged: {e}"))
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

    /// Apply the change to `rec`, as the write does to the record on disk
    /// and the cache does to its copy.
    pub(super) fn apply(&self, rec: &mut ImageRecord) -> Result<(), String> {
        match self {
            Patch::Rating(rating) => rec.rating = *rating,
            Patch::Flag(flag) => rec.flag = *flag,
            Patch::Label(label) => rec.label = *label,
            Patch::Adjustments(change) => {
                let merged = change.apply(&rec.adjustments)?;
                rec.edit(change.at, |rec| rec.adjustments = merged);
            }
            Patch::Touchups { touchups, at } => {
                rec.edit(*at, |rec| rec.touchups = touchups.clone())
            }
            Patch::Masks { masks, at } => rec.edit(*at, |rec| rec.masks.set(masks.clone())),
            Patch::Detection { target, id, at } => {
                rec.edit(*at, |rec| rec.detections.set(*target, *id))
            }
            Patch::Rotation { rotation, at } => rec.edit(*at, |rec| rec.rotation = *rotation),
            Patch::Score(score) => rec.score = Some(score.clone()),
            #[cfg(test)]
            Patch::Edited(secs) => rec.edited = Some(*secs),
            #[cfg(test)]
            Patch::Fail => return Err("this change can't apply".to_string()),
        }
        Ok(())
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
/// - `authored` holds every op per path, in order, for as long as a load could
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
    authored: HashMap<PathBuf, std::collections::VecDeque<Authored>>,
    /// Loads requested and not yet applied or abandoned.
    loads: usize,
    /// The marks of those loads, counted: no op older than the oldest is
    /// ever replayed, so `authored` keeps none.
    load_marks: std::collections::BTreeMap<u64, usize>,
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
    /// Notices from browser writes (a sidecar kept aside), for the user.
    #[cfg(target_arch = "wasm32")]
    notice_tx: Sender<String>,
    #[cfg(target_arch = "wasm32")]
    notice_rx: Receiver<String>,
    done_rx: Receiver<Done>,

    /// `None` until the first write. Browsing a folder spawns no thread.
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<Sender<(PathBuf, u64, WriteOp)>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<std::thread::JoinHandle<()>>,
    /// The writer's end of `tx`, kept here too, so writes still in the
    /// channel when a writer dies are drained and done inline, not lost.
    #[cfg(not(target_arch = "wasm32"))]
    queue_rx: Option<SharedQueue>,
    /// Tests: run every write inline, as with no writer thread.
    #[cfg(test)]
    inline_only: bool,

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
        #[cfg(target_arch = "wasm32")]
        let (notice_tx, notice_rx) = channel();
        Writeback {
            pending: HashMap::new(),
            writes: 0,
            outstanding: std::collections::BTreeSet::new(),
            authored: HashMap::new(),
            loads: 0,
            load_marks: std::collections::BTreeMap::new(),
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
            #[cfg(target_arch = "wasm32")]
            notice_tx,
            #[cfg(target_arch = "wasm32")]
            notice_rx,
            done_rx,
            #[cfg(not(target_arch = "wasm32"))]
            tx: None,
            #[cfg(not(target_arch = "wasm32"))]
            worker: None,
            #[cfg(not(target_arch = "wasm32"))]
            queue_rx: None,
            #[cfg(test)]
            inline_only: false,
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
        let mark = self.outstanding.first().copied().unwrap_or(self.writes + 1);
        *self.load_marks.entry(mark).or_insert(0) += 1;
        LoadMark(mark)
    }

    /// Retire a load, whether its result arrived or it was never started.
    pub(super) fn end_load(&mut self, mark: LoadMark) {
        self.loads = self.loads.saturating_sub(1);
        if let Some(n) = self.load_marks.get_mut(&mark.0) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.load_marks.remove(&mark.0);
            }
        }
        // Keep only what a load still in flight, or a later one, could
        // replay: the queue itself, and ops at or after the oldest mark.
        let floor = self.replay_floor();
        let outstanding = &self.outstanding;
        self.authored.retain(|_, ops| {
            ops.retain(|a| a.seq >= floor || outstanding.contains(&a.seq));
            !ops.is_empty()
        });
    }

    /// The oldest op a load in flight may replay; past every op with none.
    fn replay_floor(&self) -> u64 {
        self.load_marks
            .keys()
            .next()
            .copied()
            .unwrap_or(self.writes.saturating_add(1))
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
        for (path, ops) in &self.authored {
            let ops: Vec<&Authored> = ops.iter().filter(|a| a.seq >= mark.0).collect();
            if ops.is_empty() {
                continue;
            }
            let photo = ops
                .iter()
                .all(|a| matches!(a.op, WriteOp::Patch(_) | WriteOp::Delete));
            if photo {
                if path.parent() != Some(dir) {
                    continue;
                }
                let Some(name) = path.file_name() else {
                    continue;
                };
                // Every change since the mark, in order, onto what the load
                // read: the disk ends up the same way.
                let mut rec = images.remove(name).unwrap_or_default();
                for a in ops {
                    match &a.op {
                        // A change that can't apply fails its write, which
                        // says so; the load's record stands.
                        WriteOp::Patch(op) => {
                            let _ = op.patch.apply(&mut rec);
                        }
                        WriteOp::Delete => rec = ImageRecord::default(),
                        WriteOp::PutGroup(_) | WriteOp::DeleteGroup => {}
                    }
                }
                if !rec.is_empty() {
                    images.insert(name.to_os_string(), rec);
                }
                continue;
            }
            if path.parent() != Some(groups_dir.as_path()) {
                continue;
            }
            let Some(id) = path.file_stem().and_then(GroupId::from_stem) else {
                continue;
            };
            for a in ops {
                match &a.op {
                    WriteOp::PutGroup(group) => {
                        group_writes.push((a.seq, GroupWrite::Put(id.clone(), group.clone())))
                    }
                    WriteOp::DeleteGroup => {
                        group_writes.push((a.seq, GroupWrite::Delete(id.clone())))
                    }
                    WriteOp::Patch(_) | WriteOp::Delete => {}
                }
            }
        }
        group_writes.sort_by_key(|(seq, _)| *seq);
        for (_, write) in &group_writes {
            groups.apply(write);
        }
    }

    /// Take the next sequence number and record the op against `path`, after
    /// every earlier one still remembered for it.
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
        self.authored
            .entry(path.to_path_buf())
            .or_default()
            .push_back(Authored {
                seq,
                op: op.clone(),
            });
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
            }
        }
        // This op has landed; no load in flight replays one older than its
        // mark, so the path's history shrinks as its writes land.
        let floor = self.replay_floor();
        let outstanding = &self.outstanding;
        if let Some(ops) = self.authored.get_mut(path) {
            // Ops are in seq order: the landed one goes if no load needs it,
            // and so does every landed one before the floor at the front.
            if seq < floor {
                if let Ok(i) = ops.binary_search_by_key(&seq, |a| a.seq) {
                    ops.remove(i);
                }
            }
            while ops
                .front()
                .is_some_and(|a| a.seq < floor && !outstanding.contains(&a.seq))
            {
                ops.pop_front();
            }
            if ops.is_empty() {
                self.authored.remove(path);
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
        self.reap_dead_writer();
        match self.sender() {
            // The store holds the receiver too (`queue_rx`), so the send
            // can't fail; a writer that dies leaves it for `reap_dead_writer`.
            Some(tx) => {
                let _ = tx.send((path.to_path_buf(), seq, op));
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
        if !behind_parked(&parked, path) {
            match guarded(path, || perform_locked(&mut locks, path, op)) {
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

    /// A writer thread that has ended (it should not, short of a panic)
    /// is let go, so the inline path takes over its parked writes and the
    /// next write starts a fresh one.
    fn reap_dead_writer(&mut self) {
        if self.tx.is_none() {
            return;
        }
        if self.worker.as_ref().is_some_and(|h| !h.is_finished()) {
            return;
        }
        self.tx = None;
        if let Some(handle) = self.worker.take() {
            if handle.join().is_err() {
                eprintln!("[folder store] the sidecar writer stopped; writing inline");
            }
        }
        // What the dead writer never took is done here, in order, so every
        // accepted write is performed or parked.
        let Some(queue) = self.queue_rx.take() else {
            return;
        };
        let left: Vec<(PathBuf, u64, WriteOp)> = queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .try_iter()
            .collect();
        for (path, seq, op) in left {
            self.perform_inline(&path, seq, &op);
        }
    }

    fn sender(&mut self) -> Option<&Sender<(PathBuf, u64, WriteOp)>> {
        #[cfg(test)]
        if self.inline_only {
            return None;
        }
        if self.tx.is_none() {
            let (tx, rx) = channel::<(PathBuf, u64, WriteOp)>();
            let queue: SharedQueue = std::sync::Arc::new(std::sync::Mutex::new(rx));
            let for_writer = queue.clone();
            let done_tx = self.done_tx.clone();
            let locks = super::locks::SaveLocks::new(self.locks.clone());
            let parked = self.parked.clone();
            let spawned = std::thread::Builder::new()
                .name("folder-store-write".into())
                .spawn(move || {
                    // Held for the writer's life; a writer that dies lets go,
                    // and the store drains what is left.
                    let rx = for_writer
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    write_loop(&rx, &done_tx, locks, &parked);
                });
            match spawned {
                Ok(handle) => {
                    self.worker = Some(handle);
                    self.tx = Some(tx);
                    self.queue_rx = Some(queue);
                }
                Err(e) => {
                    eprintln!("[folder store] could not start the sidecar writer: {e}");
                    return None;
                }
            }
        }
        self.tx.as_ref()
    }

    /// What the writes have to tell the user that isn't a failure: a
    /// sidecar kept aside, a lock or file still being tried.
    pub(super) fn take_notices(&mut self) -> Vec<String> {
        super::locks::take_notices(&self.locks)
    }

    /// Drain finished writes, returning the failures for the caller to report.
    pub(super) fn pump(&mut self) -> Vec<String> {
        let mut errors = Vec::new();
        self.drain_completions(&mut errors);
        self.reap_dead_writer();
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
        let started = std::time::Instant::now();
        while !self.pending.is_empty() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            // Everything left is parked (another program holds its folder,
            // or its lock can't be made just now): after a round of tries,
            // say so rather than wait out the timeout.
            // `try_lock`: the writer can hold the parked writes across a
            // write, and the deadline must hold.
            let parked_now = match self.parked.try_lock() {
                Ok(parked) => Some(parked.len()),
                Err(std::sync::TryLockError::Poisoned(p)) => Some(p.into_inner().len()),
                Err(std::sync::TryLockError::WouldBlock) => None,
            };
            if started.elapsed() >= PARKED_RETRY * 3
                && parked_now.is_some_and(|n| self.outstanding.len() <= n)
            {
                errors.push(format!(
                    "{} photos' changes wait for a folder another program holds, or one that can't be locked now; they were not saved",
                    self.pending.len()
                ));
                return errors;
            }
            // A writer that has died is noticed here, and its parked
            // writes are then tried inline, so nothing waits on it.
            self.reap_dead_writer();
            if self.tx.is_none() {
                self.retry_parked_inline();
            }
            match self
                .done_rx
                .recv_timeout(left.min(std::time::Duration::from_millis(100)))
            {
                Ok((path, seq, err)) => {
                    self.retire(&path, seq);
                    if let Some(e) = err {
                        errors.push(e);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
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
/// write for that photo behind it, so a photo's writes keep their order and
/// other photos' writes go on, and all of them
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
        if behind_parked(&parked, &path) {
            parked.push_back((path, seq, op));
            continue;
        }
        match guarded(&path, || perform_locked(&mut locks, &path, &op)) {
            Saved::Done(result) => {
                let _ = done_tx.send((path, seq, result.err()));
            }
            Saved::Parked => parked.push_back((path, seq, op)),
        }
    }
}

/// The receiving end of the writer's queue, shared with the store.
#[cfg(not(target_arch = "wasm32"))]
type SharedQueue =
    std::sync::Arc<std::sync::Mutex<std::sync::mpsc::Receiver<(PathBuf, u64, WriteOp)>>>;

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

/// Whether a write for `path` must wait behind a parked one for the same
/// path, so one photo's (or one group file's) writes keep their order.
/// Order across paths doesn't matter, so other photos' writes go on.
#[cfg(not(target_arch = "wasm32"))]
fn behind_parked(
    parked: &std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    path: &Path,
) -> bool {
    parked.iter().any(|(p, _, _)| p == path)
}

/// Try the parked writes again, in order; a path still parked keeps the
/// rest of its writes parked behind the first.
#[cfg(not(target_arch = "wasm32"))]
fn retry_parked(
    parked: &mut std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    locks: &mut super::locks::SaveLocks,
    done_tx: &Sender<Done>,
) {
    retry_parked_with(parked, done_tx, |path, op| perform_locked(locks, path, op));
}

/// A write that panics (it should not) fails with an error the user sees,
/// rather than end the writer thread, or stay parked first and end every
/// writer after it.
#[cfg(not(target_arch = "wasm32"))]
fn guarded(path: &Path, write: impl FnOnce() -> Saved) -> Saved {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(write)) {
        Ok(saved) => saved,
        Err(_) => Saved::Done(Err(format!(
            "could not save {}: the write stopped unexpectedly",
            path.display()
        ))),
    }
}

/// [`retry_parked`] with the write given. Each write leaves the queue only
/// once it is done; one that panics fails with an error ([`guarded`]).
#[cfg(not(target_arch = "wasm32"))]
fn retry_parked_with(
    parked: &mut std::collections::VecDeque<(PathBuf, u64, WriteOp)>,
    done_tx: &Sender<Done>,
    mut write: impl FnMut(&Path, &WriteOp) -> Saved,
) {
    let mut held: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut i = 0;
    while let Some((path, seq, op)) = parked.get(i) {
        if held.contains(path) {
            i += 1;
            continue;
        }
        match guarded(path, || write(path, op)) {
            Saved::Done(result) => {
                let _ = done_tx.send((path.clone(), *seq, result.err()));
                parked.remove(i);
            }
            Saved::Parked => {
                held.insert(path.clone());
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
        WriteOp::Patch(_) | WriteOp::Delete => path.parent(),
        WriteOp::PutGroup(_) | WriteOp::DeleteGroup => path.parent()?.parent()?.parent(),
    }
}

/// Run `op` under its folder's lock: see [`super::locks::SaveLocks`]. A
/// delete where nothing was ever saved locks nothing, and makes no
/// `.lightphotos/` to lock it; a folder that is gone isn't made again.
#[cfg(not(target_arch = "wasm32"))]
fn perform_locked(locks: &mut super::locks::SaveLocks, path: &Path, op: &WriteOp) -> Saved {
    let shared = locks.shared();
    let Some(dir) = op_folder(path, op) else {
        return perform(&shared, path, op);
    };
    let deletes = matches!(op, WriteOp::Delete | WriteOp::DeleteGroup);
    if deletes && !dir.join(crate::sidecar::SIDECAR_DIR).is_dir() {
        return perform(&shared, path, op);
    }
    let Some(key) = locks.key_of(dir) else {
        return Saved::Done(Err(format!(
            "could not save {}: its folder is gone",
            path.display()
        )));
    };
    locks.write_in(&key, || perform(&shared, path, op))
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

/// Do `op`. A photo's change is applied to the record read from disk and
/// written; a change that can't apply fails the write, with the reason.
///
/// Reading the sidecar can fail three ways. An I/O error that may pass parks
/// the write, to be tried again; one that won't (no permission, not a file)
/// fails it. A file that read but is not a record we understand (it doesn't
/// parse, or is over the size cap) is moved aside to
/// `<sidecar>.unreadable-<n>`, never deleted, and the change goes onto the
/// cache's copy of the record, if the change carries one; without one the
/// write fails and the file is left alone. See [`replace_unreadable`].
#[cfg(not(target_arch = "wasm32"))]
fn perform(shared: &super::locks::SharedLocks, path: &Path, op: &WriteOp) -> Saved {
    use crate::paths::{remove_if_present, write_atomic_in_dir};
    let op = match op {
        WriteOp::Patch(op) => op,
        WriteOp::Delete => return Saved::Done(crate::sidecar::delete_record(path)),
        WriteOp::PutGroup(group) => {
            return Saved::Done(
                super::group_file::to_bytes(group)
                    .and_then(|b| write_atomic_in_dir(path, &b).map_err(|e| e.to_string())),
            )
        }
        WriteOp::DeleteGroup => {
            return Saved::Done(remove_if_present(path).map_err(|e| e.to_string()))
        }
    };
    let key = path.display().to_string();
    match crate::sidecar::read_record_checked(path) {
        Ok(mut rec) => {
            super::locks::settled(shared, &key);
            Saved::Done(
                op.patch
                    .apply(&mut rec)
                    .and_then(|()| crate::sidecar::write_record(path, &rec)),
            )
        }
        Err(crate::sidecar::ReadError::Io { why, lasting: true }) => {
            super::locks::settled(shared, &key);
            Saved::Done(Err(format!("could not save {}: {why}", path.display())))
        }
        Err(crate::sidecar::ReadError::Io {
            why,
            lasting: false,
        }) => {
            super::locks::transient(shared, &key, || {
                format!(
                    "could not read {} to save it ({why}); still trying",
                    path.display()
                )
            });
            Saved::Parked
        }
        Err(crate::sidecar::ReadError::Unreadable(why)) => {
            super::locks::settled(shared, &key);
            Saved::Done(replace_unreadable(shared, path, op, &why))
        }
    }
}

/// `path`'s sidecar read, but is not a record (`why`). With the cache's
/// copy in the change, the new record (the change onto that copy) is
/// written to a temp file first, then the bad file is moved aside to the
/// first free `<sidecar>.unreadable-<n>`, then the temp file takes its
/// place. A failure at any step leaves the old sidecar or the new one,
/// never neither. Without a copy, nothing is touched and the write fails.
#[cfg(not(target_arch = "wasm32"))]
fn replace_unreadable(
    shared: &super::locks::SharedLocks,
    path: &Path,
    op: &PatchOp,
    why: &str,
) -> Result<(), String> {
    let name = |p: &Path| {
        p.file_name().map_or_else(
            || p.display().to_string(),
            |n| n.to_string_lossy().into_owned(),
        )
    };
    let Some(mut rec) = op.fallback.clone() else {
        return Err(format!(
            "could not save {}: its sidecar could not be read ({why}) and was left as it is",
            name(path)
        ));
    };
    op.patch.apply(&mut rec)?;
    let sidecar = crate::sidecar::sidecar_path(path).ok_or("cannot determine sidecar path")?;
    let aside = (1..=1000u32)
        .map(|n| {
            let mut aside = sidecar.as_os_str().to_os_string();
            aside.push(format!(".unreadable-{n}"));
            PathBuf::from(aside)
        })
        .find(|aside| !aside.exists())
        .ok_or_else(|| format!("could not keep {} aside: no free name", sidecar.display()))?;
    let bytes = serde_json::to_vec_pretty(&rec).map_err(|e| e.to_string())?;
    crate::paths::write_atomic_then(&sidecar, &bytes, || std::fs::rename(&sidecar, &aside))
        .map_err(|e| format!("could not save {}: {e}", name(path)))?;
    super::locks::notice(
        shared,
        format!(
            "{}'s sidecar could not be read ({why}); it was kept as {} and saved anew",
            name(path),
            name(&aside)
        ),
    );
    Ok(())
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

    /// What the writes have to tell the user that isn't a failure.
    pub(super) fn take_notices(&mut self) -> Vec<String> {
        self.notice_rx.try_iter().collect()
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
            let notice_tx = self.notice_tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                use crate::web::web_folder_store_fs as fs;
                let result = match (&op, is_group, body) {
                    (WriteOp::Patch(op), _, _) => {
                        let fallback = op.fallback.clone();
                        match fs::update_sidecar(&dir_handle, &name, fallback, |rec| {
                            op.patch.apply(rec)
                        })
                        .await
                        {
                            Ok(Some(notice)) => {
                                let _ = notice_tx.send(notice);
                                Ok(())
                            }
                            Ok(None) => Ok(()),
                            Err(e) => Err(e),
                        }
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
        wb.enqueue(&path, rated(1));
        wb.enqueue(&path, rated(4));

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
        wb.enqueue(&queued, rated(1));
        let patch = || {
            WriteOp::patch(
                Patch::Adjustments(Box::new(AdjustmentsChange {
                    before: Adjustments::default(),
                    after: exposed,
                    at: 7,
                })),
                None,
            )
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
        wb.enqueue(Path::new("/photos/a/x.jpg"), rated(5));

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
        wb.enqueue(&path, rated(2));
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
        WriteOp::patch(Patch::Rating(Some(stars)), None)
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
        wb.inline_only = true;
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
        let first = wb.folder_key_of(&photo);
        std::fs::remove_dir_all(&dir).unwrap();
        let gone = wb.folder_key_of(&dir.with_extension("none").join("a.jpg"));
        std::fs::create_dir_all(&dir).unwrap();
        // Hold the old inode's slot so the new folder can't reuse it.
        let keep = temp("key-afresh-keep");
        wb.forget_folder_key(&dir);
        let again = wb.folder_key_of(&photo);
        let fresh = FolderKey::of(&dir);
        let missing = dir.with_extension("later");
        let _ = std::fs::remove_dir_all(&missing);
        let none_yet = wb.folder_key_of(&missing.join("a.jpg"));
        std::fs::create_dir_all(&missing).unwrap();
        let found = wb.folder_key_of(&missing.join("a.jpg"));
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
        let _ = wb.folder_key_of(&photo);
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
        wb.inline_only = true;
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

    /// Review 3.7 round 2 #8 and round 3 #5: a write that panics while
    /// the parked writes are tried again fails with an error, and neither
    /// stays first nor takes the others with it.
    #[test]
    fn a_parked_write_that_panics_fails_alone_with_an_error() {
        let dir = temp("parked-panic");
        let mut parked = std::collections::VecDeque::new();
        for (seq, name) in (1u64..).zip(["a.jpg", "b.jpg", "c.jpg"]) {
            parked.push_back((dir.join(name), seq, rated(1)));
        }
        let (done_tx, done_rx) = channel();
        retry_parked_with(&mut parked, &done_tx, |path, _| {
            if path.ends_with("b.jpg") {
                panic!("a write that panics");
            }
            Saved::Done(Ok(()))
        });
        let done: Vec<(u64, bool)> = done_rx
            .try_iter()
            .map(|(_, seq, err)| (seq, err.is_some()))
            .collect();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(done, vec![(1, false), (2, true), (3, false)]);
        assert!(parked.is_empty(), "nothing stays parked behind it");
    }

    /// Review 3.7 round 3 #6: a writer thread that has died with writes
    /// parked is noticed by `pump`, which then lands them inline.
    #[test]
    fn a_dead_writers_parked_writes_land_from_pump() {
        let dir = temp("dead-writer-pump");
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        let (tx, rx) = channel::<(PathBuf, u64, WriteOp)>();
        let worker = std::thread::spawn(move || drop(rx));
        while !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        wb.tx = Some(tx);
        wb.worker = Some(worker);
        let seq = wb.accept(&path, &rated(4));
        lock_parked(&wb.parked).push_back((path.clone(), seq, rated(4)));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while wb.backlog() > 0 && std::time::Instant::now() < deadline {
            let _ = wb.pump();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let after = crate::sidecar::read_record(&path).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(after, Some(4));
        assert_eq!(wb.backlog(), 0);
    }

    /// With no writer thread, an edit is still written, inline.
    #[test]
    fn a_write_with_no_writer_thread_still_reaches_the_disk() {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-writeback-dead-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        wb.inline_only = true;
        wb.enqueue(&path, rated(3));
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

    /// Review 3.7 round 4: writes still in the channel when the writer dies
    /// are drained and done inline, not dropped with it.
    #[test]
    fn writes_left_in_a_dead_writers_channel_are_done() {
        let dir = temp("dead-channel");
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        let (tx, rx) = channel::<(PathBuf, u64, WriteOp)>();
        let queue: SharedQueue = std::sync::Arc::new(std::sync::Mutex::new(rx));
        let worker = std::thread::spawn(|| {});
        while !worker.is_finished() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let seq = wb.accept(&path, &rated(5));
        tx.send((path.clone(), seq, rated(5))).unwrap();
        wb.tx = Some(tx);
        wb.worker = Some(worker);
        wb.queue_rx = Some(queue);
        let errors = wb.flush_blocking(std::time::Duration::from_secs(3));
        let after = crate::sidecar::read_record(&path).unwrap().rating;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(errors.is_empty(), "{errors:?}");
        assert_eq!(after, Some(5));
        assert_eq!(wb.backlog(), 0);
    }

    /// Review 3.7 round 5 #2: at quit, writes only parked behind another
    /// program's lock are reported at once, not waited for to the timeout.
    #[test]
    fn the_quit_flush_does_not_wait_out_parked_writes() {
        use crate::sidecar::{FolderLock, Writer};
        let dir = temp("flush-parked");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut wb = new_wb();
        wb.enqueue(&dir.join("a.jpg"), rated(2));
        let started = std::time::Instant::now();
        let errors = wb.flush_blocking(std::time::Duration::from_secs(10));
        let took = started.elapsed();
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(took < std::time::Duration::from_secs(4), "waited {took:?}");
        assert!(
            errors.iter().any(|e| e.contains("another program")),
            "{errors:?}"
        );
    }

    /// Review 3.7 round 5 #7: the history kept for loads shrinks as writes
    /// land: nothing older than the oldest load in flight, nothing at all
    /// with none.
    #[test]
    fn the_write_history_is_trimmed_to_what_a_load_can_replay() {
        let dir = temp("authored-trim");
        let path = dir.join("a.jpg");
        let mut wb = new_wb();
        for stars in 1..=5u8 {
            wb.enqueue(&path, rated(stars));
        }
        let _ = wb.flush_blocking(std::time::Duration::from_secs(5));
        let none_in_flight = wb.authored.len();
        let mark = wb.begin_load();
        for stars in 1..=5u8 {
            wb.enqueue(&path, rated(stars));
        }
        let _ = wb.flush_blocking(std::time::Duration::from_secs(5));
        let kept_for_load = wb.authored.get(&path).map_or(0, |ops| ops.len());
        wb.end_load(mark);
        let after_load = wb.authored.len();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(none_in_flight, 0);
        assert_eq!(kept_for_load, 5, "what the load in flight may replay");
        assert_eq!(after_load, 0);
    }
}
