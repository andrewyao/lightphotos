// SPDX-License-Identifier: MIT OR Apache-2.0

//! Folder listing, rating filters, burst grouping by time, and grid/tree
//! arrow-key movement.

use crate::decode::image_decode::is_image_extension;
use crate::sidecar::Flag;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

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

/// Which photos a flag filter shows. The grid offers every choice but
/// `NotRejected` and starts on `All`, as Lightroom does, dimming rejects rather than
/// hiding them; the Compare pane offers [`FlagFilter::COMPARE`] and starts
/// by hiding rejects.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FlagFilter {
    All,
    Picked,
    Unflagged,
    Rejected,
    /// Picked or unflagged.
    NotRejected,
}

impl FlagFilter {
    /// The Compare pane's choices, in menu order.
    pub const COMPARE: [FlagFilter; 4] = [
        FlagFilter::NotRejected,
        FlagFilter::Picked,
        FlagFilter::Unflagged,
        FlagFilter::Rejected,
    ];

    pub fn matches(self, flag: Option<Flag>) -> bool {
        match self {
            FlagFilter::All => true,
            FlagFilter::Picked => flag == Some(Flag::Pick),
            FlagFilter::Unflagged => flag.is_none(),
            FlagFilter::Rejected => flag == Some(Flag::Reject),
            FlagFilter::NotRejected => flag != Some(Flag::Reject),
        }
    }
}

/// Where a folder entry sits among the stacks, keyed by stack id `K`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot<K> {
    /// In no stack.
    Single,
    /// The stack's cover (its representative).
    Cover { stack: K, expanded: bool },
    /// Any other member of the stack.
    Member { stack: K, expanded: bool },
}

/// The entries among `0..n` that get a Grid cell, in entry order. A single
/// or a member of an expanded stack shows when it `passes`. A collapsed
/// stack shows as its cover when any of its entries passes, so a stack whose
/// cover fails the filters still shows when another member passes. The
/// other members of a collapsed stack never show.
#[hotpath::measure]
pub fn grid_cells<K: Eq + Hash>(
    n: usize,
    slot: impl Fn(usize) -> Slot<K>,
    passes: impl Fn(usize) -> bool,
) -> Vec<usize> {
    let slots: Vec<(Slot<K>, bool)> = (0..n).map(|i| (slot(i), passes(i))).collect();
    let mut stack_passes: HashMap<&K, bool> = HashMap::new();
    for (s, pass) in &slots {
        if let Slot::Cover {
            stack,
            expanded: false,
        }
        | Slot::Member {
            stack,
            expanded: false,
        } = s
        {
            *stack_passes.entry(stack).or_default() |= *pass;
        }
    }
    slots
        .iter()
        .enumerate()
        .filter(|(_, (s, pass))| match s {
            Slot::Single
            | Slot::Cover { expanded: true, .. }
            | Slot::Member { expanded: true, .. } => *pass,
            Slot::Cover { stack, .. } => stack_passes[stack],
            Slot::Member { .. } => false,
        })
        .map(|(i, _)| i)
        .collect()
}

