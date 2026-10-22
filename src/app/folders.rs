//! The folders the user has added: the tops of the folder tree. The list is
//! the open catalog's, remembered across launches so the landing page can
//! list them. On the web the catalog lists folders by the names `web_fs`
//! keeps their handles under, each of which the browser must allow again on
//! a later visit.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::*;

/// Those of `roots` that aren't a folder right now. Empty on the web, whose
/// missing roots land with the catalog's record in `poll_catalog_load`.
pub(super) fn missing_among(roots: &[PathBuf]) -> HashSet<PathBuf> {
    if cfg!(target_arch = "wasm32") {
        return HashSet::new();
    }
    roots.iter().filter(|r| !r.is_dir()).cloned().collect()
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
    /// catalog saves the list.
    pub(super) fn add_root(&mut self, dir: &Path) {
        self.missing_roots.remove(dir);
        if self.catalog.add_folder(dir) {
            self.missing_roots.retain(|r| !r.starts_with(dir));
        }
    }

    /// Expand the tree from `dir`'s root down to `dir`, listing each folder
    /// on the way, so `dir` shows as a row.
    pub(super) fn reveal_folder(&mut self, dir: &Path) {
        let Some(root) = self
            .catalog
            .folders()
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
        if !self.catalog.remove_folder(root) {
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
            // The roots passed over weren't there; a drive unplugged since
            // launch isn't in `missing_roots` yet.
            let roots = self.catalog.folders();
            let next = roots.iter().position(|r| self.root_ready(r));
            #[cfg(not(target_arch = "wasm32"))]
            self.missing_roots.extend(
                roots
                    .get(..next.unwrap_or(roots.len()))
                    .unwrap_or(&[])
                    .iter()
                    .cloned(),
            );
            match next.and_then(|i| self.catalog.folders().get(i).cloned()) {
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
            root.is_dir()
        }
        #[cfg(target_arch = "wasm32")]
        {
            self.web_root_allowed(root)
        }
    }

    /// Back to the landing page with no folder showing.
    pub(super) fn close_folder(&mut self) {
        self.teardown_loupe_state();
        self.finish_folder_jobs();
        self.forget_loupe_decodes();
        self.burst_scan = None;
        self.expanded_stacks.clear();
        self.playlist = None;
        self.recompute_visible();
        self.clear_selection();
        self.folder_sel = None;
        self.folder_anchor = None;
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

    /// A folder holding `a.jpg` and a `sub` folder holding `b.jpg`.
    fn photos(tag: &str) -> PathBuf {
        let dir = temp_folder(tag);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.jpg"), b"").unwrap();
        std::fs::write(dir.join("sub/b.jpg"), b"").unwrap();
        dir
    }

    #[test]
    fn the_catalogs_folders_that_are_not_there_are_marked_missing() {
        let here = temp_folder("roots-here");
        let roots = vec![PathBuf::from("/lp-gone/a"), here.clone()];
        assert_eq!(
            missing_among(&roots),
            HashSet::from([PathBuf::from("/lp-gone/a")])
        );
        assert!(missing_among(&[]).is_empty());
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

    fn one_pixel() -> std::sync::Arc<crate::decode::image_decode::DecodedImage> {
        use crate::decode::image_decode::{DecodedImage, DecodedImageFields};
        std::sync::Arc::new(DecodedImage::new_tracked(DecodedImageFields {
            width: 1,
            height: 1,
            rgba: vec![0; 4],
            pixel_format: crate::decode::image_decode::PixelFormat::Srgb8,
        }))
    }

    /// A full decode is a whole frame the Grid never shows, and another
    /// folder never shows the old one's previews either.
    #[test]
    fn leaving_the_loupe_drops_fulls_and_a_new_folder_drops_previews() {
        use crate::jobs::loader::Loader;
        use crate::jobs::thumbnail::Origin;
        let (a, b) = (photos("loupe-trim-a"), photos("loupe-trim-b"));
        let mut app = App::new(None);
        app.open(a.clone());
        app.loader = Some(Loader::queue_only_for_test());
        let photo = a.join("a.jpg");
        let loader = app.loader.as_mut().unwrap();
        loader.insert_full_external(photo.clone(), one_pixel(), Origin::Decoded);
        loader.insert_preview_external(photo.clone(), 2560, one_pixel(), Origin::Decoded);

        app.mode = ViewMode::Loupe;
        app.pump();
        assert!(app.loader.as_ref().unwrap().get_full(&photo).is_some());
        app.shown = crate::app::Shown::Full(photo.clone());
        app.mode = ViewMode::Grid;
        app.pump();
        let loader = app.loader.as_ref().unwrap();
        assert!(loader.get_full(&photo).is_none(), "left the Loupe");
        assert!(loader.get_preview(&photo, 2560).is_some());
        assert!(
            !matches!(app.shown, crate::app::Shown::Full(_)),
            "the full on screen goes too"
        );

        app.open(b);
        let loader = app.loader.as_ref().unwrap();
        assert!(loader.get_preview(&photo, 2560).is_none(), "changed folder");
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

    /// The other root's drive was unplugged after launch, so `missing_roots`
    /// doesn't know yet. It must not open as a photo with its parent as a
    /// new root.
    #[test]
    fn removing_the_root_showing_skips_a_root_that_has_gone() {
        let (gone, b) = (photos("roots-rm-gone"), photos("roots-rm-stay"));
        let mut app = App::new(None);
        app.open(gone.clone());
        app.open(b.clone());
        std::fs::remove_dir_all(&gone).unwrap();
        app.remove_root(&b);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&gone));
        assert!(app.is_missing_root(&gone));
        assert!(!app.has_playlist());
        assert_eq!(app.mode, ViewMode::Grid);
    }

    /// A path that is neither a folder nor a file, such as a photo deleted
    /// between Finder's open and ours, opens nothing.
    #[test]
    fn opening_a_path_that_is_gone_changes_nothing() {
        let gone = temp_folder("open-gone").join("x.jpg");
        let mut app = App::new(None);
        app.open(gone);
        assert!(app.folder_roots().is_empty());
        assert!(!app.has_playlist());
        assert_eq!(app.mode, ViewMode::Grid);
    }

    #[test]
    fn a_missing_root_stays_listed_and_does_not_open() {
        let gone = temp_folder("roots-gone");
        std::fs::remove_dir_all(&gone).unwrap();
        let mut app = App::new(None);
        app.catalog.add_folder(&gone);
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
        app.catalog.add_folder(&back);
        app.missing_roots.insert(back.clone());
        app.open_folder(back.clone());
        assert!(app.has_playlist());
        assert!(!app.is_missing_root(&back));
    }

    /// With folders listed but none open, the title names no folder; once
    /// one is open it names that one, not the first in the list.
    #[test]
    fn the_title_names_the_folder_showing_not_the_first_root() {
        let (a, b) = (photos("title-a"), photos("title-b"));
        let mut app = App::new(None);
        app.catalog.add_folder(&a);
        app.catalog.add_folder(&b);
        assert_eq!(app.window_title(), "LightPhotos");
        app.open(b.clone());
        let name = b.file_name().unwrap().to_string_lossy();
        assert_eq!(app.window_title(), format!("LightPhotos - {name}"));
    }

    /// A real click on Add Folder over the Grid's folder list.
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
        let row = painted.pos_of(&name);
        assert!(add.y < row.y, "Add Folder sits over the folder list");
        assert!(
            add.x < row.x,
            "Add Folder sits flush left, not centred over the list"
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
        app.catalog.add_folder(&a);
        app.catalog.add_folder(&b);
        let painted = settled(&mut app);
        let name = |p: &Path| p.file_name().unwrap().to_string_lossy().into_owned();
        assert!(painted.texts().contains(&name(&a).as_str()));
        assert!(
            painted.texts().contains(&crate::i18n::t().add_folder),
            "Add Folder adds another folder"
        );
        assert!(
            !painted.has(crate::i18n::t().open_folder),
            "with folders listed, no home card asks for one"
        );
        let (actions, _) = click(&mut app, painted.pos_of(&name(&b)));
        assert_eq!(actions, vec![crate::ui::UiAction::OpenFolder(b.clone())]);
        app.apply_ui_actions(actions);
        assert_eq!(app.folder_sel.as_deref(), Some(b.as_path()));
    }
}
