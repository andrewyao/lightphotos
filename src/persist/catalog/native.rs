// SPDX-License-Identifier: MIT OR Apache-2.0

//! The catalog on disk, and the thread that writes it.
//!
//! Every write is a whole file, `catalog.json` or one shard, so the queue
//! keeps only the newest bytes per file: rating twenty photos in a row
//! writes their directory's shard once or twice, not twenty times.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

#[cfg(test)]
use super::DirMarks;
use super::{Catalog, CatalogError, CatalogRef, Record};
use crate::paths::{remove_if_present, write_atomic_in_dir, CappedReadError};

const RECORD_FILE: &str = "catalog.json";
const PRESETS_FILE: &str = "presets.json";
const MARKS_DIR: &str = "marks";

static DEFAULT_DIR_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Where `--drive` keeps its catalogs, so a drive run never touches
/// `~/Pictures`. Set once, before the first catalog opens.
pub(crate) fn override_default_dir(dir: PathBuf) {
    let _ = DEFAULT_DIR_OVERRIDE.set(dir);
}

/// The folder new catalogs go in and the default one lives in:
/// `~/Pictures/LightPhotos`, or the override. Tests never call it: their
/// catalogs are in memory or at an explicit path.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn default_dir() -> Option<PathBuf> {
    if let Some(dir) = DEFAULT_DIR_OVERRIDE.get() {
        return Some(dir.clone());
    }
    default_dir_from(
        std::env::var_os("HOME").as_deref(),
        std::env::var_os("USERPROFILE").as_deref(),
    )
}

fn default_dir_from(
    home: Option<&std::ffi::OsStr>,
    userprofile: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    let home = if cfg!(windows) { userprofile } else { home }?;
    Some(PathBuf::from(home).join("Pictures").join("LightPhotos"))
}

