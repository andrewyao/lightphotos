// SPDX-License-Identifier: MIT OR Apache-2.0

//! A catalog: a named set of top-level photo folders, the Folders panel's
//! roots, plus a disposable copy of each rated, flagged or labelled photo's
//! marks. The per-folder sidecars under [`crate::sidecar::SIDECAR_DIR`] stay
//! the truth; the copy here is a projection of them, rewritten whole from the
//! open folder's records, so it can be thrown away and rebuilt at any time.
//!
//! Natively a catalog is a directory `<name>.lightphotos` holding
//! `catalog.json`, `presets.json` and one `marks/<fnv1a>.json` shard per
//! photo directory. The browser keeps each catalog in IndexedDB: a record
//! per catalog, a preset-folders record, and a shard per photo directory,
//! written by `web.rs`.
//!
//! The preset folders have a record of their own, never a field in
//! `catalog.json`: v0.1.26 and v0.1.27 rewrite that file as `{v, folders}`
//! and would drop a field they don't know.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::sidecar::{ColorLabel, Flag, ImageRecord};

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod native;
#[cfg(target_arch = "wasm32")]
pub(crate) mod web;

/// The directory extension that marks a catalog.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const EXT: &str = "lightphotos";
/// The name of the one catalog every install starts with.
pub(crate) const DEFAULT_NAME: &str = "default";
/// Format version written into `catalog.json` and each shard.
const VERSION: u32 = 1;
/// A `catalog.json` past this is refused rather than parsed: a thousand
/// folder paths fit in a tenth of it.
pub(crate) const MAX_RECORD_BYTES: usize = 1 << 20;
/// A shard past this is refused on read. A directory of 50 000 marked
/// photos serializes to a few megabytes. Only tests read shards until the
/// cross-folder reader exists.
#[cfg(test)]
pub(crate) const MAX_SHARD_BYTES: usize = 16 << 20;
/// Folders a catalog may list.
pub(crate) const MAX_FOLDERS: usize = 1000;
/// The longest browser catalog name, in characters.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) const MAX_NAME_CHARS: usize = 100;

/// The catalog the browser opens when catalog `failed` won't load: the one
/// a switch came from, or at launch (`came_from` is `None`) the default, as
/// natively. `None` keeps `failed` open, its folders in memory only.
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn web_fallback<'a>(failed: &str, came_from: Option<&'a str>) -> Option<&'a str> {
    let to = came_from.unwrap_or(DEFAULT_NAME);
    (to != failed).then_some(to)
}

/// `name` as a browser catalog's name: trimmed, non-empty, at most
/// [`MAX_NAME_CHARS`], with no `/` (which reads as a path) and no NUL (which
/// separates the catalog from the directory in a marks key).
#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn web_catalog_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("a catalog needs a name".into());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!(
            "a catalog name has at most {MAX_NAME_CHARS} characters"
        ));
    }
    if name.contains(['/', '\0']) {
        return Err(format!("{name:?} has a / or a NUL in it"));
    }
    Ok(name.to_string())
}

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

    /// A browser catalog's location: its name, by [`web_catalog_name`].
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn named(name: &str) -> Result<CatalogRef, String> {
        Ok(CatalogRef {
            name: web_catalog_name(name)?,
        })
    }

    /// The browser's default catalog.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn default_web() -> CatalogRef {
        CatalogRef {
            name: DEFAULT_NAME.to_string(),
        }
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

/// Why a catalog on disk could not be opened or made.
#[cfg(not(target_arch = "wasm32"))]
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
}

#[cfg(not(target_arch = "wasm32"))]
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
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl std::error::Error for CatalogError {}

fn too_many_folders(max: usize) -> String {
    format!("a catalog lists at most {max} folders")
}

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
/// changes stay in memory and merge into the record, never over it. Only a
/// `Loaded` record is ever written.
#[derive(Debug, PartialEq, Eq)]
enum Record {
    /// The folders added while the record is out, which are all the tree
    /// shows, and the folders removed meanwhile, which the record may list.
    Pending {
        added: Vec<PathBuf>,
        removed: Vec<PathBuf>,
    },
    Loaded(Vec<PathBuf>),
    /// The stored record could not be read or will not parse. The folders
    /// live in memory for this visit and the record stays as it is.
    Unreadable(Vec<PathBuf>),
}

impl Record {
    fn pending() -> Record {
        Record::Pending {
            added: Vec::new(),
            removed: Vec::new(),
        }
    }

    fn folders(&self) -> &[PathBuf] {
        match self {
            Record::Pending { added: f, .. } | Record::Loaded(f) | Record::Unreadable(f) => f,
        }
    }

