// SPDX-License-Identifier: MIT OR Apache-2.0

//! A catalog: a named set of top-level photo folders, the Folders panel's
//! roots, plus a disposable copy of each rated, flagged or labelled photo's
//! marks. The per-folder sidecars under [`crate::sidecar::SIDECAR_DIR`] stay
//! the truth; the copy here is a projection of them, rewritten whole from the
//! open folder's records, so it can be thrown away and rebuilt at any time.
//!
//! Natively a catalog is a directory `<name>.lightphotos` holding
//! `catalog.json` and one `marks/<fnv1a>.json` shard per photo directory.
//! The browser build keeps one in-memory catalog until its IndexedDB store
//! lands; `web.rs` is the seam it fills.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sidecar::{ColorLabel, Flag, ImageRecord};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native;
#[cfg(target_arch = "wasm32")]
mod web;

/// The directory extension that marks a catalog.
pub(crate) const EXT: &str = "lightphotos";
/// The name of the one catalog every install starts with.
pub(crate) const DEFAULT_NAME: &str = "default";
/// Format version written into `catalog.json` and each shard.
const VERSION: u32 = 1;
/// A `catalog.json` past this is refused rather than parsed: a thousand
/// folder paths fit in a tenth of it.
pub(crate) const MAX_RECORD_BYTES: usize = 1 << 20;
/// A shard past this is refused. A directory of 50 000 marked photos
/// serializes to a few megabytes.
pub(crate) const MAX_SHARD_BYTES: usize = 16 << 20;
/// Folders a catalog may list.
pub(crate) const MAX_FOLDERS: usize = 1000;

/// A validated catalog location. Natively a directory path whose last
/// component is `<non-empty stem>.lightphotos`; on the web a non-empty
/// name. Parsed once at the boundary and trusted after.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CatalogRef {
    #[cfg(not(target_arch = "wasm32"))]
    path: PathBuf,
    name: String,
}

