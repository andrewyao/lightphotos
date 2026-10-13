//! Where the user left off, saved as it changes and restored at the next
//! launch. The Folders list itself is the catalog's; the session only
//! says which of its folders was showing.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::*;

/// `prefs` key for the last session.
const SESSION_PREF: &str = "session";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SessionView {
    #[default]
    Grid,
    Loupe,
}

/// Where the user was: the folder showing, the selected photo, and whether
/// it is open in the Loupe. Natively every path is full; on the web each
/// starts with its folder's kept name.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub(crate) struct Session {
    /// The folder showing, inside one of the catalog's folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<PathBuf>,
    pub(crate) photo: Option<PathBuf>,
    pub(crate) view: SessionView,
}

/// A session as saved by this build, by the build before catalogs, which
/// kept the Folders list in `folders`, or by one before that, which kept
/// one folder in `root` and its subfolder showing in `dir`.
#[derive(Deserialize)]
struct StoredSession {
    #[serde(default)]
    folders: Option<Vec<PathBuf>>,
    #[serde(default)]
    root: Option<PathBuf>,
    #[serde(default)]
    dir: Option<PathBuf>,
    #[serde(default)]
    photo: Option<PathBuf>,
    #[serde(default)]
    view: SessionView,
}

impl StoredSession {
    /// The session, and the Folders list an older build saved with it, which
    /// seeds the default catalog once.
    fn split(self) -> (Session, Vec<PathBuf>) {
        let (legacy, dir) = match (self.folders, self.root) {
            (Some(folders), _) => (folders, self.dir),
            (None, Some(root)) => {
                let dir = self.dir.unwrap_or_else(|| root.clone());
                (vec![root], Some(dir))
            }
            (None, None) => (Vec::new(), self.dir),
        };
        let session = Session {
            dir,
            photo: self.photo,
            view: self.view,
        };
        (session, legacy)
    }
}

impl Session {
    /// The saved session, if one parses, with the Folders list an older
    /// build kept in it. A session saved in an older shape is saved again in
    /// this one, which drops that list.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Option<(Session, Vec<PathBuf>)> {
        let json = crate::persist::prefs::load(SESSION_PREF)?;
        let stored: StoredSession = serde_json::from_str(&json).ok()?;
        let (session, legacy) = stored.split();
        if serde_json::to_string(&session).ok().as_deref() != Some(json.as_str()) {
            session.save();
        }
        Some((session, legacy))
    }

    fn save(&self) {
        // A test must never write the developer's own session.
        if cfg!(test) {
            return;
        }
        let saved = serde_json::to_string(self)
            .map_err(|e| e.to_string())
            .and_then(|json| crate::persist::prefs::save(SESSION_PREF, &json));
        if let Err(e) = saved {
            eprintln!("[session] could not save the session: {e}");
        }
    }

    /// The folder to reopen, as (its root among `roots`, the folder itself),
    /// if there is one.
    pub(crate) fn place<'a>(&'a self, roots: &'a [PathBuf]) -> Option<(&'a Path, &'a Path)> {
        let dir = self.dir.as_deref()?;
        let root = roots.iter().find(|f| dir.starts_with(f))?;
        Some((root, dir))
    }

    /// The root, then each folder below it down to the one showing.
    pub(crate) fn folder_chain(&self, roots: &[PathBuf]) -> Vec<PathBuf> {
        self.place(roots)
            .map(|(root, dir)| super::folders::folder_chain(root, dir))
            .unwrap_or_default()
    }
}

/// The session to reopen: `session` when its folder is among `roots` and
/// `exists`, else the first of `roots` that `exists`, at its top in the
/// Grid. `None` leaves the landing page.
fn to_restore(
    session: Option<&Session>,
    roots: &[PathBuf],
    exists: impl Fn(&Path) -> bool,
) -> Option<Session> {
    if let Some(session) = session {
        if session.place(roots).is_some_and(|(root, _)| exists(root)) {
            return Some(session.clone());
        }
    }
    let root = roots.iter().find(|f| exists(f))?;
    Some(Session {
        dir: Some(root.clone()),
        photo: None,
        view: SessionView::Grid,
    })
}

impl App {
    /// The session as it stands. The landing page keeps the last place, and
    /// has no session before any folder was opened.
    fn current_session(&self) -> Option<Session> {
        let Some(dir) = self.playlist.as_ref().map(|pl| pl.dir().to_path_buf()) else {
            return self.session.clone();
        };
        let view = match self.mode {
            ViewMode::Loupe => SessionView::Loupe,
            ViewMode::Grid => SessionView::Grid,
        };
        Some(Session {
            dir: Some(dir),
            photo: self.selected_path(),
            view,
        })
    }

