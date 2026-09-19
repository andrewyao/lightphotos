// SPDX-License-Identifier: GPL-3.0-or-later

//! Folder listing, rating filters, burst grouping by time, and grid/tree
//! arrow-key movement.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Extensions we treat as images. Keep in sync with the UTIs in Info.plist.
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

/// Star-rating filter comparator. The app's filter is `Option<(Cmp, u8)>`, and
/// `None` shows everything. "Unrated" is `(Eq, 0)`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cmp {
    Gte,
    Eq,
    Lte,
}

impl Cmp {
    /// `rating` is 0 when unset.
    pub fn matches(self, rating: u8, value: u8) -> bool {
        match self {
            Cmp::Gte => rating >= value,
            Cmp::Eq => rating == value,
            Cmp::Lte => rating <= value,
        }
    }
}

/// Indices of `entries` that pass `filter`, in order. `rating_of` returns
/// 0..=5, with 0 for unset, so unset photos never pass `Gte` or `Eq` with a
/// positive value.
pub fn visible_indices(
    entries: &[PathBuf],
    filter: Option<(Cmp, u8)>,
    rating_of: impl Fn(&Path) -> u8,
) -> Vec<usize> {
    match filter {
        None => (0..entries.len()).collect(),
        Some((cmp, value)) => (0..entries.len())
            .filter(|&i| cmp.matches(rating_of(&entries[i]), value))
            .collect(),
    }
}

/// A 0-based, increasing burst id per entry. A new burst starts when an entry's
/// capture time is more than `gap` from the last known time, in either
/// direction. Entries with no time (`None`) join the current burst and never
/// split one.
pub fn group_by_time(times: &[Option<SystemTime>], gap: Duration) -> Vec<u32> {
    let mut ids = Vec::with_capacity(times.len());
    let mut group = 0u32;
    let mut last_known: Option<SystemTime> = None;
    for t in times.iter() {
        if let Some(t) = t {
            if let Some(prev) = last_known {
                let diff = t.duration_since(prev).or_else(|_| prev.duration_since(*t));
                if diff.map(|d| d > gap).unwrap_or(false) {
                    group += 1;
                }
            }
            last_known = Some(*t);
        }
        ids.push(group);
    }
    ids
}

/// Empty when `dir` can't be read. Skips entries that fail to read.
fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

/// Sort case-insensitively by file name. `web_fs.rs` uses this too, so the
/// browser build lists folders in the same order.
pub(crate) fn sort_by_name(entries: &mut [PathBuf]) {
    entries.sort_by(|a, b| {
        let an = a.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let bn = b.file_name().map(|s| s.to_string_lossy().to_lowercase());
        an.cmp(&bn)
    });
}

fn sorted_images_in(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = read_dir_paths(dir)
        .into_iter()
        .filter(|p| p.is_file() && is_image(p))
        .collect();
    sort_by_name(&mut entries);
    entries
}

/// Whether a folder named `name` shows in the folder tree. Hides dot-folders
/// and macOS bundles (`.app`, `.photoslibrary`). Shared by the native and
/// browser folder listings.
pub fn is_listable_subdir(name: &str) -> bool {
    if name.starts_with('.') {
        return false;
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    !matches!(ext.as_deref(), Some("app") | Some("photoslibrary"))
}

/// Subfolders of `dir` that pass [`is_listable_subdir`], sorted by name. Empty
/// on a read error.
#[cfg(not(target_arch = "wasm32"))]
pub fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = read_dir_paths(dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(is_listable_subdir)
        })
        .collect();
    sort_by_name(&mut entries);
    entries
}

/// Arrow-key move in the grid. `pos` and the result are positions in the
/// visible list, not playlist indices. `dx` steps columns, `dy` steps rows.
/// The result is clamped to `0..len`.
pub fn grid_move(pos: usize, len: usize, cols: usize, dx: isize, dy: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let cols = cols.max(1) as isize;
    let delta = dx + dy * cols;
    let p = pos as isize + delta;
    p.clamp(0, len as isize - 1) as usize
}

