//! Folder navigation: given an opened image, list its sibling images in the
//! same directory (sorted), and support prev/next.

use std::path::{Path, PathBuf};

/// Extensions we treat as images (matches the UTIs declared in Info.plist).
const IMAGE_EXTS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "tiff", "tif", "bmp", "heic", "heif",
    // common camera RAW
    "cr2", "cr3", "nef", "arw", "dng", "raf", "rw2", "orf", "pef", "srw",
];

pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

/// The set of images in a folder plus the index of the current one.
pub struct Playlist {
    entries: Vec<PathBuf>,
    index: usize,
}

impl Playlist {
    /// Build a playlist from the folder containing `current`, positioned on it.
    pub fn from_file(current: &Path) -> Self {
        let dir = current.parent().unwrap_or_else(|| Path::new("."));
        let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|rd| {
                rd.filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| p.is_file() && is_image(p))
                    .collect()
            })
            .unwrap_or_default();

        // Natural-ish sort: case-insensitive by file name.
        entries.sort_by(|a, b| {
            let an = a.file_name().map(|s| s.to_string_lossy().to_lowercase());
            let bn = b.file_name().map(|s| s.to_string_lossy().to_lowercase());
            an.cmp(&bn)
        });

        let canon = std::fs::canonicalize(current).ok();
        let index = entries
            .iter()
            .position(|p| std::fs::canonicalize(p).ok() == canon)
            .or_else(|| entries.iter().position(|p| p == current))
            .unwrap_or(0);

        // If the folder somehow yielded nothing, fall back to the single file.
        if entries.is_empty() {
            entries.push(current.to_path_buf());
        }

        Self { entries, index }
    }

    pub fn current(&self) -> &Path {
        &self.entries[self.index]
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn position(&self) -> usize {
        self.index
    }

    /// Move to next image (wraps). Returns the new current path.
    pub fn next(&mut self) -> &Path {
        self.index = (self.index + 1) % self.entries.len();
        self.current()
    }

    /// Move to previous image (wraps). Returns the new current path.
    pub fn prev(&mut self) -> &Path {
        self.index = (self.index + self.entries.len() - 1) % self.entries.len();
        self.current()
    }

    /// Paths of the neighbors to preload (prev and next), if distinct.
    pub fn neighbors(&self) -> Vec<PathBuf> {
        let n = self.entries.len();
        if n <= 1 {
            return vec![];
        }
        let next = (self.index + 1) % n;
        let prev = (self.index + n - 1) % n;
        let mut out = vec![self.entries[next].clone()];
        if prev != next {
            out.push(self.entries[prev].clone());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_sorted_siblings_and_navigates_with_wrap() {
        let dir = Path::new("/tmp/iv-test");
        let start = dir.join("b.jpg");
        if !start.exists() {
            eprintln!("skipping: {} not present", start.display());
            return;
        }
        let mut pl = Playlist::from_file(&start);
        // a.png, b.jpg, c.tiff sorted by name; we opened b -> index 1.
        assert!(pl.len() >= 3);
        assert_eq!(pl.current().file_name().unwrap(), "b.jpg");

        assert_eq!(pl.next().file_name().unwrap(), "c.tiff");
        // next from last wraps back to the first entry.
        assert_eq!(pl.next().file_name().unwrap(), "a.png");
        assert_eq!(pl.prev().file_name().unwrap(), "c.tiff");
    }
}
