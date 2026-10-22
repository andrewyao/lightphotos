//! What an agent may do (control channel, MCP, WebMCP). It drives the app
//! through the same [`Command`]s as the keyboard, but it reads photo names
//! that anyone can write, so it can be talked into things. Every command it
//! sends passes [`App::run_from_agent`], which lets it touch the disk only
//! where the user already chose to: inside a folder the user opened this
//! session, or the export form's own folder. It may open any folder, but a
//! folder it opened is not one of the user's.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::App;
use crate::command::{Command, CommandError, Outcome};
use crate::export::metadata::Keep;
use crate::export::{ExportSettings, ExportTarget, FolderChoice};

/// The folders this session's agent work is held to.
#[derive(Default)]
pub(super) struct AgentScope {
    /// Folders the user opened this session: through the UI, the command
    /// line or `--drive`, never through an agent.
    user_folders: HashSet<PathBuf>,
    /// An agent's command is running, so a folder it opens is not the user's.
    running: bool,
    /// While an agent's export starts: where its folder may be made.
    export_roots: Option<Vec<PathBuf>>,
    /// Why the export refused its folder once made, for the agent's reply.
    export_refused: Option<String>,
}

const EXPORT_REFUSED: &str = "an agent can export only into a folder the user opened, \
                              or the export form's own folder";

impl AgentScope {
    /// The export's folder, now that it exists, checked again: a link put
    /// in its path since the first check is followed here. Always true for
    /// an export the user started.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn export_folder_ok(&mut self, dir: &Path) -> bool {
        let Some(roots) = &self.export_roots else {
            return true;
        };
        let ok = within_any(dir, roots);
        if !ok {
            self.export_refused = Some(EXPORT_REFUSED.into());
        }
        ok
    }
}

impl App {
    /// Notes that `dir` was opened, which makes it the user's unless an
    /// agent's command opened it.
    pub(super) fn note_opened_folder(&mut self, dir: &Path) {
        if !self.agent.running {
            self.agent.user_folders.insert(dir.to_path_buf());
        }
    }

    /// Runs a command an agent sent, once its paths pass the checks above.
    pub(crate) fn run_from_agent(&mut self, mut cmd: Command) -> Result<Outcome, CommandError> {
        let refuse = |reason: String| CommandError::Failed { reason };
        let roots = self.agent_roots();
        match &mut cmd {
            Command::Export(p) => {
                let settings = confine_agent_export(
                    p.settings.take(),
                    self.export_settings(),
                    self.folder_sel.as_deref(),
                    &roots,
                )
                .map_err(refuse)?;
                p.settings = Some(settings);
            }
            Command::NewCatalog(p) | Command::OpenCatalog(p) => {
                self.agent_catalog_ok(&p.path, &roots).map_err(refuse)?;
            }
            Command::AddPresetFolder(p) => {
                if !within_any(&p.path, &roots) {
                    return Err(refuse(
                        "an agent can add a preset folder only inside a folder the user opened; \
                         the user adds others with Add preset folder…"
                            .into(),
                    ));
                }
            }
            // Any folder: what it opens does not become the user's.
            Command::OpenFolder(_) => {}
            // Name photos of the open folder, a catalog opened before, or
            // nothing on disk at all.
            Command::SwitchCatalog(_)
            | Command::View(_)
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
        let is_export = matches!(cmd, Command::Export(_));
        if is_export {
            self.agent.export_roots = Some(roots);
        }
        self.agent.running = true;
        let out = self.run(cmd);
        self.agent.running = false;
        self.agent.export_roots = None;
        match self.agent.export_refused.take() {
            Some(reason) if is_export => Err(refuse(reason)),
            _ => out,
        }
    }

    /// Where an agent may write: the user's folders and the form's folder.
    fn agent_roots(&self) -> Vec<PathBuf> {
        let form_folder = match &self.export_settings().target {
            #[cfg(not(target_arch = "wasm32"))]
            ExportTarget::Folder(FolderChoice::Custom(dir)) => Some(dir.clone()),
            _ => None,
        };
        self.agent
            .user_folders
            .iter()
            .cloned()
            .chain(form_folder)
            .collect()
    }

    /// A catalog an agent may make or open: in the app's catalog folder, in
    /// one of the user's folders, or (to open) one the user opened before.
    #[cfg(not(target_arch = "wasm32"))]
    fn agent_catalog_ok(&self, path: &Path, roots: &[PathBuf]) -> Result<(), String> {
        let at = super::catalog::catalog_ref_for(path)?;
        let in_app_folder = crate::persist::catalog::native::default_dir()
            .is_some_and(|dir| crate::paths::is_within(at.path(), &dir));
        let known = self.recent_catalogs.iter().any(|r| r.path() == at.path());
        if in_app_folder || known || within_any(at.path(), roots) {
            return Ok(());
        }
        Err(
            "an agent can make or open a catalog only in the app's catalog folder or a \
             folder the user opened"
                .into(),
        )
    }

    /// The browser keeps its catalogs in its own storage, not at a path.
    #[cfg(target_arch = "wasm32")]
    fn agent_catalog_ok(&self, _path: &Path, _roots: &[PathBuf]) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn within_any(dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| crate::paths::is_within(dir, root))
}

/// The browser names no folder by path: it reaches only the picked one.
#[cfg(target_arch = "wasm32")]
fn within_any(_dir: &Path, _roots: &[PathBuf]) -> bool {
    false
}

/// The settings an agent's `export` runs with, or why it may not run. An
/// agent picks the size, format, sharpening and naming. The rest is the
/// form's: the album, the copyright, and whether location and identity data
/// stay in (only when the form keeps them too). The destination must be
/// inside `roots`, or Immich when the form uploads there.
pub(super) fn confine_agent_export(
    given: Option<ExportSettings>,
    form: &ExportSettings,
    open_folder: Option<&Path>,
    roots: &[PathBuf],
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
            let _ = (open_folder, roots);
            true
        }
        #[cfg(not(target_arch = "wasm32"))]
        ExportTarget::Folder(FolderChoice::ExportsSubfolder) => {
            open_folder.is_some_and(|dir| within_any(&dir.join(crate::export::EXPORTS_DIR), roots))
        }
        #[cfg(not(target_arch = "wasm32"))]
        ExportTarget::Folder(FolderChoice::Custom(dir)) => within_any(dir, roots),
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

