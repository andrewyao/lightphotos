// SPDX-License-Identifier: MIT OR Apache-2.0

//! The photos of a folder tree, a batch at a time, so a run over a tree of
//! any size never holds all of its photos at once.

use std::path::{Path, PathBuf};

use super::source::MAX_DEPTH;

/// The most photos in a batch.
pub(super) const BATCH: usize = 256;

/// The photos in a folder and in each subfolder the folder tree lists under
/// it, down to [`MAX_DEPTH`] levels, as batches of at most [`BATCH`] photos
/// from one folder each. A link to a folder is not followed, so the walk
/// stays inside the folder and can't loop. Each step lists a folder, so it
/// runs off the UI thread.
pub(super) struct PhotoWalk {
    /// Folders still to list, and how deep each is.
    todo: Vec<(PathBuf, usize)>,
    /// The listed folder's photos not yet yielded.
    listed: std::vec::IntoIter<PathBuf>,
}

impl PhotoWalk {
    pub(super) fn new(dir: &Path) -> PhotoWalk {
        PhotoWalk {
            todo: vec![(dir.to_path_buf(), 0)],
            listed: Vec::new().into_iter(),
        }
    }
}

impl Iterator for PhotoWalk {
    type Item = Vec<PathBuf>;

    fn next(&mut self) -> Option<Vec<PathBuf>> {
        loop {
            let batch: Vec<PathBuf> = self.listed.by_ref().take(BATCH).collect();
            if !batch.is_empty() {
                return Some(batch);
            }
            let (dir, depth) = self.todo.pop()?;
            if depth < MAX_DEPTH {
                let subdirs = crate::navigation::list_subdirs(&dir)
                    .into_iter()
                    .filter(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir()));
                self.todo.extend(subdirs.rev().map(|d| (d, depth + 1)));
            }
            self.listed = crate::navigation::Playlist::from_dir(&dir)
                .entries()
                .to_vec()
                .into_iter();
        }
    }
}
