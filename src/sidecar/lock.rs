// SPDX-License-Identifier: MIT OR Apache-2.0

//! One writer per folder. The app and a headless `lightphotos-mcp` never
//! write the same folder's sidecars at once. The design and its failure
//! cases are in `docs/design/folder-lock.md`.
//!
//! - A folder is named by its [`FolderKey`], however its path is spelled.
//!
//! - The app holds a folder's [`FolderLock`] for as long as the folder is
//!   open (and, after it leaves, until the writes queued for the folder land;
//!   see `persist::folder_store::locks`). A folder another program holds
//!   opens read-only for the visit. A headless
//!   MCP refuses to write a folder the app holds, and names the app, so the
//!   agent can send its command through the app (`--attach`) instead.
//! - Headless MCP holds the lock while one command reads and writes. So
//!   does the app for a write that lands in a folder it has left, such as
//!   the rest of an Auto Tone batch.
//! - The lock is the OS's advisory file lock, so a process that crashes
//!   drops it with itself. No one deletes the lock file, which would let two
//!   writers lock two different files. The holder writes its pid and mode
//!   to a separate [`WHO_FILE`], which no lock covers, so it stays readable
//!   on Windows too, where a held lock also locks the file's bytes. It is
//!   for messages only; a dead pid there blocks no one.
//! - The app never waits long for a lock: see
//!   `persist::folder_store::locks`.
//!
//! The browser build has no file locks.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::SIDECAR_DIR;

/// The lock file inside a folder's [`SIDECAR_DIR`].
pub const LOCK_FILE: &str = ".lock";

/// Beside [`LOCK_FILE`]: the last holder's `pid mode` line.
pub const WHO_FILE: &str = ".lock.who";

/// How long a headless MCP command waits for one that holds a lock
/// briefly: another command, or the app saving into a folder it has left.
/// A folder open in the app is never waited for, since the app holds it
/// until it moves on. The app itself never waits.
pub const LOCK_WAIT: Duration = Duration::from_secs(2);

/// Who holds a lock, as its lock file records it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Writer {
    /// The app, with the folder open.
    App,
    /// The app, saving one write into a folder it has left.
    AppSave,
    /// The app, quitting: it lets go in a moment, so one that relaunches
    /// tries again for a while.
    AppQuitting,
    /// A headless `lightphotos-mcp` command.
    Mcp,
}

impl Writer {
    fn as_str(self) -> &'static str {
        match self {
            Writer::App => "app",
            Writer::AppSave => "app-save",
            Writer::AppQuitting => "app-quitting",
            Writer::Mcp => "mcp",
        }
    }

    fn parse(s: &str) -> Option<Writer> {
        match s {
            "app" => Some(Writer::App),
            "app-save" => Some(Writer::AppSave),
            "app-quitting" => Some(Writer::AppQuitting),
            "mcp" => Some(Writer::Mcp),
            _ => None,
        }
    }
}

/// The last holder a lock file names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub writer: Writer,
}

/// Why a [`FolderLock`] was not taken.
#[derive(Debug)]
pub enum LockError {
    /// Another writer holds it: the app with the folder open, or a brief
    /// writer for all of the wait. Names the holder as its lock file does,
    /// when it can be read.
    Busy(Option<Holder>),
    /// The lock file could not be made or opened just now: a disk or
    /// network fault, too many open files. It can pass.
    Unavailable(String),
    /// The lock file can't be made here and won't be: a read-only volume,
    /// no permission, the folder (or `.lightphotos`) gone or not a folder,
    /// a planted link.
    Refused(String),
    /// The volume has no file locks (`ENOTSUP`, `ENOLCK`), such as some
    /// network shares. It won't pass while the folder is there.
    Unsupported(String),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy(Some(Holder {
                pid,
                writer: Writer::App | Writer::AppQuitting,
            })) => write!(f, "LightPhotos (pid {pid}) has this folder open"),
            LockError::Busy(Some(Holder { pid, writer })) => write!(
                f,
                "another program ({}, pid {pid}) is saving this folder's edits",
                writer.as_str()
            ),
            LockError::Busy(None) => write!(f, "another program is saving this folder's edits"),
            LockError::Unavailable(e) | LockError::Refused(e) => {
                write!(f, "cannot lock the folder's edits: {e}")
            }
            LockError::Unsupported(e) => write!(f, "this volume can't lock files: {e}"),
        }
    }
}

