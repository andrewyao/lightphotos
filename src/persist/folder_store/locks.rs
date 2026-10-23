// SPDX-License-Identifier: MIT OR Apache-2.0

//! The app's side of `sidecar::lock` (`docs/design/folder-lock.md` §4):
//! which folders' locks the app holds, and for how long.
//!
//! - The lock of a folder that opens is taken on the thread that loads its
//!   sidecars, before it reads them ([`LockRequest::run`]), so any wait is
//!   off the UI thread and the decision lands with the records. The UI
//!   keeps the lock for the visit ([`Locks::finish`]). After a folder change or close it keeps it
//!   until the writes queued for that folder have landed
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

/// How far apart opening a folder tries a busy lock again.
const RETRY_GAP: Duration = Duration::from_millis(20);
/// Tries for a lock busy for no named reason (a probe) or with our own
/// pid (a hand-over): about 100 ms.
const BRIEF_TRIES: u32 = 5;
/// Tries for a lock whose holder is an app that is quitting: as long as
/// that app lives, which the OS lock tells, up to about 15 s.
const QUITTING_TRIES: u32 = 750;

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
    /// The lock is being taken, off the UI thread, with the load; nothing
    /// is edited until it lands.
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
}

impl Locks {
    pub(super) fn shared(&self) -> SharedLocks {
        self.shared.clone()
    }

    /// Start opening `dir`: the old folder's lock drains, and the returned
    /// request takes the new one off the UI thread.
    pub(super) fn request(&mut self, dir: &Path, has_photos: bool) -> LockRequest {
        self.leave();
        LockRequest {
            dir: dir.to_path_buf(),
            has_photos,
            shared: self.shared.clone(),
        }
    }

    /// Take in what a request found. `current` is false for a request made
    /// for a folder since left: a lock it took drains like any other.
    pub(super) fn finish(&mut self, opened: Opened, current: bool) -> (Option<FolderKey>, Visit) {
        let Opened { key, outcome } = opened;
        let Some(key) = key else {
            return (None, Visit::ReadOnly(HeldBy::Unlockable));
        };
        if !current {
            if let Outcome::Taken(taken) = outcome {
                self.draining.insert(key, taken);
            }
            return (None, Visit::Unlocked);
        }
        let visit = match outcome {
            Outcome::Taken(taken) => {
                self.open = Some((key.clone(), taken));
                Visit::Held
            }
            Outcome::AlreadyHeld => {
                if !self.open.as_ref().is_some_and(|(k, _)| *k == key) {
                    if let Some(held) = self.draining.remove(&key) {
                        self.open = Some((key.clone(), held));
                    }
                }
                Visit::Held
            }
            Outcome::Visit(visit) => visit,
        };
        (Some(key), visit)
    }

    /// Let go of the locks the writer thread took. Only with no write in
    /// flight, so none is mid-write when its lock goes. Dropped outside the
    /// mutex, since dropping one clears its holder line.
    pub(super) fn release_writer(&mut self) {
        let taken: Vec<FolderLock> = lock(&self.shared).writer.drain().map(|(_, l)| l).collect();
        drop(taken);
    }

    /// The open folder is left: its lock drains.
    pub(super) fn leave(&mut self) {
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

/// Taking a folder's lock, for the thread that loads its sidecars.
pub(crate) struct LockRequest {
    dir: std::path::PathBuf,
    has_photos: bool,
    shared: SharedLocks,
}

/// What a [`LockRequest`] found.
pub(crate) struct Opened {
    key: Option<FolderKey>,
    outcome: Outcome,
}

enum Outcome {
    /// The lock, newly taken or taken over from the writer thread.
    Taken(FolderLock),
    /// The UI holds it already: the folder is open or draining.
    AlreadyHeld,
    /// No lock to take.
    Visit(Visit),
}

impl Opened {
    /// No lock could be taken (the load thread didn't start): read-only.
    pub(crate) fn unlockable() -> Opened {
        Opened {
            key: None,
            outcome: Outcome::Visit(Visit::ReadOnly(HeldBy::Unlockable)),
        }
    }

    /// Whether the visit may sweep derived files: not a read-only one.
    pub(crate) fn may_sweep(&self) -> bool {
        !matches!(self.outcome, Outcome::Visit(Visit::ReadOnly(_)))
    }
}

impl LockRequest {
    /// Take the folder's lock. It can wait (for a probe, our own writer, or
    /// an app that is quitting), so it runs off the UI thread.
    pub(crate) fn run(self) -> Opened {
        let Some(key) = FolderKey::of(&self.dir) else {
            return Opened {
                key: None,
                outcome: Outcome::Visit(Visit::ReadOnly(HeldBy::Unlockable)),
            };
        };
        let outcome = if self.has_photos {
            take(&self.shared, &key)
        } else {
            Outcome::Visit(Visit::Unlocked)
        };
        Opened {
            key: Some(key),
            outcome,
        }
    }
}

fn take(shared: &SharedLocks, key: &FolderKey) -> Outcome {
    let (mut brief, mut quitting) = (BRIEF_TRIES, QUITTING_TRIES);
    loop {
        {
            let mut shared = lock(shared);
            if shared.lockless.contains(key) {
                return Outcome::Visit(Visit::Lockless { new: false });
            }
            if shared.held.contains(key) {
                return Outcome::AlreadyHeld;
            }
            if let Some(taken) = shared.writer.remove(key) {
                shared.held.insert(key.clone());
                return Outcome::Taken(taken);
            }
        }
        match FolderLock::try_acquire(key.path(), Writer::App) {
            Ok(taken) => {
                lock(shared).held.insert(key.clone());
                return Outcome::Taken(taken);
            }
            Err(LockError::Busy(holder)) => {
                let tries = match holder {
                    // An app that is quitting lets go when its writes have
                    // landed, or when it ends.
                    Some(Holder {
                        writer: Writer::AppQuitting,
                        ..
                    }) => &mut quitting,
                    // A probe names no holder; our own pid is a hand-over
                    // in progress.
                    None => &mut brief,
                    Some(h) if h.pid == std::process::id() => &mut brief,
                    Some(_) => return Outcome::Visit(Visit::ReadOnly(held_by(holder))),
                };
                *tries = tries.saturating_sub(1);
                if *tries == 0 {
                    return Outcome::Visit(Visit::ReadOnly(held_by(holder)));
                }
                std::thread::sleep(RETRY_GAP);
            }
            Err(LockError::Unsupported(e)) => {
                eprintln!(
                    "[folder store] {e}; writing {} without a lock",
                    key.path().display()
                );
                let new = lock(shared).lockless.insert(key.clone());
                return Outcome::Visit(Visit::Lockless { new });
            }
            // Without the lock another program could take the folder and
            // the app's edits would go unsaved.
            Err(e @ LockError::Unavailable(_)) => {
                eprintln!("[folder store] {e}; opening read-only");
                return Outcome::Visit(Visit::ReadOnly(HeldBy::Unlockable));
            }
        }
    }
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
            // Can pass, so not remembered; the write goes ahead and fails
            // on its own if the folder can't be written.
            Err(LockError::Unavailable(_)) => Saved::Done(write()),
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
        let request = locks.request(&dir, true);
        let (_, visit) = locks.finish(request.run(), true);
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