/// Orders Grid `cells` by `key`, each cell taking the key of the entry
/// `anchor` names, so the members of an expanded stack sort by their cover
/// and stay together in their order. Cells with no key go last whichever
/// way the sort runs. Ties and that tail keep their anchors' entry order.
#[hotpath::measure]
pub fn sort_cells<K: Ord>(
    cells: &mut Vec<usize>,
    anchor: impl Fn(usize) -> usize,
    key: impl Fn(usize) -> Option<K>,
    descending: bool,
) {
    let mut keyed: Vec<(Option<K>, usize, usize)> = cells
        .iter()
        .map(|&i| {
            let a = anchor(i);
            (key(a), a, i)
        })
        .collect();
    // Stable, so a stack's members, which share a key and an anchor, keep
    // the order they came in.
    keyed.sort_by(|(ka, aa, _), (kb, ab, _)| {
        let by_key = match (ka, kb) {
            (Some(x), Some(y)) if descending => y.cmp(x),
            (Some(x), Some(y)) => x.cmp(y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        by_key.then(aa.cmp(ab))
    });
    *cells = keyed.into_iter().map(|(_, _, i)| i).collect();
}

/// Frames this close to the one before belong to the same burst. A chain of
/// them can run longer, so a slow burst stays whole.
pub(crate) const BURST_GAP: Duration = Duration::from_secs(1);

/// The bursts among `photos`: runs of two or more, in capture order, each
/// frame within `gap` of the one before. Ties in time keep name order.
pub(crate) fn bursts(mut photos: Vec<(PathBuf, SystemTime)>, gap: Duration) -> Vec<Vec<PathBuf>> {
    photos.sort_by(|(pa, ta), (pb, tb)| {
        ta.cmp(tb)
            .then_with(|| name_key(pa.file_name()).cmp(&name_key(pb.file_name())))
    });
    let times: Vec<_> = photos.iter().map(|(_, t)| Some(*t)).collect();
    let ids = group_by_time(&times, gap);
    let mut runs: Vec<Vec<PathBuf>> = Vec::new();
    let mut last = None;
    for ((path, _), id) in photos.into_iter().zip(ids) {
        match runs.last_mut() {
            Some(run) if last == Some(id) => run.push(path),
            _ => {
                runs.push(vec![path]);
                last = Some(id);
            }
        }
    }
    runs.retain(|r| r.len() >= 2);
    runs
}

/// A 0-based, increasing burst id per entry. A new burst starts when an entry's
/// capture time is more than `gap` from the last known time, in either
/// direction. Entries with no time (`None`) join the current burst and never
/// split one.
fn group_by_time(times: &[Option<SystemTime>], gap: Duration) -> Vec<u32> {
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
#[hotpath::measure]
/// `dir`'s entries whose name passes `keep_name` and whose kind is `kind`.
/// The kind comes from the directory listing, so a folder of 10k photos
/// costs no `stat` per entry. Only a symlink, or a listing without the
/// kind, is stat'ed, so a link counts as what it leads to.
/// Each kept path comes with whether the listing says it is a symlink.
fn read_dir_paths(
    dir: &Path,
    kind: EntryKind,
    keep_name: impl Fn(&Path) -> bool,
) -> Vec<(PathBuf, bool)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.filter_map(Result::ok)
        .map(|e| (e.path(), e.file_type()))
        .filter(|(p, _)| keep_name(p))
        .filter(|(p, ft)| match ft {
            Ok(ft) if !ft.is_symlink() => match kind {
                EntryKind::File => ft.is_file(),
                EntryKind::Dir => ft.is_dir(),
            },
            _ => match kind {
                EntryKind::File => p.is_file(),
                EntryKind::Dir => p.is_dir(),
            },
        })
        // A listing without the kind counts as a link, so it is checked.
        .map(|(p, ft)| (p, ft.map_or(true, |ft| ft.is_symlink())))
        .collect()
}

#[derive(Clone, Copy)]
enum EntryKind {
    File,
    // Only `list_subdirs` asks for folders, and the browser lists its
    // folders through `web_fs.rs`.
    #[cfg_attr(
        target_arch = "wasm32",
        allow(dead_code, reason = "the browser lists folders through web_fs.rs")
    )]
    Dir,
}

/// Sort case-insensitively by file name. `web_fs.rs` uses this too, so the
/// browser build lists folders in the same order.
pub(crate) fn sort_by_name(entries: &mut [PathBuf]) {
    entries.sort_by_cached_key(|p| name_key(p.file_name()));
}

fn name_key(name: Option<&OsStr>) -> Option<String> {
    name.map(|s| s.to_string_lossy().to_lowercase())
}

fn cmp_name_key(name: Option<&OsStr>, key: Option<&str>) -> std::cmp::Ordering {
    match (name.map(OsStr::to_string_lossy), key) {
        (Some(n), Some(k)) if n.is_ascii() => {
            n.bytes().map(|b| b.to_ascii_lowercase()).cmp(k.bytes())
        }
        (n, k) => n.map(|n| n.to_lowercase()).as_deref().cmp(&k),
    }
}

#[hotpath::measure]
/// The images in `dir`, sorted, and which of them are symlinks.
fn sorted_images_in(dir: &Path) -> (Vec<PathBuf>, HashSet<PathBuf>) {
    let listed = read_dir_paths(dir, EntryKind::File, is_image_extension);
    let links = listed
        .iter()
        .filter(|(_, link)| *link)
        .map(|(p, _)| p.clone())
        .collect();
    let mut entries: Vec<PathBuf> = listed.into_iter().map(|(p, _)| p).collect();
    sort_by_name(&mut entries);
    (entries, links)
}