    /// Save the session when it has changed since the last save. Cheap enough
    /// to run once per event-loop turn.
    pub(crate) fn save_session_if_changed(&mut self) {
        // The web's Folders list fills once the kept names land. A save
        // before then would record no folders, and the startup restore
        // would then find no folder to reopen.
        #[cfg(target_arch = "wasm32")]
        if self.web.saved_folders_pending() {
            return;
        }
        let Some(now) = self.current_session() else {
            return;
        };
        if self.session.as_ref() != Some(&now) {
            now.save();
            self.session = Some(now);
        }
    }

    /// At launch, and after a catalog switch, open the last session where it
    /// left off, or the catalog's first listed folder when that place is
    /// gone. With no folder to open, the landing page stays.
    pub(crate) fn restore_last_session(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let exists = Path::is_dir;
        // The browser lists a kept folder only once it allows it again.
        #[cfg(target_arch = "wasm32")]
        let exists = |_: &Path| true;
        let Some(session) = to_restore(self.session.as_ref(), self.catalog.folders(), exists)
        else {
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        self.restore_session(&session);
        #[cfg(target_arch = "wasm32")]
        self.request_session_restore(session);
    }

    /// Open the session's root like `open`, then its subfolder, photo and
    /// view. A missing subfolder falls back to the root, and a missing photo
    /// leaves the grid with nothing selected.
    #[cfg(not(target_arch = "wasm32"))]
    fn restore_session(&mut self, session: &Session) {
        let Some((root, _)) = session.place(self.catalog.folders()) else {
            return;
        };
        let root = root.to_path_buf();
        self.open(root);
        let chain = session.folder_chain(self.catalog.folders());
        if let Some(dir) = chain.last().filter(|d| chain.len() > 1 && d.is_dir()) {
            for folder in &chain {
                self.ensure_subdirs(folder);
                self.expanded.insert(folder.clone());
            }
            self.load_folder(dir.clone());
        }
        self.restore_session_view(session);
    }

    /// Select the session's photo in the loaded playlist and, for a Loupe
    /// session, open it.
    pub(super) fn restore_session_view(&mut self, session: &Session) {
        let pos = session.photo.as_deref().and_then(|photo| {
            let pl = self.playlist.as_ref()?;
            let idx = pl.index_of(photo.file_name()?)?;
            (pl.entry(idx) == Some(photo))
                .then(|| self.place_of(idx).cell())
                .flatten()
        });
        if let Some(pos) = pos {
            self.select_single(pos);
            if session.view == SessionView::Loupe {
                self.enter_loupe();
            }
        }
        self.update_window_title();
        self.normalize_focus();
        self.request_redraw();
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;

    /// `root/a.jpg`, `root/b.jpg`, `root/sub/c.jpg`, `root/sub/d.jpg`.
    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("lightphotos-session-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        for p in ["a.jpg", "b.jpg", "sub/c.jpg", "sub/d.jpg"] {
            std::fs::write(root.join(p), b"").unwrap();
        }
        root
    }

    fn session(root: &Path, photo: Option<&str>, view: SessionView) -> Session {
        Session {
            dir: Some(photo.map_or(root.to_path_buf(), |p| {
                root.join(p).parent().unwrap().to_path_buf()
            })),
            photo: photo.map(|p| root.join(p)),
            view,
        }
    }

    /// An App whose catalog lists `roots`, as the last launch left it.
    fn app_listing(roots: &[&Path]) -> App {
        let mut app = App::new(None);
        for root in roots {
            app.catalog.add_folder(root);
        }
        app
    }

    #[test]
    fn the_landing_page_has_no_session() {
        assert_eq!(App::new(None).current_session(), None);
    }

    #[test]
    fn a_session_records_the_chosen_folder_photo_and_view() {
        let root = tree("derive");
        let mut app = App::new(None);
        app.open(root.clone());
        assert_eq!(
            app.current_session(),
            Some(session(&root, None, SessionView::Grid))
        );

        app.select_single(1);
        app.enter_loupe();
        assert_eq!(
            app.current_session(),
            Some(session(&root, Some("b.jpg"), SessionView::Loupe))
        );
    }

    #[test]
    fn browsing_into_a_subfolder_records_it_under_the_chosen_folder() {
        let root = tree("subfolder");
        let mut app = App::new(None);
        app.open(root.clone());
        app.select_single(0);
        app.save_session_if_changed();
        assert_eq!(
            app.session.as_ref(),
            Some(&session(&root, Some("a.jpg"), SessionView::Grid))
        );

        app.open_folder(root.join("sub"));
        app.select_single(1);
        app.enter_loupe();
        app.save_session_if_changed();
        assert_eq!(
            app.session.as_ref(),
            Some(&session(&root, Some("sub/d.jpg"), SessionView::Loupe))
        );
    }

    #[test]
    fn launching_restores_the_subfolder_photo_and_loupe() {
        let root = tree("restore-sub");
        let saved = session(&root, Some("sub/d.jpg"), SessionView::Loupe);
        let mut app = app_listing(&[&root]);
        app.session = Some(saved.clone());
        app.restore_last_session();

        assert_eq!(app.folder_roots(), std::slice::from_ref(&root));
        assert_eq!(app.folder_sel.as_deref(), Some(root.join("sub").as_path()));
        assert!(app.expanded.contains(&root));
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.current_session(), Some(saved));
    }

