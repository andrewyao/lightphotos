// SPDX-License-Identifier: MIT OR Apache-2.0

//! Derived per-photo signals, remembered between sessions.
//!
//! Capture time and face quality are both functions of one photo's
//! bytes, and face quality is expensive. On 6016x6016 photos a face
//! analysis measures about 73 ms because Vision decodes the file itself at
//! full resolution, against 32 ms for a whole thumbnail decode. Recomputing
//! them on every folder visit would make grouping too heavy to run unasked.
//!
//! These are deliberately *not* fields on [`ImageRecord`](crate::catalog::ImageRecord).
//! That record is the user's authored edit state, the thing worth backing up,
//! and its sidecar is deleted once it holds nothing. A cached signal living
//! there would keep the sidecar alive forever, would rewrite it on every
//! background computation rather than on a user edit, and would have nowhere
//! to put an invalidation key. Authored state and derived state have different
//! lifetimes, so they get different containers. The quality score is the
//! exception: it is measured on the edited photo, only on request, and keys
//! its staleness to the edits, so it lives in the sidecar beside them.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::scoring::facequality::FaceQuality;

/// Cache file name, inside `<photo dir>/.lightphotos/`. Neither `.xmp` nor
/// `.thumb.jpg`, so the sidecar scan and the thumbnail sweep both skip it.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const CACHE_FILE: &str = "signals.json";

/// How long a `record` may leave new signals unwritten. A crash loses at most
/// this much recomputation, and the folder switch flushes anyway.
#[cfg(not(target_arch = "wasm32"))]
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

    #[cfg(not(target_arch = "wasm32"))]
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
    Faces(FaceQuality),
}

/// What is known about one photo. Every field is optional because the three
/// signals are computed by different passes at different times.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq)]
pub struct PhotoSignals {
    /// Validity key over the file's mtime and length. A photo edited by
    /// another program gets a new key, and every signal below is discarded.
    #[serde(rename = "k")]
    key: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capture: Option<CaptureTime>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faces: Option<FaceQuality>,
}

impl PhotoSignals {
    #[cfg(not(target_arch = "wasm32"))]
    fn is_empty(&self) -> bool {
        self.capture.is_none() && self.faces.is_none()
    }
}

