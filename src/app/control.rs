//! The app's side of the control channel (`shell::control`). Answers each
//! request on the main thread. A `wait` or `screenshot` stays here until the
//! frame loop can answer it, so neither blocks the thread.

use std::time::Duration;

use web_time::Instant;

use super::{App, PhotoList, State};
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
            Op::Commands => json(self.listing()),
            Op::State => json(State::of(self)),
            Op::List => json(PhotoList::of(self)),
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
            let _ = reply.send(self.screenshot());
        }
        if self.control.waits.is_empty() {
            return None;
        }
        let settled = self.settled_across_a_frame();
        let now = Instant::now();
        for (reply, deadline) in std::mem::take(&mut self.control.waits) {
            if settled {
                let _ = reply.send(json(State::of(self)));
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
    fn a_screenshot_without_a_window_is_an_error() {
        let mut app = App::new(None);
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

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_export_outside_the_open_folder_is_refused() {
        use crate::command::{Command, ExportParams, Photos};
        use crate::export::{ExportSettings, ExportTarget, FolderChoice};
        let (mut app, _, photo) = user_opened("control-export");
        let photos = vec![photo];
        let outside = crate::app::test_support::temp_folder("control-export-out");
        let export = Command::Export(ExportParams {
            photos: Photos::Named(photos),
            settings: Some(ExportSettings {
                target: ExportTarget::Folder(FolderChoice::Custom(outside.clone())),
                ..ExportSettings::default()
            }),
        });
        let (tx, rx) = mpsc::channel();
        app.answer_control(live(Op::Run(Box::new(export))), tx);
        let refused = json_of(rx.try_recv().unwrap()).unwrap_err();
        assert!(refused.contains("an agent can export only"), "{refused}");
        assert!(!app.export_running(), "no batch started");
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    }

    /// An App where the user (not an agent) opened a folder of one photo.
    #[cfg(not(target_arch = "wasm32"))]
    fn user_opened(tag: &str) -> (App, std::path::PathBuf, std::path::PathBuf) {
        let dir = crate::app::test_support::temp_folder(tag);
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        let mut app = App::new(None);
        app.open_from_user(dir.clone());
        (app, dir, photo)
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

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_folder_the_user_never_chose_is_no_export_root() {
        use crate::command::{Command, ExportParams, OpenFolderParams, Photos};
        let (mut app, user_dir, _) = user_opened("agent-root-user");
        let elsewhere = crate::app::test_support::temp_folder("agent-root-elsewhere");
        std::fs::write(elsewhere.join("b.jpg"), []).unwrap();
        // Loaded, but not at an entry point: as the tree or a restore does.
        app.load_folder(elsewhere.clone());
        let export = |name: &str| {
            Command::Export(ExportParams {
                photos: Photos::Named(vec![name.into()]),
                settings: None,
            })
        };
        let refused = agent_runs(&mut app, export("b.jpg")).unwrap_err();
        assert!(refused.contains("an agent can export only"), "{refused}");
        assert!(!elsewhere.join("Exports").exists());

        // Back in the folder the user opened, the same export is allowed.
        agent_runs(
            &mut app,
            Command::OpenFolder(OpenFolderParams {
                path: user_dir.clone(),
            }),
        )
        .unwrap();
        let allowed = agent_runs(&mut app, export("a.jpg"));
        assert!(
            !format!("{allowed:?}").contains("an agent can export only"),
            "{allowed:?}"
        );
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_cannot_place_a_catalog_or_preset_folder_outside_the_users_folders() {
        use crate::command::{CatalogPathParams, Command, PresetFolderParams};
        let (mut app, _, _) = user_opened("agent-paths-user");
        let outside = crate::app::test_support::temp_folder("agent-paths-out");
        let catalog = outside.join("Evil.lightphotos");
        let made = agent_runs(
            &mut app,
            Command::NewCatalog(CatalogPathParams {
                path: catalog.clone(),
            }),
        );
        assert!(made.is_err(), "{made:?}");
        assert!(!catalog.exists(), "nothing written outside");
        let opened = agent_runs(
            &mut app,
            Command::OpenCatalog(CatalogPathParams {
                path: catalog.clone(),
            }),
        );
        assert!(
            opened.as_ref().is_err_and(|e| e.contains("an agent")),
            "{opened:?}"
        );
        let presets = agent_runs(
            &mut app,
            Command::AddPresetFolder(PresetFolderParams {
                path: outside.clone(),
            }),
        );
        assert!(
            presets.as_ref().is_err_and(|e| e.contains("an agent")),
            "{presets:?}"
        );
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn export_here(name: &str) -> crate::command::Command {
        crate::command::Command::Export(crate::command::ExportParams {
            photos: crate::command::Photos::Named(vec![name.into()]),
            settings: None,
        })
    }

    /// `dir` with one empty photo, `b.jpg`, in it.
    #[cfg(not(target_arch = "wasm32"))]
    fn photo_folder(tag: &str) -> std::path::PathBuf {
        let dir = crate::app::test_support::temp_folder(tag);
        std::fs::write(dir.join("b.jpg"), []).unwrap();
        dir
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn an_agent_cannot_open_a_folder_outside_the_users() {
        use crate::command::{Command, OpenFolderParams};
        let (mut app, _, _) = user_opened("agent-open-user");
        let outside = photo_folder("agent-open-out");
        let opened = agent_runs(
            &mut app,
            Command::OpenFolder(OpenFolderParams {
                path: outside.clone(),
            }),
        );
        assert!(
            opened.as_ref().is_err_and(|e| e.contains("an agent")),
            "{opened:?}"
        );
        assert_ne!(app.folder_sel.as_deref(), Some(outside.as_path()));
        assert!(!outside.join(".lightphotos").exists(), "no sidecars there");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_keystroke_after_an_agent_moved_the_cursor_adds_no_root() {
        use crate::command::{Command, OpenFolderParams};
        let (mut app, _, _) = user_opened("agent-key-user");
        let outside = photo_folder("agent-key-out");
        let child = outside.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let _ = agent_runs(
            &mut app,
            Command::OpenFolder(OpenFolderParams { path: child }),
        );
        // Left in the tree: up to the parent, which the agent's folder named.
        app.folder_collapse();
        let in_outside = app
            .folder_sel
            .as_deref()
            .is_some_and(|f| f.starts_with(&outside));
        let export = agent_runs(&mut app, export_here("b.jpg"));
        assert!(!(in_outside && export.is_ok()), "{export:?}");
    }

    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_crafted_catalog_an_agent_opened_adds_no_roots_on_restore() {
        use crate::command::{CatalogPathParams, Command};
        use crate::persist::catalog::{Catalog, CatalogRef};
        let (mut app, user_dir, _) = user_opened("agent-cat-user");
        let outside = photo_folder("agent-cat-out");
        let at = CatalogRef::parse(&user_dir.join("Crafted.lightphotos")).unwrap();
        drop(Catalog::create_at(at.clone(), vec![outside.clone()]).unwrap());
        let _ = agent_runs(
            &mut app,
            Command::OpenCatalog(CatalogPathParams {
                path: at.path().to_path_buf(),
            }),
        );
        // What the next launch does with the remembered catalog.
        app.restore_last_session();
        let in_outside = app
            .folder_sel
            .as_deref()
            .is_some_and(|f| f.starts_with(&outside));
        let export = agent_runs(&mut app, export_here("b.jpg"));
        assert!(!(in_outside && export.is_ok()), "{export:?}");
    }
}
