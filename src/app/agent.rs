//! What an agent may do (control channel, MCP, WebMCP). It drives the app
//! through the same [`Command`]s as the keyboard, but it reads photo names
//! that anyone can write, so it can be talked into things. Every command it
//! sends passes [`App::run_from_agent`], which keeps it inside the user's
//! roots: the folders the user chose at an explicit entry point (the folder
//! picker, a path on the command line or a `--drive` run, a file Finder
//! hands over, or a catalog the user opens or makes in the UI, whose
//! folders count then). Nothing else adds a root: not a folder an agent
//! opened, not a launch restore, not tree or keyboard navigation. The roots
//! are saved, so a restore uses the saved set and not what a catalog lists.

use std::path::{Path, PathBuf};

use super::{App, StatusKind};
use crate::command::{Command, CommandError, ExportParams, Outcome};
use crate::export::metadata::Keep;
use crate::export::{ExportSettings, ExportTarget, FolderChoice};

/// `prefs` key holding the user's roots.
const USER_ROOTS_PREF: &str = "agent_user_roots";
/// The most roots kept; the oldest go first.
const MAX_USER_ROOTS: usize = 256;

pub(super) const EXPORT_REFUSED: &str = "an agent can export only into a folder the user \
                                         opened, or the export form's own folder";

/// The folders the user chose, newest last.
#[derive(Default)]
pub(super) struct UserRoots(Vec<PathBuf>);

impl UserRoots {
    /// The saved set, or none.
    pub(super) fn load() -> UserRoots {
        // A test must never read the developer's own settings.
        if cfg!(test) {
            return UserRoots::default();
        }
        let saved = crate::persist::prefs::load(USER_ROOTS_PREF)
            .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
            .unwrap_or_default();
        UserRoots(saved)
    }

    fn add(&mut self, dir: &Path) {
        if self.0.iter().any(|r| r == dir) {
            return;
        }
        self.0.push(dir.to_path_buf());
        let over = self.0.len().saturating_sub(MAX_USER_ROOTS);
        self.0.drain(..over);
        self.save();
    }

    fn save(&self) {
        // A test must never write the developer's own settings.
        if cfg!(test) {
            return;
        }
        let saved = serde_json::to_string(&self.0)
            .map_err(|e| e.to_string())
            .and_then(|json| crate::persist::prefs::save(USER_ROOTS_PREF, &json));
        if let Err(e) = saved {
            eprintln!("[agent] could not save the user's folders: {e}");
        }
    }

    pub(super) fn holds(&self, path: &Path) -> bool {
        within_any(path, &self.0)
    }
}

impl App {
    /// The user opened `path` at an explicit entry point: open it, and make
    /// it (a file's folder, or a catalog's folders) one of the user's roots.
    pub(crate) fn open_from_user(&mut self, path: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        let catalog = crate::persist::catalog::CatalogRef::parse(&path).ok();
        #[cfg(not(target_arch = "wasm32"))]
        let root = if path.is_dir() {
            Some(path.clone())
        } else {
            path.parent().map(Path::to_path_buf)
        };
        self.open(path);
        #[cfg(not(target_arch = "wasm32"))]
        match catalog {
            Some(at) if self.catalog.at() == &at => self.adopt_catalog_folders(),
            Some(_) => {}
            None => {
                if let Some(root) = root {
                    self.user_roots.add(&root);
                }
            }
        }
    }

    /// The user opened or made the open catalog in the UI: its folders are
    /// the user's.
    pub(super) fn adopt_catalog_folders(&mut self) {
        for dir in self.catalog.folders().to_vec() {
            self.user_roots.add(&dir);
        }
    }

