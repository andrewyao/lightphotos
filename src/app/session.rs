//! Where the user left off, saved as it changes and restored at the next
//! launch.

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

/// The folders the user added, and where they were: the folder showing, the
/// selected photo, and whether it is open in the Loupe. Natively every path
/// is full; on the web each starts with its folder's kept name.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "StoredSession")]
pub(crate) struct Session {
    /// The Folders panel's list.
    pub(crate) folders: Vec<PathBuf>,
    /// The folder showing, inside one of `folders`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dir: Option<PathBuf>,
    pub(crate) photo: Option<PathBuf>,
    pub(crate) view: SessionView,
}

/// A session as saved by this build or by one before `folders`, which kept
/// one folder in `root` and its subfolder showing in `dir`, `None` for
/// `root` itself.
#[derive(Deserialize)]
struct StoredSession {
    folders: Option<Vec<PathBuf>>,
    root: Option<PathBuf>,
    #[serde(default)]
    dir: Option<PathBuf>,
    #[serde(default)]
    photo: Option<PathBuf>,
    #[serde(default)]
    view: SessionView,
}

impl TryFrom<StoredSession> for Session {
    type Error = &'static str;

    /// `folders` when it is there; otherwise the old `root` becomes the
    /// list. A session with neither has nothing to reopen.
    fn try_from(stored: StoredSession) -> Result<Self, Self::Error> {
        let (folders, dir) = match (stored.folders, stored.root) {
            (Some(folders), _) => (folders, stored.dir),
            (None, Some(root)) => {
                let dir = stored.dir.unwrap_or_else(|| root.clone());
                (vec![root], Some(dir))
            }
            (None, None) => return Err("a session needs folders or root"),
        };
        Ok(Session {
            folders,
            dir,
            photo: stored.photo,
            view: stored.view,
        })
    }
}

impl Session {
    /// The saved session, if one parses. A session saved in an older shape
    /// is saved again in this one, which drops its `root`.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Option<Session> {
        let json = crate::persist::prefs::load(SESSION_PREF)?;
        let session: Session = serde_json::from_str(&json).ok()?;
        if serde_json::to_string(&session).ok().as_deref() != Some(json.as_str()) {
            session.save();
        }
        Some(session)
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

    /// The folder to reopen, as (its root, the folder itself), if there is
    /// one.
    pub(crate) fn place(&self) -> Option<(&Path, &Path)> {
        let dir = self.dir.as_deref()?;
        let root = self.folders.iter().find(|f| dir.starts_with(f))?;
        Some((root, dir))
    }

    /// The session to reopen at launch: this one when its folder is listed
    /// and `exists`, else the first listed folder that `exists`, at its top
    /// in the Grid.
    fn to_restore(&self, exists: impl Fn(&Path) -> bool) -> Option<Session> {
        if self.place().is_some_and(|(root, _)| exists(root)) {
            return Some(self.clone());
        }
        let root = self.folders.iter().find(|f| exists(f))?;
        Some(Session {
            folders: self.folders.clone(),
            dir: Some(root.clone()),
            photo: None,
            view: SessionView::Grid,
        })
    }

    /// The root, then each folder below it down to the one showing.
    pub(crate) fn folder_chain(&self) -> Vec<PathBuf> {
        self.place()
            .map(|(root, dir)| super::folders::folder_chain(root, dir))
            .unwrap_or_default()
    }
}