    fn folders_mut(&mut self) -> &mut Vec<PathBuf> {
        match self {
            Record::Pending { added: f, .. } | Record::Loaded(f) | Record::Unreadable(f) => f,
        }
    }

    /// Add `dir` by [`add_root`]'s rule. Returns whether the list changed;
    /// past [`MAX_FOLDERS`] the add is refused.
    fn add(&mut self, dir: &Path) -> Result<bool, String> {
        let roots = self.folders_mut();
        if roots.len() >= MAX_FOLDERS && !roots.iter().any(|r| dir.starts_with(r)) {
            return Err(too_many_folders(MAX_FOLDERS));
        }
        let changed = add_root(roots, dir);
        if let Record::Pending { removed, .. } = self {
            removed.retain(|r| r != dir);
        }
        Ok(changed)
    }

    /// Drop `dir`. Returns whether it was listed. While the record is out
    /// the removal is remembered, so a record that lists `dir` does not
    /// bring it back.
    fn remove(&mut self, dir: &Path) -> bool {
        let roots = self.folders_mut();
        let before = roots.len();
        roots.retain(|r| r != dir);
        let changed = roots.len() != before;
        if let Record::Pending { removed, .. } = self {
            if changed && !removed.iter().any(|r| r == dir) {
                removed.push(dir.to_path_buf());
            }
        }
        changed
    }

    /// The stored list arrived. The changes made while it was pending apply
    /// on top of it. Returns whether that differs from what is stored.
    fn load(&mut self, stored: &[PathBuf]) -> bool {
        let folders = self.merged_onto(stored);
        let merged = folders != stored;
        *self = Record::Loaded(folders);
        merged
    }

    /// The stored list could not be read. What is in memory stays for this
    /// visit, and the store is never written over.
    fn unreadable(&mut self) {
        let folders = std::mem::take(self.folders_mut());
        *self = Record::Unreadable(folders);
    }