/// One folder, however its path is spelled: `a/b`, `a/./b/`, a relative
/// path, a symlink to it, or another letter case on a case-insensitive
/// volume. Two keys are equal when they name the same folder on disk (its
/// device and inode on Unix, its resolved path elsewhere). The lock code
/// compares folders only through this.
#[derive(Clone, Debug)]
pub struct FolderKey {
    id: FolderId,
    path: PathBuf,
}

#[cfg(unix)]
type FolderId = (u64, u64);
#[cfg(not(unix))]
type FolderId = String;

impl FolderKey {
    /// The key of the folder `dir`, or `None` when it isn't one, such as a
    /// folder that is gone.
    pub fn of(dir: &Path) -> Option<FolderKey> {
        let dir = if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        };
        let path = dir.canonicalize().ok()?;
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_dir() {
            return None;
        }
        #[cfg(unix)]
        let id = {
            use std::os::unix::fs::MetadataExt;
            (meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let id = path.to_string_lossy().to_lowercase();
        Some(FolderKey { id, path })
    }

    /// The key of the folder `photo` is in.
    pub fn of_photo(photo: &Path) -> Option<FolderKey> {
        FolderKey::of(photo.parent()?)
    }

    /// The folder's resolved path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl PartialEq for FolderKey {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for FolderKey {}

impl std::hash::Hash for FolderKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// The exclusive right to write one folder's photo sidecars. Released when
/// dropped, or by the OS when the process ends.
pub struct FolderLock {
    /// Closed after `drop` runs, which lets go of the lock.
    _file: std::fs::File,
    /// The [`WHO_FILE`] and the line this holder wrote to it.
    who: std::path::PathBuf,
    line: String,
}

impl Drop for FolderLock {
    /// Clear the holder's line while the lock is still held, so the next
    /// contender never reads a holder that has gone. A line another holder
    /// wrote since is left alone.
    fn drop(&mut self) {
        let ours = crate::paths::read_capped(&self.who, 64)
            .ok()
            .flatten()
            .is_some_and(|b| b == self.line.as_bytes());
        if ours {
            let _ = crate::paths::remove_if_present(&self.who);
        }
    }
}

/// Whether a lock call's error means the volume has no file locks at all,
/// rather than a failure that can pass.
///
/// `ENOTSUP`, `ENOLCK` (no lock manager, as on some NFS mounts) and `EINVAL`
/// (the file system can't lock this kind of file) mean that. `EIO` and the
/// rest can pass, so they make the lock unavailable for this try only.
/// Whether an error making or opening the lock file will be there next
/// time too, so a write waiting for the lock should fail rather than wait.
pub fn lasting(kind: std::io::ErrorKind) -> bool {
    use std::io::ErrorKind as K;
    matches!(
        kind,
        K::PermissionDenied
            | K::ReadOnlyFilesystem
            | K::NotADirectory
            | K::IsADirectory
            | K::NotFound
            | K::InvalidFilename
    )
}

fn is_unsupported(e: &std::io::Error) -> bool {
    // ENOLCK and EINVAL have no `ErrorKind` of their own.
    #[cfg(target_os = "macos")]
    const ENOLCK: i32 = 77;
    #[cfg(target_os = "linux")]
    const ENOLCK: i32 = 37;
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    const ENOLCK: i32 = i32::MIN;
    #[cfg(unix)]
    const EINVAL: i32 = 22;
    #[cfg(not(unix))]
    const EINVAL: i32 = i32::MIN;
    e.kind() == std::io::ErrorKind::Unsupported
        || matches!(e.raw_os_error(), Some(n) if n == ENOLCK || n == EINVAL)
}

impl FolderLock {
    /// Rename this holder `app-quitting` in its line, so an app that
    /// relaunches on the folder while this one exits tries again for a
    /// moment instead of opening it read-only.
    pub fn mark_quitting(&mut self) {
        let Some(pid) = self.line.split_whitespace().next().map(str::to_string) else {
            return;
        };
        let line = format!("{pid} {}\n", Writer::AppQuitting.as_str());
        if crate::paths::write_atomic(&self.who, line.as_bytes(), None).is_ok() {
            self.line = line;
        }
    }

    /// [`FolderLock::acquire`] with no wait, the only way the app takes a
    /// lock: it never waits for one.
    pub fn try_acquire(dir: &Path, who: Writer) -> Result<FolderLock, LockError> {
        FolderLock::acquire(dir, who, Duration::ZERO)
    }

    /// Lock the sidecars of the photos in `dir` for `who`, waiting up to
    /// `wait` for a brief writer. Creates `dir/.lightphotos/` if needed, but
    /// never `dir` itself: a folder deleted outside the app stays deleted.
    pub fn acquire(dir: &Path, who: Writer, wait: Duration) -> Result<FolderLock, LockError> {
        let unavailable = |e: std::io::Error| {
            if lasting(e.kind()) {
                LockError::Refused(e.to_string())
            } else {
                LockError::Unavailable(e.to_string())
            }
        };
        let refused = |e: std::io::Error| LockError::Refused(e.to_string());
        if !dir.is_dir() {
            return Err(LockError::Refused(format!(
                "{} is not a folder",
                dir.display()
            )));
        }
        let sidecars = dir.join(SIDECAR_DIR);
        crate::paths::refuse_linked_sidecar_dir(&sidecars).map_err(refused)?;
        match std::fs::create_dir(&sidecars) {
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => return Err(unavailable(e)),
            _ => {}
        }
        let path = sidecars.join(LOCK_FILE);
        // The OS lock works on a file opened for reading only, so a lock
        // file this user can't write (another user made it) still locks.
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                std::fs::File::open(&path).map_err(unavailable)?
            }
            Err(e) => return Err(unavailable(e)),
        };
        // A folder someone else supplied can plant a link here. Writing the
        // holder through it would overwrite whatever it points at, so the
        // file opened must be the regular file at this name.
        refuse_unless_same_file(&path, &file).map_err(refused)?;
        let deadline = Instant::now() + wait;
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(std::fs::TryLockError::WouldBlock) => {
                    let holder = holder(&sidecars);
                    let open_in_app = holder.is_some_and(|h| h.writer == Writer::App);
                    if open_in_app || Instant::now() >= deadline {
                        return Err(LockError::Busy(holder));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(std::fs::TryLockError::Error(e)) if is_unsupported(&e) => {
                    return Err(LockError::Unsupported(e.to_string()))
                }
                Err(std::fs::TryLockError::Error(e)) => return Err(unavailable(e)),
            }
        }
        let line = format!("{} {}\n", std::process::id(), who.as_str());
        let who_path = sidecars.join(WHO_FILE);
        if let Err(e) = crate::paths::write_atomic(&who_path, line.as_bytes(), None) {
            // The holder's name is for messages only; the lock still holds.
            eprintln!("[sidecar] could not name the lock holder: {e}");
        }
        Ok(FolderLock {
            _file: file,
            who: who_path,
            line,
        })
    }
}

