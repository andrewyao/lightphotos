//! The folders the user has added: the tops of the folder tree, remembered
//! across launches so the landing page can list them. Natively the list is
//! the session's `folders`; on the web it is the folders `web_fs` keeps in
//! IndexedDB, each of which the browser must allow again on a later visit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

/// The saved session's folders, tidied by `add_root`, and those of them
/// that aren't a folder right now. Empty on the web, whose kept folders
/// `poll_saved_folders` adds once they load.
pub(super) fn remembered(session: Option<&session::Session>) -> (Vec<PathBuf>, HashSet<PathBuf>) {
    if cfg!(target_arch = "wasm32") {
        return (Vec::new(), HashSet::new());
    }
    let mut roots = Vec::new();
    for root in session.map_or(&[][..], |s| s.folders.as_slice()) {
        add_root(&mut roots, root);
    }
    let missing = roots.iter().filter(|r| !r.is_dir()).cloned().collect();
    (roots, missing)
}

/// Add `dir` to `roots`, which stays sorted with no root inside another.
/// A folder already in the tree changes nothing; a folder holding roots
/// takes their place. Returns whether `roots` changed.
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
    /// Add `dir` to the tree's roots, unless a root already holds it. The
    /// session saves the list.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn add_root(&mut self, dir: &Path) {
        self.missing_roots.remove(dir);
        if add_root(&mut self.folder_roots, dir) {
            self.missing_roots.retain(|r| !r.starts_with(dir));
        }
    }

    /// Expand the tree from `dir`'s root down to `dir`, listing each folder
    /// on the way, so `dir` shows as a row.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn reveal_folder(&mut self, dir: &Path) {
        let Some(root) = self
            .folder_roots
            .iter()
            .find(|r| dir.starts_with(r))
            .cloned()
        else {
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
        #[cfg(target_arch = "wasm32")]
        self.forget_web_root(root);
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
                .find(|r| self.root_ready(r))
                .cloned();
            match next {
                Some(next) => self.open(next),
                None => self.close_folder(),
            }
        }
        self.request_redraw();
    }

    /// Whether `root` can open without asking: natively, it is there; on
    /// the web, the browser has allowed it on this visit.
    fn root_ready(&self, root: &Path) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            !self.missing_roots.contains(root)
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.web_root_allowed(root)
        }
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
    fn the_saved_folders_are_tidied_and_missing_ones_marked() {
        let saved = session::Session {
            folders: paths(&["/lp-gone/b", "/lp-gone/a", "/lp-gone/a/x"]),
            dir: None,
            photo: None,
            view: session::SessionView::Grid,
        };
        let (roots, missing) = remembered(Some(&saved));
        assert_eq!(roots, paths(&["/lp-gone/a", "/lp-gone/b"]));
        assert_eq!(missing, roots.iter().cloned().collect());
        assert_eq!(remembered(None), (Vec::new(), HashSet::new()));
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

    /// A real click on Add Folder under the Grid's folder list.
    #[test]
    fn add_folder_opens_the_picker() {
        use crate::app::test_support::{click, settled};

        let folder = photos("roots-plus");
        let mut app = App::new(None);
        app.open(folder.clone());
        let painted = settled(&mut app);
        let name = folder.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            !painted.texts().contains(&crate::i18n::t().open_folder),
            "the Grid's header no longer has Open Folder"
        );
        let add = painted.pos_of(crate::i18n::t().add_folder);
        assert!(
            add.y > painted.pos_of(&name).y,
            "Add Folder sits under the folder it would add after"
        );
        let (actions, _) = click(&mut app, add);
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
            painted.texts().contains(&crate::i18n::t().add_folder),
            "Add Folder adds another folder"
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
