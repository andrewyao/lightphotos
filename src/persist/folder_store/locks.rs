// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's side of `sidecar::lock` (`docs/design/folder-lock.md` §4):
//! which folders' locks the app holds, and for how long.
//!
//! - All of the UI's lock state lives on the UI thread, which never
//!   sleeps for a lock. Opening a folder tries its lock once
//!   ([`Locks::open`]). A lock busy for a moment (a probe, our own writer,
//!   an app that is quitting) leaves the visit `Opening`, and each frame
//!   tries once more ([`Locks::retry`]) until it is taken or the wait runs
//!   out. The UI keeps the lock for the visit. After a folder change or
//!   close it keeps it until the writes queued for that folder have landed
//!   ([`Locks::settle`]).
//! - The writer thread takes the lock of a folder the app doesn't hold for
//!   the writes into it, and lets go when the next write is for another
//!   folder or the queue is empty ([`SaveLocks`]). A write into a folder
//!   another program holds is parked, never refused.
//! - The UI takes over a lock the writer holds when it opens that folder,
//!   so the two threads never turn each other away. No thread holds the
//!   shared mutex across file I/O.
//! - Folders are compared only by [`FolderKey`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use super::HeldBy;
use crate::sidecar::{FolderKey, FolderLock, Holder, LockError, Writer};

/// How long opening a folder tries a lock busy for no named reason (a
/// probe) or with our own pid (a hand-over).
const BRIEF_WAIT: Duration = Duration::from_millis(100);
/// How long opening a folder tries a lock whose holder is an app that is
/// quitting: as long as that app lives, which the OS lock tells, up to this.
const QUITTING_WAIT: Duration = Duration::from_secs(15);

/// What the UI thread and the writer thread share, under one mutex that
/// is never held across file I/O.
#[derive(Default)]
pub(super) struct Shared {
    /// Folders whose lock the UI holds: the open one and those draining.
    held: HashSet<FolderKey>,
    /// Locks the writer took for folders the UI doesn't hold.
    writer: HashMap<FolderKey, FolderLock>,
    /// Folders on volumes without file locks.
    lockless: HashSet<FolderKey>,
}

pub(super) type SharedLocks = Arc<Mutex<Shared>>;

fn lock(shared: &SharedLocks) -> std::sync::MutexGuard<'_, Shared> {
    shared.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a visit may write, decided when the folder opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Visit {
    /// No folder open, or one with no photos, which takes no lock.
    Unlocked,
    /// The app holds the folder's lock.
    Held,
    /// The lock was busy for a moment and is tried again each frame. The
    /// visit takes no edit until it is decided.
    Opening,
    /// Nothing in the folder is saved for the whole visit.
    ReadOnly(HeldBy),
    /// The volume has no file locks; written without one. `new` is true
    /// the first time this session, for the notice.
    Lockless { new: bool },
}

/// The UI thread's locks.
#[derive(Default)]
pub(super) struct Locks {
    shared: SharedLocks,
    open: Option<(FolderKey, FolderLock)>,
    /// Folders left behind whose queued writes have not all landed.
    draining: HashMap<FolderKey, FolderLock>,
    /// The open folder's lock while the visit is `Opening`.
    attempt: Option<Attempt>,
}

/// Trying a busy lock again, for an `Opening` visit.
struct Attempt {
    key: FolderKey,
    since: std::time::Instant,
    /// An app that is quitting held it at some try. It clears its holder
    /// line just before it lets go, so a later try that finds no name is
    /// still that app, and keeps its wait.
    quitting: bool,
}

impl Locks {
    pub(super) fn shared(&self) -> SharedLocks {
        self.shared.clone()
    }