    #[test]
    fn a_missing_subfolder_reopens_the_chosen_folder() {
        let root = tree("sub-gone");
        let mut saved = session(&root, Some("sub/d.jpg"), SessionView::Loupe);
        saved.dir = Some(root.join("gone"));
        saved.photo = Some(root.join("gone/d.jpg"));
        let mut app = app_listing(&[&root]);
        app.restore_session(&saved);
        assert_eq!(
            app.current_session(),
            Some(session(&root, None, SessionView::Grid))
        );
    }

    fn parse(json: &str) -> Option<(Session, Vec<PathBuf>)> {
        serde_json::from_str::<StoredSession>(json)
            .ok()
            .map(StoredSession::split)
    }

    #[test]
    fn a_session_saved_before_subfolders_were_remembered_still_loads() {
        let (old, legacy) =
            parse(r#"{"root":"/photos","photo":"/photos/a.jpg","view":"Loupe"}"#).unwrap();
        assert_eq!(legacy, vec![PathBuf::from("/photos")]);
        assert_eq!(old.folder_chain(&legacy), vec![PathBuf::from("/photos")]);
    }

    /// The migration: an old session's `root` or `folders` become the
    /// catalog seed, and the saved form has neither.
    #[test]
    fn an_old_sessions_folders_leave_it_as_the_catalog_seed() {
        let old =
            r#"{"root":"/photos","dir":"/photos/sub","photo":"/photos/sub/a.jpg","view":"Loupe"}"#;
        let (s, legacy) = parse(old).unwrap();
        assert_eq!(legacy, vec![PathBuf::from("/photos")]);
        assert_eq!(
            s.place(&legacy),
            Some((Path::new("/photos"), Path::new("/photos/sub")))
        );
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("\"root\""), "{saved}");
        assert!(!saved.contains("\"folders\""), "{saved}");
        assert_eq!(parse(&saved), Some((s, Vec::new())));

        let (s, legacy) =
            parse(r#"{"folders":["/a","/b"],"root":"/old","dir":"/a","photo":null,"view":"Grid"}"#)
                .unwrap();
        assert_eq!(legacy, vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(s.dir, Some(PathBuf::from("/a")));
    }

    #[test]
    fn a_session_with_no_folder_restores_nothing() {
        let (s, legacy) = parse(r#"{"photo":null,"view":"Grid"}"#).unwrap();
        assert_eq!(legacy, Vec::<PathBuf>::new());
        assert_eq!(s.dir, None);
        assert_eq!(parse("{"), None);
        assert_eq!(parse(r#"{"dir":7}"#), None);
    }

    /// Removing the last folder leaves the catalog empty, so a restore stays
    /// on the landing page.
    #[test]
    fn an_empty_folders_list_restores_nothing() {
        let root = tree("emptied");
        let mut app = App::new(None);
        app.open(root.clone());
        app.save_session_if_changed();
        app.remove_root(&root);
        assert!(app.folder_roots().is_empty());
        app.restore_last_session();
        assert!(app.playlist.is_none());
    }

    #[test]
    fn a_missing_folder_leaves_the_landing_page() {
        let gone = Path::new("/lightphotos-no-such-folder");
        let mut app = app_listing(&[gone]);
        app.session = Some(session(gone, None, SessionView::Grid));
        app.restore_last_session();
        assert!(app.playlist.is_none());
        assert!(!app.folder_pick_pending());
    }

    /// The folder that was showing was removed from the list, so the next
    /// launch opens the folder still listed.
    #[test]
    fn removing_the_open_folder_reopens_another_listed_one() {
        let (open, other) = (tree("removed-open"), tree("still-listed"));
        let mut app = App::new(None);
        app.open(other.clone());
        app.open(open.clone());
        app.select_single(1);
        app.enter_loupe();
        app.save_session_if_changed();
        app.remove_root(&open);
        app.save_session_if_changed();
        let saved = app.session.clone();
        assert_eq!(app.folder_roots(), std::slice::from_ref(&other));

        let mut app = app_listing(&[&other]);
        app.session = saved;
        app.restore_last_session();
        assert_eq!(app.folder_sel.as_deref(), Some(other.as_path()));
        assert_eq!(app.mode, ViewMode::Grid);
    }

    /// The folder that was showing is gone from disk; another listed one
    /// opens at its top.
    #[test]
    fn a_missing_folder_falls_back_to_another_listed_one() {
        let other = tree("fallback");
        let gone = PathBuf::from("/lightphotos-no-such-folder");
        let mut app = app_listing(&[&gone, &other]);
        app.session = Some(session(&gone, None, SessionView::Loupe));
        app.restore_last_session();
        assert_eq!(app.folder_sel.as_deref(), Some(other.as_path()));
        assert_eq!(app.mode, ViewMode::Grid);
    }

    /// With no session at all, a catalog's first folder that exists opens.
    #[test]
    fn without_a_session_the_first_listed_folder_opens() {
        let (gone, here) = (PathBuf::from("/lightphotos-no-such-folder"), tree("first"));
        let mut app = app_listing(&[&gone, &here]);
        app.restore_last_session();
        assert_eq!(app.folder_sel.as_deref(), Some(here.as_path()));
    }

    #[test]
    fn the_folder_chain_runs_from_the_root_down_to_the_subfolder() {
        let s = Session {
            dir: Some(PathBuf::from("/photos/2024/june")),
            photo: None,
            view: SessionView::Grid,
        };
        let roots = vec![PathBuf::from("/photos")];
        assert_eq!(
            s.folder_chain(&roots),
            ["/photos", "/photos/2024", "/photos/2024/june"].map(PathBuf::from)
        );
        assert!(s.folder_chain(&[PathBuf::from("/elsewhere")]).is_empty());
    }

    #[test]
    fn choosing_another_folder_replaces_the_saved_one() {
        let (first, second) = (tree("first"), tree("second"));
        let mut app = App::new(None);
        app.save_session_if_changed();
        assert_eq!(
            app.session.as_ref(),
            None,
            "nothing to save on the landing page"
        );

        app.open(first.clone());
        app.save_session_if_changed();
        assert_eq!(
            app.session
                .as_ref()
                .and_then(|s| s.place(app.folder_roots()))
                .map(|p| p.0),
            Some(first.as_path())
        );

        app.open(second.clone());
        app.save_session_if_changed();
        assert_eq!(
            app.session.as_ref(),
            Some(&session(&second, None, SessionView::Grid))
        );
    }

    #[test]
    fn a_session_in_a_second_folder_records_that_folder_as_its_root() {
        let (first, second) = (tree("root-one"), tree("root-two"));
        let mut app = App::new(None);
        app.open(first.clone());
        app.open(second.clone());
        app.open_folder(second.join("sub"));
        app.select_single(0);
        let want = session(&second, Some("sub/c.jpg"), SessionView::Grid);
        assert_eq!(app.current_session().as_ref(), Some(&want));
        assert_eq!(
            want.place(app.folder_roots()),
            Some((second.as_path(), second.join("sub").as_path()))
        );
    }

    #[test]
    fn launching_restores_the_folder_photo_and_loupe() {
        let root = tree("restore");
        let saved = session(&root, Some("b.jpg"), SessionView::Loupe);
        let mut app = app_listing(&[&root]);
        app.session = Some(saved.clone());
        app.restore_last_session();

        assert_eq!(app.folder_roots(), std::slice::from_ref(&root));
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.current_session(), Some(saved));
    }

    #[test]
    fn a_first_launch_offers_open_folder_and_the_tour() {
        use crate::app::test_support::settled;
        use crate::i18n::t;

        let mut app = App::new(None);
        let painted = settled(&mut app);
        assert!(painted.texts().contains(&t().open_folder));
        assert!(painted.texts().contains(&t().take_tour));
    }

    #[test]
    fn a_grid_session_restores_the_selection_without_opening_the_loupe() {
        let root = tree("grid");
        let saved = session(&root, Some("b.jpg"), SessionView::Grid);
        let mut app = app_listing(&[&root]);
        app.restore_session(&saved);
        assert_eq!(app.mode, ViewMode::Grid);
        assert_eq!(app.current_session(), Some(saved));
    }

    #[test]
    fn a_missing_photo_reopens_the_folder_with_nothing_selected() {
        let root = tree("photo-gone");
        let mut app = app_listing(&[&root]);
        app.restore_session(&session(&root, Some("gone.jpg"), SessionView::Loupe));
        assert_eq!(
            app.current_session(),
            Some(session(&root, None, SessionView::Grid))
        );
    }
}
