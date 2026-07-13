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

/// Star-rating filter comparator. The active filter is `Option<(Cmp, u8)>` on
/// `App`; `None` shows everything. The toolbar surfaces all three (`≥`, `=`,
/// `≤`) as a comparator selector applied to the clicked star level, plus an
/// "Unrated" shortcut (`Eq` with 0).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Cmp {
    /// rating >= value
    Gte,
    /// rating == value
    Eq,
    /// rating <= value
    Lte,
}

impl Cmp {
    /// Does `rating` (0 when unset) satisfy this comparator against `value`?
    pub fn matches(self, rating: u8, value: u8) -> bool {
        match self {
            Cmp::Gte => rating >= value,
            Cmp::Eq => rating == value,
            Cmp::Lte => rating <= value,
        }
    }
}

/// Compute the visible indices over `entries` given a `rating_of` lookup
/// (returns the 0..=5 rating, 0 when unset) and an optional filter.
///
/// Pure and total: `None` filter yields every index in order; a `Some(cmp, v)`
/// filter keeps only entries whose rating satisfies `cmp` against `v`. Unset
/// ratings count as 0, so they match `Lte`/no-filter but never `Gte`/`Eq` with
/// a positive value.
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

/// Immediate entries of `dir` as paths, ignoring individual entry errors.
/// Returns an empty vec when the directory can't be read at all.
fn read_dir_paths(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).collect())
        .unwrap_or_default()
}

/// Sort paths in place, case-insensitively by file name.
fn sort_by_name(entries: &mut [PathBuf]) {
    entries.sort_by(|a, b| {
        let an = a.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let bn = b.file_name().map(|s| s.to_string_lossy().to_lowercase());
        an.cmp(&bn)
    });
}

/// List the image files directly in `dir`, sorted case-insensitively by name.
fn sorted_images_in(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = read_dir_paths(dir)
        .into_iter()
        .filter(|p| p.is_file() && is_image(p))
        .collect();
    sort_by_name(&mut entries);
    entries
}

/// List the immediate subdirectories of `dir`, sorted case-insensitively by
/// name (matching the `from_dir` image sort). Skips hidden entries (names
/// starting with `.`) and macOS bundles (`.app`/`.photoslibrary`). On a read
/// error returns an empty vec.
pub fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = read_dir_paths(dir)
        .into_iter()
        .filter(|p| p.is_dir())
        .filter(|p| {
            let name = match p.file_name().and_then(|s| s.to_str()) {
                Some(n) => n,
                None => return false,
            };
            if name.starts_with('.') {
                return false;
            }
            let ext = p.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
            !matches!(ext.as_deref(), Some("app") | Some("photoslibrary"))
        })
        .collect();
    sort_by_name(&mut entries);
    entries
}

/// Arrow-key movement within the grid, operating on *positions in the visible
/// list* (0..len). `dx` is the horizontal step (-1/+1), `dy` the vertical step
/// in rows (-1/+1); `cols` is the current column count. Returns the clamped new
/// position. Pure so it can be unit-tested independent of egui layout.
pub fn grid_move(pos: usize, len: usize, cols: usize, dx: isize, dy: isize) -> usize {
    if len == 0 {
        return 0;
    }
    let cols = cols.max(1) as isize;
    let delta = dx + dy * cols;
    let p = pos as isize + delta;
    p.clamp(0, len as isize - 1) as usize
}

/// The folder tree as a flat, top-to-bottom list of the rows currently visible
/// in the sidebar: `root` first, then a DFS pre-order walk that descends only
/// into expanded folders. This is the order arrow-key navigation moves through.
/// `is_expanded` reports whether a folder is open; `children` returns its
/// immediate subdirectories (already sorted). Pure so it can be unit-tested
/// independent of the filesystem and egui layout.
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
}

impl Playlist {
    /// Build a playlist from the images *inside* `dir` (a directory), sorted
    /// case-insensitively, positioned at index 0. Used when a folder is opened
    /// directly (→ Grid mode). Unlike `from_file`, it does NOT walk a parent.
    pub fn from_dir(dir: &Path) -> Self {
        let entries = sorted_images_in(dir);
        Self { entries, index: 0 }
    }