    /// Open `dir`: the old folder's lock drains, and the new one is tried
    /// once, with no wait. A lock busy for a moment gives `Opening`; then
    /// [`Locks::retry`] each frame decides.
    pub(super) fn open(&mut self, dir: &Path, has_photos: bool) -> (Option<FolderKey>, Visit) {
        self.leave();
        let Some(key) = FolderKey::of(dir) else {
            return (None, Visit::ReadOnly(HeldBy::Unlockable));
        };
        if !has_photos {
            return (Some(key), Visit::Unlocked);
        }
        let visit = match self.try_once(&key) {
            Ok(visit) => visit,
            Err(holder) => {
                self.attempt = Some(Attempt {
                    key: key.clone(),
                    since: std::time::Instant::now(),
                    quitting: is_quitting(holder.as_ref()),
                });
                Visit::Opening
            }
        };
        (Some(key), visit)
    }

    /// Try an `Opening` visit's lock once more, with no wait. `Some` once
    /// it is decided: taken, or read-only when the wait for its holder has
    /// run out.
    pub(super) fn retry(&mut self) -> Option<Visit> {
        let attempt = self.attempt.as_ref()?;
        let (key, waited, seen) = (
            attempt.key.clone(),
            attempt.since.elapsed(),
            attempt.quitting,
        );
        let visit = match self.try_once(&key) {
            Ok(visit) => visit,
            Err(holder) => {
                let quitting = seen || is_quitting(holder.as_ref());
                if let Some(attempt) = self.attempt.as_mut() {
                    attempt.quitting = quitting;
                }
                let budget = if quitting { QUITTING_WAIT } else { BRIEF_WAIT };
                if waited < budget {
                    return None;
                }
                Visit::ReadOnly(held_by(holder))
            }
        };
        self.attempt = None;
        Some(visit)
    }

    /// Try `key`'s lock once. `Err` names a holder worth waiting for: none
    /// named (a probe), our own pid (a hand-over), or an app that is
    /// quitting.
    fn try_once(&mut self, key: &FolderKey) -> Result<Visit, Option<Holder>> {
        {
            let mut shared = lock(&self.shared);
            if shared.lockless.contains(key) {
                return Ok(Visit::Lockless { new: false });
            }
            if shared.held.contains(key) {
                drop(shared);
                if let Some(held) = self.draining.remove(key) {
                    self.open = Some((key.clone(), held));
                }
                return Ok(Visit::Held);
            }
            if let Some(taken) = shared.writer.remove(key) {
                shared.held.insert(key.clone());
                drop(shared);
                self.open = Some((key.clone(), taken));
                return Ok(Visit::Held);
            }
        }
        match FolderLock::try_acquire(key.path(), Writer::App) {
            Ok(taken) => {
                lock(&self.shared).held.insert(key.clone());
                self.open = Some((key.clone(), taken));
                Ok(Visit::Held)
            }
            Err(LockError::Busy(holder)) => match holder {
                Some(Holder {
                    writer: Writer::AppQuitting,
                    ..
                })
                | None => Err(holder),
                Some(h) if h.pid == std::process::id() => Err(holder),
                Some(_) => Ok(Visit::ReadOnly(held_by(holder))),
            },
            Err(LockError::Unsupported(e)) => {
                eprintln!(
                    "[folder store] {e}; writing {} without a lock",
                    key.path().display()
                );
                let new = lock(&self.shared).lockless.insert(key.clone());
                Ok(Visit::Lockless { new })
            }
            // Without the lock another program could take the folder and
            // the app's edits would go unsaved.
            Err(e @ LockError::Unavailable(_)) => {
                eprintln!("[folder store] {e}; opening read-only");
                Ok(Visit::ReadOnly(HeldBy::Unlockable))
            }
        }
    }

    /// Let go of the locks the writer thread took. Only with no write in
    /// flight, so none is mid-write when its lock goes. Dropped outside the
    /// mutex, since dropping one clears its holder line.
    pub(super) fn release_writer(&mut self) {
        let taken: Vec<FolderLock> = lock(&self.shared).writer.drain().map(|(_, l)| l).collect();
        drop(taken);
    }

    /// The open folder is left: its lock drains, and an `Opening` visit's
    /// attempt just ends.
    pub(super) fn leave(&mut self) {
        self.attempt = None;
        if let Some((key, held)) = self.open.take() {
            self.draining.insert(key, held);
        }
    }

