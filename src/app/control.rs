//! The app's side of the control channel (`shell::control`). Answers each
//! request on the main thread. A `wait` or `screenshot` stays here until the
//! frame loop can answer it, so neither blocks the thread.

use std::time::Duration;

use web_time::Instant;

use super::App;
use crate::shell::control::{Answer, ChannelError, Op, Payload, Reply, Request};

/// Requests the frame loop answers later.
#[derive(Default)]
pub(crate) struct Pending {
    #[cfg(not(target_arch = "wasm32"))]
    shots: Vec<Reply>,
    waits: Vec<(Reply, Instant)>,
}

/// How often the loop looks again while a `wait` is pending.
const WAIT_POLL: Duration = Duration::from_millis(16);

fn json(value: impl serde::Serialize) -> Answer {
    serde_json::to_value(value).map(Payload::Json).map_err(|e| {
        ChannelError::Failed {
            reason: e.to_string(),
        }
        .into()
    })
}

impl App {
    /// Dropping `reply` unanswered tells the transport nothing ran.
    pub(crate) fn answer_control(&mut self, request: Request, reply: Reply) {
        let Request { op, expires } = request;
        let now = Instant::now();
        if now >= expires {
            return;
        }
        // A result shows in the window even with no input to wake it.
        self.request_redraw();
        let answer = match op {
            Op::Run(cmd) => self.run_from_agent(*cmd).map_err(Into::into).and_then(json),
            Op::Commands => json(self.agent_listing()),
            Op::State => json(self.agent_state()),
            Op::List => json(self.agent_list()),
            #[cfg(not(target_arch = "wasm32"))]
            Op::Screenshot => return self.control.shots.push(reply),
            Op::Wait(timeout) => {
                return self
                    .control
                    .waits
                    .push((reply, (now + timeout).min(expires)));
            }
        };
        let _ = reply.send(answer);
    }

    /// Answers the held requests. `about_to_wait` calls it after `pump`;
    /// returns how soon to call again.
    pub(crate) fn poll_control(&mut self) -> Option<Duration> {
        #[cfg(not(target_arch = "wasm32"))]
        for reply in std::mem::take(&mut self.control.shots) {
            // Checked as the picture is taken: a request on another
            // connection may have changed the folder since this one came.
            let answer = match self.screenshot_shows_outside() {
                Some(reason) => Err(ChannelError::Failed { reason }.into()),
                None => self.screenshot(),
            };
            let _ = reply.send(answer);
        }
        if self.control.waits.is_empty() {
            return None;
        }
        let settled = self.settled_across_a_frame();
        let now = Instant::now();
        for (reply, deadline) in std::mem::take(&mut self.control.waits) {
            if settled {
                let _ = reply.send(json(self.agent_state()));
            } else if now >= deadline {
                let _ = reply.send(Err(ChannelError::Timeout.into()));
            } else {
                self.control.waits.push((reply, deadline));
            }
        }
        (!self.control.waits.is_empty()).then_some(WAIT_POLL)
    }

    /// True when nothing is in flight before and after one frame: decodes,
    /// sidecar writes, batches and animations. An occluded window gets no
    /// frame here: on Metal its surface can block the main thread for a
    /// second, and `Occluded(false)` draws it again once revealed.
    pub(crate) fn settled_across_a_frame(&mut self) -> bool {
        let quiet_before = self.pump().is_none();
        if !self.occluded {
            self.redraw();
        }
        let quiet_after = self.pump().is_none();
        quiet_before && quiet_after && !self.batch_running() && self.repaint_at.is_none()
    }

