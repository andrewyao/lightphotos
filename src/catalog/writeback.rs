// SPDX-License-Identifier: GPL-3.0-or-later

//! Sidecar I/O, off the frame.
//!
//! A sidecar write is a `create_dir_all`, a serialize, a temp file and a
//! rename: around 80-120 µs on APFS. Rating a selection of 20 000 photos does
//! that 20 000 times, so doing it inline froze the window for seconds. Every
//! [`Catalog`](super::Catalog) mutation now hands the work to this queue and
//! returns.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};

use super::ImageRecord;

/// One queued sidecar mutation for one photo.
///
/// The payload is a whole-record snapshot rather than a delta, because a
/// `Catalog` mutation is already a read-modify-write of the entire
/// [`ImageRecord`]. A later entry for a path therefore supersedes every
/// earlier one, which is what lets [`Writeback::overlay`] answer a read with a
/// single value and lets wasm admit one task per path without losing a field.
#[derive(Clone)]
pub(super) enum WriteOp {
    Put(ImageRecord),
    Delete,
}

/// A photo whose sidecar work has not reached the disk yet.
struct Pending {
    /// Queued or in-flight operations, counted so the entry retires only when
    /// the last of them reports.
    inflight: u32,
    /// The newest snapshot for this path.
    latest: WriteOp,
}

/// A finished operation: the path it was for, and why it failed.
type Done = (PathBuf, Option<String>);

/// Concurrent File System Access writes.
///
/// Not a memory budget, since a record serializes to a few hundred bytes. It
/// caps how many writes the browser has open at once, because the old code
/// detached one task per mutation and a bulk rate could hand the browser
/// 20 000 of them.
#[cfg(target_arch = "wasm32")]
const WEB_WRITE_WINDOW: usize = 32;

/// Sidecar writes and deletes that have left [`Catalog`](super::Catalog) but
/// not yet reached the disk.
///
/// # Invariants
///
/// - An entry carries the photo's own path, not just its file name, so a
///   folder switch cannot misroute a queued write into a same-named photo in
///   the new folder. `Catalog` keys its cache by file name; this queue does
///   not.
/// - Operations on one path complete in submission order. Native gets that
///   from a single FIFO worker, wasm from admitting at most one task per path.
/// - `pending` is exact: a path is in it from `enqueue` until its completion
///   is drained, which is what makes [`Writeback::overlay`] correct.
pub(super) struct Writeback {
    pending: HashMap<PathBuf, Pending>,
    done_tx: Sender<Done>,
    done_rx: Receiver<Done>,

    /// `None` until the first write. Browsing a folder spawns no thread.
    #[cfg(not(target_arch = "wasm32"))]
    tx: Option<Sender<(PathBuf, WriteOp)>>,
    #[cfg(not(target_arch = "wasm32"))]
    worker: Option<std::thread::JoinHandle<()>>,

    /// Accepted but not yet spawned. wasm has no worker thread, so `pump` is
    /// the scheduler.
    #[cfg(target_arch = "wasm32")]
    queue: std::collections::VecDeque<(PathBuf, WriteOp, web_sys::FileSystemDirectoryHandle)>,
    #[cfg(target_arch = "wasm32")]
    in_flight: std::collections::HashSet<PathBuf>,
}