impl Catalog {
    /// Open the catalog at `at`. A missing `catalog.json` is an empty
    /// catalog; one that will not parse is an error, and nothing here ever
    /// writes over it.
    pub(crate) fn open(at: CatalogRef) -> Result<Catalog, CatalogError> {
        let path = at.path();
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(CatalogError::NotACatalog(path.to_path_buf())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(CatalogError::Missing(path.to_path_buf()))
            }
            Err(e) => return Err(CatalogError::Io(path.to_path_buf(), e)),
        }
        let record = path.join(RECORD_FILE);
        let folders = match read_capped(&record, super::MAX_RECORD_BYTES) {
            Ok(Some(bytes)) => {
                super::parse_record(&bytes).map_err(|e| CatalogError::Parse(record, e))?
            }
            Ok(None) => Vec::new(),
            Err(e) => return Err(e),
        };
        let presets = path.join(PRESETS_FILE);
        let mut catalog = Catalog::on_disk(at);
        catalog.load_record(folders);
        // The error leaves the path out, as `land_presets` names the file.
        let stored =
            crate::paths::read_capped(&presets, super::MAX_RECORD_BYTES).map_err(|e| e.to_string());
        catalog.land_presets(&presets.display().to_string(), stored);
        Ok(catalog)
    }

    /// Create a new catalog at `at` listing `folders`, writing its
    /// `catalog.json` before returning. Refused where a catalog already is,
    /// or where anything that is not an empty directory is.
    pub(crate) fn create_at(
        at: CatalogRef,
        folders: Vec<PathBuf>,
    ) -> Result<Catalog, CatalogError> {
        let path = at.path();
        if path.join(RECORD_FILE).exists() {
            return Err(CatalogError::Exists(path.to_path_buf()));
        }
        match std::fs::metadata(path) {
            Ok(m) if !m.is_dir() => return Err(CatalogError::NotACatalog(path.to_path_buf())),
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(CatalogError::Io(path.to_path_buf(), e)),
        }
        let mut tidy = Vec::new();
        for dir in folders.iter().take(super::MAX_FOLDERS) {
            super::add_root(&mut tidy, dir);
        }
        let bytes = super::record_bytes(&tidy)
            .map_err(|e| CatalogError::Parse(path.join(RECORD_FILE), e))?;
        write_atomic_in_dir(&path.join(RECORD_FILE), &bytes)
            .map_err(|e| CatalogError::Io(path.to_path_buf(), e))?;
        let mut catalog = Catalog::on_disk(at);
        catalog.folders = Record::Loaded(tidy);
        catalog.preset_folders = Record::Loaded(Vec::new());
        Ok(catalog)
    }

    /// `open`, or `create_at` with `seed` when `at` is not there yet.
    pub(crate) fn open_or_create(
        at: CatalogRef,
        seed: Vec<PathBuf>,
    ) -> Result<Catalog, CatalogError> {
        match Catalog::open(at.clone()) {
            Err(CatalogError::Missing(_)) => Catalog::create_at(at, seed),
            other => other,
        }
    }

    /// A catalog with a writer and its record still to come.
    fn on_disk(at: CatalogRef) -> Catalog {
        let mut catalog = Catalog::empty(at);
        catalog.writer = Some(Writer::new());
        catalog.folders = Record::pending();
        catalog.preset_folders = Record::pending();
        catalog
    }

    /// The shard for `dir` as stored, or `None` when there is none, it will
    /// not parse, or it is for another directory. The next projection
    /// replaces a bad one. Only tests read shards until the cross-folder
    /// reader exists.
    #[cfg(test)]
    pub(crate) fn load_shard(&self, dir: &Path) -> Option<DirMarks> {
        let file = self.shard_path(dir);
        let bytes = read_capped(&file, super::MAX_SHARD_BYTES).ok()??;
        match super::parse_shard(&bytes, dir) {
            Ok(marks) => Some(marks),
            Err(e) => {
                eprintln!("[catalog] ignoring shard {}: {e}", file.display());
                None
            }
        }
    }

    pub(crate) fn shard_path(&self, dir: &Path) -> PathBuf {
        self.at.path().join(MARKS_DIR).join(super::shard_name(dir))
    }

    pub(super) fn save_record(&mut self) {
        let Record::Loaded(folders) = &self.folders else {
            return;
        };
        let Some(writer) = &mut self.writer else {
            return;
        };
        match super::record_bytes(folders) {
            Ok(bytes) => writer.put(self.at.path().join(RECORD_FILE), Some(bytes)),
            Err(e) => self.note_error(e),
        }
    }

    pub(super) fn save_presets(&mut self) {
        let Record::Loaded(folders) = &self.preset_folders else {
            return;
        };
        let Some(writer) = &mut self.writer else {
            return;
        };
        match super::record_bytes(folders) {
            Ok(bytes) => writer.put(self.at.path().join(PRESETS_FILE), Some(bytes)),
            Err(e) => self.note_error(e),
        }
    }

    pub(super) fn put_shard(&mut self, dir: &Path, bytes: Option<Vec<u8>>) {
        let file = self.shard_path(dir);
        if let Some(writer) = &mut self.writer {
            writer.put(file, bytes);
        }
    }

    /// Surface finished writes' failures. Once per frame.
    pub(crate) fn pump(&mut self) {
        let errors = self.writer.as_mut().map(Writer::pump).unwrap_or_default();
        for e in errors {
            self.note_error(e);
        }
    }

    /// Files not yet on disk.
    pub(crate) fn backlog(&self) -> usize {
        self.writer.as_ref().map_or(0, Writer::backlog)
    }

    /// Wait for every queued write, or `timeout`.
    pub(crate) fn flush_blocking(&mut self, timeout: Duration) {
        let errors = self
            .writer
            .as_mut()
            .map(|w| w.flush_blocking(timeout))
            .unwrap_or_default();
        for e in errors {
            self.note_error(e);
        }
    }
}

/// `file`'s bytes, `None` when it is not there, an error past `max`.
fn read_capped(file: &Path, max: usize) -> Result<Option<Vec<u8>>, CatalogError> {
    crate::paths::read_capped(file, max).map_err(|e| match e {
        CappedReadError::Io(e) => CatalogError::Io(file.to_path_buf(), e),
        CappedReadError::TooBig(max) => CatalogError::TooBig(file.to_path_buf(), max),
    })
}

/// Newest bytes per file, in first-queued order. `None` deletes the file.
#[derive(Default)]
struct Queue {
    order: VecDeque<PathBuf>,
    latest: HashMap<PathBuf, Option<Vec<u8>>>,
    in_flight: usize,
    closed: bool,
}

impl Queue {
    /// Queue `bytes` for `file`, replacing anything queued for it.
    fn push(&mut self, file: PathBuf, bytes: Option<Vec<u8>>) {
        if self.latest.insert(file.clone(), bytes).is_none() {
            self.order.push_back(file);
        }
    }
}