    /// Let go of the folders left behind whose writes have all landed.
    pub(super) fn settle(&mut self, writes_pending_in: impl Fn(&FolderKey) -> bool) {
        let done: Vec<FolderKey> = self
            .draining
            .keys()
            .filter(|k| !writes_pending_in(k))
            .cloned()
            .collect();
        if done.is_empty() {
            return;
        }
        {
            let mut shared = lock(&self.shared);
            for key in &done {
                shared.held.remove(key);
            }
        }
        for key in done {
            self.draining.remove(&key);
        }
    }

    /// The app is quitting: every lock the UI holds names its holder
    /// `app-quitting`, so a relaunch on the folder waits for it a moment.
    pub(super) fn mark_quitting(&mut self) {
        for held in self
            .open
            .iter_mut()
            .map(|(_, l)| l)
            .chain(self.draining.values_mut())
        {
            held.mark_quitting();
        }
    }
}

fn is_quitting(holder: Option<&Holder>) -> bool {
    matches!(
        holder,
        Some(Holder {
            writer: Writer::AppQuitting,
            ..
        })
    )
}

/// Who holds a busy lock, for the read-only line. Our own pid as the app
/// can't be the holder (ours is cleared before we let go, and the UI takes
/// over a lock our writer holds), so it reads as someone unnamed.
fn held_by(holder: Option<Holder>) -> HeldBy {
    match holder {
        Some(Holder {
            pid,
            writer: Writer::App | Writer::AppSave | Writer::AppQuitting,
        }) if pid == std::process::id() => HeldBy::Other,
        Some(Holder {
            pid,
            writer: Writer::App | Writer::AppSave | Writer::AppQuitting,
        }) => HeldBy::App(pid),
        Some(Holder {
            pid,
            writer: Writer::Mcp,
        }) => HeldBy::Mcp(pid),
        None => HeldBy::Other,
    }
}

/// What a write into a folder came to.
pub(super) enum Saved {
    /// It ran, with its result.
    Done(Result<(), String>),
    /// Another program holds the folder: keep it and try again.
    Parked,
}

/// The writer thread's locks.
pub(super) struct SaveLocks {
    shared: SharedLocks,
}

impl SaveLocks {
    pub(super) fn new(shared: SharedLocks) -> SaveLocks {
        SaveLocks { shared }
    }

    /// Run `write` into the folder `key`: as it is where the app already
    /// holds the lock or the volume has none, else after taking the lock
    /// (once, no wait). A lock another program holds parks the write.
    pub(super) fn write_in(
        &mut self,
        key: &FolderKey,
        write: impl FnOnce() -> Result<(), String>,
    ) -> Saved {
        self.release_except(Some(key));
        {
            let shared = lock(&self.shared);
            if shared.held.contains(key)
                || shared.writer.contains_key(key)
                || shared.lockless.contains(key)
            {
                drop(shared);
                return Saved::Done(write());
            }
        }
        match FolderLock::try_acquire(key.path(), Writer::AppSave) {
            Ok(taken) => {
                lock(&self.shared).writer.insert(key.clone(), taken);
                Saved::Done(write())
            }
            Err(LockError::Busy(_)) => Saved::Parked,
            Err(LockError::Unsupported(_)) => {
                lock(&self.shared).lockless.insert(key.clone());
                Saved::Done(write())
            }
            // The lock can't be made (a read-only volume, `.lightphotos`
            // a file): the write fails with that reason rather than land in
            // the folder without its lock.
            Err(e @ LockError::Unavailable(_)) => Saved::Done(Err(format!(
                "could not save in {}: {e}",
                key.path().display()
            ))),
        }
    }

