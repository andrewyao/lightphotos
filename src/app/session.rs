//! Where the user left off, saved as it changes so the landing page's Reopen
//! Session button can bring it back after a restart.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::*;

/// `prefs` key for the last session.
const SESSION_PREF: &str = "session";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum SessionView {
    Grid,
    Loupe,
}

/// The folder the user last chose, and where they were in it. Browsing into
/// a subfolder doesn't change it. On the web every path is relative to the
/// picked folder, whose name is `root`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Session {
    /// The chosen folder: a full path natively, the folder's name on the web.
    pub(crate) root: PathBuf,
    pub(crate) photo: Option<PathBuf>,
    pub(crate) view: SessionView,
}

impl Session {
    /// The saved session, if one parses.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn load() -> Option<Session> {
        serde_json::from_str(&crate::prefs::load(SESSION_PREF)?).ok()
    }

    fn save(&self) {
        // A test must never write the developer's own session.
        if cfg!(test) {
            return;
        }
        let saved = serde_json::to_string(self)
            .map_err(|e| e.to_string())
            .and_then(|json| crate::prefs::save(SESSION_PREF, &json));
        if let Err(e) = saved {
            eprintln!("[session] could not save the session: {e}");
        }
    }
}

impl App {
    /// The session as it stands, or `None` on the landing page and while a
    /// subfolder of the chosen folder is showing.
    pub(crate) fn current_session(&self) -> Option<Session> {
        let root = self.folder_root.clone()?;
        if self.playlist.as_ref()?.dir() != root {
            return None;
        }
        let view = match self.mode {
            ViewMode::Loupe => SessionView::Loupe,
            ViewMode::Grid | ViewMode::Survey => SessionView::Grid,
        };
        Some(Session {
            root,
            photo: self.selected_path(),
            view,
        })
    }

    /// The session the Reopen Session button restores.
    pub(crate) fn saved_session(&self) -> Option<&Session> {
        self.session.as_ref()
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

    /// Open the saved session, or the folder picker when there is none or its
    /// folder is gone.
    pub(crate) fn reopen_session(&mut self) {
        let Some(session) = self.session.clone() else {
            self.open_folder_picker();
            return;
        };
        #[cfg(not(target_arch = "wasm32"))]
        if session.root.is_dir() {
            self.restore_session(&session);
        } else {
            self.open_folder_picker();
        }
        #[cfg(target_arch = "wasm32")]
        self.request_session_reopen(session);
    }

    /// Open `session.root` like `open`, then its photo and view. A missing
    /// photo leaves the grid with nothing selected.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn restore_session(&mut self, session: &Session) {
        self.open(session.root.clone());
        self.restore_session_view(session);
    }

    /// Select the session's photo in the loaded playlist and, for a Loupe
    /// session, open it.
    pub(super) fn restore_session_view(&mut self, session: &Session) {
        let pos = session.photo.as_deref().and_then(|photo| {
            let pl = self.playlist.as_ref()?;
            let idx = pl.entries().iter().position(|p| p == photo)?;
            self.visible.iter().position(|&i| i == idx)
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

#[cfg(all(test, not(target_arch = "wasm32")))]
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
            root: root.to_path_buf(),
            photo: photo.map(|p| root.join(p)),
            view,
        }
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
    fn browsing_into_a_subfolder_keeps_the_chosen_folder() {
        let root = tree("subfolder");
        let mut app = App::new(None);
        app.open(root.clone());
        app.select_single(0);
        app.save_session_if_changed();
        let chosen = session(&root, Some("a.jpg"), SessionView::Grid);
        assert_eq!(app.saved_session(), Some(&chosen));

        app.open_folder(root.join("sub"));
        app.select_single(1);
        app.enter_loupe();
        app.save_session_if_changed();
        assert_eq!(app.saved_session(), Some(&chosen));
    }

    #[test]
    fn choosing_another_folder_replaces_the_saved_one() {
        let (first, second) = (tree("first"), tree("second"));
        let mut app = App::new(None);
        app.save_session_if_changed();
        assert_eq!(app.saved_session(), None, "nothing to save on the landing page");

        app.open(first.clone());
        app.save_session_if_changed();
        assert_eq!(app.saved_session().map(|s| &s.root), Some(&first));

        app.open(second.clone());
        app.save_session_if_changed();
        assert_eq!(
            app.saved_session(),
            Some(&session(&second, None, SessionView::Grid))
        );
    }

    #[test]
    fn reopening_restores_the_folder_photo_and_loupe() {
        let root = tree("restore");
        let saved = session(&root, Some("b.jpg"), SessionView::Loupe);
        let mut app = App::new(None);
        app.session = Some(saved.clone());
        app.reopen_session();

        assert_eq!(app.folder_root.as_deref(), Some(root.as_path()));
        assert_eq!(app.mode, ViewMode::Loupe);
        assert_eq!(app.current_session(), Some(saved));
    }

    /// A real click on the landing page's button, through the whole UI.
    #[test]
    fn the_landing_page_button_reopens_the_saved_session() {
        use crate::app::presets::tests::{click, settled};
        use crate::i18n::t;
        use crate::ui::UiAction;

        let root = tree("click");
        let saved = session(&root, Some("a.jpg"), SessionView::Loupe);
        let mut app = App::new(None);
        app.session = Some(saved.clone());
        let painted = settled(&mut app);
        let (actions, _) = click(&mut app, painted.pos_of(t().reopen_session));
        assert_eq!(actions, vec![UiAction::ReopenSession]);
        app.apply_ui_actions(actions);
        assert_eq!(app.current_session(), Some(saved));
    }

    #[test]
    fn a_first_launch_has_no_reopen_session_button() {
        use crate::app::presets::tests::settled;
        use crate::i18n::t;

        let mut app = App::new(None);
        let painted = settled(&mut app);
        assert!(painted.texts().contains(&t().choose_folder));
        assert!(!painted.texts().contains(&t().reopen_session));
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
