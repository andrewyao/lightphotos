// SPDX-License-Identifier: GPL-3.0-or-later

//! Derived per-photo signals, remembered between sessions.
//!
//! Capture time, sharpness, dHash and face quality are all functions of one
//! photo's bytes, and three of them are expensive. On 6016x6016 photos a face
//! analysis measures about 73 ms because Vision decodes the file itself at
//! full resolution, against 32 ms for a whole thumbnail decode. Recomputing
//! them on every folder visit is the main reason the grouping features are too
//! heavy to run unasked.
//!
//! These are deliberately *not* fields on [`ImageRecord`](crate::catalog::ImageRecord).
//! That record is the user's authored edit state, the thing worth backing up,
//! and its sidecar is deleted once it holds nothing. A cached hash living
//! there would keep the sidecar alive forever, would rewrite it on every
//! background computation rather than on a user edit, and would have nowhere
//! to put an invalidation key. Authored state and derived state have different
//! lifetimes, so they get different containers.
//!
//! Feature-print distances are absent on purpose. They are keyed by an
//! (anchor, member) pair rather than by a photo, so they are not a property of
//! a file at all, and their anchors move as grouping changes. Persisting the
//! prints themselves through `VNFeaturePrintObservation`'s
//! `dataRepresentation` is the right follow-up.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::facequality::FaceQuality;

/// Cache file name, inside `<photo dir>/.lightphotos/`. Neither `.xmp` nor
/// `.thumb.jpg`, so the sidecar scan and the thumbnail sweep both skip it.
pub(crate) const CACHE_FILE: &str = "signals.json";

/// How long a `record` may leave new signals unwritten. A crash loses at most
/// this much recomputation, and the folder switch flushes anyway.
const WRITE_INTERVAL: Duration = Duration::from_secs(5);

/// A photo's capture time as stored. `Unreadable` is a real answer, not a
/// missing one: it means the EXIF and the mtime were both tried and failed, and
/// recording it stops the next session retrying a file that has no answer.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CaptureTime {
    Unreadable,
    /// Milliseconds since the Unix epoch, matching the thumbnail cache key's
    /// unit so native and web agree.
    At(i64),
}

impl CaptureTime {
    fn from_system_time(t: Option<SystemTime>) -> CaptureTime {
        let Some(t) = t else {
            return CaptureTime::Unreadable;
        };
        match t.duration_since(UNIX_EPOCH) {
            Ok(d) => CaptureTime::At(d.as_millis() as i64),
            // Older than 1970. Rare, but it must round-trip rather than clamp.
            Err(e) => CaptureTime::At(-(e.duration().as_millis() as i64)),
        }
    }

    pub fn to_system_time(self) -> Option<SystemTime> {
        match self {
            CaptureTime::Unreadable => None,
            CaptureTime::At(ms) if ms >= 0 => {
                UNIX_EPOCH.checked_add(Duration::from_millis(ms as u64))
            }
            CaptureTime::At(ms) => UNIX_EPOCH.checked_sub(Duration::from_millis(ms.unsigned_abs())),
        }
    }
}

/// One computed signal, as the app hands it over.
pub enum Signal {
    Capture(Option<SystemTime>),
    Sharpness(f64),
    PHash(u64),
    Faces(FaceQuality),
}

/// What is known about one photo. Every field is optional because the four
/// signals are computed by different passes at different times, and a folder
/// the user never opened the duplicate tools on holds only the first two.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct PhotoSignals {
    /// Validity key over the file's mtime and length. A photo edited by
    /// another program gets a new key, and every signal below is discarded.
    #[serde(rename = "k")]
    key: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phash: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faces: Option<FaceQuality>,
}

impl PhotoSignals {
    fn is_empty(&self) -> bool {
        self.capture.is_none()
            && self.sharpness.is_none()
            && self.phash.is_none()
            && self.faces.is_none()
    }
}

/// Validity key from the source's mtime and length, the same shape as
/// [`crate::thumbnail::cache_key`]. The path is not hashed, so moving a folder
/// keeps its cache valid. Bump the version string to discard every entry.
fn signal_key(mtime_ms: u64, len: u64) -> u64 {
    let mut h = crate::hash::Fnv1a::new();
    h.write(b"lightphotos-signals-v1");
    h.write(&mtime_ms.to_le_bytes());
    h.write(&len.to_le_bytes());
    h.finish()
}

#[cfg(not(target_arch = "wasm32"))]
fn current_key(photo: &Path) -> Option<u64> {
    let meta = std::fs::metadata(photo).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    Some(signal_key(mtime_ms, meta.len()))
}

