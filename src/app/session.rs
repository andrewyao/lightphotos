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
    /// The Folders list a build before catalogs kept here. Saved again, as
    /// `folders`, until a launch has seeded a default catalog on disk with
    /// it; `App::new` empties it then, and every save after drops it.
    #[serde(rename = "folders", skip_serializing_if = "Vec::is_empty")]
    pub(crate) legacy_folders: Vec<PathBuf>,
    /// The folder showing, inside one of the catalog's folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<PathBuf>,
    pub(crate) photo: Option<PathBuf>,
    pub(crate) view: SessionView,
    /// The folder the user had open (`agent.rs`), which a restore puts back
    /// rather than make the restored folder the anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) anchor: Option<super::agent::Anchor>,
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
    #[serde(default)]
    anchor: Option<super::agent::Anchor>,
}

impl StoredSession {
    /// The session, with the Folders list an older build saved with it,
    /// which seeds the default catalog.
    fn into_session(self) -> Session {
        let (legacy_folders, dir) = match (self.folders, self.root) {
            (Some(folders), _) => (folders, self.dir),
            (None, Some(root)) => {
                let dir = self.dir.unwrap_or_else(|| root.clone());
                (vec![root], Some(dir))
            }
            (None, None) => (Vec::new(), self.dir),
        };
        Session {
            legacy_folders,
            dir,
            photo: self.photo,
            view: self.view,
            anchor: self.anchor,
        }
    }
}

impl Session {
    /// The saved session, if one parses. Nothing is written here: the
    /// first `save_session_if_changed` saves it in this build's shape.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Option<Session> {
        let json = crate::persist::prefs::load(SESSION_PREF)?;
        let stored: StoredSession = serde_json::from_str(&json).ok()?;
        Some(stored.into_session())
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
        legacy_folders: Vec::new(),
        dir: Some(root.clone()),
        photo: None,
        view: SessionView::Grid,
        anchor: None,
    })
}

impl App {
    /// The session as it stands. The landing page keeps the last place, and
    /// has no session before any folder was opened. The legacy Folders
    /// list rides along until the default catalog holds it.
    fn current_session(&self) -> Option<Session> {
        let legacy_folders = self.legacy_folders.clone();
        let Some(dir) = self.playlist.as_ref().map(|pl| pl.dir().to_path_buf()) else {
            return self.session.clone().map(|session| Session {
                legacy_folders,
                ..session
            });
        };
        let view = match self.mode {
            ViewMode::Loupe => SessionView::Loupe,
            ViewMode::Grid => SessionView::Grid,
        };
        Some(Session {
            legacy_folders,
            dir: Some(dir),
            photo: self.selected_path(),
            view,
            anchor: self.folder_anchor.clone(),
        })
    }