/// Whether another writer holds `dir`'s lock now, as a [`LockError::Busy`]
/// naming it, without taking the lock or writing anything. For reports
/// such as a headless MCP's `get_state`.
pub fn probe(dir: &Path) -> Option<LockError> {
    let sidecars = dir.join(SIDECAR_DIR);
    crate::paths::refuse_linked_sidecar_dir(&sidecars).ok()?;
    let path = sidecars.join(LOCK_FILE);
    let file = std::fs::File::open(&path).ok()?;
    refuse_unless_same_file(&path, &file).ok()?;
    match file.try_lock() {
        Ok(()) => None,
        Err(std::fs::TryLockError::WouldBlock) => Some(LockError::Busy(holder(&sidecars))),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

/// The [`WHO_FILE`] line, or `None` when it is missing or not one.
fn holder(sidecars: &Path) -> Option<Holder> {
    let bytes = crate::paths::read_capped(&sidecars.join(WHO_FILE), 64).ok()??;
    let text = String::from_utf8(bytes).ok()?;
    let mut parts = text.split_whitespace();
    let pid = parts.next()?.parse().ok()?;
    let writer = Writer::parse(parts.next()?)?;
    Some(Holder { pid, writer })
}

#[cfg(unix)]
fn refuse_unless_same_file(path: &Path, file: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let named = std::fs::symlink_metadata(path)?;
    let opened = file.metadata()?;
    if named.file_type().is_file() && named.dev() == opened.dev() && named.ino() == opened.ino() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a plain file",
            path.display()
        )))
    }
}