/// wasm has no OS paths to stat, so nothing persists there and no key is ever
/// checked. See [`SignalCache::load`].
#[cfg(target_arch = "wasm32")]
fn current_key(_photo: &Path) -> Option<u64> {
    None
}

/// The file as it sits on disk. A struct rather than a bare map so a later
/// version can add fields beside `entries` without a migration.
#[derive(Serialize, Deserialize, Default)]
struct CacheFile {
    entries: HashMap<String, PhotoSignals>,
}

/// One folder's derived signals.
///
/// Every mutation comes from the app's own thread, which is the only place
/// that drains the worker channels, so there is no shared writer to serialize.
/// The file I/O is the part that must stay off the frame, and that is what the
/// writer thread is for.
pub struct SignalCache {
    dir: Option<PathBuf>,
    entries: HashMap<OsString, PhotoSignals>,
    dirty: bool,
    #[cfg(not(target_arch = "wasm32"))]
    writer: Option<writer::Writer>,
    #[cfg(not(target_arch = "wasm32"))]
    last_write: std::time::Instant,
}

impl SignalCache {
    /// A cache attached to no folder. Records nothing and writes nothing.
    pub fn empty() -> SignalCache {
        SignalCache {
            dir: None,
            entries: HashMap::new(),
            dirty: false,
            #[cfg(not(target_arch = "wasm32"))]
            writer: None,
            #[cfg(not(target_arch = "wasm32"))]
            last_write: std::time::Instant::now(),
        }
    }

    /// Read `dir`'s cache. A missing, unreadable or corrupt file is not an
    /// error, it just means every signal is recomputed, exactly as a first
    /// visit does.
    ///
    /// On wasm this always starts empty. Persisting would need the picked
    /// folder's `FileSystemDirectoryHandle` rather than a path, and the
    /// browser build has no thread to write from.
    pub fn load(dir: &Path) -> SignalCache {
        let mut cache = SignalCache::empty();
        cache.dir = Some(dir.to_path_buf());
        #[cfg(not(target_arch = "wasm32"))]
        {
            cache.writer = Some(writer::Writer::new());
            cache.entries = read_file(dir);
        }
        cache
    }

    /// What is known about `path`, or `None` when nothing is, or when the file
    /// changed since the signals were computed.
    pub fn get(&self, path: &Path) -> Option<&PhotoSignals> {
        let entry = self.entries.get(path.file_name()?)?;
        (current_key(path)? == entry.key).then_some(entry)
    }

    /// Remember one signal. A photo whose bytes changed since the last visit
    /// starts a fresh entry rather than mixing old signals with new.
    pub fn record(&mut self, path: &Path, signal: Signal) {
        if self.dir.is_none() {
            return;
        }
        let (Some(name), Some(key)) = (path.file_name(), current_key(path)) else {
            return;
        };
        let entry = self.entries.entry(name.to_os_string()).or_default();
        if entry.key != key {
            *entry = PhotoSignals {
                key,
                ..PhotoSignals::default()
            };
        }
        match signal {
            Signal::Capture(t) => entry.capture = Some(CaptureTime::from_system_time(t)),
            Signal::Sharpness(v) => entry.sharpness = Some(v),
            Signal::PHash(v) => entry.phash = Some(v),
            Signal::Faces(q) => entry.faces = Some(q),
        }
        self.dirty = true;
    }

    /// Write unsaved signals at most every [`WRITE_INTERVAL`]. The frame loop
    /// calls this every tick, so the debounce lives here rather than in
    /// [`record`](Self::record), which stays a map write on a worker result.
    pub fn flush_if_due(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        if self.dirty && self.last_write.elapsed() >= WRITE_INTERVAL {
            self.queue_write();
        }
    }

    /// Drop everything known about `path`, for a photo the user deleted.
    pub fn forget(&mut self, path: &Path) {
        let Some(name) = path.file_name() else {
            return;
        };
        if self.entries.remove(name).is_some() {
            self.dirty = true;
        }
    }