impl CatalogRef {
    /// `path` as a catalog location, refused unless its last component is
    /// `<stem>.lightphotos` with a non-empty stem.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn parse(path: &Path) -> Result<CatalogRef, CatalogError> {
        let bad = || CatalogError::BadName(path.display().to_string());
        let file = path.file_name().and_then(|n| n.to_str()).ok_or_else(bad)?;
        let stem = file
            .strip_suffix(&format!(".{EXT}"))
            .filter(|s| !s.is_empty())
            .ok_or_else(bad)?;
        Ok(CatalogRef {
            path: path.to_path_buf(),
            name: stem.to_string(),
        })
    }

    /// The catalog `name` would be inside `dir`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn in_dir(dir: &Path, name: &str) -> Result<CatalogRef, CatalogError> {
        if name.is_empty() || name.contains(['/', '\\']) {
            return Err(CatalogError::BadName(name.to_string()));
        }
        CatalogRef::parse(&dir.join(format!("{name}.{EXT}")))
    }

    /// Whether `path` names a catalog directory, for the folder tree to skip.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn is_catalog_name(file_name: &str) -> bool {
        file_name
            .strip_suffix(&format!(".{EXT}"))
            .is_some_and(|stem| !stem.is_empty())
    }

    #[cfg(target_arch = "wasm32")]
    pub(crate) fn named(name: &str) -> Result<CatalogRef, CatalogError> {
        if name.trim().is_empty() || name.contains('\0') {
            return Err(CatalogError::BadName(name.to_string()));
        }
        Ok(CatalogRef {
            name: name.to_string(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

/// The culling marks on one photo. Never stored empty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PhotoMarks {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) rating: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) flag: Option<Flag>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) label: Option<ColorLabel>,
}

impl PhotoMarks {
    pub(crate) fn of(rec: &ImageRecord) -> PhotoMarks {
        PhotoMarks {
            rating: rec.rating,
            flag: rec.flag,
            label: rec.label,
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.rating.is_none() && self.flag.is_none() && self.label.is_none()
    }
}

/// One directory's marked photos by file name. Holds no empty entries.
pub(crate) type DirMarks = BTreeMap<String, PhotoMarks>;

#[derive(Debug)]
pub(crate) enum CatalogError {
    /// Not `<stem>.lightphotos`, or an empty name.
    BadName(String),
    /// The catalog directory is not there.
    Missing(PathBuf),
    /// The path exists and is not a catalog directory.
    NotACatalog(PathBuf),
    /// `create_at` on a catalog that already exists.
    Exists(PathBuf),
    Io(PathBuf, std::io::Error),
    /// A `catalog.json` that will not parse. Never overwritten.
    Parse(PathBuf, String),
    TooBig(PathBuf, usize),
    TooManyFolders(usize),
}

impl std::fmt::Display for CatalogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CatalogError::BadName(name) => write!(f, "{name} is not a <name>.{EXT} catalog"),
            CatalogError::Missing(p) => write!(f, "{} is not there", p.display()),
            CatalogError::NotACatalog(p) => write!(f, "{} is not a catalog", p.display()),
            CatalogError::Exists(p) => write!(f, "{} already exists", p.display()),
            CatalogError::Io(p, e) => write!(f, "{}: {e}", p.display()),
            CatalogError::Parse(p, e) => write!(f, "{} will not parse: {e}", p.display()),
            CatalogError::TooBig(p, max) => {
                write!(f, "{} is over the {max}-byte limit", p.display())
            }
            CatalogError::TooManyFolders(max) => {
                write!(f, "a catalog lists at most {max} folders")
            }
        }
    }
}

impl std::error::Error for CatalogError {}

/// The stored `catalog.json`.
#[derive(Serialize, Deserialize)]
struct RecordFile {
    v: u32,
    #[serde(default)]
    folders: Vec<PathBuf>,
}

/// One `marks/<hash>.json`. `dir` is inside so a hash collision or a shard
/// left from another path is caught and replaced.
#[derive(Serialize, Deserialize)]
struct Shard {
    v: u32,
    dir: PathBuf,
    #[serde(default)]
    photos: DirMarks,
}

/// The folder list, and whether the stored record has arrived. Native reads
/// it at open; the browser gets it from an async store. Until it lands root
/// changes stay in memory and merge into the record, never over it.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    Pending(Vec<PathBuf>),
    Loaded(Vec<PathBuf>),
}

impl Record {
    fn folders(&self) -> &[PathBuf] {
        match self {
            Record::Pending(f) | Record::Loaded(f) => f,
        }
    }

    fn folders_mut(&mut self) -> &mut Vec<PathBuf> {
        match self {
            Record::Pending(f) | Record::Loaded(f) => f,
        }
    }
}

/// Add `dir` to `roots`, which stays sorted with no root inside another.
/// A folder already in the tree changes nothing; a folder holding roots
/// takes their place. Returns whether `roots` changed.
pub(crate) fn add_root(roots: &mut Vec<PathBuf>, dir: &Path) -> bool {
    if roots.iter().any(|r| dir.starts_with(r)) {
        return false;
    }
    roots.retain(|r| !r.starts_with(dir));
    roots.push(dir.to_path_buf());
    roots.sort_by_cached_key(|r| r.to_string_lossy().to_lowercase());
    true
}

/// One open catalog: its roots and the marks copy's write queue.
pub(crate) struct Catalog {
    at: CatalogRef,
    folders: Record,
    /// Signature of the shard last queued per photo directory, so a
    /// projection that changed nothing writes nothing.
    written: HashMap<PathBuf, u64>,
    /// `None` for a catalog that is never written: a test's, or the web's
    /// until its store exists.
    #[cfg(not(target_arch = "wasm32"))]
    writer: Option<native::Writer>,
    last_error: Option<String>,
}

