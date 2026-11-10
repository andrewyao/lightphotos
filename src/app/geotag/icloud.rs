// SPDX-License-Identifier: MIT OR Apache-2.0

//! LightRemote's GPX tracks in its iCloud Drive folder: one file a day,
//! `YYYY-MM-DD.gpx`, named for the phone's local date. A file iCloud hasn't
//! downloaded yet is either a `.YYYY-MM-DD.gpx.icloud` placeholder or, on a
//! Mac whose iCloud Drive runs as a file provider, a dataless file under its
//! own name; reading that one would block until it downloads, so neither is
//! read until it's here.
//!
//! The folder is read straight from disk, which works because LightPhotos
//! has no app sandbox.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::NaiveDate;

/// The most days one run reads, so a typo'd year can't stat thousands of
/// files.
pub(crate) const MAX_DAYS: i64 = 366;
/// How long a run waits for iCloud to download the days it needs.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(60);
/// `--drive` waits less, inside its 30 s `idle`.
const DRIVE_DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(8);
const POLL: Duration = Duration::from_millis(250);

/// The folder, as `--drive --icloud-gpx-dir` points it at a fake one.
static DIR_OVERRIDE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where `--drive` reads LightRemote's tracks instead. Set once, before the
/// app starts.
pub(crate) fn override_dir(dir: PathBuf) {
    let _ = DIR_OVERRIDE.set(dir);
}

/// Where LightRemote keeps its tracks and how long to wait for a download.
#[derive(Clone, Debug)]
pub(crate) struct Folder {
    pub(crate) dir: PathBuf,
    pub(crate) timeout: Duration,
}

impl Folder {
    /// LightRemote's iCloud container on this Mac, or the `--drive`
    /// override. `None` off macOS, or with no home folder.
    pub(crate) fn default_for_this_mac() -> Option<Folder> {
        if let Some(dir) = DIR_OVERRIDE.get() {
            return Some(Folder {
                dir: dir.clone(),
                timeout: DRIVE_DOWNLOAD_TIMEOUT,
            });
        }
        if !cfg!(target_os = "macos") {
            return None;
        }
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty())?;
        Some(Folder {
            dir: Path::new(&home)
                .join("Library/Mobile Documents/iCloud~com~andrewyao~LightRemote/Documents/GPX"),
            timeout: DOWNLOAD_TIMEOUT,
        })
    }
}

/// Where one day's track is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DayState {
    OnMac,
    /// In iCloud, not downloaded.
    InICloud,
    Missing,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Day {
    pub(crate) date: NaiveDate,
    pub(crate) state: DayState,
}

pub(crate) fn file_name(date: NaiveDate) -> String {
    format!("{}.gpx", date.format("%Y-%m-%d"))
}

/// `first` to `last` when that is a range a run reads.
pub(crate) fn range(first: NaiveDate, last: NaiveDate) -> Option<(NaiveDate, NaiveDate)> {
    let span = last.signed_duration_since(first).num_days();
    (0..MAX_DAYS).contains(&span).then_some((first, last))
}

/// Each day from the one before `first` to the one after `last`. A camera's
/// wall-clock date can fall on the phone's day either side of it, so a run
/// reads those too. An empty list for a range [`range`] refuses.
pub(crate) fn scan(dir: &Path, first: NaiveDate, last: NaiveDate) -> Vec<Day> {
    let Some((first, last)) = range(first, last) else {
        return Vec::new();
    };
    let start = first.pred_opt().unwrap_or(first);
    let end = last.succ_opt().unwrap_or(last);
    start
        .iter_days()
        .take_while(|d| *d <= end)
        .map(|date| Day {
            date,
            state: state(dir, date),
        })
        .collect()
}

fn state(dir: &Path, date: NaiveDate) -> DayState {
    let name = file_name(date);
    match std::fs::metadata(dir.join(&name)) {
        Ok(m) if m.is_file() && !dataless(&m) => DayState::OnMac,
        Ok(m) if m.is_file() => DayState::InICloud,
        _ if dir.join(format!(".{name}.icloud")).is_file() => DayState::InICloud,
        _ => DayState::Missing,
    }
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

/// Ask iCloud for each day in `days` still in it, then wait until every one
/// is on this Mac or `folder.timeout` passes. Blocks, so it runs off the UI
/// thread. Returns the days as they ended.
pub(crate) fn download(folder: &Folder, first: NaiveDate, last: NaiveDate) -> Vec<Day> {
    let deadline = Instant::now() + folder.timeout;
    let mut days = scan(&folder.dir, first, last);
    for day in days.iter().filter(|d| d.state == DayState::InICloud) {
        if let Err(e) = start_download(&folder.dir.join(file_name(day.date))) {
            eprintln!("[lightphotos] iCloud download of {}: {e}", day.date);
        }
    }
    while days.iter().any(|d| d.state == DayState::InICloud) && Instant::now() < deadline {
        std::thread::sleep(POLL);
        days = scan(&folder.dir, first, last);
    }
    days
}

/// Start iCloud downloading the file at `path`. Returns at once.
#[cfg(target_os = "macos")]
fn start_download(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};
    let path = path.to_str().ok_or("the path is not valid UTF-8")?;
    let url = NSURL::fileURLWithPath(&NSString::from_str(path));
    NSFileManager::defaultManager()
        .startDownloadingUbiquitousItemAtURL_error(&url)
        .map_err(|e| e.localizedDescription().to_string())
}

#[cfg(not(target_os = "macos"))]
fn start_download(_: &Path) -> Result<(), String> {
    Err("iCloud Drive is only on macOS".into())
}