/// The sidebar folder rows, top to bottom, in the order arrow keys move
/// through them. A pre-order walk from `root` that enters only expanded folders.
/// `children` must return subfolders already sorted.
pub fn flatten_visible_tree(
    root: &Path,
    is_expanded: &impl Fn(&Path) -> bool,
    children: &impl Fn(&Path) -> Vec<PathBuf>,
) -> Vec<PathBuf> {
    fn walk(
        node: &Path,
        is_expanded: &impl Fn(&Path) -> bool,
        children: &impl Fn(&Path) -> Vec<PathBuf>,
        out: &mut Vec<PathBuf>,
    ) {
        out.push(node.to_path_buf());
        if is_expanded(node) {
            for child in children(node) {
                walk(&child, is_expanded, children, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, is_expanded, children, &mut out);
    out
}

/// The set of images in a folder plus the index of the current one.
pub struct Playlist {
    entries: Vec<PathBuf>,
    index: usize,
    dir: PathBuf,
}

impl Playlist {
    /// The images inside `dir`, positioned at index 0. Used when a folder is
    /// opened directly.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_dir(dir: &Path) -> Self {
        let entries = sorted_images_in(dir);
        Self {
            entries,
            index: 0,
            dir: dir.to_path_buf(),
        }
    }

    /// The browser version of `from_dir`. Browser folder handles list entries
    /// asynchronously, so the caller lists them first. `entries` must already
    /// be filtered and sorted with [`sort_by_name`].
    #[cfg(target_arch = "wasm32")]
    pub fn from_entries(dir: PathBuf, entries: Vec<PathBuf>) -> Self {
        Self {
            entries,
            index: 0,
            dir,
        }
    }

    /// The images in `current`'s folder, positioned on `current`.
    pub fn from_file(current: &Path) -> Self {
        let dir = current.parent().unwrap_or_else(|| Path::new("."));
        let mut entries = sorted_images_in(dir);

        // Compare normalized paths so `current` matches its entry even when
        // spelled differently (relative, symlinked).
        let target = crate::paths::normalize(current);
        let index = entries
            .iter()
            .position(|p| crate::paths::normalize(p) == target)
            .unwrap_or(0);

        if entries.is_empty() {
            entries.push(current.to_path_buf());
        }

        Self {
            entries,
            index,
            dir: dir.to_path_buf(),
        }
    }

    /// The starting index: the opened file for `from_file`, 0 for `from_dir`.
    pub fn position(&self) -> usize {
        self.index
    }

    /// The folder these images live in. The catalog sidecar sits here.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// All images, unfiltered.
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    pub fn entry(&self, index: usize) -> Option<&Path> {
        self.entries.get(index).map(|p| p.as_path())
    }

    /// Drop entries where `remove` is true, such as trashed files. This shifts
    /// indices, so the caller must rebuild derived views (`recompute_visible`).
    pub fn remove_matching(&mut self, remove: impl Fn(&Path) -> bool) {
        self.entries.retain(|p| !remove(p.as_path()));
        if self.index >= self.entries.len() {
            self.index = self.entries.len().saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn visible_indices_no_filter_is_identity() {
        let e = paths(&["a", "b", "c"]);
        assert_eq!(visible_indices(&e, None, |_| 0), vec![0, 1, 2]);
    }

    #[test]
    fn visible_indices_filters_per_cmp() {
        let e = paths(&["a", "b", "c", "d"]);
        let rating = |p: &Path| match p.to_str().unwrap() {
            "b" => 3,
            "c" => 5,
            "d" => 1,
            _ => 0,
        };

        // Unset (0) never passes a positive Gte.
        assert_eq!(visible_indices(&e, Some((Cmp::Gte, 3)), rating), vec![1, 2]);
        assert_eq!(
            visible_indices(&e, Some((Cmp::Gte, 1)), rating),
            vec![1, 2, 3]
        );
        assert_eq!(
            visible_indices(&e, Some((Cmp::Gte, 6)), rating),
            Vec::<usize>::new()
        );

        assert_eq!(visible_indices(&e, Some((Cmp::Eq, 5)), rating), vec![2]);
        assert_eq!(visible_indices(&e, Some((Cmp::Eq, 0)), rating), vec![0]);

        // Unset (0) always passes Lte.
        assert_eq!(visible_indices(&e, Some((Cmp::Lte, 1)), rating), vec![0, 3]);
        assert_eq!(
            visible_indices(&e, Some((Cmp::Lte, 5)), rating),
            vec![0, 1, 2, 3]
        );
        assert_eq!(visible_indices(&e, Some((Cmp::Lte, 0)), rating), vec![0]);
    }

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn group_by_time_empty_and_single() {
        assert_eq!(
            group_by_time(&[], Duration::from_secs(2)),
            Vec::<u32>::new()
        );
        assert_eq!(group_by_time(&[t(10)], Duration::from_secs(2)), vec![0]);
    }

    #[test]
    fn group_by_time_splits_on_large_gap() {
        let times = [t(0), t(1), t(10), t(11)];
        assert_eq!(
            group_by_time(&times, Duration::from_secs(3)),
            vec![0, 0, 1, 1]
        );
    }

    #[test]
    fn group_by_time_all_within_gap_is_one_group() {
        let times = [t(0), t(1), t(2), t(3)];
        assert_eq!(
            group_by_time(&times, Duration::from_secs(2)),
            vec![0, 0, 0, 0]
        );
    }

    #[test]
    fn group_by_time_unknown_time_does_not_split() {
        // A missing time joins the current burst.
        let times = [t(0), None, t(1)];
        assert_eq!(group_by_time(&times, Duration::from_secs(3)), vec![0, 0, 0]);
        // The gap is measured from the last known time, so 0 to 100 still splits.
        let times = [t(0), None, t(100)];
        assert_eq!(group_by_time(&times, Duration::from_secs(3)), vec![0, 0, 1]);
    }

    #[test]
    fn group_by_time_leading_and_all_unknown() {
        assert_eq!(
            group_by_time(&[None, None], Duration::from_secs(2)),
            vec![0, 0]
        );
        assert_eq!(
            group_by_time(&[None, t(0), t(100)], Duration::from_secs(3)),
            vec![0, 0, 1]
        );
    }

    #[test]
    fn grid_move_math() {
        assert_eq!(grid_move(0, 7, 3, 1, 0), 1); // right
        assert_eq!(grid_move(0, 7, 3, -1, 0), 0); // left clamps at start
        assert_eq!(grid_move(0, 7, 3, 0, 1), 3); // down a row
        assert_eq!(grid_move(3, 7, 3, 0, -1), 0); // up a row
        assert_eq!(grid_move(6, 7, 3, 1, 0), 6); // right clamps at end
        assert_eq!(grid_move(5, 7, 3, 0, 1), 6); // down clamps to last item
        assert_eq!(grid_move(0, 0, 3, 1, 0), 0); // empty list
        assert_eq!(grid_move(2, 7, 0, 1, 0), 3); // cols=0 treated as 1
    }

    #[test]
    fn flatten_visible_tree_walks_expanded_dfs() {
        let kids = |p: &Path| match p.to_str().unwrap() {
            "root" => paths(&["root/a", "root/b"]),
            "root/a" => paths(&["root/a/a1", "root/a/a2"]),
            _ => vec![],
        };

        let none = |_: &Path| false;
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &none, &kids),
            paths(&["root"])
        );

        let only_root = |p: &Path| p == Path::new("root");
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &only_root, &kids),
            paths(&["root", "root/a", "root/b"])
        );

        // Pre-order: a's children come before its sibling b.
        let root_and_a = |p: &Path| p == Path::new("root") || p == Path::new("root/a");
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &root_and_a, &kids),
            paths(&["root", "root/a", "root/a/a1", "root/a/a2", "root/b"])
        );
    }

    #[test]
    fn flatten_visible_tree_descends_multiple_levels() {
        use std::path::{Path, PathBuf};
        let kids = |p: &Path| -> Vec<PathBuf> {
            match p.to_str().unwrap() {
                "root" => vec![PathBuf::from("root/a"), PathBuf::from("root/b")],
                "root/a" => vec![PathBuf::from("root/a/a1"), PathBuf::from("root/a/a2")],
                "root/a/a1" => vec![PathBuf::from("root/a/a1/i")],
                "root/a/a1/i" => vec![PathBuf::from("root/a/a1/i/leaf")],
                _ => vec![],
            }
        };
        let expanded = |p: &Path| {
            matches!(
                p.to_str().unwrap(),
                "root" | "root/a" | "root/a/a1" | "root/a/a1/i"
            )
        };
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &expanded, &kids),
            vec![
                PathBuf::from("root"),
                PathBuf::from("root/a"),
                PathBuf::from("root/a/a1"),
                PathBuf::from("root/a/a1/i"),
                PathBuf::from("root/a/a1/i/leaf"),
                PathBuf::from("root/a/a2"),
                PathBuf::from("root/b"),
            ]
        );
    }

    #[test]
    fn lists_sorted_siblings_positioned_on_opened_file() {
        let dir = Path::new("/tmp/iv-test");
        let start = dir.join("b.jpg");
        if !start.exists() {
            eprintln!("skipping: {} not present", start.display());
            return;
        }
        let pl = Playlist::from_file(&start);
        // Fixture: a.png, b.jpg, c.tiff.
        assert!(pl.entries().len() >= 3);
        assert_eq!(
            pl.entry(pl.position()).unwrap().file_name().unwrap(),
            "b.jpg"
        );
        assert_eq!(pl.entry(0).unwrap().file_name().unwrap(), "a.png");
        assert_eq!(pl.entry(2).unwrap().file_name().unwrap(), "c.tiff");
    }

    #[test]
    fn list_subdirs_returns_sorted_visible_dirs() {
        let root = std::env::temp_dir().join(format!(
            "iv-subdirs-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("file.txt"), b"x").unwrap();

        let got = list_subdirs(&root);
        let names: Vec<String> = got
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["a", "b"]);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn is_listable_subdir_filters_hidden_and_bundles() {
        assert!(is_listable_subdir("2024"));
        assert!(is_listable_subdir("Exports"));
        assert!(is_listable_subdir("My Photos"));
        assert!(!is_listable_subdir(".git"));
        assert!(!is_listable_subdir(".lightphotos"));
        assert!(!is_listable_subdir("Photos.app"));
        assert!(!is_listable_subdir("Library.photoslibrary"));
        assert!(!is_listable_subdir("Thing.APP"));
    }

    #[test]
    fn from_dir_enumerates_images_at_index_zero() {
        let dir = Path::new("/tmp/iv-test");
        if !dir.join("a.png").exists() {
            eprintln!("skipping: {} not present", dir.display());
            return;
        }
        let pl = Playlist::from_dir(dir);
        assert_eq!(pl.position(), 0);
        assert!(pl.entries().len() >= 3);
        assert_eq!(pl.entry(0).unwrap().file_name().unwrap(), "a.png");
    }
}