impl Catalog {
    /// A catalog that is never written anywhere.
    pub(crate) fn in_memory(name: &str) -> Catalog {
        #[cfg(not(target_arch = "wasm32"))]
        let at = CatalogRef {
            path: PathBuf::from(format!("{name}.{EXT}")),
            name: name.to_string(),
        };
        #[cfg(target_arch = "wasm32")]
        let at = CatalogRef {
            name: name.to_string(),
        };
        Catalog::empty(at)
    }

    /// No folders, no writer, record loaded.
    fn empty(at: CatalogRef) -> Catalog {
        Catalog {
            at,
            folders: Record::Loaded(Vec::new()),
            written: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            writer: None,
            last_error: None,
        }
    }

    pub(crate) fn at(&self) -> &CatalogRef {
        &self.at
    }

    pub(crate) fn name(&self) -> &str {
        self.at.name()
    }

    /// The Folders panel's roots, sorted, none inside another.
    pub(crate) fn folders(&self) -> &[PathBuf] {
        self.folders.folders()
    }

    /// Whether `dir` is one of the folders or inside one.
    pub(crate) fn contains(&self, dir: &Path) -> bool {
        self.folders().iter().any(|f| dir.starts_with(f))
    }

    /// Add `dir` as a root, by [`add_root`]'s rule. Returns whether the list
    /// changed. Past [`MAX_FOLDERS`] the add is refused and reported.
    pub(crate) fn add_folder(&mut self, dir: &Path) -> bool {
        let roots = self.folders.folders_mut();
        if roots.len() >= MAX_FOLDERS && !roots.iter().any(|r| dir.starts_with(r)) {
            self.last_error = Some(CatalogError::TooManyFolders(MAX_FOLDERS).to_string());
            return false;
        }
        let changed = add_root(roots, dir);
        if changed {
            self.save_record();
        }
        changed
    }

    /// Drop `dir` from the roots. Returns whether it was listed.
    pub(crate) fn remove_folder(&mut self, dir: &Path) -> bool {
        let roots = self.folders.folders_mut();
        let before = roots.len();
        roots.retain(|r| r != dir);
        let changed = roots.len() != before;
        if changed {
            self.save_record();
        }
        changed
    }

    /// The stored record arrived. Roots added while it was pending merge in
    /// on top of it.
    fn load_record(&mut self, stored: Vec<PathBuf>) {
        let mut folders = Vec::new();
        for dir in stored.iter().chain(self.folders.folders()) {
            add_root(&mut folders, dir);
        }
        let merged = folders != stored;
        self.folders = Record::Loaded(folders);
        if merged {
            self.save_record();
        }
    }

    /// The latest persist failure, each returned once.
    pub(crate) fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    fn note_error(&mut self, e: impl std::fmt::Display) {
        eprintln!("[catalog] {e}");
        self.last_error = Some(e.to_string());
    }

    /// Replace `dir`'s shard with `marks`, when `dir` is inside a catalog
    /// folder and the shard would differ from the one last queued. Empty
    /// marks delete the shard. Idempotent: the same marks twice queue once.
    pub(crate) fn record_marks(&mut self, dir: &Path, marks: &DirMarks) {
        if !self.contains(dir) {
            return;
        }
        let bytes = if marks.is_empty() {
            None
        } else {
            let shard = Shard {
                v: VERSION,
                dir: dir.to_path_buf(),
                photos: marks.clone(),
            };
            match serde_json::to_vec(&shard) {
                Ok(bytes) => Some(bytes),
                Err(e) => return self.note_error(format!("{}: {e}", dir.display())),
            }
        };
        let sig = bytes.as_deref().map_or(0, signature);
        if self.written.get(dir) == Some(&sig) {
            return;
        }
        self.written.insert(dir.to_path_buf(), sig);
        self.put_shard(dir, bytes);
    }
}

/// A stable hash of shard bytes, `0` reserved for "no shard".
fn signature(bytes: &[u8]) -> u64 {
    let mut h = crate::hash::Fnv1a::new();
    h.write(bytes);
    h.finish().max(1)
}