/// The write thread, started on the first write.
pub(super) struct Writer {
    queue: Arc<(Mutex<Queue>, Condvar)>,
    done_tx: Sender<String>,
    done_rx: Receiver<String>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Writer {
    fn new() -> Writer {
        let (done_tx, done_rx) = channel();
        Writer {
            queue: Arc::new((Mutex::new(Queue::default()), Condvar::new())),
            done_tx,
            done_rx,
            worker: None,
        }
    }

    fn put(&mut self, file: PathBuf, bytes: Option<Vec<u8>>) {
        if self.worker.is_none() {
            self.start();
        }
        if self.worker.is_none() {
            // No thread: do it here rather than lose the write.
            if let Err(e) = perform(&file, bytes.as_deref()) {
                let _ = self.done_tx.send(e);
            }
            return;
        }
        let (lock, wake) = &*self.queue;
        lock.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(file, bytes);
        wake.notify_all();
    }

    fn start(&mut self) {
        let queue = self.queue.clone();
        let done_tx = self.done_tx.clone();
        let spawned = std::thread::Builder::new()
            .name("catalog-write".into())
            .spawn(move || {
                let (lock, wake) = &*queue;
                loop {
                    let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    let next = loop {
                        if let Some(file) = q.order.pop_front() {
                            break Some(file);
                        }
                        if q.closed {
                            break None;
                        }
                        q = wake.wait(q).unwrap_or_else(PoisonError::into_inner);
                    };
                    let Some(file) = next else { return };
                    let bytes = q.latest.remove(&file).flatten();
                    q.in_flight += 1;
                    drop(q);
                    // The error goes out before the write counts as done, so
                    // a flush that wakes on the count always collects it.
                    if let Err(e) = perform(&file, bytes.as_deref()) {
                        let _ = done_tx.send(e);
                    }
                    let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
                    q.in_flight -= 1;
                    wake.notify_all();
                }
            });
        match spawned {
            Ok(handle) => self.worker = Some(handle),
            Err(e) => eprintln!("[catalog] could not start the catalog writer: {e}"),
        }
    }

    fn backlog(&self) -> usize {
        let q = self.queue.0.lock().unwrap_or_else(PoisonError::into_inner);
        q.order.len() + q.in_flight
    }

    fn pump(&mut self) -> Vec<String> {
        self.done_rx.try_iter().collect()
    }

    fn flush_blocking(&mut self, timeout: Duration) -> Vec<String> {
        let deadline = Instant::now() + timeout;
        let (lock, wake) = &*self.queue;
        let mut q = lock.lock().unwrap_or_else(PoisonError::into_inner);
        while q.order.len() + q.in_flight > 0 {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            q = wake
                .wait_timeout(q, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        drop(q);
        self.pump()
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        // The thread finishes the queue and exits, so a dropped catalog never
        // loses a shard. The join is unbounded for the same reason the
        // sidecar writer's is: a write is worth more than a prompt exit.
        {
            let (lock, wake) = &*self.queue;
            lock.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
            wake.notify_all();
        }
        if let Some(handle) = self.worker.take() {
            let _ = handle.join();
        }
    }
}

fn perform(file: &Path, bytes: Option<&[u8]>) -> Result<(), String> {
    match bytes {
        Some(bytes) => write_atomic_in_dir(file, bytes),
        None => remove_if_present(file),
    }
    .map_err(|e| format!("{}: {e}", file.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::catalog::{PhotoMarks, EXT};
    use crate::sidecar::{ColorLabel, Flag};

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-catalog-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn marks(name: &str, rating: Option<u8>, flag: Option<Flag>) -> DirMarks {
        let mut m = DirMarks::new();
        m.insert(
            name.into(),
            PhotoMarks {
                rating,
                flag,
                label: None,
            },
        );
        m
    }

    #[test]
    fn a_catalog_ref_needs_a_named_lightphotos_directory() {
        for bad in [".lightphotos", "a", "a.lightphoto", "", "x/.lightphotos"] {
            assert!(CatalogRef::parse(Path::new(bad)).is_err(), "{bad} parsed");
        }
        let at = CatalogRef::parse(Path::new("/p/Trip 2024.lightphotos")).unwrap();
        assert_eq!(at.name(), "Trip 2024");
        assert!(CatalogRef::in_dir(Path::new("/p"), "").is_err());
        assert!(CatalogRef::in_dir(Path::new("/p"), "a/b").is_err());
        assert_eq!(
            CatalogRef::in_dir(Path::new("/p"), "trip").unwrap().path(),
            Path::new("/p/trip.lightphotos")
        );
        assert!(CatalogRef::is_catalog_name("x.lightphotos"));
        assert!(!CatalogRef::is_catalog_name(".lightphotos"));
        assert!(!CatalogRef::is_catalog_name("x.lightphotos.bak"));
    }

    #[test]
    fn the_default_dir_is_pictures_lightphotos_under_home() {
        let got = default_dir_from(
            Some(std::ffi::OsStr::new("/Users/x")),
            Some(std::ffi::OsStr::new("C:\\Users\\x")),
        )
        .unwrap();
        if cfg!(windows) {
            assert_eq!(
                got,
                PathBuf::from("C:\\Users\\x")
                    .join("Pictures")
                    .join("LightPhotos")
            );
        } else {
            assert_eq!(got, PathBuf::from("/Users/x/Pictures/LightPhotos"));
        }
        assert_eq!(default_dir_from(None, None), None);
    }

    #[test]
    fn create_then_open_round_trips_the_folders() {
        let dir = scratch("roundtrip");
        let at = CatalogRef::in_dir(&dir, "trip").unwrap();
        let folders = vec![dir.join("b"), dir.join("a"), dir.join("a/inner")];
        let created = Catalog::create_at(at.clone(), folders).unwrap();
        assert_eq!(created.folders(), &[dir.join("a"), dir.join("b")]);
        drop(created);

        let opened = Catalog::open(at.clone()).unwrap();
        assert_eq!(opened.folders(), &[dir.join("a"), dir.join("b")]);
        assert!(matches!(
            Catalog::create_at(at, Vec::new()),
            Err(CatalogError::Exists(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_catalog_is_missing_and_a_file_is_not_a_catalog() {
        let dir = scratch("missing");
        let at = CatalogRef::in_dir(&dir, "nope").unwrap();
        assert!(matches!(Catalog::open(at), Err(CatalogError::Missing(_))));
        let file = dir.join(format!("file.{EXT}"));
        std::fs::write(&file, b"x").unwrap();
        let at = CatalogRef::parse(&file).unwrap();
        assert!(matches!(
            Catalog::open(at.clone()),
            Err(CatalogError::NotACatalog(_))
        ));
        assert!(matches!(
            Catalog::create_at(at, Vec::new()),
            Err(CatalogError::NotACatalog(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unparseable_record_is_an_error_and_is_never_overwritten() {
        let dir = scratch("bad-record");
        let at = CatalogRef::in_dir(&dir, "bad").unwrap();
        std::fs::create_dir_all(at.path()).unwrap();
        for bad in [
            &b"{\"v\":1,\"folders\":[\"/a\""[..],
            b"[]",
            b"",
            b"\xff\xfe",
        ] {
            std::fs::write(at.path().join(RECORD_FILE), bad).unwrap();
            assert!(
                matches!(Catalog::open(at.clone()), Err(CatalogError::Parse(..))),
                "{bad:?}"
            );
            assert!(matches!(
                Catalog::open_or_create(at.clone(), vec![dir.clone()]),
                Err(CatalogError::Parse(..))
            ));
            assert_eq!(
                std::fs::read(at.path().join(RECORD_FILE)).unwrap(),
                bad,
                "the bad record is left for the user"
            );
        }
        std::fs::write(
            at.path().join(RECORD_FILE),
            vec![b' '; super::super::MAX_RECORD_BYTES + 1],
        )
        .unwrap();
        assert!(matches!(Catalog::open(at), Err(CatalogError::TooBig(..))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn folder_changes_reach_catalog_json() {
        let dir = scratch("folders");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), Vec::new()).unwrap();
        assert!(cat.add_folder(&dir.join("p")));
        assert!(!cat.add_folder(&dir.join("p/sub")));
        cat.flush_blocking(Duration::from_secs(10));
        assert_eq!(
            Catalog::open(at.clone()).unwrap().folders(),
            &[dir.join("p")]
        );
        assert!(cat.remove_folder(&dir.join("p")));
        assert!(!cat.remove_folder(&dir.join("p")));
        cat.flush_blocking(Duration::from_secs(10));
        assert!(Catalog::open(at).unwrap().folders().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches the list landing in `catalog.json`, which v0.1.26 and
    /// v0.1.27 rewrite without it, or not coming back on open.
    #[test]
    fn preset_folders_round_trip_through_their_own_file() {
        let dir = scratch("presets-roundtrip");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), vec![dir.join("photos")]).unwrap();
        cat.flush_blocking(Duration::from_secs(10));
        let record = at.path().join(RECORD_FILE);
        let before = std::fs::read(&record).unwrap();

        assert!(cat.add_preset_folder(&dir.join("Presets/Film")).unwrap());
        assert!(cat.add_preset_folder(&dir.join("Presets/Travel")).unwrap());
        assert!(
            !cat.add_preset_folder(&dir.join("Presets/Film/Kodak"))
                .unwrap(),
            "a folder inside one already listed is already scanned"
        );
        cat.flush_blocking(Duration::from_secs(10));

        assert_eq!(
            std::fs::read(&record).unwrap(),
            before,
            "catalog.json is not touched"
        );
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(at.path().join(PRESETS_FILE)).unwrap()).unwrap();
        assert_eq!(json["v"], 1);
        let want = [dir.join("Presets/Film"), dir.join("Presets/Travel")];
        let opened = Catalog::open(at.clone()).unwrap();
        assert_eq!(opened.preset_folders(), want);
        assert_eq!(opened.folders(), [dir.join("photos")]);
        drop(opened);

        assert!(cat.remove_preset_folder(&dir.join("Presets/Film")));
        assert!(!cat.remove_preset_folder(&dir.join("Presets/Film")));
        cat.add_folder(&dir.join("more photos"));
        cat.flush_blocking(Duration::from_secs(10));
        let opened = Catalog::open(at).unwrap();
        assert_eq!(opened.preset_folders(), [dir.join("Presets/Travel")]);
        assert_eq!(
            opened.folders(),
            [dir.join("more photos"), dir.join("photos")],
            "a photo-folder change keeps the preset folders"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches the file's path twice in the message, once from the read
    /// and once from the record.
    #[test]
    fn a_presets_file_that_cannot_be_read_names_it_once() {
        let dir = scratch("presets-unreadable");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        std::fs::create_dir_all(at.path().join(PRESETS_FILE)).unwrap();

        let mut cat = Catalog::open(at).unwrap();

        let why = cat.take_presets_error().unwrap_or_default();
        assert_eq!(why.matches(PRESETS_FILE).count(), 1, "{why}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A catalog from before preset folders has no `presets.json`.
    #[test]
    fn a_catalog_with_no_presets_file_has_no_preset_folders() {
        let dir = scratch("presets-missing");
        let at = CatalogRef::in_dir(&dir, "old").unwrap();
        std::fs::create_dir_all(at.path()).unwrap();
        std::fs::write(at.path().join(RECORD_FILE), br#"{"v":1,"folders":["/p"]}"#).unwrap();

        let mut cat = Catalog::open(at.clone()).unwrap();

        assert!(cat.preset_folders().is_empty());
        assert!(cat.take_error().is_none());
        cat.add_folder(Path::new("/q"));
        cat.flush_blocking(Duration::from_secs(10));
        assert!(
            !at.path().join(PRESETS_FILE).exists(),
            "a photo-folder change writes no presets file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Catches a broken preset list failing the whole catalog, or being
    /// replaced by the next add.
    #[test]
    fn an_unparseable_presets_file_is_reported_and_never_overwritten() {
        let dir = scratch("presets-bad");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        std::fs::create_dir_all(at.path()).unwrap();
        std::fs::write(at.path().join(RECORD_FILE), br#"{"v":1,"folders":["/p"]}"#).unwrap();
        let file = at.path().join(PRESETS_FILE);
        for bad in [
            &br#"{"v":1,"folders":["/a""#[..],
            b"[]",
            b"\xff\xfe",
            br#"{"v":1,"folders":[7]}"#,
        ] {
            std::fs::write(&file, bad).unwrap();
            let mut cat = Catalog::open(at.clone()).unwrap();
            assert_eq!(
                cat.folders(),
                [PathBuf::from("/p")],
                "the catalog still opens"
            );
            let why = cat.take_presets_error();
            assert!(
                why.as_deref().is_some_and(|w| w.contains("presets.json")),
                "{bad:?} is reported, naming the file: {why:?}"
            );
            assert!(cat.take_error().is_none(), "as no failed save");
            assert!(cat.preset_folders().is_empty());

            assert!(cat.add_preset_folder(Path::new("/presets")).unwrap());
            assert_eq!(cat.preset_folders(), [PathBuf::from("/presets")]);
            assert!(cat.remove_preset_folder(Path::new("/presets")));
            cat.flush_blocking(Duration::from_secs(10));
            assert_eq!(std::fs::read(&file).unwrap(), bad, "left for the user");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marks_land_in_a_shard_named_by_the_directory_and_empty_marks_delete_it() {
        let dir = scratch("shard");
        let photos = dir.join("photos");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), vec![photos.clone()]).unwrap();
        let mut m = marks("a.jpg", Some(4), Some(Flag::Pick));
        m.insert(
            "b.jpg".into(),
            PhotoMarks {
                label: Some(ColorLabel::Red),
                ..Default::default()
            },
        );
        cat.record_marks(&photos, &m);
        cat.flush_blocking(Duration::from_secs(10));
        let file = cat.shard_path(&photos);
        assert_eq!(file.parent().unwrap(), at.path().join(MARKS_DIR));
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
        assert_eq!(json["v"], 1);
        assert_eq!(json["dir"], photos.to_string_lossy().as_ref());
        assert_eq!(json["photos"]["a.jpg"]["rating"], 4);
        assert_eq!(json["photos"]["a.jpg"]["flag"], "pick");
        assert_eq!(json["photos"]["b.jpg"]["label"], "red");
        assert_eq!(cat.load_shard(&photos), Some(m));

        cat.record_marks(&photos, &DirMarks::new());
        cat.flush_blocking(Duration::from_secs(10));
        assert!(!file.exists(), "the last mark cleared deletes the shard");
        assert_eq!(cat.load_shard(&photos), None);
        assert!(cat.take_error().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_or_foreign_shard_is_ignored_and_then_replaced() {
        let dir = scratch("bad-shard");
        let photos = dir.join("photos");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at, vec![photos.clone()]).unwrap();
        let file = cat.shard_path(&photos);
        for bad in [
            &b"{"[..],
            br#"{"v":1,"dir":"/somewhere/else","photos":{"a.jpg":{"rating":1}}}"#,
            &[0u8; 3],
        ] {
            write_atomic_in_dir(&file, bad).unwrap();
            assert_eq!(cat.load_shard(&photos), None, "{bad:?}");
        }
        let m = marks("a.jpg", Some(2), None);
        cat.record_marks(&photos, &m);
        cat.flush_blocking(Duration::from_secs(10));
        assert_eq!(cat.load_shard(&photos), Some(m));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_queue_keeps_one_entry_per_file_holding_the_newest_bytes() {
        let mut q = Queue::default();
        let (a, b) = (PathBuf::from("/a"), PathBuf::from("/b"));
        q.push(a.clone(), Some(b"1".to_vec()));
        q.push(b.clone(), None);
        q.push(a.clone(), Some(b"2".to_vec()));
        q.push(a.clone(), None);
        assert_eq!(q.order, VecDeque::from([a.clone(), b]));
        assert_eq!(q.latest[&a], None, "the delete queued last wins");
    }

    #[test]
    fn the_last_marks_recorded_are_what_lands() {
        let dir = scratch("coalesce");
        let photos = dir.join("photos");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at, vec![photos.clone()]).unwrap();
        for stars in 1..=5 {
            cat.record_marks(&photos, &marks("a.jpg", Some(stars), None));
        }
        cat.flush_blocking(Duration::from_secs(10));
        assert_eq!(cat.backlog(), 0);
        assert_eq!(cat.load_shard(&photos).unwrap()["a.jpg"].rating, Some(5));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_write_failure_is_reported_once() {
        let dir = scratch("fail");
        let photos = dir.join("photos");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), vec![photos.clone()]).unwrap();
        std::fs::write(at.path().join(MARKS_DIR), b"not a directory").unwrap();
        cat.record_marks(&photos, &marks("a.jpg", Some(1), None));
        cat.flush_blocking(Duration::from_secs(10));
        assert!(cat.take_error().is_some());
        assert!(cat.take_error().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