impl Writeback {
    pub(super) fn new() -> Writeback {
        let (done_tx, done_rx) = channel();
        Writeback {
            pending: HashMap::new(),
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

    /// Serve the newest queued snapshot for every path under `dir` on top of
    /// `images`.
    ///
    /// A background load's snapshot predates anything still queued here, so
    /// without this a folder round-trip during a long flush would put the
    /// pre-edit record back into the cache, and `reconcile_catalog_mirrors`
    /// would then drop the user's change from the grid.
    pub(super) fn overlay(&self, dir: &Path, images: &mut HashMap<OsString, ImageRecord>) {
        for (path, pending) in &self.pending {
            if path.parent() != Some(dir) {
                continue;
            }
            let Some(name) = path.file_name() else {
                continue;
            };
            match &pending.latest {
                WriteOp::Put(rec) => {
                    images.insert(name.to_os_string(), rec.clone());
                }
                WriteOp::Delete => {
                    images.remove(name);
                }
            }
        }
    }

    fn accept(&mut self, path: &Path, op: &WriteOp) {
        match self.pending.get_mut(path) {
            Some(entry) => {
                entry.inflight += 1;
                entry.latest = op.clone();
            }
            None => {
                self.pending.insert(
                    path.to_path_buf(),
                    Pending {
                        inflight: 1,
                        latest: op.clone(),
                    },
                );
            }
        }
    }

    fn retire(&mut self, path: &Path) {
        if let Some(entry) = self.pending.get_mut(path) {
            entry.inflight -= 1;
            if entry.inflight == 0 {
                self.pending.remove(path);
            }
        }
    }

    fn drain_completions(&mut self, errors: &mut Vec<String>) {
        while let Ok((path, err)) = self.done_rx.try_recv() {
            #[cfg(target_arch = "wasm32")]
            self.in_flight.remove(&path);
            self.retire(&path);
            if let Some(e) = err {
                errors.push(e);
            }
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Writeback {
    pub(super) fn enqueue(&mut self, path: &Path, op: WriteOp) {
        self.accept(path, &op);
        match self.sender() {
            Some(tx) => {
                let _ = tx.send((path.to_path_buf(), op));
            }
            // Without a worker the write still has to happen, or the edit is
            // lost with no way for the user to find out.
            None => {
                let err = perform(path, &op).err();
                let _ = self.done_tx.send((path.to_path_buf(), err));
            }
        }
    }

    fn sender(&mut self) -> Option<&Sender<(PathBuf, WriteOp)>> {
        if self.tx.is_none() {
            let (tx, rx) = channel::<(PathBuf, WriteOp)>();
            let done_tx = self.done_tx.clone();
            let spawned = std::thread::Builder::new()
                .name("catalog-write".into())
                .spawn(move || {
                    while let Ok((path, op)) = rx.recv() {
                        let err = perform(&path, &op).err();
                        let _ = done_tx.send((path, err));
                    }
                });
            match spawned {
                Ok(handle) => {
                    self.worker = Some(handle);
                    self.tx = Some(tx);
                }
                Err(e) => {
                    eprintln!("[catalog] could not start the sidecar writer: {e}");
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
        errors
    }

    /// Wait for the queue to empty, or for `timeout` to elapse.
    pub(super) fn flush_blocking(&mut self, timeout: std::time::Duration) -> Vec<String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut errors = Vec::new();
        self.drain_completions(&mut errors);
        while !self.pending.is_empty() {
            let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) else {
                break;
            };
            match self.done_rx.recv_timeout(left) {
                Ok((path, err)) => {
                    self.retire(&path);
                    if let Some(e) = err {
                        errors.push(e);
                    }
                }
                Err(_) => break,
            }
        }
        errors
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Drop for Writeback {
    fn drop(&mut self) {
        // Closing the channel makes the worker finish what is queued and then
        // exit, so dropping a `Catalog` never loses an edit.
        self.tx = None;
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn perform(path: &Path, op: &WriteOp) -> Result<(), String> {
    match op {
        WriteOp::Put(rec) => {
            let sidecar =
                sidecar_path(path).ok_or_else(|| "cannot determine sidecar path".to_string())?;
            write_sidecar_file(&sidecar, rec)
        }
        WriteOp::Delete => {
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
}

/// `<dir>/.lightphotos/<filename>.xmp`, or `None` for a path like `/` or `..`.
/// Built from `OsString` so non-UTF-8 names stay exact.
#[cfg(not(target_arch = "wasm32"))]
fn sidecar_path(path: &Path) -> Option<PathBuf> {
    let dir = path.parent()?;
    let name = path.file_name()?;
    let mut sidecar_name = name.to_os_string();
    sidecar_name.push(".");
    sidecar_name.push(super::SIDECAR_EXT);
    Some(dir.join(super::SIDECAR_DIR).join(sidecar_name))
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
    let tmp = sidecar.with_extension(format!("{}.tmp", super::SIDECAR_EXT));
    std::fs::write(&tmp, &bytes).map_err(|e| e.to_string())?;
    if let Err(e) = std::fs::rename(&tmp, sidecar) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

#[cfg(target_arch = "wasm32")]
impl Writeback {
    /// Queue `op` against the folder handle it must land in. Capturing the
    /// handle here, rather than reading the catalog's current one at write
    /// time, is what lets a write outlive a folder switch.
    pub(super) fn enqueue(
        &mut self,
        path: &Path,
        op: WriteOp,
        dir_handle: web_sys::FileSystemDirectoryHandle,
    ) {
        self.accept(path, &op);
        self.queue.push_back((path.to_path_buf(), op, dir_handle));
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
            let Some((path, _, _)) = self.queue.front() else {
                return;
            };
            if self.in_flight.contains(path) {
                return;
            }
            let (path, op, dir_handle) = self.queue.pop_front().unwrap();
            let Some(name) = path.file_name().map(std::ffi::OsString::from) else {
                let _ = self
                    .done_tx
                    .send((path, Some("path has no file name".to_string())));
                continue;
            };
            let body = match &op {
                WriteOp::Put(rec) => match serde_json::to_vec_pretty(rec) {
                    Ok(bytes) => Some(bytes),
                    Err(e) => {
                        let _ = self.done_tx.send((path, Some(e.to_string())));
                        continue;
                    }
                },
                WriteOp::Delete => None,
            };
            self.in_flight.insert(path.clone());
            let done_tx = self.done_tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let result = match body {
                    Some(bytes) => {
                        crate::web_catalog_fs::write_sidecar(&dir_handle, &name, &bytes).await
                    }
                    None => crate::web_catalog_fs::delete_sidecar(&dir_handle, &name).await,
                };
                let err = result
                    .err()
                    .map(|e| format!("could not save {}: {e}", name.to_string_lossy()));
                let _ = done_tx.send((path, err));
            });
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    #[test]
    fn a_later_write_to_one_path_supersedes_an_earlier_one_in_the_overlay() {
        let mut wb = Writeback::new();
        let path = PathBuf::from("/photos/a.jpg");
        wb.enqueue(
            &path,
            WriteOp::Put(ImageRecord {
                rating: Some(1),
                ..Default::default()
            }),
        );
        wb.enqueue(
            &path,
            WriteOp::Put(ImageRecord {
                rating: Some(4),
                ..Default::default()
            }),
        );

        let mut images = HashMap::new();
        wb.overlay(Path::new("/photos"), &mut images);
        assert_eq!(
            images
                .get(std::ffi::OsStr::new("a.jpg"))
                .and_then(|r| r.rating),
            Some(4),
            "the overlay must serve the newest queued snapshot, not the first"
        );
    }

    #[test]
    fn the_overlay_leaves_other_folders_alone() {
        let mut wb = Writeback::new();
        wb.enqueue(
            Path::new("/photos/a/x.jpg"),
            WriteOp::Put(ImageRecord {
                rating: Some(5),
                ..Default::default()
            }),
        );

        let mut images = HashMap::new();
        wb.overlay(Path::new("/photos/b"), &mut images);
        assert!(
            images.is_empty(),
            "a queued write for one folder must not appear in another folder's cache"
        );
    }

    #[test]
    fn a_queued_delete_removes_a_loaded_record_from_the_overlay() {
        let mut wb = Writeback::new();
        wb.enqueue(Path::new("/photos/a.jpg"), WriteOp::Delete);

        let mut images = HashMap::new();
        images.insert(
            OsString::from("a.jpg"),
            ImageRecord {
                rating: Some(3),
                ..Default::default()
            },
        );
        wb.overlay(Path::new("/photos"), &mut images);
        assert!(
            images.is_empty(),
            "a record whose deletion is still queued must not come back from a stale load"
        );
    }
}