impl App {
    /// The session as it stands. The landing page keeps the last place, and
    /// has no session before any folder was opened.
    fn current_session(&self) -> Option<Session> {
        let folders = self.folder_roots.clone();
        let Some(dir) = self.playlist.as_ref().map(|pl| pl.dir().to_path_buf()) else {
            return self
                .session
                .clone()
                .map(|session| Session { folders, ..session });
        };
        let view = match self.mode {
            ViewMode::Loupe => SessionView::Loupe,
            ViewMode::Grid => SessionView::Grid,
        };
        Some(Session {
            folders,
            dir: Some(dir),
            photo: self.selected_path(),
            view,
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

    /// At launch, open the last session where it left off, or the first
    /// listed folder when that place is gone. With no folders left, the
    /// landing page stays.
    pub(crate) fn restore_last_session(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let exists = Path::is_dir;
        // The browser lists a kept folder only once it allows it again.
        #[cfg(target_arch = "wasm32")]
        let exists = |_: &Path| true;
        let Some(session) = self.session.as_ref().and_then(|s| s.to_restore(exists)) else {
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
        let Some((root, _)) = session.place() else {
            return;
        };
        self.open(root.to_path_buf());
        let chain = session.folder_chain();
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
            folders: vec![root.to_path_buf()],
            dir: Some(photo.map_or(root.to_path_buf(), |p| {
                root.join(p).parent().unwrap().to_path_buf()
            })),
            photo: photo.map(|p| root.join(p)),
            view,
        }
    }

    /// `first` and `second` in the order the Folders list keeps them.
    fn both(first: &Path, second: &Path) -> Vec<PathBuf> {
        let mut roots = vec![first.to_path_buf(), second.to_path_buf()];
        roots.sort_by_cached_key(|r| r.to_string_lossy().to_lowercase());
        roots
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
        let mut app = App::new(None);
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
        let mut app = App::new(None);
        app.restore_session(&saved);
        assert_eq!(
            app.current_session(),
            Some(session(&root, None, SessionView::Grid))
        );
    }

    fn parse(json: &str) -> Option<Session> {
        serde_json::from_str(json).ok()
    }

    #[test]
    fn a_session_saved_before_subfolders_were_remembered_still_loads() {
        let old = parse(r#"{"root":"/photos","photo":"/photos/a.jpg","view":"Loupe"}"#).unwrap();
        assert_eq!(old.folders, vec![PathBuf::from("/photos")]);
        assert_eq!(old.folder_chain(), vec![PathBuf::from("/photos")]);
    }

    /// The migration: an old session's `root` becomes `folders`, and the
    /// saved form never has `root` again.
    #[test]
    fn an_old_sessions_root_becomes_its_folders() {
        let old =
            r#"{"root":"/photos","dir":"/photos/sub","photo":"/photos/sub/a.jpg","view":"Loupe"}"#;
        let s = parse(old).unwrap();
        assert_eq!(s.folders, vec![PathBuf::from("/photos")]);
        assert_eq!(
            s.place(),
            Some((Path::new("/photos"), Path::new("/photos/sub")))
        );
        let saved = serde_json::to_string(&s).unwrap();
        assert!(!saved.contains("\"root\""), "{saved}");
        assert_eq!(parse(&saved), Some(s));
    }

    #[test]
    fn folders_win_over_a_leftover_root() {
        let s = parse(r#"{"folders":["/a"],"root":"/old","dir":"/a","photo":null,"view":"Grid"}"#)
            .unwrap();
        assert_eq!(s.folders, vec![PathBuf::from("/a")]);
        assert!(!serde_json::to_string(&s).unwrap().contains("/old"));
    }

    #[test]
    fn a_session_with_neither_folders_nor_root_is_none() {
        assert_eq!(parse(r#"{"photo":null,"view":"Grid"}"#), None);
        assert!(parse(r#"{"folders":[],"photo":null,"view":"Grid"}"#).is_some());
    }

    /// Removing the last folder leaves an empty list, so the next launch
    /// stays on the landing page.
    #[test]
    fn an_empty_folders_list_restores_nothing() {
        let root = tree("emptied");
        let mut app = App::new(None);
        app.open(root.clone());
        app.save_session_if_changed();
        app.remove_root(&root);
        app.save_session_if_changed();
        let saved = app.session.clone();
        assert_eq!(saved.as_ref().map(|s| s.folders.len()), Some(0));

        let mut app = App::new(None);
        app.session = saved;
        app.restore_last_session();
        assert!(app.playlist.is_none());
    }

    #[test]
    fn a_missing_folder_leaves_the_landing_page() {
        let mut app = App::new(None);
        app.session = Some(session(
            Path::new("/lightphotos-no-such-folder"),
            None,
            SessionView::Grid,
        ));
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
        assert_eq!(
            saved.as_ref().map(|s| s.folders.clone()),
            Some(vec![other.clone()])
        );

        let mut app = App::new(None);
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
        let mut saved = session(&gone, None, SessionView::Loupe);
        saved.folders = both(&gone, &other);
        let mut app = App::new(None);
        app.session = Some(saved);
        app.restore_last_session();
        assert_eq!(app.folder_sel.as_deref(), Some(other.as_path()));
        assert_eq!(app.mode, ViewMode::Grid);
    }

    #[test]
    fn the_folder_chain_runs_from_the_root_down_to_the_subfolder() {
        let s = Session {
            folders: vec![PathBuf::from("/photos")],
            dir: Some(PathBuf::from("/photos/2024/june")),
            photo: None,
            view: SessionView::Grid,
        };
        assert_eq!(
            s.folder_chain(),
            ["/photos", "/photos/2024", "/photos/2024/june"].map(PathBuf::from)
        );
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
            app.session.as_ref().and_then(Session::place).map(|p| p.0),
            Some(first.as_path())
        );

        app.open(second.clone());
        app.save_session_if_changed();
        assert_eq!(
            app.session.as_ref(),
            Some(&Session {
                folders: both(&first, &second),
                ..session(&second, None, SessionView::Grid)
            })
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
        let want = Session {
            folders: both(&first, &second),
            ..session(&second, Some("sub/c.jpg"), SessionView::Grid)
        };
        assert_eq!(app.current_session().as_ref(), Some(&want));
        assert_eq!(
            want.place(),
            Some((second.as_path(), second.join("sub").as_path()))
        );
    }

    #[test]
    fn launching_restores_the_folder_photo_and_loupe() {
        let root = tree("restore");
        let saved = session(&root, Some("b.jpg"), SessionView::Loupe);
        let mut app = App::new(None);
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
        let mut app = App::new(None);
        app.restore_session(&saved);
        assert_eq!(app.mode, ViewMode::Grid);
        assert_eq!(app.current_session(), Some(saved));
    }

    #[test]
    fn a_missing_photo_reopens_the_folder_with_nothing_selected() {
        let root = tree("photo-gone");
        let mut app = App::new(None);
        app.restore_session(&session(&root, Some("gone.jpg"), SessionView::Loupe));
        assert_eq!(
            app.current_session(),
            Some(session(&root, None, SessionView::Grid))
        );
    }
}