/// `marks/<fnv1a of the path>.json`, the shard file name for `dir`.
// The web stub keeps no shards yet, so only native reads these.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn shard_name(dir: &Path) -> String {
    let mut h = crate::hash::Fnv1a::new();
    h.write(dir.to_string_lossy().as_bytes());
    format!("{:016x}.json", h.finish())
}

/// Parse a shard's bytes, refusing one over the cap or not for `dir`.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn parse_shard(bytes: &[u8], dir: &Path) -> Result<DirMarks, String> {
    if bytes.len() > MAX_SHARD_BYTES {
        return Err(format!("{} bytes is over the shard limit", bytes.len()));
    }
    let shard: Shard = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if shard.dir != dir {
        return Err(format!("the shard is for {}", shard.dir.display()));
    }
    Ok(shard
        .photos
        .into_iter()
        .filter(|(_, m)| !m.is_empty())
        .collect())
}

/// Parse `catalog.json`'s bytes.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn parse_record(bytes: &[u8]) -> Result<Vec<PathBuf>, String> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(format!("{} bytes is over the catalog limit", bytes.len()));
    }
    let record: RecordFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if record.folders.len() > MAX_FOLDERS {
        return Err(CatalogError::TooManyFolders(MAX_FOLDERS).to_string());
    }
    let mut folders = Vec::new();
    for dir in &record.folders {
        add_root(&mut folders, dir);
    }
    Ok(folders)
}