    /// Save the session when it has changed since the last save. Cheap enough
    /// to run once per event-loop turn.
    pub(crate) fn save_session_if_changed(&mut self) {
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
        // The browser's roots arrive with the catalog's record. A root it
        // keeps a handle for lists once it allows it again; one it doesn't
        // is missing.
        #[cfg(target_arch = "wasm32")]
        if self.restore_waits_for_catalog() {
            return;
        }
        #[cfg(target_arch = "wasm32")]
        let exists = |root: &Path| !self.missing_roots.contains(root);
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
        self.open(root.clone());
        let chain = session.folder_chain(self.catalog.folders());
        if let Some(dir) = chain.last().filter(|d| chain.len() > 1 && d.is_dir()) {
            for folder in &chain {
                self.ensure_subdirs(folder);
                self.expanded.insert(folder.clone());
            }
            self.load_folder(dir.clone());
        }
        // A session saved without an anchor (by an older build, or a
        // catalog's first folder) keeps the folder it reopened as the anchor.
        if let Some(saved) = session.anchor.as_ref() {
            self.folder_anchor = self.restored_anchor(saved, &root);
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

    /// The session the app saves showing `photo` (or `root`): the folder
    /// showing is the one the user opened, so it is the anchor too.
    fn session(root: &Path, photo: Option<&str>, view: SessionView) -> Session {
        let dir = photo.map_or(root.to_path_buf(), |p| {
            root.join(p).parent().unwrap().to_path_buf()
        });
        Session {
            legacy_folders: Vec::new(),
            anchor: super::super::agent::Anchor::new(&dir, true),
            dir: Some(dir),
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
        // The saved anchor was the subfolder; the restore shows the folder
        // above it, so no anchor: an agent gets nothing the user didn't have.
        let shown = Session {
            anchor: None,
            ..session(&root, None, SessionView::Grid)
        };
        assert_eq!(app.current_session(), Some(shown));
    }

    fn parse(json: &str) -> Option<Session> {
        serde_json::from_str::<StoredSession>(json)
            .ok()
            .map(StoredSession::into_session)
    }

    #[test]
    fn a_session_saved_before_subfolders_were_remembered_still_loads() {
        let old = parse(r#"{"root":"/photos","photo":"/photos/a.jpg","view":"Loupe"}"#).unwrap();
        assert_eq!(old.legacy_folders, vec![PathBuf::from("/photos")]);
        assert_eq!(
            old.folder_chain(&old.legacy_folders),
            vec![PathBuf::from("/photos")]
        );
    }

    /// The migration: an old session's `root` or `folders` become the
    /// catalog seed, saved again as `folders` until a catalog holds them.
    #[test]
    fn an_old_sessions_folders_ride_along_as_the_catalog_seed() {
        let old =
            r#"{"root":"/photos","dir":"/photos/sub","photo":"/photos/sub/a.jpg","view":"Loupe"}"#;
        let s = parse(old).unwrap();
        assert_eq!(s.legacy_folders, vec![PathBuf::from("/photos")]);
        assert_eq!(
            s.place(&s.legacy_folders),
            Some((Path::new("/photos"), Path::new("/photos/sub")))
        );
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("\"root\""), "{saved}");
        assert!(saved.contains(r#""folders":["/photos"]"#), "{saved}");
        assert_eq!(parse(&saved), Some(s.clone()));

        let held = Session {
            legacy_folders: Vec::new(),
            ..s
        };
        let saved = serde_json::to_string(&held).unwrap();
        assert!(!saved.contains("\"folders\""), "{saved}");
        assert_eq!(parse(&saved), Some(held));

        let s =
            parse(r#"{"folders":["/a","/b"],"root":"/old","dir":"/a","photo":null,"view":"Grid"}"#)
                .unwrap();
        assert_eq!(
            s.legacy_folders,
            vec![PathBuf::from("/a"), PathBuf::from("/b")]
        );
        assert_eq!(s.dir, Some(PathBuf::from("/a")));
    }

    /// A launch that cannot make the default catalog leaves the list in
    /// the app, and every save it makes keeps it for the launch that can.
    #[test]
    fn a_session_saved_before_the_default_catalog_exists_keeps_its_folders() {
        let root = tree("keep-legacy");
        let mut app = App::new(None);
        app.legacy_folders = vec![PathBuf::from("/kept")];
        app.save_session_if_changed();
        assert_eq!(app.session, None, "nothing to save on the landing page");

        app.open(root.clone());
        app.select_single(0);
        app.save_session_if_changed();
        let saved = serde_json::to_string(app.session.as_ref().unwrap()).unwrap();
        assert!(saved.contains(r#""folders":["/kept"]"#), "{saved}");
        let back = parse(&saved).unwrap();
        assert_eq!(back.legacy_folders, vec![PathBuf::from("/kept")]);
        assert_eq!(back.dir.as_deref(), Some(root.as_path()));

        app.legacy_folders.clear();
        app.save_session_if_changed();
        let saved = serde_json::to_string(app.session.as_ref().unwrap()).unwrap();
        assert!(!saved.contains("\"folders\""), "held now: {saved}");
    }

    #[test]
    fn a_session_with_no_folder_restores_nothing() {
        let s = parse(r#"{"photo":null,"view":"Grid"}"#).unwrap();
        assert_eq!(s.legacy_folders, Vec::<PathBuf>::new());
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
            legacy_folders: Vec::new(),
            dir: Some(PathBuf::from("/photos/2024/june")),
            photo: None,
            view: SessionView::Grid,
            anchor: None,
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

    #[test]
    fn a_restore_keeps_an_anchor_that_held_only_one_photos_folder() {
        use crate::command::{Command, OpenFolderParams};
        let root = tree("anchor-restore");
        let mut first = App::new(None);
        first.open_from_user(root.join("a.jpg"));
        let saved = first.current_session().unwrap();

        let mut next = App::new(None);
        next.add_root(&root);
        next.restore_session(&saved);
        let (tx, rx) = std::sync::mpsc::channel();
        let open = Command::OpenFolder(OpenFolderParams {
            path: root.join("sub"),
        });
        let request = crate::shell::control::Request {
            op: crate::shell::control::Op::Run(Box::new(open)),
            expires: web_time::Instant::now() + std::time::Duration::from_secs(60),
        };
        next.answer_control(request, tx);
        let answer = rx.try_recv().unwrap();
        assert!(
            answer.is_err(),
            "the restored anchor still holds no subfolder"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_restore_without_a_saved_anchor_anchors_the_restored_folder() {
        use crate::command::{Command, OpenFolderParams};
        let root = tree("anchor-unsaved");
        let saved = Session {
            anchor: None,
            ..session(&root, None, SessionView::Grid)
        };
        let mut app = app_listing(&[&root]);
        app.restore_session(&saved);
        let (tx, rx) = std::sync::mpsc::channel();
        let open = Command::OpenFolder(OpenFolderParams {
            path: root.join("sub"),
        });
        let request = crate::shell::control::Request {
            op: crate::shell::control::Op::Run(Box::new(open)),
            expires: web_time::Instant::now() + std::time::Duration::from_secs(60),
        };
        app.answer_control(request, tx);
        assert!(
            rx.try_recv().unwrap().is_ok(),
            "the restored folder is the anchor"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