    /// Let go of the locks the writer took, but `keep`'s. Dropped outside
    /// the mutex, since dropping one clears its holder line.
    pub(super) fn release_except(&mut self, keep: Option<&FolderKey>) {
        let taken: Vec<FolderLock> = {
            let mut shared = lock(&self.shared);
            let gone: Vec<FolderKey> = shared
                .writer
                .keys()
                .filter(|k| Some(*k) != keep)
                .cloned()
                .collect();
            gone.iter()
                .filter_map(|k| shared.writer.remove(k))
                .collect()
        };
        drop(taken);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(tag: &str) -> (std::path::PathBuf, FolderKey) {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-locks-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = FolderKey::of(&dir).unwrap();
        (dir, key)
    }

    fn free(dir: &Path) -> bool {
        FolderLock::try_acquire(dir, Writer::Mcp).is_ok()
    }

    /// The UI opening a folder whose lock our own writer holds takes it
    /// over, so a writable folder never opens read-only over our own lock.
    #[test]
    fn opening_takes_over_the_writers_lock() {
        let (dir, key) = folder("takeover");
        let mut locks = Locks::default();
        let mut save = SaveLocks::new(locks.shared());
        let wrote = save.write_in(&key, || Ok(()));
        let (_, visit) = locks.open(&dir, true);
        save.release_except(None);
        let still_held = !free(&dir);
        drop(locks);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(wrote, Saved::Done(Ok(()))));
        assert_eq!(visit, Visit::Held);
        assert!(still_held, "the UI holds it now, not the writer");
    }

    /// The writer lets go of a left folder's lock as soon as its next write
    /// is for another folder, not when its whole queue empties.
    #[test]
    fn the_writer_lets_go_when_it_moves_to_another_folder() {
        let (a, ka) = folder("move-a");
        let (b, kb) = folder("move-b");
        let mut save = SaveLocks::new(SharedLocks::default());
        let _ = save.write_in(&ka, || Ok(()));
        let a_held = !free(&a);
        let _ = save.write_in(&kb, || Ok(()));
        let a_free = free(&a);
        save.release_except(None);
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
        assert!(a_held && a_free);
    }

    /// A write into a folder another program holds is parked, not refused.
    #[test]
    fn a_write_into_a_held_folder_is_parked() {
        let (dir, key) = folder("parked");
        let other = FolderLock::try_acquire(&dir, Writer::Mcp).unwrap();
        let mut save = SaveLocks::new(SharedLocks::default());
        let mut ran = false;
        let saved = save.write_in(&key, || {
            ran = true;
            Ok(())
        });
        drop(other);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(saved, Saved::Parked));
        assert!(!ran);
    }

    /// An app that is quitting clears its holder line just before it lets
    /// go. A try in that gap finds no name, and must keep the quitting
    /// wait rather than the brief one, or a relaunch opens read-only.
    #[test]
    fn a_quitting_holder_keeps_its_wait_once_its_line_is_cleared() {
        let (dir, _) = folder("quit-gap");
        let mut old = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        old.mark_quitting();
        let mut locks = Locks::default();
        let (_, opening) = locks.open(&dir, true);
        std::fs::remove_file(
            dir.join(crate::sidecar::SIDECAR_DIR)
                .join(crate::sidecar::WHO_FILE),
        )
        .unwrap();
        std::thread::sleep(BRIEF_WAIT + Duration::from_millis(50));
        let in_gap = locks.retry();
        drop(old);
        let after = locks.retry();
        drop(locks);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(opening, Visit::Opening);
        assert_eq!(in_gap, None, "still waiting for the quitting app");
        assert_eq!(after, Some(Visit::Held));
    }

    /// A busy lock whose line names our own app is never "another window".
    #[test]
    fn our_own_pid_is_never_another_window() {
        let me = std::process::id();
        let mine = |writer| Some(Holder { pid: me, writer });
        assert_eq!(held_by(mine(Writer::App)), HeldBy::Other);
        assert_eq!(held_by(mine(Writer::AppSave)), HeldBy::Other);
        let other = Some(Holder {
            pid: me.wrapping_add(1),
            writer: Writer::AppQuitting,
        });
        assert_eq!(held_by(other), HeldBy::App(me.wrapping_add(1)));
    }
}
