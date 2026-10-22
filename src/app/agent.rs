//! What an agent may do (control channel, MCP, WebMCP). It drives the app
//! through the same [`Command`]s as the keyboard, but it reads photo names
//! that anyone can write, so it can be talked into things. Every command it
//! sends passes [`App::run_from_agent`], which holds it to the anchor: the
//! folder the user has open. Every folder change that is not an agent's
//! command moves the anchor (the picker, the command line, Finder, the tree
//! and the keyboard, a catalog, a launch restore); an agent's command never
//! does. So an agent works in the user's folder and the folders inside it,
//! can go back up to the anchor, and never above it. Catalogs and preset
//! folders are the user's alone.

use std::path::{Path, PathBuf};

use super::{App, StatusKind};
use crate::command::{Command, CommandError, ExportParams, Outcome};
use crate::export::metadata::Keep;
use crate::export::{ExportSettings, ExportTarget, FolderChoice};

pub(super) const EXPORT_REFUSED: &str = "an agent can export only into the folder the user \
                                         has open, a folder inside it, or the export form's \
                                         own folder";

const USER_ONLY: &str = "an agent can't make, open or switch catalogs, or add preset folders; \
                         the user does that in the app";

/// Where an agent's export may land: inside the anchor, or inside the
/// export form's own folder. Both are kept resolved.
// The browser exports only into the picked folder; nothing reads this there.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) struct ExportConfine {
    anchor: Option<PathBuf>,
    form_folder: Option<PathBuf>,
}