    /// Drop entries whose photo is gone, so the file does not grow forever.
    /// Called before a write, where the directory listing is already warm.
    #[cfg(not(target_arch = "wasm32"))]
    fn sweep_orphans(&mut self) {
        let Some(dir) = &self.dir else {
            return;
        };
        let Ok(listing) = std::fs::read_dir(dir) else {
            return;
        };
        let live: std::collections::HashSet<OsString> =
            listing.flatten().map(|e| e.file_name()).collect();
        let before = self.entries.len();
        self.entries
            .retain(|name, s| live.contains(name) && !s.is_empty());
        if self.entries.len() != before {
            self.dirty = true;
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn queue_write(&mut self) {
        self.sweep_orphans();
        let (Some(dir), Some(writer)) = (&self.dir, &self.writer) else {
            return;
        };
        let named: HashMap<String, PhotoSignals> = self
            .entries
            .iter()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), *v)))
            .collect();
        let Ok(bytes) = serde_json::to_vec(&CacheFile { entries: named }) else {
            return;
        };
        writer.submit(
            dir.join(crate::catalog::SIDECAR_DIR).join(CACHE_FILE),
            bytes,
        );
        self.dirty = false;
        self.last_write = std::time::Instant::now();
    }

    /// Push any unwritten signals to disk and wait, up to `timeout`, for them
    /// to land. Called when the folder changes, so the outgoing folder's work
    /// is not lost when this cache is replaced.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn flush_blocking(&mut self, timeout: Duration) {
        if self.dirty {
            self.queue_write();
        }
        if let Some(writer) = &self.writer {
            writer.wait_idle(timeout);
        }
    }
}

impl Drop for SignalCache {
    fn drop(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        self.flush_blocking(Duration::from_secs(2));
    }
}

/// Read `dir`'s cache file. Every failure is a miss.
#[cfg(not(target_arch = "wasm32"))]
fn read_file(dir: &Path) -> HashMap<OsString, PhotoSignals> {
    let path = dir.join(crate::catalog::SIDECAR_DIR).join(CACHE_FILE);
    let Ok(bytes) = std::fs::read(&path) else {
        return HashMap::new();
    };
    let Ok(file) = serde_json::from_slice::<CacheFile>(&bytes) else {
        return HashMap::new();
    };
    file.entries
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect()
}

/// A photo name is stored as a `String`, so a non-UTF-8 filename simply never
/// caches. Recomputing is always correct, and the alternative is an encoding
/// in the file format that only a few filenames would ever use.
#[cfg(not(target_arch = "wasm32"))]
fn _name_must_be_utf8(name: &OsStr) -> Option<&str> {
    name.to_str()
}

#[cfg(not(target_arch = "wasm32"))]
mod writer {
    use std::path::PathBuf;
    use std::sync::mpsc::{channel, Sender};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    /// Serializing is cheap and the whole map is small, so each job carries a
    /// full snapshot and a later one simply supersedes an earlier one.
    pub(super) struct Writer {
        tx: Option<Sender<(PathBuf, Vec<u8>)>>,
        outstanding: Arc<(Mutex<usize>, Condvar)>,
    }

    impl Writer {
        pub(super) fn new() -> Writer {
            let (tx, rx) = channel::<(PathBuf, Vec<u8>)>();
            let outstanding = Arc::new((Mutex::new(0usize), Condvar::new()));
            let theirs = Arc::clone(&outstanding);
            // A target that cannot spawn simply never persists, the same
            // degradation a read-only folder gets.
            let spawned = std::thread::Builder::new()
                .name("signalcache-writer".to_string())
                .spawn(move || {
                    while let Ok((path, bytes)) = rx.recv() {
                        write_atomically(&path, &bytes);
                        let (lock, cv) = &*theirs;
                        if let Ok(mut n) = lock.lock() {
                            *n = n.saturating_sub(1);
                            cv.notify_all();
                        }
                    }
                });
            Writer {
                tx: spawned.is_ok().then_some(tx),
                outstanding,
            }
        }

        pub(super) fn submit(&self, path: PathBuf, bytes: Vec<u8>) {
            let Some(tx) = &self.tx else {
                return;
            };
            let (lock, _) = &*self.outstanding;
            if let Ok(mut n) = lock.lock() {
                *n += 1;
            }
            let _ = tx.send((path, bytes));
        }

        pub(super) fn wait_idle(&self, timeout: Duration) {
            let (lock, cv) = &*self.outstanding;
            let Ok(guard) = lock.lock() else {
                return;
            };
            let _ = cv.wait_timeout_while(guard, timeout, |n| *n > 0);
        }
    }

    impl Drop for Writer {
        fn drop(&mut self) {
            // Close the channel so the thread's `recv` ends and the process
            // does not wait on a parked writer at quit.
            self.tx = None;
        }
    }