/// Validity key from the source's mtime and length, the same shape as
/// [`crate::thumbnail::cache_key`]. The path is not hashed, so moving a folder
/// keeps its cache valid. Bump the version string to discard every entry.
#[cfg(not(target_arch = "wasm32"))]
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
#[cfg(not(target_arch = "wasm32"))]
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

    /// A cache for `dir` that records in memory and never touches the disk.
    /// It stands in while `dir`'s file loads on another thread, and
    /// [`absorb`](Self::absorb) hands what it recorded to the loaded cache.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn detached(dir: &Path) -> SignalCache {
        let mut cache = SignalCache::empty();
        cache.dir = Some(dir.to_path_buf());
        cache
    }

    /// The folder this cache belongs to.
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    /// Take `newer`'s entries on top of this cache's. A field `newer` has wins,
    /// and an entry for different bytes replaces the old one outright.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn absorb(&mut self, newer: SignalCache) {
        for (name, n) in &newer.entries {
            let entry = self.entries.entry(name.clone()).or_default();
            if entry.key != n.key {
                *entry = *n;
            } else {
                entry.capture = n.capture.or(entry.capture);
                entry.faces = n.faces.or(entry.faces);
            }
            self.dirty = true;
        }
    }

    /// What is known about `path`, or `None` when nothing is, or when the file
    /// changed since the signals were computed.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn get(&self, path: &Path) -> Option<&PhotoSignals> {
        let entry = self.entries.get(path.file_name()?)?;
        (current_key(path)? == entry.key).then_some(entry)
    }

    /// Remember one signal. A photo whose bytes changed since the last visit
    /// starts a fresh entry rather than mixing old signals with new.
    ///
    /// Entries are keyed by file name, so a photo outside this folder is
    /// dropped. Workers are not cancelled on a folder switch, and their late
    /// results would otherwise land on a same-named photo here.
    pub fn record(&mut self, path: &Path, signal: Signal) {
        if self.dir.as_deref() != path.parent() {
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
    #[hotpath::measure]
    fn queue_write(&mut self) {
        if self.writer.is_none() {
            return;
        }
        self.sweep_orphans();
        let (Some(dir), Some(writer)) = (&self.dir, &self.writer) else {
            return;
        };
        // A photo name is stored as a `String`, so a non-UTF-8 filename simply
        // never caches. Recomputing is always correct, and the alternative is
        // an encoding in the file format that only a few filenames would use.
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
    fn write_atomically(path: &std::path::Path, bytes: &[u8]) {
        let Some(dir) = path.parent() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let _ = crate::paths::write_atomic(path, bytes);
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

    fn faces(n: u32) -> Signal {
        Signal::Faces(FaceQuality {
            faces: n,
            min_eye_openness: None,
        })
    }

    fn face_count(s: &PhotoSignals) -> Option<u32> {
        s.faces.map(|f| f.faces)
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
            cache.record(&photo, Signal::Capture(None));
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
        assert_eq!(s.capture, Some(CaptureTime::Unreadable));
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
            cache.record(&photo, faces(12));
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
    fn a_cache_file_with_removed_fields_still_loads() {
        let dir = unique_dir("old-fields");
        let photo = write_photo(&dir, "a.jpg", b"pixels");
        let key = current_key(&photo).expect("the photo has a key");
        let cache_dir = dir.join(crate::catalog::SIDECAR_DIR);
        std::fs::create_dir_all(&cache_dir).unwrap();
        let file = cache_dir.join(CACHE_FILE);
        std::fs::write(
            &file,
            format!(
                r#"{{"entries":{{"a.jpg":{{"k":{key},"sharpness":12.5,"phash":3735928559,"faces":{{"faces":12,"min_eye_openness":null}}}}}}}}"#
            ),
        )
        .unwrap();

        let mut cache = SignalCache::load(&dir);
        let s = cache.get(&photo).expect("the old entry is a hit");
        assert_eq!(face_count(s), Some(12));

        cache.record(&photo, faces(13));
        cache.flush_blocking(Duration::from_secs(5));
        let written = std::fs::read_to_string(&file).unwrap();
        assert!(
            written.contains(r#""faces":13"#),
            "the new value is written: {written}"
        );
        for gone in ["phash", "sharpness"] {
            assert!(
                !written.contains(gone),
                "the next write drops {gone}: {written}"
            );
        }

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
        cache.record(&kept, faces(1));
        cache.record(&gone, faces(2));
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
        cache.record(&photo, faces(1));
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

    /// A face analysis submitted in the previous folder can finish after the
    /// switch. Camera folders reuse names like `IMG_0001.JPG`, so keying by
    /// name alone would file it under this folder's photo of the same name.
    #[test]
    fn a_signal_for_a_photo_in_another_folder_is_ignored() {
        let here = unique_dir("here");
        let elsewhere = unique_dir("elsewhere");
        let ours = write_photo(&here, "IMG_0001.JPG", b"ours");
        let theirs = write_photo(&elsewhere, "IMG_0001.JPG", b"a different photo");

        let mut cache = SignalCache::load(&here);
        cache.record(&ours, faces(1));
        cache.record(&theirs, faces(99));

        let s = cache.get(&ours).expect("our photo's entry survives");
        assert_eq!(face_count(s), Some(1));

        let _ = std::fs::remove_dir_all(&here);
        let _ = std::fs::remove_dir_all(&elsewhere);
    }

    /// Signals computed while the folder's file was still loading must reach
    /// the loaded cache, and must win over what the file held for them.
    #[test]
    fn a_detached_cache_hands_its_records_to_the_loaded_one() {
        let dir = unique_dir("absorb");
        let a = write_photo(&dir, "a.jpg", b"pixels");
        let b = write_photo(&dir, "b.jpg", b"other pixels");
        {
            let mut cache = SignalCache::load(&dir);
            cache.record(&a, faces(1));
            cache.record(&a, Signal::Capture(None));
            cache.flush_blocking(Duration::from_secs(5));
        }

        let mut pending = SignalCache::detached(&dir);
        pending.record(&a, faces(2));
        pending.record(&b, faces(3));
        pending.flush_blocking(Duration::from_secs(5));
        assert!(
            SignalCache::load(&dir).get(&b).is_none(),
            "a detached cache never writes, so it cannot race the load"
        );

        let mut loaded = SignalCache::load(&dir);
        loaded.absorb(pending);
        let sa = loaded.get(&a).expect("a is known");
        assert_eq!(face_count(sa), Some(2), "the newer value wins");
        assert_eq!(
            sa.capture,
            Some(CaptureTime::Unreadable),
            "a field only the file had survives"
        );
        assert_eq!(loaded.get(&b).and_then(face_count), Some(3));

        loaded.flush_blocking(Duration::from_secs(5));
        assert_eq!(
            SignalCache::load(&dir).get(&b).and_then(face_count),
            Some(3),
            "the absorbed records are written with the next flush"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_cache_attached_to_no_folder_records_nothing() {
        let dir = unique_dir("empty");
        let photo = write_photo(&dir, "a.jpg", b"pixels");

        let mut cache = SignalCache::empty();
        cache.record(&photo, faces(1));
        assert!(cache.get(&photo).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
