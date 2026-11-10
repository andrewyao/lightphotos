// SPDX-License-Identifier: MIT OR Apache-2.0

//! Where a run's GPX tracks come from. [`GpxSource::resolve`] turns either
//! kind into file names and bytes, and [`load`] joins those into one track,
//! so nothing past here knows which kind it was.

use std::path::{Path, PathBuf};

use chrono::NaiveDate;

use super::icloud::{self, Day, DayState};
use crate::geotag::{Track, MAX_GPX_BYTES};

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum GpxSource {
    /// A GPX file the user picked.
    File(PathBuf),
    /// LightRemote's tracks for these days, from its iCloud folder.
    ICloud { first: NaiveDate, last: NaiveDate },
}

/// GPX files read, and why a part of the source gave none.
#[derive(Debug, Default)]
pub(crate) struct Resolved {
    pub(crate) files: Vec<(String, Vec<u8>)>,
    pub(crate) notes: Vec<Note>,
}

/// Something the user should hear about a run's tracks.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Note {
    /// Days in the picked range with no track.
    Missing(Vec<NaiveDate>),
    /// Days in the picked range iCloud didn't download in time.
    NotDownloaded(Vec<NaiveDate>),
    /// Days iCloud downloaded for this run.
    Downloaded(Vec<NaiveDate>),
    /// A file that was there but couldn't be used.
    Skipped { file: String, reason: String },
}

/// A track ready to match on, and what the user should hear about it.
#[derive(Debug)]
pub(crate) struct Loaded {
    pub(crate) track: Track,
    pub(crate) notes: Vec<Note>,
}

impl GpxSource {
    /// Read the source's files. For iCloud, `folder` is LightRemote's
    /// folder and `before` is how its days stood when the run began, to
    /// tell a day iCloud downloaded from one that was already here.
    pub(crate) fn resolve(&self, folder: Option<&Path>, before: &[Day]) -> Resolved {
        match self {
            GpxSource::File(path) => {
                let file = name_of(path);
                match read_capped(path) {
                    Ok(bytes) => Resolved {
                        files: vec![(file, bytes)],
                        notes: Vec::new(),
                    },
                    Err(reason) => Resolved {
                        files: Vec::new(),
                        notes: vec![Note::Skipped { file, reason }],
                    },
                }
            }
            GpxSource::ICloud { first, last } => {
                let Some(dir) = folder else {
                    return Resolved::default();
                };
                resolve_days(dir, *first, *last, before)
            }
        }
    }
}

fn resolve_days(dir: &Path, first: NaiveDate, last: NaiveDate, before: &[Day]) -> Resolved {
    let mut out = Resolved::default();
    let (mut missing, mut late, mut downloaded) = (Vec::new(), Vec::new(), Vec::new());
    for day in icloud::scan(dir, first, last) {
        let picked = (first..=last).contains(&day.date);
        let was_in_icloud = before
            .iter()
            .any(|b| b.date == day.date && b.state == DayState::InICloud);
        match day.state {
            DayState::Missing if picked => missing.push(day.date),
            DayState::InICloud if picked => late.push(day.date),
            DayState::Missing | DayState::InICloud => {}
            DayState::OnMac => {
                let file = icloud::file_name(day.date);
                match read_capped(&dir.join(&file)) {
                    Ok(bytes) => {
                        if was_in_icloud && picked {
                            downloaded.push(day.date);
                        }
                        out.files.push((file, bytes));
                    }
                    Err(reason) => out.notes.push(Note::Skipped { file, reason }),
                }
            }
        }
    }
    for (dates, note) in [
        (missing, Note::Missing as fn(_) -> _),
        (late, Note::NotDownloaded),
        (downloaded, Note::Downloaded),
    ] {
        if !dates.is_empty() {
            out.notes.push(note(dates));
        }
    }
    out
}

/// Parse each file and join them. A file that doesn't parse is skipped with
/// its reason; with none left, the error names every reason.
pub(crate) fn load(resolved: Resolved) -> Result<Loaded, String> {
    let Resolved { files, mut notes } = resolved;
    let mut tracks = Vec::new();
    for (file, bytes) in files {
        match Track::parse(&file, &bytes) {
            Ok(track) => tracks.push(track),
            Err(e) => notes.push(Note::Skipped {
                file,
                reason: e.to_string(),
            }),
        }
    }
    match Track::merge(tracks) {
        Some(track) => Ok(Loaded { track, notes }),
        None => Err(notes
            .iter()
            .filter_map(|n| match n {
                Note::Skipped { file, reason } => Some(format!("{file}: {reason}")),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("; ")),
    }
}

fn name_of(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
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