    /// Temp file then rename, so a crash mid-write leaves the previous cache
    /// intact rather than a truncated one. Best effort throughout: a
    /// read-only folder just recomputes next session.
    fn write_atomically(path: &PathBuf, bytes: &[u8]) {
        let Some(dir) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        if std::fs::write(&tmp, bytes).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lp-signalcache-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn write_photo(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, bytes).expect("write fixture");
        p
    }

    #[test]
    fn a_recorded_signal_is_there_on_the_next_visit() {
        let dir = unique_dir("roundtrip");
        let photo = write_photo(&dir, "a.jpg", b"pixels");

        {
            let mut cache = SignalCache::load(&dir);
            cache.record(&photo, Signal::Sharpness(12.5));
            cache.record(&photo, Signal::PHash(0xdead_beef));
            cache.record(
                &photo,
                Signal::Faces(FaceQuality {
                    faces: 2,
                    min_eye_openness: Some(0.31),
                }),
            );
            cache.flush_blocking(Duration::from_secs(5));
        }

        let reopened = SignalCache::load(&dir);
        let s = reopened.get(&photo).expect("the entry survives a reload");
        assert_eq!(s.sharpness, Some(12.5));
        assert_eq!(s.phash, Some(0xdead_beef));
        assert_eq!(s.faces.expect("faces recorded").faces, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The defect this guards is serving a signal computed from bytes the file
    /// no longer has, which would show a stale blink verdict forever.
    #[test]
    fn a_photo_edited_since_its_signals_were_computed_is_a_miss() {
        let dir = unique_dir("invalidate");
        let photo = write_photo(&dir, "a.jpg", b"pixels");

        {
            let mut cache = SignalCache::load(&dir);
            cache.record(&photo, Signal::Sharpness(12.5));
            cache.flush_blocking(Duration::from_secs(5));
        }
        assert!(
            SignalCache::load(&dir).get(&photo).is_some(),
            "the unchanged file must hit"
        );

        write_photo(&dir, "a.jpg", b"different pixels, different length");
        assert!(
            SignalCache::load(&dir).get(&photo).is_none(),
            "a changed file must not serve its old signals"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_corrupt_cache_file_recomputes_instead_of_failing() {
        let dir = unique_dir("corrupt");
        let photo = write_photo(&dir, "a.jpg", b"pixels");
        let cache_dir = dir.join(crate::catalog::SIDECAR_DIR);
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::write(cache_dir.join(CACHE_FILE), b"{not json at all").unwrap();

        let cache = SignalCache::load(&dir);
        assert!(cache.get(&photo).is_none(), "a corrupt file is a miss");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_photo_stops_taking_up_room_in_the_file() {
        let dir = unique_dir("sweep");
        let kept = write_photo(&dir, "kept.jpg", b"pixels");
        let gone = write_photo(&dir, "gone.jpg", b"pixels");

        let mut cache = SignalCache::load(&dir);
        cache.record(&kept, Signal::Sharpness(1.0));
        cache.record(&gone, Signal::Sharpness(2.0));
        std::fs::remove_file(&gone).unwrap();
        cache.flush_blocking(Duration::from_secs(5));

        let reopened = SignalCache::load(&dir);
        assert!(reopened.get(&kept).is_some(), "the live photo is kept");
        assert!(
            reopened.entries.get(gone.file_name().unwrap()).is_none(),
            "the deleted photo's entry is swept"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forgetting_a_photo_drops_what_was_known_about_it() {
        let dir = unique_dir("forget");
        let photo = write_photo(&dir, "a.jpg", b"pixels");

        let mut cache = SignalCache::load(&dir);
        cache.record(&photo, Signal::Sharpness(1.0));
        assert!(cache.get(&photo).is_some());
        cache.forget(&photo);
        assert!(cache.get(&photo).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_capture_time_round_trips_as_a_real_answer() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_123);
        assert_eq!(
            CaptureTime::from_system_time(Some(t)).to_system_time(),
            Some(t)
        );
        assert_eq!(
            CaptureTime::from_system_time(None),
            CaptureTime::Unreadable,
            "a failed read is recorded, so the next session does not retry it"
        );
        assert_eq!(CaptureTime::Unreadable.to_system_time(), None);
    }

    #[test]
    fn a_cache_attached_to_no_folder_records_nothing() {
        let dir = unique_dir("empty");
        let photo = write_photo(&dir, "a.jpg", b"pixels");

        let mut cache = SignalCache::empty();
        cache.record(&photo, Signal::Sharpness(1.0));
        assert!(cache.get(&photo).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