    /// Build a playlist from the folder containing `current`, positioned on it.
    pub fn from_file(current: &Path) -> Self {
        let dir = current.parent().unwrap_or_else(|| Path::new("."));
        let mut entries = sorted_images_in(dir);

        // Match by normalized identity so the position is correct even when the
        // entry and `current` differ in canonical form. normalize() falls back
        // to the raw path on failure, so two missing files compare by raw path
        // rather than spuriously matching as None == None.
        let target = crate::paths::normalize(current);
        let index = entries
            .iter()
            .position(|p| crate::paths::normalize(p) == target)
            .unwrap_or(0);

        // If the folder somehow yielded nothing, fall back to the single file.
        if entries.is_empty() {
            entries.push(current.to_path_buf());
        }

        Self { entries, index }
    }

    /// Index of the entry the playlist was positioned on at construction
    /// (the opened file for `from_file`, or 0 for `from_dir`).
    pub fn position(&self) -> usize {
        self.index
    }

    /// The full sorted image list (the filtered view is derived over this).
    pub fn entries(&self) -> &[PathBuf] {
        &self.entries
    }

    /// The path at `index` in the full list, if in range.
    pub fn entry(&self, index: usize) -> Option<&Path> {
        self.entries.get(index).map(|p| p.as_path())
    }

    /// Drop every entry for which `remove` returns true (e.g. files sent to the
    /// Trash). All indices are invalidated afterwards — the caller must rebuild
    /// any derived view (e.g. `recompute_visible`). `index` is clamped.
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
        // ratings: a=0, b=3, c=5, d=1
        let e = paths(&["a", "b", "c", "d"]);
        let rating = |p: &Path| match p.to_str().unwrap() {
            "b" => 3,
            "c" => 5,
            "d" => 1,
            _ => 0,
        };

        // Gte: unset(0) never matches a positive threshold.
        assert_eq!(visible_indices(&e, Some((Cmp::Gte, 3)), rating), vec![1, 2]);
        assert_eq!(visible_indices(&e, Some((Cmp::Gte, 1)), rating), vec![1, 2, 3]);
        assert_eq!(visible_indices(&e, Some((Cmp::Gte, 6)), rating), Vec::<usize>::new());

        // Eq.
        assert_eq!(visible_indices(&e, Some((Cmp::Eq, 5)), rating), vec![2]);
        assert_eq!(visible_indices(&e, Some((Cmp::Eq, 0)), rating), vec![0]);

        // Lte: unset(0) always matches; 0 matches Lte but not Gte>=1/Eq>=1.
        assert_eq!(visible_indices(&e, Some((Cmp::Lte, 1)), rating), vec![0, 3]);
        assert_eq!(visible_indices(&e, Some((Cmp::Lte, 5)), rating), vec![0, 1, 2, 3]);
        assert_eq!(visible_indices(&e, Some((Cmp::Lte, 0)), rating), vec![0]);
    }

    #[test]
    fn grid_move_math() {
        // 3 columns, 7 items (positions 0..=6).
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
        // Tree:  root -> [a -> [a1, a2], b]
        let kids = |p: &Path| match p.to_str().unwrap() {
            "root" => paths(&["root/a", "root/b"]),
            "root/a" => paths(&["root/a/a1", "root/a/a2"]),
            _ => vec![],
        };

        // Collapsed root: just the root row.
        let none = |_: &Path| false;
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &none, &kids),
            paths(&["root"])
        );

        // Root expanded, children collapsed: root + its two immediate children.
        let only_root = |p: &Path| p == Path::new("root");
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &only_root, &kids),
            paths(&["root", "root/a", "root/b"])
        );

        // root and `a` expanded: a's subtree appears before sibling b (pre-order).
        let root_and_a =
            |p: &Path| p == Path::new("root") || p == Path::new("root/a");
        assert_eq!(
            flatten_visible_tree(Path::new("root"), &root_and_a, &kids),
            paths(&["root", "root/a", "root/a/a1", "root/a/a2", "root/b"])
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
        // a.png, b.jpg, c.tiff sorted by name; opening b positions on index 1.
        assert!(pl.entries().len() >= 3);
        assert_eq!(
            pl.entry(pl.position()).unwrap().file_name().unwrap(),
            "b.jpg"
        );
        // Sorted case-insensitively by file name.
        assert_eq!(pl.entry(0).unwrap().file_name().unwrap(), "a.png");
        assert_eq!(pl.entry(2).unwrap().file_name().unwrap(), "c.tiff");
    }

    #[test]
    fn list_subdirs_returns_sorted_visible_dirs() {
        // Unique temp dir so parallel test runs don't collide.
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