    /// `base`, the stored record or a first record's seed, with the changes
    /// made while pending: `(base ∪ added) − removed`, tidied like the tree.
    fn merged_onto(&self, base: &[PathBuf]) -> Vec<PathBuf> {
        let (added, removed) = match self {
            Record::Pending { added, removed } => (added.as_slice(), removed.as_slice()),
            Record::Loaded(_) | Record::Unreadable(_) => (&[][..], &[][..]),
        };
        let mut folders = Vec::new();
        for dir in base.iter().chain(added) {
            if folders.len() < MAX_FOLDERS {
                add_root(&mut folders, dir);
            }
        }
        folders.retain(|f| !removed.contains(f));
        folders
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

/// One open catalog: its roots, its preset folders and the marks copy's
/// write queue.
pub(crate) struct Catalog {
    at: CatalogRef,
    folders: Record,
    preset_folders: Record,
    /// Signature of the shard last queued per photo directory, so a
    /// projection that changed nothing writes nothing.
    written: HashMap<PathBuf, u64>,
    /// `None` for a catalog that is never written, a test's. The browser's
    /// writes go through `web.rs`'s page-wide queue.
    #[cfg(not(target_arch = "wasm32"))]
    writer: Option<native::Writer>,
    last_error: Option<String>,
    /// Why the preset-folders record could not be read, once, kept apart
    /// from `last_error` because nothing failed to save.
    presets_error: Option<String>,
}

impl Catalog {
    /// A catalog that is never written anywhere: a test's, or the one the
    /// app runs on when the default cannot be made.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn in_memory(name: &str) -> Catalog {
        Catalog::empty(CatalogRef {
            path: PathBuf::from(format!("{name}.{EXT}")),
            name: name.to_string(),
        })
    }

    /// No folders, no writer, record loaded.
    fn empty(at: CatalogRef) -> Catalog {
        Catalog {
            at,
            folders: Record::Loaded(Vec::new()),
            preset_folders: Record::Loaded(Vec::new()),
            written: HashMap::new(),
            #[cfg(not(target_arch = "wasm32"))]
            writer: None,
            last_error: None,
            presets_error: None,
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
        let changed = self.folders.add(dir).unwrap_or_else(|e| {
            self.last_error = Some(e);
            false
        });
        if changed {
            self.save_record();
        }
        changed
    }

    /// Drop `dir` from the roots. Returns whether it was listed.
    pub(crate) fn remove_folder(&mut self, dir: &Path) -> bool {
        let changed = self.folders.remove(dir);
        if changed {
            self.save_record();
        }
        changed
    }

    /// The stored record arrived. The changes made while it was pending
    /// apply on top of it.
    fn load_record(&mut self, stored: Vec<PathBuf>) {
        if self.folders.load(&stored) {
            self.save_record();
        }
    }

    /// The folders presets are read from, sorted, none inside another.
    pub(crate) fn preset_folders(&self) -> &[PathBuf] {
        self.preset_folders.folders()
    }

    /// Add a preset folder, by [`add_root`]'s rule: a folder inside one
    /// already listed is already scanned. Returns whether the list changed;
    /// past [`MAX_FOLDERS`] the add is refused with why. Writes only the
    /// preset-folders record.
    pub(crate) fn add_preset_folder(&mut self, dir: &Path) -> Result<bool, String> {
        let changed = self.preset_folders.add(dir)?;
        if changed {
            self.save_presets();
        }
        Ok(changed)
    }

    /// Drop a preset folder. Returns whether it was listed.
    pub(crate) fn remove_preset_folder(&mut self, dir: &Path) -> bool {
        let changed = self.preset_folders.remove(dir);
        if changed {
            self.save_presets();
        }
        changed
    }

    /// The stored preset-folders record arrived from `source`: its JSON,
    /// `None` when there is none, or why it could not be read. A missing one
    /// is an empty list. One that cannot be read or parsed is reported by
    /// [`take_presets_error`](Self::take_presets_error), and the catalog
    /// never writes over it.
    pub(crate) fn land_presets(&mut self, source: &str, stored: Result<Option<Vec<u8>>, String>) {
        let parsed = stored.and_then(|bytes| bytes.map(|b| parse_record(&b)).transpose());
        match parsed {
            Ok(folders) => {
                if self.preset_folders.load(&folders.unwrap_or_default()) {
                    self.save_presets();
                }
            }
            Err(e) => {
                eprintln!("[catalog] {source}: {e}");
                self.preset_folders.unreadable();
                self.presets_error = Some(format!("{source}: {e}"));
            }
        }
    }

    /// Why the preset folders could not be read, returned once.
    pub(crate) fn take_presets_error(&mut self) -> Option<String> {
        self.presets_error.take()
    }

    /// Whether the stored record has yet to arrive.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn loading(&self) -> bool {
        matches!(self.folders, Record::Pending { .. })
    }

    /// The browser's stored record arrived: its JSON, `None` when there is
    /// none yet, or why it could not be read. A missing record is made from
    /// `seed` and the changes made while it was pending. One that cannot be
    /// read or parsed is an error, and the catalog never writes over it.
    #[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
    pub(crate) fn land_record(
        &mut self,
        stored: Result<Option<Vec<u8>>, String>,
        seed: &[PathBuf],
    ) -> Result<(), String> {
        let parsed = stored.and_then(|bytes| bytes.map(|b| parse_record(&b)).transpose());
        match parsed {
            Ok(Some(folders)) => self.load_record(folders),
            Ok(None) => {
                self.folders = Record::Loaded(self.folders.merged_onto(seed));
                self.save_record();
            }
            Err(e) => {
                self.folders.unreadable();
                return Err(format!("{}: {e}", self.name()));
            }
        }
        Ok(())
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

/// `marks/<fnv1a of the path>.json`, the shard file name for `dir`. The
/// browser keys its shards by the path itself.
#[cfg(not(target_arch = "wasm32"))]
fn shard_name(dir: &Path) -> String {
    let mut h = crate::hash::Fnv1a::new();
    h.write(dir.to_string_lossy().as_bytes());
    format!("{:016x}.json", h.finish())
}

/// Parse a shard's bytes, refusing one over the cap or not for `dir`.
#[cfg(test)]
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
fn parse_record(bytes: &[u8]) -> Result<Vec<PathBuf>, String> {
    if bytes.len() > MAX_RECORD_BYTES {
        return Err(format!("{} bytes is over the catalog limit", bytes.len()));
    }
    let record: RecordFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if record.folders.len() > MAX_FOLDERS {
        return Err(too_many_folders(MAX_FOLDERS));
    }
    let mut folders = Vec::new();
    for dir in &record.folders {
        add_root(&mut folders, dir);
    }
    Ok(folders)
}

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
        cat.folders = Record::Pending {
            added: paths(&["/added/while/pending"]),
            removed: Vec::new(),
        };
        cat.load_record(paths(&["/stored"]));
        assert_eq!(
            cat.folders(),
            paths(&["/added/while/pending", "/stored"]).as_slice()
        );
        assert!(matches!(cat.folders, Record::Loaded(_)));
    }

    /// A folder removed while the record was out stays removed when the
    /// record lists it, and the merged list is written back over it.
    #[test]
    fn a_folder_removed_while_pending_stays_removed() {
        let dir = crate::app::test_support::temp_folder("pending-remove");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat =
            Catalog::create_at(at.clone(), paths(&["/stored", "/gone", "/back"])).unwrap();
        cat.folders = Record::pending();
        for added in ["/picked", "/gone", "/back", "/dropped"] {
            assert!(cat.add_folder(Path::new(added)));
        }
        for removed in ["/gone", "/back", "/dropped"] {
            assert!(cat.remove_folder(Path::new(removed)));
        }
        assert!(cat.add_folder(Path::new("/back")), "re-added after removal");
        cat.load_record(paths(&["/back", "/gone", "/stored"]));
        let want = paths(&["/back", "/picked", "/stored"]);
        assert_eq!(cat.folders(), want.as_slice());
        cat.flush_blocking(std::time::Duration::from_secs(10));
        drop(cat);
        assert_eq!(Catalog::open(at).unwrap().folders(), want.as_slice());

        let mut first = Catalog::in_memory("y");
        first.folders = Record::pending();
        first.add_folder(Path::new("/kept"));
        first.remove_folder(Path::new("/kept"));
        first
            .land_record(Ok(None), &paths(&["/kept", "/other"]))
            .unwrap();
        assert_eq!(
            first.folders(),
            paths(&["/other"]).as_slice(),
            "the seed loses it too"
        );
    }

    #[test]
    fn a_browser_catalog_that_wont_load_falls_back_like_native() {
        assert_eq!(
            web_fallback("trip", None),
            Some(DEFAULT_NAME),
            "a launch opens the default"
        );
        assert_eq!(
            web_fallback(DEFAULT_NAME, None),
            None,
            "a broken default stays open, in memory"
        );
        assert_eq!(
            web_fallback("broken", Some("trip")),
            Some("trip"),
            "a switch goes back where it came from"
        );
        assert_eq!(web_fallback("trip", Some("trip")), None);
    }

    #[test]
    fn a_web_catalog_name_is_trimmed_and_refuses_paths_nul_and_length() {
        assert_eq!(web_catalog_name("  Trip 2024 ").unwrap(), "Trip 2024");
        assert_eq!(web_catalog_name("旅行").unwrap(), "旅行");
        for bad in ["", "   ", "a/b", "/", "a\0b"] {
            assert!(web_catalog_name(bad).is_err(), "{bad:?}");
        }
        let longest = "é".repeat(MAX_NAME_CHARS);
        assert_eq!(web_catalog_name(&longest).unwrap(), longest);
        assert!(web_catalog_name(&format!("{longest}x")).is_err());
    }

    /// The browser's first visit: no record yet, so the kept folders and
    /// any picked while it loaded become the record, and it is written.
    #[test]
    fn a_missing_record_is_made_from_the_seed_and_the_pending_roots() {
        let dir = crate::app::test_support::temp_folder("land-missing");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), Vec::new()).unwrap();
        cat.folders = Record::Pending {
            added: paths(&["/picked"]),
            removed: Vec::new(),
        };
        assert!(cat.loading());
        cat.land_record(Ok(None), &paths(&["/kept/b", "/kept/a"]))
            .unwrap();
        assert!(!cat.loading());
        assert_eq!(cat.folders(), paths(&["/kept/a", "/kept/b", "/picked"]));
        cat.flush_blocking(std::time::Duration::from_secs(10));
        drop(cat);
        assert_eq!(
            Catalog::open(at).unwrap().folders(),
            paths(&["/kept/a", "/kept/b", "/picked"])
        );
    }

    /// A stored record that won't parse, or couldn't be read, keeps its
    /// roots in memory and is never written over, however they change.
    #[test]
    fn an_unreadable_record_is_an_error_and_never_overwritten() {
        let dir = crate::app::test_support::temp_folder("land-bad");
        let at = CatalogRef::in_dir(&dir, "c").unwrap();
        let mut cat = Catalog::create_at(at.clone(), paths(&["/on/disk"])).unwrap();
        cat.flush_blocking(std::time::Duration::from_secs(10));
        let file = at.path().join("catalog.json");
        let before = std::fs::read(&file).unwrap();
        for stored in [
            Ok(Some(b"{\"v\":1,\"folders\":".to_vec())),
            Ok(Some(b"null".to_vec())),
            Err("IndexedDB is unavailable".to_string()),
        ] {
            cat.folders = Record::Pending {
                added: paths(&["/picked"]),
                removed: Vec::new(),
            };
            let why = cat.land_record(stored, &paths(&["/seed"])).unwrap_err();
            assert!(why.starts_with("c: "), "{why}");
            assert!(!cat.loading(), "the restore must not wait forever");
            assert_eq!(cat.folders(), paths(&["/picked"]), "the seed is unused");
            assert!(cat.add_folder(Path::new("/added")));
            assert!(cat.remove_folder(Path::new("/picked")));
            cat.flush_blocking(std::time::Duration::from_secs(10));
            assert_eq!(std::fs::read(&file).unwrap(), before);
        }
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
