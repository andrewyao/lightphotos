//! The folders the user has added: the tops of the folder tree, remembered
//! across launches natively so the landing page can list them. The web has
//! one root, the picked folder; removing it closes it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

/// `prefs` key for the added folders.
#[cfg(not(target_arch = "wasm32"))]
const FOLDERS_PREF: &str = "folders";

/// The remembered roots, and those of them that aren't a folder right now.
/// Empty in tests, which must never read the developer's own list, and on
/// the web, whose folders come back only through the picker.
pub(super) fn remembered() -> (Vec<PathBuf>, HashSet<PathBuf>) {
    #[cfg(any(test, target_arch = "wasm32"))]
    {
        (Vec::new(), HashSet::new())
    }
    #[cfg(not(any(test, target_arch = "wasm32")))]
    {
        let roots = crate::persist::prefs::load(FOLDERS_PREF)
            .map(|json| parse_roots(&json))
            .unwrap_or_default();
        let missing = roots.iter().filter(|r| !r.is_dir()).cloned().collect();
        (roots, missing)
    }
}

/// The saved list, tidied by `add_root`. An unreadable list is dropped
/// rather than stopping the launch.
#[cfg(not(target_arch = "wasm32"))]
#[cfg_attr(test, allow(dead_code))]
fn parse_roots(json: &str) -> Vec<PathBuf> {
    let saved: Vec<PathBuf> = match serde_json::from_str(json) {
        Ok(saved) => saved,
        Err(e) => {
            eprintln!("[folders] could not read the saved folders: {e}");
            return Vec::new();
        }
    };
    let mut roots = Vec::new();
    for root in &saved {
        add_root(&mut roots, root);
    }
    roots
}

/// The web keeps its one folder through the picker's handle instead.
#[cfg(not(target_arch = "wasm32"))]
fn save_roots(roots: &[PathBuf]) {
    // A test must never write the developer's own list.
    if cfg!(test) {
        return;
    }
    let saved = serde_json::to_string(roots)
        .map_err(|e| e.to_string())
        .and_then(|json| crate::persist::prefs::save(FOLDERS_PREF, &json));
    if let Err(e) = saved {
        eprintln!("[folders] could not save the folders: {e}");
    }
}

/// Add `dir` to `roots`, which stays sorted with no root inside another.
/// A folder already in the tree changes nothing; a folder holding roots
/// takes their place. Returns whether `roots` changed.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn add_root(roots: &mut Vec<PathBuf>, dir: &Path) -> bool {
    if roots.iter().any(|r| dir.starts_with(r)) {
        return false;
    }
    roots.retain(|r| !r.starts_with(dir));
    roots.push(dir.to_path_buf());
    roots.sort_by_cached_key(|r| r.to_string_lossy().to_lowercase());
    true
}

/// `root`, then each folder below it down to `dir`. Just `root` when `dir`
/// isn't inside it.
pub(super) fn folder_chain(root: &Path, dir: &Path) -> Vec<PathBuf> {
    if !dir.starts_with(root) {
        return vec![root.to_path_buf()];
    }
    let mut chain: Vec<PathBuf> = dir
        .ancestors()
        .take_while(|a| a.starts_with(root))
        .map(Path::to_path_buf)
        .collect();
    chain.reverse();
    chain
}

impl App {
    /// The root whose tree holds `dir`.
    pub(super) fn root_of(&self, dir: &Path) -> Option<&Path> {
        self.folder_roots
            .iter()
            .find(|r| dir.starts_with(r))
            .map(PathBuf::as_path)
    }