#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
fn record_bytes(folders: &[PathBuf]) -> Result<Vec<u8>, String> {
    serde_json::to_vec(&RecordFile {
        v: VERSION,
        folders: folders.to_vec(),
    })
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn added_roots_stay_sorted_without_duplicates() {
        let mut roots = Vec::new();
        assert!(add_root(&mut roots, Path::new("/photos/zoo")));
        assert!(add_root(&mut roots, Path::new("/photos/Beach")));
        assert!(!add_root(&mut roots, Path::new("/photos/zoo")));
        assert_eq!(roots, paths(&["/photos/Beach", "/photos/zoo"]));
    }

    #[test]
    fn a_folder_inside_a_root_is_not_a_new_root() {
        let mut roots = paths(&["/photos"]);
        assert!(!add_root(&mut roots, Path::new("/photos/2024/june")));
        assert_eq!(roots, paths(&["/photos"]));
    }

    #[test]
    fn a_folder_holding_roots_takes_their_place() {
        let mut roots = paths(&["/photos/a", "/photos/b", "/other"]);
        assert!(add_root(&mut roots, Path::new("/photos")));
        assert_eq!(roots, paths(&["/other", "/photos"]));
    }

    #[test]
    fn a_pending_record_merges_into_the_loaded_one() {
        let mut cat = Catalog::in_memory("x");
        cat.folders = Record::Pending(paths(&["/added/while/pending"]));
        cat.load_record(paths(&["/stored"]));
        assert_eq!(
            cat.folders(),
            paths(&["/added/while/pending", "/stored"]).as_slice()
        );
        assert!(matches!(cat.folders, Record::Loaded(_)));
    }

    #[test]
    fn the_folder_cap_refuses_one_more_and_reports_it() {
        let mut cat = Catalog::in_memory("x");
        for i in 0..MAX_FOLDERS {
            assert!(cat.add_folder(&PathBuf::from(format!("/f{i}"))));
        }
        assert!(!cat.add_folder(Path::new("/one-more")));
        assert!(cat.take_error().is_some());
        assert!(
            !cat.add_folder(Path::new("/f1/inside")),
            "a folder inside a root is still not a new one"
        );
        assert_eq!(cat.folders().len(), MAX_FOLDERS);
    }

    #[test]
    fn marks_outside_every_folder_are_not_recorded() {
        let mut cat = Catalog::in_memory("x");
        cat.add_folder(Path::new("/photos"));
        let mut marks = DirMarks::new();
        marks.insert(
            "a.jpg".into(),
            PhotoMarks {
                rating: Some(3),
                ..Default::default()
            },
        );
        cat.record_marks(Path::new("/elsewhere"), &marks);
        assert!(cat.written.is_empty());
        cat.record_marks(Path::new("/photos/trip"), &marks);
        assert_eq!(cat.written.len(), 1);
    }

    #[test]
    fn a_shard_with_another_dir_inside_is_refused() {
        let bytes = br#"{"v":1,"dir":"/other","photos":{"a.jpg":{"rating":2}}}"#;
        assert!(parse_shard(bytes, Path::new("/photos")).is_err());
        let marks = parse_shard(bytes, Path::new("/other")).unwrap();
        assert_eq!(marks.get("a.jpg").and_then(|m| m.rating), Some(2));
    }

    #[test]
    fn malformed_and_truncated_shards_and_records_are_errors_not_panics() {
        let dir = Path::new("/photos");
        for garbage in [
            &b""[..],
            b"{",
            b"[]",
            b"null",
            br#"{"v":1,"dir":"/photos","photos":{"a.jpg":{"rating":2}"#,
            &[0xff, 0xfe, 0x00],
        ] {
            assert!(parse_shard(garbage, dir).is_err(), "{garbage:?}");
            assert!(parse_record(garbage).is_err(), "{garbage:?}");
        }
        for bad_shard in [
            &br#"{"v":1,"dir":"/photos","photos":{"a.jpg":{"rating":"x"}}}"#[..],
            br#"{"v":1,"dir":"/photos","photos":[1,2]}"#,
            br#"{"v":1,"dir":7,"photos":{}}"#,
            br#"{"v":1,"photos":{}}"#,
        ] {
            assert!(parse_shard(bad_shard, dir).is_err(), "{bad_shard:?}");
        }
        for bad_record in [
            &br#"{"v":1,"folders":"/a"}"#[..],
            br#"{"v":1,"folders":[1]}"#,
            br#"{"v":"1"}"#,
        ] {
            assert!(parse_record(bad_record).is_err(), "{bad_record:?}");
        }
        let too_big = vec![b' '; MAX_SHARD_BYTES + 1];
        assert!(parse_shard(&too_big, dir).is_err());
        let too_big = vec![b' '; MAX_RECORD_BYTES + 1];
        assert!(parse_record(&too_big).is_err());
        let too_many = format!(
            r#"{{"v":1,"folders":[{}]}}"#,
            (0..=MAX_FOLDERS)
                .map(|i| format!("\"/f{i}\""))
                .collect::<Vec<_>>()
                .join(",")
        );
        assert!(parse_record(too_many.as_bytes()).is_err());
    }

    #[test]
    fn an_empty_entry_in_a_shard_is_dropped_on_read() {
        let bytes = br#"{"v":1,"dir":"/p","photos":{"a.jpg":{},"b.jpg":{"flag":"pick"}}}"#;
        let marks = parse_shard(bytes, Path::new("/p")).unwrap();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks["b.jpg"].flag, Some(Flag::Pick));
    }

    #[test]
    fn a_record_tidies_its_folders_like_the_tree() {
        let bytes = br#"{"v":1,"folders":["/b","/a/x","/a"]}"#;
        assert_eq!(parse_record(bytes).unwrap(), paths(&["/a", "/b"]));
        assert_eq!(parse_record(br#"{"v":1}"#).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn recording_the_same_marks_twice_queues_once() {
        let mut cat = Catalog::in_memory("x");
        cat.add_folder(Path::new("/photos"));
        let mut marks = DirMarks::new();
        marks.insert(
            "a.jpg".into(),
            PhotoMarks {
                flag: Some(Flag::Reject),
                ..Default::default()
            },
        );
        cat.record_marks(Path::new("/photos"), &marks);
        let first = cat.written[Path::new("/photos")];
        cat.record_marks(Path::new("/photos"), &marks);
        assert_eq!(cat.written[Path::new("/photos")], first);
        cat.record_marks(Path::new("/photos"), &DirMarks::new());
        assert_eq!(
            cat.written[Path::new("/photos")],
            0,
            "empty marks mean no shard"
        );
    }
}