/// Whether a folder named `name` shows in the folder tree. Hides dot-folders
/// and macOS bundles (`.app`, `.photoslibrary`). Shared by the native and
/// browser folder listings.
pub fn is_listable_subdir(name: &str) -> bool {
    if name.starts_with('.') {
        return false;
    }
    // A catalog saved inside a photo folder is not a folder of photos.
    #[cfg(not(target_arch = "wasm32"))]
    if crate::persist::catalog::CatalogRef::is_catalog_name(name) {
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
#[hotpath::measure]
pub fn list_subdirs(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = read_dir_paths(dir, EntryKind::Dir, |p| {
        p.file_name()
            .and_then(|s| s.to_str())
            .is_some_and(is_listable_subdir)
    })
    .into_iter()
    .map(|(p, _)| p)
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
    /// The entries that are symlinks, noted from the listing, so a check
    /// for links that lead out of the folder looks only at these.
    // The browser lists file handles, never links, so nothing reads it there.
    #[cfg_attr(
        target_arch = "wasm32",
        allow(dead_code, reason = "the browser lists file handles, never links")
    )]
    links: HashSet<PathBuf>,
}

impl Playlist {
    /// The entries that are symlinks.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn links(&self) -> &HashSet<PathBuf> {
        &self.links
    }

    /// The images inside `dir`, positioned at index 0. Used when a folder is
    /// opened directly.
    #[cfg(not(target_arch = "wasm32"))]
    #[hotpath::measure]
    pub fn from_dir(dir: &Path) -> Self {
        let (entries, links) = sorted_images_in(dir);
        Self {
            entries,
            index: 0,
            dir: dir.to_path_buf(),
            links,
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
            links: HashSet::new(),
        }
    }

    /// The images in `current`'s folder, positioned on `current`.
    #[hotpath::measure]
    pub fn from_file(current: &Path) -> Self {
        let dir = current.parent().unwrap_or_else(|| Path::new("."));
        let (mut entries, mut links) = sorted_images_in(dir);

        // Every entry is `dir` joined with a name, so the entry with
        // `current`'s name is `current`. Only when no name matches (another
        // case on a case-insensitive disk) compare normalized paths, which
        // costs a `realpath` per entry.
        let index = current
            .file_name()
            .and_then(|name| entries.iter().position(|p| p.file_name() == Some(name)))
            .or_else(|| {
                let target = crate::paths::normalize(current);
                entries
                    .iter()
                    .position(|p| crate::paths::normalize(p) == target)
            })
            .unwrap_or(0);

        if entries.is_empty() {
            if current
                .symlink_metadata()
                .map_or(true, |m| m.file_type().is_symlink())
            {
                links.insert(current.to_path_buf());
            }
            entries.push(current.to_path_buf());
        }

        Self {
            entries,
            index,
            dir: dir.to_path_buf(),
            links,
        }
    }

    /// The starting index: the opened file for `from_file`, 0 for `from_dir`.
    pub fn position(&self) -> usize {
        self.index
    }

    /// The folder these images live in. The folder store sidecar sits here.
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

    /// The index of the entry named `name`. A binary search on
    /// [`sort_by_name`]'s key finds the run of case-insensitive twins, and a
    /// scan of that run finds the exact name.
    pub fn index_of(&self, name: &OsStr) -> Option<usize> {
        let key = name_key(Some(name));
        let key = key.as_deref();
        let start = self
            .entries
            .partition_point(|p| cmp_name_key(p.file_name(), key).is_lt());
        self.entries[start..]
            .iter()
            .take_while(|p| cmp_name_key(p.file_name(), key).is_eq())
            .position(|p| p.file_name() == Some(name))
            .map(|i| start + i)
    }

    /// Drop entries where `remove` is true, such as trashed files, and return
    /// the old indices of the dropped entries, ascending. Every later index
    /// shifts down, so indices held elsewhere go through [`shift_index`].
    pub fn remove_matching(&mut self, remove: impl Fn(&Path) -> bool) -> Vec<usize> {
        let removed: Vec<usize> = (0..self.entries.len())
            .filter(|&i| remove(&self.entries[i]))
            .collect();
        self.entries.retain(|p| !remove(p.as_path()));
        if self.index >= self.entries.len() {
            self.index = self.entries.len().saturating_sub(1);
        }
        removed
    }
}