impl ExportConfine {
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn holds(&self, dir: &Path) -> bool {
        resolve(dir).is_some_and(|real| {
            [&self.anchor, &self.form_folder]
                .into_iter()
                .flatten()
                .any(|root| real.starts_with(root))
        })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn resolve(path: &Path) -> Option<PathBuf> {
    crate::paths::resolve(path)
}

/// The browser names no folder by path: it reaches only the picked one.
#[cfg(target_arch = "wasm32")]
fn resolve(_path: &Path) -> Option<PathBuf> {
    None
}

/// `path` made absolute against the working directory, with `.` and `..`
/// taken out by name. `lightphotos ../shoot` must name that folder, not
/// whatever a later launch's working directory makes of it.
#[cfg(not(target_arch = "wasm32"))]
fn absolute(path: PathBuf) -> PathBuf {
    let Ok(full) = std::path::absolute(&path) else {
        return path;
    };
    let mut out = PathBuf::new();
    for part in full.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

impl App {
    /// Opens a path from the command line or Finder, made absolute first.
    pub(crate) fn open_from_user(&mut self, path: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        let path = absolute(path);
        self.open(path);
    }

    /// Whether `path`, resolved, is the anchor or inside it.
    fn inside_anchor(&self, path: &Path) -> bool {
        let anchor = self.folder_anchor.as_deref().and_then(resolve);
        match (resolve(path), anchor) {
            (Some(real), Some(anchor)) => real.starts_with(anchor),
            _ => false,
        }
    }

    /// Runs a command an agent sent, once it passes the checks here. The
    /// anchor is put back after, so nothing the command opened moves it.
    pub(crate) fn run_from_agent(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        let refuse = |reason: &str| CommandError::Failed {
            reason: reason.into(),
        };
        match &cmd {
            Command::Export(p) => return self.agent_export(p.clone()),
            Command::NewCatalog(_)
            | Command::OpenCatalog(_)
            | Command::SwitchCatalog(_)
            | Command::AddPresetFolder(_) => return Err(refuse(USER_ONLY)),
            Command::OpenFolder(p) => {
                if !self.inside_anchor(&p.path) {
                    return Err(refuse(
                        "an agent can open only the folder the user has open, or a folder \
                         inside it",
                    ));
                }
            }
            // These act on the open folder's photos, or on nothing on disk.
            Command::View(_)
            | Command::Zoom(_)
            | Command::Filter(_)
            | Command::Sort(_)
            | Command::Select(_)
            | Command::Navigate(_)
            | Command::Rate(_)
            | Command::Flag(_)
            | Command::Label(_)
            | Command::Rotate(_)
            | Command::Score(_)
            | Command::Develop(_)
            | Command::Reset(_)
            | Command::AutoTone(_)
            | Command::Monochrome(_)
            | Command::Crop(_)
            | Command::AddMask(_)
            | Command::SetMask(_)
            | Command::ClearMasks(_)
            | Command::CopySettings(_)
            | Command::PasteSettings(_)
            | Command::Preset(_)
            | Command::SavePreset(_)
            | Command::Undo => {}
        }
        let anchor = self.folder_anchor.clone();
        let out = self.run(cmd);
        self.folder_anchor = anchor;
        out
    }

    /// An agent's export, held to the anchor and the form's folder.
    fn agent_export(&mut self, p: ExportParams) -> Result<Outcome, CommandError> {
        let refuse = |reason: String| CommandError::Failed { reason };
        self.check(&Command::Export(p.clone()))?;
        let form_folder: Option<PathBuf> = match &self.export_settings().target {
            #[cfg(not(target_arch = "wasm32"))]
            ExportTarget::Folder(FolderChoice::Custom(dir)) => resolve(dir),
            _ => None,
        };
        let confine = ExportConfine {
            anchor: self.folder_anchor.as_deref().and_then(resolve),
            form_folder,
        };
        let Ok(settings) = confine_agent_export(
            p.settings,
            self.export_settings(),
            self.folder_sel.as_deref(),
            &confine,
        ) else {
            return Err(refuse(self.refuse_agent_export()));
        };
        let paths = self.resolve(&p.photos)?;
        let pending = self
            .export_with(paths.clone(), Some(settings), Some(&confine))
            .map_err(refuse)?;
        self.request_redraw();
        Ok(Outcome {
            photos: paths.len(),
            pending,
        })
    }

    /// Says an agent's export was refused, to the user too, and returns why.
    pub(super) fn refuse_agent_export(&mut self) -> String {
        self.set_status(StatusKind::Error, EXPORT_REFUSED.into());
        self.request_redraw();
        EXPORT_REFUSED.into()
    }
}

/// The settings an agent's `export` runs with, or why it may not run. An
/// agent picks the size, format, sharpening and naming. The rest is the
/// form's: the album, the copyright, and whether location and identity data
/// stay in (only when the form keeps them too). The destination must be
/// inside `confine`, or Immich when the form uploads there.
pub(super) fn confine_agent_export(
    given: Option<ExportSettings>,
    form: &ExportSettings,
    open_folder: Option<&Path>,
    confine: &ExportConfine,
) -> Result<ExportSettings, String> {
    let mut settings = given.unwrap_or_else(|| form.clone());
    settings.keep = Keep {
        location: settings.keep.location && form.keep.location,
        identity: settings.keep.identity && form.keep.identity,
    };
    settings.album = form.album.clone();
    settings.copyright = form.copyright.clone();
    let allowed = match &settings.target {
        ExportTarget::Immich => form.target == ExportTarget::Immich,
        // The browser writes only into the picked folder's own Exports.
        #[cfg(target_arch = "wasm32")]
        ExportTarget::Folder(FolderChoice::ExportsSubfolder) => {
            let _ = (open_folder, confine);
            true
        }
        #[cfg(not(target_arch = "wasm32"))]
        ExportTarget::Folder(FolderChoice::ExportsSubfolder) => {
            open_folder.is_some_and(|dir| confine.holds(&dir.join(crate::export::EXPORTS_DIR)))
        }
        #[cfg(not(target_arch = "wasm32"))]
        ExportTarget::Folder(FolderChoice::Custom(dir)) => confine.holds(dir),
    };
    if !allowed {
        return Err(EXPORT_REFUSED.into());
    }
    Ok(settings)
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;
    use crate::export::AlbumChoice;

    /// Exports held to `anchor` alone.
    fn confine_to(anchor: &Path) -> ExportConfine {
        ExportConfine {
            anchor: resolve(anchor),
            form_folder: None,
        }
    }

    fn nowhere() -> ExportConfine {
        ExportConfine {
            anchor: None,
            form_folder: None,
        }
    }

    fn to(target: ExportTarget) -> ExportSettings {
        ExportSettings {
            target,
            ..ExportSettings::default()
        }
    }

    #[test]
    fn a_relative_path_is_made_absolute_by_name() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(absolute(PathBuf::from(".")), cwd);
        assert_eq!(
            absolute(PathBuf::from("../x/./y/..")),
            cwd.parent().unwrap().join("x")
        );
        assert_eq!(absolute(PathBuf::from("/a/b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn an_agent_exports_only_where_the_user_would() {
        let folder = temp_folder("agent-export");
        let confine = confine_to(&folder);
        let form = ExportSettings::default();
        let check = |given| confine_agent_export(Some(given), &form, Some(&folder), &confine);
        let custom = |dir: PathBuf| to(ExportTarget::Folder(FolderChoice::Custom(dir)));

        assert!(check(to(ExportTarget::default())).is_ok());
        assert!(check(custom(folder.join("Exports/web"))).is_ok());
        assert!(check(custom(std::env::temp_dir())).is_err());
        assert!(check(custom(folder.join("../elsewhere"))).is_err());
        assert!(check(to(ExportTarget::Immich)).is_err());
        assert!(
            confine_agent_export(None, &form, None, &confine).is_err(),
            "no open folder, no Exports folder in it"
        );
        let other = temp_folder("agent-export-other");
        assert!(
            confine_agent_export(None, &form, Some(&other), &confine).is_err(),
            "the form's Exports, but in a folder outside the anchor"
        );
        let immich = to(ExportTarget::Immich);
        assert!(confine_agent_export(None, &immich, None, &nowhere()).is_ok());
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[cfg(unix)]
    #[test]
    fn an_export_folder_through_a_link_out_is_refused_before_it_is_made() {
        let root = temp_folder("agent-premake");
        let outside = temp_folder("agent-premake-out");
        let photo = root.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let mut app = App::new(None);
        app.start_exporter();
        let settings = to(ExportTarget::Folder(FolderChoice::Custom(
            root.join("link/new"),
        )));
        let confine = confine_to(&root);
        let started = app.export_with(vec![photo], Some(settings), Some(&confine));
        assert!(started.is_err(), "{started:?}");
        assert!(!outside.join("new").exists(), "made outside the anchor");
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_that_cannot_be_made_fails_the_agents_export() {
        let root = temp_folder("agent-nomake");
        let photo = root.join("a.jpg");
        std::fs::write(&photo, []).unwrap();
        std::fs::write(root.join("blocker"), []).unwrap();
        let mut app = App::new(None);
        app.start_exporter();
        let settings = to(ExportTarget::Folder(FolderChoice::Custom(
            root.join("blocker/new"),
        )));
        let confine = confine_to(&root);
        let started = app.export_with(vec![photo], Some(settings), Some(&confine));
        assert!(started.is_err(), "{started:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_agent_takes_the_album_and_copyright_from_the_form() {
        let form = ExportSettings {
            target: ExportTarget::Immich,
            album: AlbumChoice::Existing {
                id: "mine".into(),
                name: "Mine".into(),
            },
            copyright: "© the user".into(),
            ..ExportSettings::default()
        };
        let asks = ExportSettings {
            album: AlbumChoice::Existing {
                id: "shared".into(),
                name: "Shared with strangers".into(),
            },
            copyright: "© someone else".into(),
            ..form.clone()
        };
        let got = confine_agent_export(Some(asks), &form, None, &nowhere()).unwrap();
        assert_eq!(got.album, form.album);
        assert_eq!(got.copyright, form.copyright);
    }

    #[test]
    fn an_agent_keeps_location_and_identity_only_when_the_form_does() {
        let immich = |keep| ExportSettings {
            target: ExportTarget::Immich,
            keep,
            ..ExportSettings::default()
        };
        let all = Keep {
            location: true,
            identity: true,
        };
        let stripped =
            confine_agent_export(Some(immich(all)), &immich(Keep::NONE), None, &nowhere());
        assert_eq!(stripped.map(|s| s.keep), Ok(Keep::NONE));
        let location = Keep {
            location: true,
            identity: false,
        };
        let kept = confine_agent_export(Some(immich(all)), &immich(location), None, &nowhere());
        assert_eq!(kept.map(|s| s.keep), Ok(location));
    }
}
