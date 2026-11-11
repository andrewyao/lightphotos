// SPDX-License-Identifier: MIT OR Apache-2.0

//! Content-addressed pixel files beside a folder's sidecars, such as mask
//! coverage and fill patches. Each kind has its own subdirectory of
//! `.lightphotos/` and names a file `<photo file name>.<id>.png`, where the
//! id is the hash of the file's pixels. Files are write-once: a writer that
//! finds its name taken knows the bytes already match.

use std::ffi::{OsStr, OsString};
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

use super::coverage::RasterId;

const FILE_EXT: &str = "png";
/// An orphaned file younger than this is kept: a job that has written its
/// file but not yet its sidecar looks like one.
#[cfg(not(target_arch = "wasm32"))]
pub(super) const ORPHAN_REAP_AFTER: std::time::Duration = std::time::Duration::from_secs(3600);

/// A side file's name: `<photo file name>.<id>.png`, such as
/// `IMG_0042.CR3.5be1c0a39d2f7e14.png`.
pub fn file_name(photo_name: &OsStr, id: RasterId) -> OsString {
    let mut name = photo_name.to_os_string();
    name.push(format!(".{id}.{FILE_EXT}"));
    name
}

/// The reverse of [`file_name`]. `None` for anything else, so a sweep leaves
/// other files alone.
pub fn parse_file_name(name: &OsStr) -> Option<(OsString, RasterId)> {
    let rest = name.to_str()?.strip_suffix(&format!(".{FILE_EXT}"))?;
    let (photo, hex) = rest.rsplit_once('.')?;
    if photo.is_empty() {
        return None;
    }
    Some((OsString::from(photo), hex.parse().ok()?))
}

/// `<dir>/.lightphotos/<subdir>/<name>` for the photo at `photo`. `None` for
/// a path with no folder or file name.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn path(photo: &Path, subdir: &str, id: RasterId) -> Option<PathBuf> {
    Some(
        photo
            .parent()?
            .join(crate::sidecar::SIDECAR_DIR)
            .join(subdir)
            .join(file_name(photo.file_name()?, id)),
    )
}

/// Removes files in `<dir>/.lightphotos/<subdir>` that `named` doesn't claim,
/// and stale write temporaries, each older than [`ORPHAN_REAP_AFTER`].
/// `named(photo, id)` says whether the photo's sidecar records `id`. A file
/// whose name isn't a side file's is left alone.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn sweep_orphans(dir: &Path, subdir: &str, named: impl Fn(&OsStr, RasterId) -> bool) {
    let files = dir.join(crate::sidecar::SIDECAR_DIR).join(subdir);
    if crate::paths::refuse_linked_sidecar_dir(&files).is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(files) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let orphan = match name.strip_suffix(".tmp") {
            Some(stem) => {
                parse_file_name(OsStr::new(crate::paths::atomic_tmp_target(stem))).is_some()
            }
            None => match parse_file_name(OsStr::new(name)) {
                Some((photo, id)) => !named(&photo, id),
                None => false,
            },
        };
        if orphan && is_older_than(&entry, ORPHAN_REAP_AFTER) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// `false` when the mtime is unreadable or in the future, so unknown files
/// are kept.
#[cfg(not(target_arch = "wasm32"))]
fn is_older_than(entry: &std::fs::DirEntry, age: std::time::Duration) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| std::time::SystemTime::now().duration_since(m).ok())
        .is_some_and(|elapsed| elapsed > age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_names_round_trip_and_reject_other_files() {
        let id = "5be1c0a39d2f7e14".parse::<RasterId>().unwrap();
        let name = file_name(OsStr::new("IMG_0042.CR3"), id);
        assert_eq!(name.to_str().unwrap(), "IMG_0042.CR3.5be1c0a39d2f7e14.png");
        assert_eq!(
            parse_file_name(&name),
            Some((OsString::from("IMG_0042.CR3"), id))
        );
        for other in [
            "IMG_0042.CR3.xmp",
            "IMG_0042.CR3.png",
            ".5be1c0a39d2f7e14.png",
            "IMG_0042.CR3.5be1c0a39d2f7e1.png",
            "IMG_0042.CR3.5be1c0a39d2f7e14.thumb.jpg",
        ] {
            assert_eq!(parse_file_name(OsStr::new(other)), None, "{other}");
        }
    }
}