#[cfg(not(unix))]
fn refuse_unless_same_file(path: &Path, _file: &std::fs::File) -> std::io::Result<()> {
    if std::fs::symlink_metadata(path)?.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "{} is not a plain file",
            path.display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_folder(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A folder open in the app turns another writer away at once, naming
    /// the app. Dropping the lock frees it at once, the way a crash would.
    #[test]
    fn a_folder_open_in_the_app_turns_a_writer_away_until_it_lets_go() {
        let dir = temp_folder("busy");
        let held = FolderLock::acquire(&dir, Writer::App, LOCK_WAIT).unwrap();
        let start = Instant::now();
        let err = FolderLock::acquire(&dir, Writer::Mcp, LOCK_WAIT)
            .err()
            .expect("the lock is held");
        let waited = start.elapsed();
        drop(held);
        let again = FolderLock::acquire(&dir, Writer::Mcp, Duration::ZERO);
        std::fs::remove_dir_all(&dir).unwrap();
        let me = Holder {
            pid: std::process::id(),
            writer: Writer::App,
        };
        assert!(matches!(err, LockError::Busy(Some(h)) if h == me), "{err}");
        assert!(waited < LOCK_WAIT, "no wait for a folder open in the app");
        assert!(again.is_ok(), "a dropped lock is free at once");
    }

    /// Every spelling of one folder is one key: a trailing separator, a
    /// `.` part, a relative path and a symlink to it. Another folder is
    /// another key, and a folder that is gone has none.
    #[test]
    fn every_spelling_of_one_folder_is_one_key() {
        let dir = temp_folder("key");
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let key = FolderKey::of(&dir).unwrap();
        // The folder as a path relative to the working directory, without
        // changing it under the other tests: up to the root, then down.
        let cwd = std::env::current_dir().unwrap();
        let mut rel = PathBuf::new();
        for _ in cwd.components().skip(1) {
            rel.push("..");
        }
        let rel = rel.join(dir.strip_prefix("/").unwrap_or(&dir));
        let relative = FolderKey::of(&rel);
        let photo_relative = FolderKey::of_photo(&rel.join("a.jpg"));
        #[cfg(unix)]
        let linked = {
            let link = dir.with_extension("link");
            let _ = std::fs::remove_file(&link);
            std::os::unix::fs::symlink(&dir, &link).unwrap();
            let k = FolderKey::of(&link);
            let _ = std::fs::remove_file(&link);
            k
        };
        let spellings = [
            FolderKey::of(&dir.join("")),
            FolderKey::of(&dir.join(".")),
            FolderKey::of(&dir.join("sub").join("..")),
            relative,
            photo_relative,
            #[cfg(unix)]
            linked,
        ];
        let other = FolderKey::of(&dir.join("sub"));
        let gone = FolderKey::of(&dir.join("gone"));
        std::fs::remove_dir_all(&dir).unwrap();
        for k in spellings {
            assert_eq!(k.as_ref(), Some(&key));
        }
        assert_ne!(other, Some(key));
        assert!(gone.is_none());
    }

    /// A lock file this user can't write, as another user's would be,
    /// still locks: the OS lock needs only read access.
    #[cfg(unix)]
    #[test]
    fn a_lock_file_this_user_cant_write_still_locks() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_folder("read-only-lock");
        drop(FolderLock::try_acquire(&dir, Writer::Mcp).unwrap());
        let file = dir.join(SIDECAR_DIR).join(LOCK_FILE);
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();
        let held = FolderLock::try_acquire(&dir, Writer::App);
        let second = FolderLock::try_acquire(&dir, Writer::Mcp).err();
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644));
        let taken = held.is_ok();
        drop(held);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(taken, "the app takes it");
        assert!(matches!(second, Some(LockError::Busy(_))), "and it locks");
    }

    /// A quitting app renames itself in its line, so a relaunch can tell.
    #[test]
    fn a_quitting_holder_says_so() {
        let dir = temp_folder("quitting");
        let mut held = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        held.mark_quitting();
        let err = FolderLock::try_acquire(&dir, Writer::Mcp).err();
        drop(held);
        let after = dir.join(SIDECAR_DIR).join(WHO_FILE).exists();
        std::fs::remove_dir_all(&dir).unwrap();
        let me = std::process::id();
        assert!(
            matches!(err, Some(LockError::Busy(Some(Holder { pid, writer: Writer::AppQuitting }))) if pid == me)
        );
        assert!(!after, "its line goes with it");
    }

    /// A holder that lets go clears its line, so the next contender never
    /// reads a holder that has gone.
    #[test]
    fn letting_go_clears_the_holders_line() {
        let dir = temp_folder("who-cleared");
        let who = dir.join(SIDECAR_DIR).join(WHO_FILE);
        let held = FolderLock::try_acquire(&dir, Writer::App).unwrap();
        let named = std::fs::read_to_string(&who).unwrap_or_default();
        drop(held);
        let after = who.exists();
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(named, format!("{} app\n", std::process::id()));
        assert!(!after, "the line goes with the lock");
    }

    /// A crashed holder leaves its line in the file but no OS lock, so it
    /// blocks no one.
    #[test]
    fn a_dead_holders_line_blocks_no_one() {
        let dir = temp_folder("stale");
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::fs::write(dir.join(SIDECAR_DIR).join(WHO_FILE), "4000000000 app\n").unwrap();
        let taken = FolderLock::acquire(&dir, Writer::Mcp, Duration::ZERO);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(taken.is_ok());
    }

    /// A folder deleted outside the app is not made again, and a pristine
    /// folder gets `.lightphotos/` for its lock and nothing above it.
    #[test]
    fn locking_never_makes_the_folder_itself() {
        let parent = temp_folder("missing");
        let gone = parent.join("gone");
        let taken = FolderLock::acquire(&gone, Writer::App, Duration::ZERO);
        let made = gone.exists();
        std::fs::remove_dir_all(&parent).unwrap();
        assert!(matches!(taken, Err(LockError::Refused(_))));
        assert!(!made, "the deleted folder stays deleted");
    }

    #[cfg(unix)]
    #[test]
    fn a_planted_link_at_the_lock_file_is_refused_and_its_target_left_alone() {
        let dir = temp_folder("link");
        let target = dir.join("precious.txt");
        std::fs::write(&target, b"keep me").unwrap();
        std::fs::create_dir_all(dir.join(SIDECAR_DIR)).unwrap();
        std::os::unix::fs::symlink(&target, dir.join(SIDECAR_DIR).join(LOCK_FILE)).unwrap();
        let taken = FolderLock::acquire(&dir, Writer::App, Duration::ZERO);
        let kept = std::fs::read(&target).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(matches!(taken, Err(LockError::Refused(_))));
        assert_eq!(kept, b"keep me");
    }

    /// Review 3.7 round 5 #2: what can't pass fails a write at once; what
    /// may pass parks it.
    #[test]
    fn lasting_errors_are_told_from_passing_ones() {
        use std::io::ErrorKind as K;
        for kind in [
            K::PermissionDenied,
            K::ReadOnlyFilesystem,
            K::NotADirectory,
            K::NotFound,
        ] {
            assert!(lasting(kind), "{kind:?}");
        }
        for kind in [
            K::Other,
            K::Interrupted,
            K::WouldBlock,
            K::TimedOut,
            K::OutOfMemory,
        ] {
            assert!(!lasting(kind), "{kind:?}");
        }
    }

    /// Review 3.7 round 5 #2: `.lightphotos` a file is a lasting refusal.
    #[test]
    fn a_sidecar_folder_that_is_a_file_refuses_the_lock() {
        let dir = temp_folder("sidecar-file");
        std::fs::write(dir.join(SIDECAR_DIR), b"not a folder").unwrap();
        let taken = FolderLock::acquire(&dir, Writer::App, Duration::ZERO);
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(
            matches!(taken, Err(LockError::Refused(_))),
            "{:?}",
            taken.err()
        );
    }
}