    /// Draws one frame into an offscreen target and reads it back, then
    /// presents to the window again.
    #[cfg(not(target_arch = "wasm32"))]
    fn screenshot(&mut self) -> Answer {
        let Some(renderer) = self.renderer.as_mut() else {
            let reason = "no screenshot: the window is not ready yet".into();
            return Err(ChannelError::Failed { reason }.into());
        };
        renderer.render_offscreen();
        self.redraw();
        let frame = self.renderer.as_mut().and_then(|r| {
            let frame = r.read_offscreen_blocking();
            r.end_offscreen();
            frame
        });
        self.request_redraw();
        frame.map(Payload::Frame).ok_or_else(|| {
            let reason = "no screenshot: the frame could not be read back".into();
            ChannelError::Failed { reason }.into()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn live(op: Op) -> Request {
        let expires = Instant::now() + Duration::from_secs(60);
        Request { op, expires }
    }

    fn json_of(answer: Answer) -> Result<serde_json::Value, String> {
        match answer {
            Ok(Payload::Json(v)) => Ok(v),
            Ok(Payload::Frame(_)) => Err("a frame".into()),
            Err(f) => Err(format!("{f:?}")),
        }
    }

    #[test]
    fn state_and_run_answer_at_once_and_a_wait_answers_from_the_loop() {
        let mut app = App::new(None);
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(Op::State), tx.clone());
        assert_eq!(json_of(rx.try_recv().unwrap()).unwrap()["mode"], "grid");

        let rate = crate::command::Command::parse(r#"{"id":"photo.rate","params":{"stars":3}}"#);
        app.answer_control(live(Op::Run(Box::new(rate.unwrap()))), tx.clone());
        let refused = json_of(rx.try_recv().unwrap()).unwrap_err();
        assert!(refused.contains("no_photos"), "{refused}");

        app.answer_control(live(Op::Wait(Duration::from_secs(5))), tx);
        assert!(rx.try_recv().is_err(), "a wait is held for the loop");
        assert_eq!(app.poll_control(), None);
        assert_eq!(json_of(rx.try_recv().unwrap()).unwrap()["mode"], "grid");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_screenshot_without_a_window_is_an_error() {
        let dir = folder("shot-no-window");
        let mut app = user_opened(&dir);
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(Op::Screenshot), tx);
        assert!(rx.try_recv().is_err());
        app.poll_control();
        assert!(json_of(rx.try_recv().unwrap())
            .unwrap_err()
            .contains("not ready"));
    }

    #[test]
    fn a_request_whose_connection_gave_up_is_dropped_unrun() {
        let (mut app, _, _) = crate::app::test_support::folder_app("control-expired", 1);
        app.select_single(0);
        let loupe =
            crate::command::Command::parse(r#"{"id":"view.mode","params":{"mode":"loupe"}}"#);
        let expired = |op| Request {
            op,
            expires: Instant::now(),
        };
        for op in [
            Op::Run(Box::new(loupe.unwrap())),
            Op::Screenshot,
            Op::Wait(Duration::from_secs(5)),
        ] {
            let (tx, rx) = mpsc::channel();
            app.answer_control(expired(op), tx);
            app.poll_control();
            assert!(
                matches!(rx.try_recv(), Err(mpsc::TryRecvError::Disconnected)),
                "no answer, and the reply channel is closed"
            );
        }
        assert_eq!(app.mode, crate::app::ViewMode::Grid, "the run never ran");
    }

    #[test]
    fn the_settle_check_does_no_frame_work_while_the_window_is_occluded() {
        // A frame here loads the selected photo for the Develop page.
        let frame_work = |occluded| {
            let (mut app, _, _) = crate::app::test_support::folder_app("settle-occluded", 1);
            app.select_single(0);
            app.develop_open = true;
            app.occluded = occluded;
            app.settled_across_a_frame();
            app.want.is_some()
        };
        assert!(frame_work(false), "a visible window draws a frame");
        assert!(!frame_work(true), "an occluded window draws none");
    }

    /// A folder holding one empty photo, `a.jpg`, and an empty `sub/`.
    #[cfg(not(target_arch = "wasm32"))]
    fn folder(tag: &str) -> std::path::PathBuf {
        let dir = crate::app::test_support::temp_folder(tag);
        std::fs::write(dir.join("a.jpg"), []).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub/b.jpg"), []).unwrap();
        dir
    }

    /// An App where the user opened `dir`, which makes it the anchor.
    #[cfg(not(target_arch = "wasm32"))]
    fn user_opened(dir: &std::path::Path) -> App {
        let mut app = App::new(None);
        app.open(dir.to_path_buf());
        app
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn agent_runs(
        app: &mut App,
        cmd: crate::command::Command,
    ) -> Result<serde_json::Value, String> {
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(Op::Run(Box::new(cmd))), tx);
        json_of(rx.try_recv().unwrap())
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn open_folder(path: &std::path::Path) -> crate::command::Command {
        crate::command::Command::OpenFolder(crate::command::OpenFolderParams {
            path: path.to_path_buf(),
        })
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn refused(out: &Result<serde_json::Value, String>) -> bool {
        out.as_ref().is_err_and(|e| e.contains("an agent"))
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_opens_folders_only_inside_the_users_open_folder() {
        let parent = crate::app::test_support::temp_folder("anchor-parent");
        let anchor = parent.join("anchor");
        let sibling = parent.join("sibling");
        for dir in [&anchor, &sibling] {
            std::fs::create_dir_all(dir.join("sub")).unwrap();
        }
        let mut app = user_opened(&anchor);
        assert!(agent_runs(&mut app, open_folder(&anchor.join("sub"))).is_ok());
        for outside in [&parent, &sibling, &anchor.join("sub/../..")] {
            let out = agent_runs(&mut app, open_folder(outside));
            assert!(refused(&out), "{}: {out:?}", outside.display());
        }
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&sibling, anchor.join("link")).unwrap();
            let out = agent_runs(&mut app, open_folder(&anchor.join("link")));
            assert!(refused(&out), "a link out: {out:?}");
        }
        let _ = std::fs::remove_dir_all(&parent);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn after_moving_down_an_agent_can_go_back_to_the_anchor_but_not_above() {
        let dir = folder("anchor-updown");
        let mut app = user_opened(&dir);
        agent_runs(&mut app, open_folder(&dir.join("sub"))).unwrap();
        // Spelled under the catalog folder the user opened.
        assert_eq!(app.folder_sel.as_deref(), Some(dir.join("sub").as_path()));
        assert!(agent_runs(&mut app, open_folder(&dir)).is_ok(), "back up");
        let shown = app.folder_sel.clone().unwrap();
        assert_eq!(
            shown.as_os_str(),
            dir.as_os_str(),
            "the root's own spelling"
        );
        let above = dir.parent().unwrap().to_path_buf();
        assert!(refused(&agent_runs(&mut app, open_folder(&above))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn the_users_tree_navigation_moves_the_anchor() {
        let first = folder("anchor-nav-first");
        let second = folder("anchor-nav-second");
        let mut app = user_opened(&first);
        // As a click or arrow key in the folder tree loads a folder.
        app.load_folder(second.clone());
        assert!(agent_runs(&mut app, open_folder(&second.join("sub"))).is_ok());
        assert!(refused(&agent_runs(&mut app, open_folder(&first))));
        let _ = std::fs::remove_dir_all(&first);
        let _ = std::fs::remove_dir_all(&second);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_runs_no_catalog_or_preset_folder_command() {
        use crate::command::{CatalogNameParams, CatalogPathParams, Command, PresetFolderParams};
        let dir = folder("anchor-catalogs");
        let mut app = user_opened(&dir);
        let inside = dir.join("Trip.lightphotos");
        for cmd in [
            Command::NewCatalog(CatalogPathParams {
                path: inside.clone(),
            }),
            Command::OpenCatalog(CatalogPathParams {
                path: inside.clone(),
            }),
            Command::SwitchCatalog(CatalogNameParams {
                name: "default".into(),
            }),
            Command::AddPresetFolder(PresetFolderParams { path: dir.clone() }),
        ] {
            let out = agent_runs(&mut app, cmd.clone());
            assert!(
                out.as_ref().is_err_and(|e| e.contains("in the app")),
                "{}: {out:?}",
                cmd.id()
            );
        }
        assert!(!inside.exists(), "no catalog made");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_exports_only_inside_the_anchor_or_to_the_forms_folder() {
        use crate::command::{Command, ExportParams, Photos};
        use crate::export::{ExportSettings, ExportTarget, FolderChoice};
        let dir = folder("anchor-export");
        let outside = crate::app::test_support::temp_folder("anchor-export-out");
        let mut app = user_opened(&dir);
        let to = |target: std::path::PathBuf| {
            Command::Export(ExportParams {
                photos: Photos::Named(vec!["a.jpg".into()]),
                settings: Some(ExportSettings {
                    target: ExportTarget::Folder(FolderChoice::Custom(target)),
                    ..ExportSettings::default()
                }),
            })
        };
        let out = agent_runs(&mut app, to(outside.clone()));
        assert!(refused(&out), "{out:?}");
        let out = agent_runs(&mut app, to(dir.join("Exports/web")));
        assert!(!refused(&out), "{out:?}");
        // Once the agent moved down, the anchor still bounds it.
        agent_runs(&mut app, open_folder(&dir.join("sub"))).unwrap();
        let back = Command::Export(ExportParams {
            photos: Photos::Named(vec!["b.jpg".into()]),
            settings: Some(ExportSettings {
                target: ExportTarget::Folder(FolderChoice::Custom(dir.join("Exports"))),
                ..ExportSettings::default()
            }),
        });
        assert!(!refused(&agent_runs(&mut app, back)));
        // The form's own folder is the user's choice, wherever it is.
        app.set_export_settings(ExportSettings {
            target: ExportTarget::Folder(FolderChoice::Custom(outside.clone())),
            ..ExportSettings::default()
        });
        let form = Command::Export(ExportParams {
            photos: Photos::Named(vec!["b.jpg".into()]),
            settings: None,
        });
        assert!(!refused(&agent_runs(&mut app, form)));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn a_relative_path_the_user_opens_is_made_absolute_without_dot_dot() {
        let dir = folder("anchor-relative");
        let cwd = std::env::current_dir().unwrap();
        let mut rel = std::path::PathBuf::new();
        for _ in 1..cwd.components().count() {
            rel.push("..");
        }
        rel.push(dir.strip_prefix("/").unwrap());
        rel.push("sub/..");
        let mut app = App::new(None);
        app.open_from_user(rel);
        let shown = app.folder_sel.clone().unwrap();
        assert!(shown.is_absolute(), "{}", shown.display());
        assert!(
            !shown
                .components()
                .any(|c| c == std::path::Component::ParentDir),
            "{}",
            shown.display()
        );
        assert_eq!(shown, dir.canonicalize().unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn opening_one_photo_anchors_only_its_folder() {
        let dir = folder("anchor-one-photo");
        let mut app = App::new(None);
        app.open_from_user(dir.join("a.jpg"));
        assert!(refused(&agent_runs(
            &mut app,
            open_folder(&dir.join("sub"))
        )));
        assert!(
            agent_runs(&mut app, open_folder(&dir)).is_ok(),
            "its own folder"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_open_loads_the_resolved_folder() {
        let dir = folder("anchor-resolved");
        std::os::unix::fs::symlink(dir.join("sub"), dir.join("alias")).unwrap();
        let mut app = user_opened(&dir);
        agent_runs(&mut app, open_folder(&dir.join("alias"))).unwrap();
        // The link resolved to `sub`, spelled under the folder the user opened.
        assert_eq!(app.folder_sel.as_deref(), Some(dir.join("sub").as_path()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_users_path_through_a_link_opens_where_the_link_goes() {
        let dir = folder("anchor-link-dotdot");
        let other = crate::app::test_support::temp_folder("anchor-link-other");
        std::fs::create_dir_all(other.join("deep")).unwrap();
        std::fs::write(other.join("o.jpg"), []).unwrap();
        std::os::unix::fs::symlink(other.join("deep"), dir.join("link")).unwrap();
        let mut app = App::new(None);
        // `link/..` is `other`, as the shell and the OS read it, not `dir`.
        app.open_from_user(dir.join("link/.."));
        let real = other.canonicalize().unwrap();
        assert_eq!(app.folder_sel.as_deref(), Some(real.as_path()));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_cannot_act_on_a_photo_that_links_outside() {
        use crate::command::{Command, ExportParams, Photos};
        let dir = folder("anchor-photo-link");
        let outside = crate::app::test_support::temp_folder("anchor-photo-link-out");
        std::fs::write(outside.join("secret.jpg"), []).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("link.jpg")).unwrap();
        let mut app = user_opened(&dir);
        let named = || Photos::Named(vec!["link.jpg".into()]);
        let export = Command::Export(ExportParams {
            photos: named(),
            settings: None,
        });
        assert!(refused(&agent_runs(&mut app, export)));
        let rate_of = |name: &str| {
            Command::parse(&format!(
                r#"{{"id":"photo.rate","params":{{"stars":3,"photos":["{name}"]}}}}"#
            ))
            .unwrap()
        };
        let rate = rate_of("link.jpg");
        assert!(refused(&agent_runs(&mut app, rate)));
        let rate_own = rate_of("a.jpg");
        assert!(agent_runs(&mut app, rate_own).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_open_keeps_the_catalog_and_its_roots_spelling() {
        let dir = folder("anchor-root-link");
        let link = dir.with_extension("link");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&dir, &link).unwrap();
        let mut app = App::new(None);
        app.open(link.clone());
        let roots = app.catalog.folders().to_vec();
        agent_runs(&mut app, open_folder(&link.join("sub"))).unwrap();
        assert_eq!(app.catalog.folders(), roots.as_slice(), "no root added");
        assert_eq!(app.folder_sel.as_deref(), Some(link.join("sub").as_path()));
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn a_screenshot_is_checked_when_it_is_taken() {
        let dir = folder("anchor-shot-late");
        let outside = crate::app::test_support::temp_folder("anchor-shot-late-out");
        std::fs::write(outside.join("secret.jpg"), []).unwrap();
        let mut app = user_opened(&dir);
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(Op::Screenshot), tx);
        // Between the request and the picture, the folder comes to show a
        // photo that links outside.
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("link.jpg")).unwrap();
        app.load_folder(dir.clone());
        app.poll_control();
        let refused = json_of(rx.try_recv().unwrap()).unwrap_err();
        assert!(refused.contains("links outside"), "{refused}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn a_user_open_keeps_the_path_as_given_unless_it_has_dot_dot() {
        let dir = folder("anchor-as-given");
        let other = crate::app::test_support::temp_folder("anchor-as-given-other");
        std::fs::write(other.join("x.jpg"), []).unwrap();
        std::os::unix::fs::symlink(other.join("x.jpg"), dir.join("link.jpg")).unwrap();
        let mut app = App::new(None);
        app.open_from_user(dir.join("link.jpg"));
        assert_eq!(
            app.folder_sel.as_deref(),
            Some(dir.as_path()),
            "the link's folder"
        );
        let mut app = App::new(None);
        app.open_from_user(dir.clone());
        assert_eq!(
            app.folder_sel.as_deref(),
            Some(dir.as_path()),
            "spelled as given"
        );
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_cannot_move_onto_a_photo_while_one_links_outside() {
        let dir = folder("anchor-select-link");
        let outside = crate::app::test_support::temp_folder("anchor-select-link-out");
        std::fs::write(outside.join("secret.jpg"), []).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("link.jpg")).unwrap();
        let mut app = user_opened(&dir);
        for json in [
            r#"{"id":"select","params":{"photos":["link.jpg"]}}"#,
            r#"{"id":"navigate","params":{"to":"next"}}"#,
            r#"{"id":"view.mode","params":{"mode":"loupe"}}"#,
        ] {
            let cmd = crate::command::Command::parse(json).unwrap();
            let out = agent_runs(&mut app, cmd);
            assert!(refused(&out), "{json}: {out:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(unix)]
    fn a_link_to_a_photo_elsewhere_in_the_anchor_is_no_outside_link() {
        let dir = folder("anchor-inner-link");
        std::fs::create_dir_all(dir.join("x")).unwrap();
        std::fs::write(dir.join("x/real.jpg"), []).unwrap();
        std::os::unix::fs::symlink(dir.join("x/real.jpg"), dir.join("sub/link.jpg")).unwrap();
        let mut app = user_opened(&dir);
        agent_runs(&mut app, open_folder(&dir.join("sub"))).unwrap();
        assert_eq!(app.screenshot_shows_outside(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn agent_asks(app: &mut App, op: Op) -> Result<serde_json::Value, String> {
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(op), tx);
        app.poll_control();
        json_of(rx.try_recv().unwrap())
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_cannot_save_presets() {
        let dir = folder("anchor-preset-save");
        let mut app = user_opened(&dir);
        let save = crate::command::Command::parse(r#"{"id":"preset.save","params":{"name":"x"}}"#);
        let out = agent_runs(&mut app, save.unwrap());
        assert!(
            out.as_ref().is_err_and(|e| e.contains("in the app")),
            "{out:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[cfg(unix)]
    fn an_agent_sees_nothing_outside_the_anchor_in_state_list_or_commands() {
        let dir = folder("anchor-reads");
        let other = folder("anchor-reads-other");
        let outside = crate::app::test_support::temp_folder("anchor-reads-out");
        std::fs::write(outside.join("secret.jpg"), []).unwrap();
        std::os::unix::fs::symlink(outside.join("secret.jpg"), dir.join("link.jpg")).unwrap();
        let mut app = App::new(None);
        app.open(other.clone());
        app.open(dir.clone());
        let link = dir.join("link.jpg");
        app.set_rating_of(link.clone(), 5);
        let pos = app
            .playlist
            .as_ref()
            .and_then(|pl| pl.entries().iter().position(|p| *p == link))
            .unwrap();
        app.select_single(pos);

        let state = agent_asks(&mut app, Op::State).unwrap();
        assert_eq!(state["folder_roots"], serde_json::json!([dir]), "{state}");
        assert_eq!(state["rating"], serde_json::Value::Null, "{state}");
        let list = agent_asks(&mut app, Op::List).unwrap();
        let shown = list["photos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "link.jpg")
            .cloned()
            .unwrap();
        assert_eq!(shown["rating"], 0, "{shown}");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn with_no_anchor_an_agent_reads_no_photos() {
        let dir = folder("anchor-none-reads");
        let mut app = user_opened(&dir);
        app.select_single(0);
        app.folder_anchor = None;
        let list = agent_asks(&mut app, Op::List).unwrap();
        assert_eq!(list["photos"], serde_json::json!([]), "{list}");
        let state = agent_asks(&mut app, Op::State).unwrap();
        assert_eq!(state["photo"], serde_json::Value::Null, "{state}");
        assert_eq!(state["folder"], serde_json::Value::Null, "{state}");
        let shot = agent_asks(&mut app, Op::Screenshot).unwrap_err();
        assert!(shot.contains("no screenshot for an agent"), "{shot}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
