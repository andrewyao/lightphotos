// SPDX-License-Identifier: MIT OR Apache-2.0

//! The GPX files a run reads: the files the user chose, and every `.gpx`
//! under the folders they chose. [`find`] lists them, [`download`] waits
//! for the ones iCloud hasn't brought down yet, and [`load`] joins them
//! into one track.
//!
//! A file in iCloud Drive that isn't downloaded is either a
//! `.name.gpx.icloud` placeholder or, where iCloud Drive runs as a file
//! provider, a dataless file under its own name. Reading the dataless one
//! would block until it downloads, so neither is read until it's here.
//! Choosing LightRemote's iCloud GPX folder is how its tracks come in.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::geotag::{Track, MAX_GPX_BYTES};

/// How deep under a chosen folder the walk goes. LightRemote's folder is
/// flat; a person's track archive is a few levels at most.
pub(crate) const MAX_DEPTH: usize = 8;
/// The most GPX files one run reads.
pub(crate) const MAX_FILES: usize = 1000;
/// The most files and folders a search looks at, so choosing a home
/// folder or a photo archive ends soon rather than walking all of it.
pub(crate) const MAX_ENTRIES: usize = 20_000;
const POLL: Duration = Duration::from_millis(250);

/// A GPX file to read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GpxFile {
    /// Its own name, even when only its placeholder is here.
    pub(crate) path: PathBuf,
    /// Not downloaded from iCloud yet.
    pub(crate) in_icloud: bool,
}

/// The GPX files found, and what the user should hear about the search.
#[derive(Debug, Default)]
pub(crate) struct Found {
    pub(crate) files: Vec<GpxFile>,
    pub(crate) notes: Vec<Note>,
}

/// Something the user should hear about a run's tracks.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Note {
    /// A file or folder that was there but couldn't be used.
    Skipped { file: String, reason: String },
    /// How many files iCloud didn't download in time.
    NotDownloaded(usize),
    /// How many files iCloud downloaded for this run.
    Downloaded(usize),
    /// More than [`MAX_FILES`] files; the rest were not read.
    TooMany,
    /// Folders more than [`MAX_DEPTH`] levels down were not read.
    TooDeep,
    /// The search stopped after [`MAX_ENTRIES`] files and folders.
    TooBig,
}

/// A track ready to match on, and what the user should hear about it.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) track: Track,
    pub(crate) notes: Vec<Note>,
}

/// The GPX files among `chosen` and under its folders, in path order, each
/// once.
/// Walks folders, so the import dialog calls it off the UI thread.
pub(crate) fn find(chosen: &[PathBuf]) -> Found {
    let mut found = Found::default();
    let mut walk = Walk::default();
    for path in chosen {
        match std::fs::metadata(path) {
            Ok(m) if m.is_dir() => walk.dir(path, 0),
            Ok(m) if m.is_file() && is_gpx(path) => {
                walk.files.insert(GpxFile {
                    path: path.clone(),
                    in_icloud: dataless(&m),
                });
            }
            Ok(_) => found.notes.push(Note::Skipped {
                file: name_of(path),
                reason: "not a GPX file".into(),
            }),
            Err(e) => found.notes.push(Note::Skipped {
                file: name_of(path),
                reason: e.to_string(),
            }),
        }
    }
    found.notes.append(&mut walk.notes);
    if walk.files.len() > MAX_FILES {
        found.notes.push(Note::TooMany);
    }
    found.files = walk.files.into_iter().take(MAX_FILES).collect();
    found
}

/// A search under the chosen folders: what it found, what it has to say,
/// and how many entries it has looked at.
#[derive(Default)]
struct Walk {
    files: BTreeSet<GpxFile>,
    notes: Vec<Note>,
    entries: usize,
}

impl Walk {
    fn note(&mut self, note: Note) {
        if !self.notes.contains(&note) {
            self.notes.push(note);
        }
    }

    fn dir(&mut self, dir: &Path, depth: usize) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                self.notes.push(Note::Skipped {
                    file: name_of(dir),
                    reason: e.to_string(),
                });
                return;
            }
        };
        let room = MAX_ENTRIES.saturating_sub(self.entries);
        let mut entries: Vec<_> = entries.flatten().take(room.saturating_add(1)).collect();
        if entries.len() > room {
            entries.truncate(room);
            self.note(Note::TooBig);
        }
        self.entries = self.entries.saturating_add(entries.len());
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            self.entry(dir, &entry, depth);
            if self.files.len() > MAX_FILES || self.entries >= MAX_ENTRIES {
                return;
            }
        }
    }

    fn entry(&mut self, dir: &Path, entry: &std::fs::DirEntry, depth: usize) {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        // Not followed through a link, so a link back up can't loop.
        let Ok(kind) = entry.file_type() else {
            return;
        };
        if kind.is_dir() {
            if name.starts_with('.') {
                return;
            }
            if depth >= MAX_DEPTH {
                self.note(Note::TooDeep);
            } else {
                self.dir(&path, depth + 1);
            }
        } else if let Some(real) = placeholder_for(&name) {
            let real = dir.join(real);
            if !real.exists() {
                self.files.insert(GpxFile {
                    path: real,
                    in_icloud: true,
                });
            }
        } else if !name.starts_with('.') && is_gpx(&path) {
            self.files.extend(gpx_file(&path));
        }
    }
}