/// Where old index `i` sits after the entries at `removed` (ascending) were
/// dropped, or `None` if `i` was one of them.
pub fn shift_index(i: usize, removed: &[usize]) -> Option<usize> {
    match removed.binary_search(&i) {
        Ok(_) => None,
        Err(before) => Some(i - before),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(names: &[&str]) -> Vec<PathBuf> {
        names.iter().map(PathBuf::from).collect()
    }

    use Slot::{Cover, Member, Single};

    /// `grid_cells` over a fixed slot table, with `passes` per entry.
    fn cells(slots: &[Slot<&str>], passes: &[bool]) -> Vec<usize> {
        assert_eq!(slots.len(), passes.len());
        grid_cells(slots.len(), |i| slots[i], |i| passes[i])
    }

    fn cover(stack: &str, expanded: bool) -> Slot<&str> {
        Cover { stack, expanded }
    }

    fn member(stack: &str, expanded: bool) -> Slot<&str> {
        Member { stack, expanded }
    }

    #[test]
    fn sorted_cells_put_the_keyless_last_in_entry_order_either_way() {
        let keys = [None, Some(2), Some(1), None, Some(2)];
        for (descending, want) in [(false, [2, 1, 4, 0, 3]), (true, [1, 4, 2, 0, 3])] {
            let mut cells = vec![0, 1, 2, 3, 4];
            sort_cells(&mut cells, |i| i, |i| keys[i], descending);
            assert_eq!(cells, want, "descending: {descending}");
        }
    }

    #[test]
    fn sorted_cells_keep_a_stack_together_under_its_cover() {
        // Entries 1, 3 and 4 are a stack with cover 3; 0 and 2 are singles.
        let anchor = |i| if i == 1 || i == 4 { 3 } else { i };
        let keys = [Some(5), Some(9), Some(1), Some(3), Some(0)];
        let mut cells = vec![0, 1, 2, 3, 4];
        sort_cells(&mut cells, anchor, |i| keys[i], false);
        assert_eq!(cells, [2, 1, 3, 4, 0]);
        sort_cells(&mut cells, anchor, |i| keys[i], true);
        assert_eq!(cells, [0, 1, 3, 4, 2]);
    }

    #[test]
    fn singles_show_exactly_when_they_pass() {
        assert_eq!(cells(&[Single; 3], &[true; 3]), vec![0, 1, 2]);
        assert_eq!(cells(&[Single; 3], &[false, true, false]), vec![1]);
    }

    #[test]
    fn unfiltered_collapsed_stacks_show_covers_and_singles_in_order() {
        let slots = [
            cover("x", false),
            member("x", false),
            Single,
            member("y", false),
            cover("y", false),
            Single,
        ];
        assert_eq!(cells(&slots, &[true; 6]), vec![0, 2, 4, 5]);
    }

    #[test]
    fn a_collapsed_stack_shows_its_cover_when_only_a_member_passes() {
        let slots = [
            cover("x", false),
            member("x", false),
            member("x", false),
            Single,
        ];
        assert_eq!(cells(&slots, &[false, false, true, false]), vec![0]);
    }

    #[test]
    fn a_collapsed_stack_with_no_passing_entry_is_absent() {
        let slots = [Single, cover("x", false), member("x", false), Single];
        assert_eq!(cells(&slots, &[true, false, false, true]), vec![0, 3]);
    }

    #[test]
    fn an_expanded_stack_shows_only_its_passing_entries_in_order() {
        let slots = [
            member("x", true),
            Single,
            cover("x", true),
            member("x", true),
            member("x", true),
        ];
        assert_eq!(
            cells(&slots, &[true, true, false, true, false]),
            vec![0, 1, 3]
        );
        assert_eq!(cells(&slots, &[true; 5]), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn expanding_one_stack_leaves_another_collapsed() {
        let slots = [
            cover("x", true),
            member("x", true),
            cover("y", false),
            member("y", false),
        ];
        assert_eq!(cells(&slots, &[false, true, false, true]), vec![1, 2]);
    }

    fn playlist(names: &[&str]) -> Playlist {
        let mut entries: Vec<PathBuf> = names.iter().map(|n| Path::new("/f").join(n)).collect();
        sort_by_name(&mut entries);
        Playlist {
            entries,
            index: 0,
            dir: PathBuf::from("/f"),
            links: HashSet::new(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_playlist_notes_which_photos_are_links() {
        let dir = std::env::temp_dir().join(format!("lp-playlist-links-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.jpg"), []).unwrap();
        std::os::unix::fs::symlink(dir.join("a.jpg"), dir.join("b.jpg")).unwrap();
        let pl = Playlist::from_dir(&dir);
        assert_eq!(pl.entries().len(), 2);
        assert_eq!(pl.links().iter().collect::<Vec<_>>(), [&dir.join("b.jpg")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn index_of_finds_the_exact_name_among_case_ties() {
        let pl = playlist(&["b.jpg", "IMG.jpg", "a.jpg", "img.JPG", "Img.jpg"]);
        for name in ["IMG.jpg", "img.JPG", "Img.jpg", "a.jpg", "b.jpg"] {
            let i = pl.index_of(OsStr::new(name)).expect(name);
            assert_eq!(pl.entry(i).unwrap().file_name().unwrap(), name);
        }
        assert_eq!(pl.index_of(OsStr::new("img.jpg")), None);
        assert_eq!(pl.index_of(OsStr::new("c.jpg")), None);
    }

    #[test]
    fn cmp_name_key_orders_exactly_like_name_key() {
        let names = [
            None,
            Some("a.jpg"),
            Some("B.JPG"),
            Some("img_0001.JPG"),
            Some("IMG_0001.jpg"),
            Some("Été.jpg"),
            Some("été.JPG"),
            Some("ÄRGER.png"),
            Some("z.jpg"),
        ];
        for a in names {
            for b in names {
                let key = name_key(b.map(OsStr::new));
                assert_eq!(
                    cmp_name_key(a.map(OsStr::new), key.as_deref()),
                    name_key(a.map(OsStr::new)).cmp(&key),
                    "{a:?} vs {b:?}"
                );
            }
        }
    }

    #[test]
    fn index_of_finds_every_entry_of_a_sorted_listing() {
        let names: Vec<String> = (0..300)
            .map(|i| match i % 3 {
                0 => format!("P{i:04}.JPG"),
                1 => format!("p{:04}.jpg", i - 1),
                _ => format!("dsc_{i}.nef"),
            })
            .collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let pl = playlist(&refs);
        for (i, p) in pl.entries().iter().enumerate() {
            assert_eq!(pl.index_of(p.file_name().unwrap()), Some(i), "{p:?}");
        }
    }

    #[test]
    fn each_flag_filter_shows_its_flags() {
        use FlagFilter::*;
        let shown = |f: FlagFilter| {
            [Some(Flag::Pick), Some(Flag::Reject), None].map(|flag| f.matches(flag))
        };
        assert_eq!(shown(NotRejected), [true, false, true]);
        assert_eq!(shown(All), [true, true, true]);
        assert_eq!(shown(Picked), [true, false, false]);
        assert_eq!(shown(Rejected), [false, true, false]);
        assert_eq!(shown(Unflagged), [false, false, true]);
    }

    #[test]
    fn rating_filter_matches_per_cmp() {
        // Entries rated 0 (unset), 3, 5 and 1.
        let ratings = [0, 3, 5, 1];
        let shown = |cmp: Cmp, value: u8| {
            grid_cells(
                ratings.len(),
                |_| Slot::<()>::Single,
                |i| cmp.matches(ratings[i], value),
            )
        };
        // Unset (0) never passes a positive Gte.
        assert_eq!(shown(Cmp::Gte, 3), vec![1, 2]);
        assert_eq!(shown(Cmp::Gte, 1), vec![1, 2, 3]);
        assert_eq!(shown(Cmp::Gte, 6), Vec::<usize>::new());
        assert_eq!(shown(Cmp::Eq, 5), vec![2]);
        assert_eq!(shown(Cmp::Eq, 0), vec![0]);
        // Unset (0) always passes Lte.
        assert_eq!(shown(Cmp::Lte, 1), vec![0, 3]);
        assert_eq!(shown(Cmp::Lte, 5), vec![0, 1, 2, 3]);
        assert_eq!(shown(Cmp::Lte, 0), vec![0]);
    }

    fn t(secs: u64) -> Option<SystemTime> {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(secs))
    }

    #[test]
    fn bursts_chain_close_frames_and_drop_lone_shots() {
        let at = |ms: u64| SystemTime::UNIX_EPOCH + Duration::from_millis(ms);
        let p = |n: &str| PathBuf::from(format!("/f/{n}.jpg"));
        // Listed out of order: c and d share a time, so name breaks the tie.
        let photos = vec![
            (p("d"), at(1_100)),
            (p("a"), at(0)),
            (p("lone"), at(5_000)),
            (p("b"), at(400)),
            (p("c"), at(1_100)),
            (p("e"), at(9_000)),
            (p("f"), at(9_900)),
        ];
        assert_eq!(
            bursts(photos, BURST_GAP),
            vec![vec![p("a"), p("b"), p("c"), p("d")], vec![p("e"), p("f")],]
        );
        assert!(bursts(vec![(p("a"), at(0))], BURST_GAP).is_empty());
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
        assert!(!is_listable_subdir("default.lightphotos"));
        assert!(!is_listable_subdir("Trip 2024.lightphotos"));
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