    fn to(target: ExportTarget) -> ExportSettings {
        ExportSettings {
            target,
            ..ExportSettings::default()
        }
    }

    #[test]
    fn an_agent_exports_only_where_the_user_would() {
        let folder = temp_folder("agent-export");
        let roots = vec![folder.clone()];
        let form = ExportSettings::default();
        let confine = |given| confine_agent_export(Some(given), &form, Some(&folder), &roots);
        let custom = |dir: PathBuf| to(ExportTarget::Folder(FolderChoice::Custom(dir)));

        assert!(confine(to(ExportTarget::default())).is_ok());
        assert!(confine(custom(folder.join("Exports/web"))).is_ok());
        assert!(confine(custom(std::env::temp_dir())).is_err());
        assert!(confine(custom(folder.join("../elsewhere"))).is_err());
        assert!(confine(to(ExportTarget::Immich)).is_err());
        assert!(
            confine_agent_export(None, &form, None, &roots).is_err(),
            "no open folder, no Exports folder in it"
        );
        let other = temp_folder("agent-export-other");
        assert!(
            confine_agent_export(None, &form, Some(&other), &roots).is_err(),
            "the form's Exports, but in a folder that is not the user's"
        );

        let immich = to(ExportTarget::Immich);
        assert!(confine_agent_export(None, &immich, None, &[]).is_ok());
        let _ = std::fs::remove_dir_all(&folder);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn an_export_folder_made_outside_the_roots_is_refused_for_the_reply() {
        let root = temp_folder("agent-recheck");
        let outside = temp_folder("agent-recheck-out");
        let mut scope = AgentScope::default();
        assert!(
            scope.export_folder_ok(&outside),
            "a user's export is not held"
        );
        scope.export_roots = Some(vec![root.clone()]);
        assert!(scope.export_folder_ok(&root.join("Exports")));
        std::os::unix::fs::symlink(&outside, root.join("Exports-link")).unwrap();
        assert!(!scope.export_folder_ok(&root.join("Exports-link")));
        assert_eq!(scope.export_refused.as_deref(), Some(EXPORT_REFUSED));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&outside);
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
        let got = confine_agent_export(Some(asks), &form, None, &[]).unwrap();
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
        let stripped = confine_agent_export(Some(immich(all)), &immich(Keep::NONE), None, &[]);
        assert_eq!(stripped.map(|s| s.keep), Ok(Keep::NONE));
        let location = Keep {
            location: true,
            identity: false,
        };
        let kept = confine_agent_export(Some(immich(all)), &immich(location), None, &[]);
        assert_eq!(kept.map(|s| s.keep), Ok(location));
    }
}