    /// Runs a command an agent sent, once its paths pass the checks here.
    pub(crate) fn run_from_agent(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        let refuse = |reason: &str| CommandError::Failed {
            reason: reason.into(),
        };
        match &cmd {
            Command::Export(p) => return self.agent_export(p.clone()),
            Command::OpenCatalog(p) => {
                self.check(&cmd)?;
                self.agent_open_catalog(&p.path).map_err(|e| refuse(&e))?;
                return Ok(Outcome::default());
            }
            Command::NewCatalog(p) => {
                if !self
                    .user_roots
                    .holds(&agent_catalog_path(&p.path).map_err(|e| refuse(&e))?)
                {
                    return Err(refuse(CATALOG_REFUSED));
                }
            }
            Command::OpenFolder(p) => {
                if !self.user_roots.holds(&p.path) {
                    return Err(refuse(
                        "an agent can open only a folder inside one the user opened",
                    ));
                }
            }
            Command::AddPresetFolder(p) => {
                if !self.user_roots.holds(&p.path) {
                    return Err(refuse(
                        "an agent can add a preset folder only inside a folder the user \
                         opened; the user adds others with Add preset folder…",
                    ));
                }
            }
            // These act on the open folder's photos (a folder the user or a
            // check above chose) or on nothing on disk.
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
        self.run(cmd)
    }

    /// An agent's export, held to the user's roots and the form's folder.
    fn agent_export(&mut self, p: ExportParams) -> Result<Outcome, CommandError> {
        let refuse = |reason: String| CommandError::Failed { reason };
        self.check(&Command::Export(p.clone()))?;
        let form_folder = match &self.export_settings().target {
            #[cfg(not(target_arch = "wasm32"))]
            ExportTarget::Folder(FolderChoice::Custom(dir)) => Some(dir.clone()),
            _ => None,
        };
        let roots: Vec<PathBuf> = self
            .user_roots
            .0
            .iter()
            .cloned()
            .chain(form_folder)
            .collect();
        let Ok(settings) = confine_agent_export(
            p.settings,
            self.export_settings(),
            self.folder_sel.as_deref(),
            &roots,
        ) else {
            return Err(refuse(self.refuse_agent_export()));
        };
        let paths = self.resolve(&p.photos)?;
        let pending = self
            .export_with(paths.clone(), Some(settings), Some(&roots))
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

    /// Opens a catalog inside the user's roots whose folders all are too, so
    /// opening it (and any later launch restore of it) reaches no folder the
    /// user never chose. It adds no roots.
    #[cfg(not(target_arch = "wasm32"))]
    fn agent_open_catalog(&mut self, path: &Path) -> Result<(), String> {
        let at = super::catalog::catalog_ref_for(path)?;
        if !self.user_roots.holds(at.path()) {
            return Err(CATALOG_REFUSED.into());
        }
        let catalog = crate::persist::catalog::Catalog::open(at).map_err(|e| e.to_string())?;
        if let Some(dir) = catalog.folders().iter().find(|d| !self.user_roots.holds(d)) {
            return Err(format!(
                "an agent can open only a catalog whose folders the user opened, and \
                 this one lists {}",
                dir.display()
            ));
        }
        self.switch_catalog(catalog);
        Ok(())
    }

    /// The browser's catalogs live in its own storage; an agent switches
    /// among them by name only.
    #[cfg(target_arch = "wasm32")]
    fn agent_open_catalog(&mut self, _path: &Path) -> Result<(), String> {
        Err(CATALOG_REFUSED.into())
    }
}

const CATALOG_REFUSED: &str = "an agent can make or open a catalog only inside a folder the \
                               user opened";

/// Where `catalog.new` would put the catalog at `path`.
#[cfg(not(target_arch = "wasm32"))]
fn agent_catalog_path(path: &Path) -> Result<PathBuf, String> {
    super::catalog::catalog_ref_for(path).map(|at| at.path().to_path_buf())
}

/// The browser's catalogs live in its own storage, so no path is the
/// user's there and `catalog.new` is refused.
#[cfg(target_arch = "wasm32")]
fn agent_catalog_path(_path: &Path) -> Result<PathBuf, String> {
    Err(CATALOG_REFUSED.into())
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn within_any(dir: &Path, roots: &[PathBuf]) -> bool {
    roots.iter().any(|root| crate::paths::is_within(dir, root))
}

/// The browser names no folder by path: it reaches only the picked one.
#[cfg(target_arch = "wasm32")]
pub(super) fn within_any(_dir: &Path, _roots: &[PathBuf]) -> bool {
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
        let settings = ExportSettings {
            target: ExportTarget::Folder(FolderChoice::Custom(root.join("link/new"))),
            ..ExportSettings::default()
        };
        let started = app.export_with(
            vec![photo],
            Some(settings),
            Some(std::slice::from_ref(&root)),
        );
        assert!(started.is_err(), "{started:?}");
        assert!(!outside.join("new").exists(), "made outside the roots");
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