/// `name.gpx` for iCloud's `.name.gpx.icloud` placeholder.
fn placeholder_for(name: &str) -> Option<&str> {
    let real = name.strip_prefix('.')?.strip_suffix(".icloud")?;
    is_gpx(Path::new(real)).then_some(real)
}

fn is_gpx(path: &Path) -> bool {
    path.extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("gpx"))
}

/// The file at `path` and whether it's still in iCloud. `None` when it
/// isn't a file.
fn gpx_file(path: &Path) -> Option<GpxFile> {
    let m = std::fs::metadata(path).ok()?;
    m.is_file().then(|| GpxFile {
        path: path.to_path_buf(),
        in_icloud: dataless(&m),
    })
}

fn is_here(path: &Path) -> bool {
    gpx_file(path).is_some_and(|f| !f.in_icloud)
}

/// A file provider's placeholder: the name and size are here, the bytes
/// are not.
#[cfg(target_os = "macos")]
fn dataless(m: &std::fs::Metadata) -> bool {
    use std::os::macos::fs::MetadataExt;
    /// `SF_DATALESS` in `<sys/stat.h>`.
    const SF_DATALESS: u32 = 0x4000_0000;
    m.st_flags() & SF_DATALESS != 0
}

#[cfg(not(target_os = "macos"))]
fn dataless(_: &std::fs::Metadata) -> bool {
    false
}

/// Ask iCloud, through `start`, for each file in `files` still in it, then
/// wait until every one whose download started is here or `timeout`
/// passes. Blocks, so it runs off the UI thread. Each file comes back
/// marked as it ended, with a note counting those that arrived and one
/// counting those that didn't.
pub(crate) fn download(
    files: Vec<GpxFile>,
    timeout: Duration,
    start: impl Fn(&Path) -> Result<(), String>,
) -> (Vec<GpxFile>, Vec<Note>) {
    let started: Vec<&Path> = files
        .iter()
        .filter(|f| f.in_icloud)
        .map(|f| f.path.as_path())
        .filter(|path| match start(path) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[lightphotos] iCloud download of {}: {e}", path.display());
                false
            }
        })
        .collect();
    let deadline = Instant::now() + timeout;
    while !started.iter().all(|p| is_here(p)) && Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    let (mut arrived, mut late) = (0usize, 0usize);
    let files = files
        .into_iter()
        .map(|f| {
            if !f.in_icloud {
                return f;
            }
            let here = is_here(&f.path);
            let count = if here { &mut arrived } else { &mut late };
            *count += 1;
            GpxFile {
                in_icloud: !here,
                ..f
            }
        })
        .collect();
    let mut notes = Vec::new();
    if arrived > 0 {
        notes.push(Note::Downloaded(arrived));
    }
    if late > 0 {
        notes.push(Note::NotDownloaded(late));
    }
    (files, notes)
}

/// Start iCloud downloading the file at `path`. Returns at once.
#[cfg(target_os = "macos")]
pub(crate) fn start_download(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let path = path.to_str().ok_or("the path is not valid UTF-8")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSFileManager::defaultManager()
        .startDownloadingUbiquitousItemAtURL_error(&url)
        .map_err(|e| e.localizedDescription().to_string())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn start_download(_: &Path) -> Result<(), String> {
    Err("iCloud Drive is only on macOS".into())
}

/// Read and parse each file that is here and join them. A file that doesn't
/// read or parse is skipped with its reason; with none left, the error is
/// every note, so it says why.
pub(crate) fn load(files: &[GpxFile], mut notes: Vec<Note>) -> Result<Loaded, Vec<Note>> {
    let mut tracks = Vec::new();
    for file in files.iter().filter(|f| !f.in_icloud) {
        let name = name_of(&file.path);
        let parsed = read_capped(&file.path)
            .and_then(|bytes| Track::parse(&name, &bytes).map_err(|e| e.to_string()));
        match parsed {
            Ok(track) => tracks.push(track),
            Err(reason) => notes.push(Note::Skipped { file: name, reason }),
        }
    }
    match Track::merge(tracks) {
        Some(track) => Ok(Loaded { track, notes }),
        None => Err(notes),
    }
}

pub(crate) fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// The file at `path`, refused unread past `MAX_GPX_BYTES`.
fn read_capped(path: &Path) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(MAX_GPX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}