    /// Add `dir` to the tree's roots and remember it, unless a root already
    /// holds it.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn add_root(&mut self, dir: &Path) {
        self.missing_roots.remove(dir);
        if add_root(&mut self.folder_roots, dir) {
            self.missing_roots.retain(|r| !r.starts_with(dir));
            save_roots(&self.folder_roots);
        }
    }

    /// Expand the tree from `dir`'s root down to `dir`, listing each folder
    /// on the way, so `dir` shows as a row.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn reveal_folder(&mut self, dir: &Path) {
        let Some(root) = self.root_of(dir).map(Path::to_path_buf) else {
            return;
        };
        for folder in folder_chain(&root, dir) {
            self.ensure_subdirs(&folder);
            self.expanded.insert(folder);
        }
    }

    /// Drop `root` from the tree and forget it. Its files and sidecars stay.
    /// When the grid shows a folder in it, show the next root that is there,
    /// or the landing page when none is.
    pub(super) fn remove_root(&mut self, root: &Path) {
        let before = self.folder_roots.len();
        self.folder_roots.retain(|r| r != root);
        if self.folder_roots.len() == before {
            return;
        }
        #[cfg(not(target_arch = "wasm32"))]
        save_roots(&self.folder_roots);
        self.missing_roots.remove(root);
        self.expanded.retain(|p| !p.starts_with(root));
        self.subdirs.retain(|p, _| !p.starts_with(root));
        let showing = self
            .playlist
            .as_ref()
            .is_some_and(|pl| pl.dir().starts_with(root));
        if showing {
            let next = self
                .folder_roots
                .iter()
                .find(|r| !self.missing_roots.contains(*r))
                .cloned();
            match next {
                Some(next) => self.open(next),
                None => self.close_folder(),
            }
        }
        self.request_redraw();
    }

    /// Back to the landing page with no folder showing, finishing the jobs
    /// that write to the folder first, as `seed_mirrors` does.
    fn close_folder(&mut self) {
        self.teardown_loupe_state();
        self.cancel_auto_tone();
        self.cancel_scoring();
        self.cancel_delete();
        self.save_edit();
        self.burst_scan = None;
        self.expanded_stacks.clear();
        self.playlist = None;
        self.recompute_visible();
        self.sel = None;
        self.selected.clear();
        self.anchor = None;
        self.folder_sel = None;
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;

    fn paths(ps: &[&str]) -> Vec<PathBuf> {
        ps.iter().map(PathBuf::from).collect()
    }

    /// A folder holding `a.jpg` and a `sub` folder holding `b.jpg`.
    fn photos(tag: &str) -> PathBuf {
        let dir = temp_folder(tag);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.jpg"), b"").unwrap();
        std::fs::write(dir.join("sub/b.jpg"), b"").unwrap();
        dir
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
    fn an_unreadable_saved_list_gives_no_folders() {
        assert_eq!(parse_roots("not json"), Vec::<PathBuf>::new());
        assert_eq!(parse_roots(r#"{"root":3}"#), Vec::<PathBuf>::new());
        assert_eq!(parse_roots(r#"["/b","/a","/a/x"]"#), paths(&["/a", "/b"]));
    }

    #[test]
    fn opening_a_second_folder_keeps_the_first_in_the_tree() {
        let (a, b) = (photos("roots-a"), photos("roots-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.open_folder(a.join("sub"));
        app.open(b.clone());

        let mut want = vec![a.clone(), b.clone()];
        want.sort_by_cached_key(|r| r.to_string_lossy().to_lowercase());
        assert_eq!(app.folder_roots(), want.as_slice());
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
        let rows = app.visible_tree();
        for row in [&a, &a.join("sub"), &b, &b.join("sub")] {
            assert!(rows.contains(row), "{} not in the tree", row.display());
        }
    }

    #[test]
    fn opening_a_subfolder_of_a_root_reveals_it() {
        let a = photos("roots-reveal");
        let mut app = App::new(None);
        app.open(a.clone());
        app.expanded.clear();
        app.open(a.join("sub"));
        assert_eq!(app.folder_roots(), std::slice::from_ref(&a));
        assert!(app.expanded.contains(&a));
        assert_eq!(app.folder_sel.as_deref(), Some(a.join("sub").as_path()));
    }

    #[test]
    fn left_arrow_stops_at_every_root() {
        let (a, b) = (photos("roots-stop-a"), photos("roots-stop-b"));
        let mut app = App::new(None);
        app.open(a);
        app.open(b.clone());
        app.expanded.remove(&b);
        app.folder_collapse();
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
    }

    #[test]
    fn removing_the_root_showing_opens_another() {
        let (a, b) = (photos("roots-rm-a"), photos("roots-rm-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.open(b.clone());
        app.remove_root(&b);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&a));
        assert_eq!(app.folder_sel.as_deref(), Some(a.as_path()));
        assert!(
            b.join("a.jpg").exists(),
            "removing a folder keeps its files"
        );
    }

    #[test]
    fn removing_the_last_root_returns_to_the_landing_page() {
        let a = photos("roots-rm-last");
        let mut app = App::new(None);
        app.open(a.clone());
        app.remove_root(&a);
        assert!(app.folder_roots().is_empty());
        assert!(!app.has_playlist());
        assert_eq!(app.folder_sel, None);
        assert!(
            a.join("a.jpg").exists(),
            "removing a folder keeps its files"
        );
    }

    #[test]
    fn a_missing_root_stays_listed_and_does_not_open() {
        let gone = temp_folder("roots-gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let mut app = App::new(None);
        app.folder_roots = vec![gone.clone()];
        app.missing_roots.insert(gone.clone());
        app.open_folder(gone.clone());
        assert!(!app.has_playlist());
        assert!(app.is_missing_root(&gone));
        assert_eq!(app.folder_roots(), &[gone]);
    }

    #[test]
    fn a_root_that_comes_back_opens() {
        let back = photos("roots-back");
        let mut app = App::new(None);
        app.folder_roots = vec![back.clone()];
        app.missing_roots.insert(back.clone());
        app.open_folder(back.clone());
        assert!(app.has_playlist());
        assert!(!app.is_missing_root(&back));
    }

    /// A real click on the + in the Grid's Folders panel.
    #[test]
    fn the_folders_plus_opens_the_picker() {
        use crate::app::test_support::{click, settled};

        let mut app = App::new(None);
        app.open(photos("roots-plus"));
        let painted = settled(&mut app);
        assert!(
            !painted.texts().contains(&crate::i18n::t().open_folder),
            "the Grid's header no longer has Open Folder"
        );
        let (actions, _) = click(&mut app, painted.pos_of("+"));
        assert_eq!(actions, vec![crate::ui::UiAction::PickFolder]);
    }

    /// A real click on the landing page's Folders list, through the whole UI.
    #[test]
    fn the_landing_page_lists_remembered_folders() {
        use crate::app::test_support::{click, settled};

        let (a, b) = (photos("roots-home-a"), photos("roots-home-b"));
        let mut app = App::new(None);
        app.folder_roots = vec![a.clone(), b.clone()];
        let painted = settled(&mut app);
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(painted.texts().contains(&name(&a).as_str()));
        assert!(
            painted.texts().contains(&"+"),
            "a + adds another folder: {:?}",
            painted.texts()
        );
        let open_folder = crate::i18n::t().open_folder;
        assert_eq!(
            painted
                .texts()
                .iter()
                .filter(|t| **t == open_folder)
                .count(),
            1,
            "with folders listed, only the home card has Open Folder"
        );
        let (actions, _) = click(&mut app, painted.pos_of(&name(&b)));
        assert_eq!(actions, vec![crate::ui::UiAction::OpenFolder(b.clone())]);
        app.apply_ui_actions(actions);
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
    }
}
