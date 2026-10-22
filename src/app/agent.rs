//! What an agent may do (control channel, MCP, WebMCP). It drives the app
//! through the same [`Command`]s as the keyboard, but it reads photo names
//! that anyone can write, so it can be talked into things. Every command it
//! sends passes [`App::run_from_agent`], which keeps it inside the user's
//! roots: the folders the user chose at an explicit entry point (the folder
//! picker, a path on the command line or a `--drive` run, a file Finder
//! hands over, or a catalog the user opens or makes in the UI, whose
//! folders count then). Nothing else adds a root: not a folder an agent
//! opened, not a launch restore, not tree or keyboard navigation. The roots
//! are saved resolved (absolute, links followed), so a restore uses the
//! saved set, whatever the working directory, and not what a catalog lists.
//!
//! In the browser no folder has a path, so no path rule applies there: an
//! agent reaches only what the user picked, and catalogs are named entries
//! in the browser's own storage.

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

/// One folder the user chose, resolved.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct Root {
    path: PathBuf,
    /// Chosen as one photo's folder: the root covers that folder and its
    /// `Exports`, not the folders inside it, so opening `~/photo.jpg` does
    /// not hand an agent the whole home folder.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    shallow: bool,
}

impl Root {
    /// Whether this root covers `real`, a resolved path.
    fn covers(&self, real: &Path) -> bool {
        match self.shallow {
            true => {
                real == self.path || real.starts_with(self.path.join(crate::export::EXPORTS_DIR))
            }
            false => real.starts_with(&self.path),
        }
    }
}

/// The folders the user chose, newest last. None is inside another.
#[derive(Clone, Default)]
pub(super) struct UserRoots(Vec<Root>);

impl UserRoots {
    /// The saved set, or none. A saved entry that is not absolute is
    /// dropped: it would mean whatever the next working directory is.
    pub(super) fn load() -> UserRoots {
        // A test must never read the developer's own settings.
        if cfg!(test) {
            return UserRoots::default();
        }
        let saved = crate::persist::prefs::load(USER_ROOTS_PREF)
            .and_then(|s| serde_json::from_str::<Vec<Root>>(&s).ok())
            .unwrap_or_default();
        UserRoots(saved.into_iter().filter(|r| r.path.is_absolute()).collect())
    }

    /// Adds each of `dirs` and saves once. A folder that does not resolve
    /// is not added.
    fn add_all<'a>(&mut self, dirs: impl IntoIterator<Item = &'a Path>, shallow: bool) {
        let mut changed = false;
        for dir in dirs {
            changed |= self.insert(dir, shallow);
        }
        if changed {
            self.save();
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn add(&mut self, dir: &Path, shallow: bool) {
        self.add_all([dir], shallow);
    }

    /// Adds `dir` resolved, unless a root covers it already; a new deep root
    /// replaces the roots inside it. Returns whether the set changed.
    fn insert(&mut self, dir: &Path, shallow: bool) -> bool {
        let Ok(real) = std::fs::canonicalize(dir) else {
            eprintln!("[agent] not a folder the user chose: {}", dir.display());
            return false;
        };
        if !real.is_absolute() {
            return false;
        }
        let covered = self.0.iter().any(|r| {
            if shallow {
                r.covers(&real)
            } else {
                !r.shallow && real.starts_with(&r.path)
            }
        });
        if covered {
            return false;
        }
        if !shallow {
            self.0.retain(|r| !r.path.starts_with(&real));
        }
        self.0.push(Root {
            path: real,
            shallow,
        });
        let over = self.0.len().saturating_sub(MAX_USER_ROOTS);
        self.0.drain(..over);
        true
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

    /// Whether a root covers `path`. The path resolves once; the roots are
    /// stored resolved.
    pub(super) fn holds(&self, path: &Path) -> bool {
        resolve(path).is_some_and(|real| self.covers(&real))
    }

    fn covers(&self, real: &Path) -> bool {
        self.0.iter().any(|r| r.covers(real))
    }
}

/// Where an agent's export may land: the user's roots, and the export
/// form's own folder.
// The browser's exports go to the picked folder only; nothing reads this there.
#[cfg_attr(target_arch = "wasm32", allow(dead_code))]
pub(super) struct ExportConfine {
    roots: UserRoots,
    /// Resolved.
    form_folder: Option<PathBuf>,
}

impl ExportConfine {
    fn new(roots: UserRoots, form_folder: Option<&Path>) -> ExportConfine {
        ExportConfine {
            roots,
            form_folder: form_folder.and_then(resolve),
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn holds(&self, dir: &Path) -> bool {
        resolve(dir).is_some_and(|real| {
            self.roots.covers(&real)
                || self
                    .form_folder
                    .as_ref()
                    .is_some_and(|f| real.starts_with(f))
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

impl App {
    /// The user opened `path` at an explicit entry point: open it, and make
    /// it one of the user's roots. A folder becomes a root with everything
    /// inside it; a photo makes its own folder a shallow root; a catalog
    /// makes its folders roots.
    pub(crate) fn open_from_user(&mut self, path: PathBuf) {
        // `lightphotos .` must name this folder, not whatever the working
        // directory of a later launch is: the catalog keeps what is opened.
        // Spelled as given otherwise, so absolute photo names still match.
        #[cfg(not(target_arch = "wasm32"))]
        let path = std::path::absolute(&path).unwrap_or(path);
        #[cfg(not(target_arch = "wasm32"))]
        let catalog = crate::persist::catalog::CatalogRef::parse(&path).ok();
        #[cfg(not(target_arch = "wasm32"))]
        let root = match path.is_dir() {
            true => Some((path.clone(), false)),
            false => path.parent().map(|dir| (dir.to_path_buf(), true)),
        };
        self.open(path);
        #[cfg(not(target_arch = "wasm32"))]
        match catalog {
            Some(at) if self.catalog.at() == &at => self.adopt_catalog_folders(),
            Some(_) => {}
            None => {
                if let Some((dir, shallow)) = root {
                    self.user_roots.add(&dir, shallow);
                }
            }
        }
    }

    /// The user opened or made the open catalog in the UI: its folders are
    /// the user's.
    pub(super) fn adopt_catalog_folders(&mut self) {
        let folders = self.catalog.folders().to_vec();
        self.user_roots
            .add_all(folders.iter().map(PathBuf::as_path), false);
    }

    /// Runs a command an agent sent, once its paths pass the checks here.
    pub(crate) fn run_from_agent(&mut self, cmd: Command) -> Result<Outcome, CommandError> {
        let refuse = |reason: &str| CommandError::Failed {
            reason: reason.into(),
        };
        match &cmd {
            Command::Export(p) => return self.agent_export(p.clone()),
            #[cfg(not(target_arch = "wasm32"))]
            Command::OpenCatalog(p) => {
                self.check(&cmd)?;
                self.agent_open_catalog(&p.path).map_err(|e| refuse(&e))?;
                return Ok(Outcome::default());
            }
            #[cfg(not(target_arch = "wasm32"))]
            Command::SwitchCatalog(p) => {
                self.check(&cmd)?;
                self.agent_switch_catalog(&p.name).map_err(|e| refuse(&e))?;
                return Ok(Outcome::default());
            }
            #[cfg(not(target_arch = "wasm32"))]
            Command::NewCatalog(p) => {
                let at = super::catalog::catalog_ref_for(&p.path).map_err(|e| refuse(&e))?;
                if !self.user_roots.holds(at.path()) {
                    return Err(refuse(CATALOG_REFUSED));
                }
            }
            // The browser's catalogs are named entries in its own storage.
            #[cfg(target_arch = "wasm32")]
            Command::OpenCatalog(_) | Command::SwitchCatalog(_) | Command::NewCatalog(_) => {}
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
        self.run(cmd)
    }

    /// An agent's export, held to the user's roots and the form's folder.
    fn agent_export(&mut self, p: ExportParams) -> Result<Outcome, CommandError> {
        let refuse = |reason: String| CommandError::Failed { reason };
        self.check(&Command::Export(p.clone()))?;
        let form_folder: Option<PathBuf> = match &self.export_settings().target {
            #[cfg(not(target_arch = "wasm32"))]
            ExportTarget::Folder(FolderChoice::Custom(dir)) => Some(dir.clone()),
            _ => None,
        };
        let confine = ExportConfine::new(self.user_roots.clone(), form_folder.as_deref());
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

    /// Opens a catalog inside the user's roots that passes
    /// [`agent_may_use`](Self::agent_may_use). It adds no roots.
    #[cfg(not(target_arch = "wasm32"))]
    fn agent_open_catalog(&mut self, path: &Path) -> Result<(), String> {
        let at = super::catalog::catalog_ref_for(path)?;
        if !self.user_roots.holds(at.path()) {
            return Err(CATALOG_REFUSED.into());
        }
        self.agent_switch_to(at)
    }

    /// `catalog.switch` by name, under the same rule as `catalog.open`
    /// except where the catalog itself lives: one opened before or the
    /// default one.
    #[cfg(not(target_arch = "wasm32"))]
    fn agent_switch_catalog(&mut self, name: &str) -> Result<(), String> {
        if name == self.catalog.name() {
            return Ok(());
        }
        let at = self.catalog_named(name)?;
        self.agent_switch_to(at)
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn agent_switch_to(&mut self, at: crate::persist::catalog::CatalogRef) -> Result<(), String> {
        let catalog = crate::persist::catalog::Catalog::open(at).map_err(|e| e.to_string())?;
        self.agent_may_use(&catalog)?;
        self.switch_catalog(catalog);
        // The restore it ran must have stayed inside the roots; if not, put
        // nothing on screen rather than the folder it reached.
        if let Some(dir) = self.folder_sel.clone() {
            if !self.user_roots.holds(&dir) {
                self.close_folder();
                return Err(format!("{} is not a folder the user opened", dir.display()));
            }
        }
        Ok(())
    }

    /// Whether an agent may switch to `catalog`: every folder it lists, and
    /// the folder switching to it would reopen, is inside the user's roots,
    /// so neither opening it nor a later launch restore of it reaches a
    /// folder the user never chose.
    #[cfg(not(target_arch = "wasm32"))]
    fn agent_may_use(&self, catalog: &crate::persist::catalog::Catalog) -> Result<(), String> {
        let restored =
            super::session::to_restore(self.session.as_ref(), catalog.folders(), Path::is_dir)
                .and_then(|session| session.dir);
        let outside = catalog
            .folders()
            .iter()
            .chain(restored.as_ref())
            .find(|dir| !self.user_roots.holds(dir));
        match outside {
            Some(dir) => Err(format!(
                "an agent can open only a catalog whose folders the user opened, and \
                 this one reaches {}",
                dir.display()
            )),
            None => Ok(()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
const CATALOG_REFUSED: &str = "an agent can make or open a catalog only inside a folder the \
                               user opened";

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

    #[test]
    fn a_root_is_stored_absolute_and_a_relative_root_holds_nothing() {
        let mut roots = UserRoots::default();
        roots.add(Path::new("."), false);
        let saved = serde_json::to_string(&roots.0).unwrap();
        assert!(!saved.contains("\".\""), "{saved}");
        let cwd = std::env::current_dir().unwrap();
        assert!(!crate::paths::is_within(&cwd.join("src"), Path::new(".")));
    }

    #[test]
    fn a_root_inside_another_is_one_root() {
        let root = temp_folder("agent-nested");
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        let mut roots = UserRoots::default();
        roots.add(&root.join("a"), false);
        roots.add(&root, false);
        roots.add(&root.join("a/b"), false);
        assert_eq!(roots.0.len(), 1, "the outer root covers the rest");
        assert!(roots.holds(&root.join("a/b")));
        let _ = std::fs::remove_dir_all(&root);
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
        let settings = ExportSettings {
            target: ExportTarget::Folder(FolderChoice::Custom(root.join("blocker/new"))),
            ..ExportSettings::default()
        };
        let confine = confine_to(&root);
        let started = app.export_with(vec![photo], Some(settings), Some(&confine));
        assert!(started.is_err(), "{started:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Exports held to `root` alone.
    fn confine_to(root: &Path) -> ExportConfine {
        let mut roots = UserRoots::default();
        roots.add(root, false);
        ExportConfine::new(roots, None)
    }

    fn nowhere() -> ExportConfine {
        ExportConfine::new(UserRoots::default(), None)
    }

    #[test]
    fn a_photos_folder_is_a_shallow_root() {
        let root = temp_folder("agent-shallow");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let mut roots = UserRoots::default();
        roots.add(&root, true);
        assert!(roots.holds(&root));
        assert!(roots.holds(&root.join("Exports/web")), "its Exports");
        assert!(!roots.holds(&root.join("sub")), "not the folders inside it");
        roots.add(&root.join("sub"), true);
        assert_eq!(roots.0.len(), 2, "a shallow root covers no other folder");
        roots.add(&root, false);
        assert_eq!(roots.0.len(), 1, "opening the folder itself covers both");
        assert!(roots.holds(&root.join("sub")));
        let _ = std::fs::remove_dir_all(&root);
    }

    fn to(target: ExportTarget) -> ExportSettings {
        ExportSettings {
            target,
            ..ExportSettings::default()
        }
    }

    #[test]
    fn an_agent_exports_only_where_the_user_would() {
        let folder = temp_folder("agent-export");
        let roots = confine_to(&folder);
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
        let settings = ExportSettings {
            target: ExportTarget::Folder(FolderChoice::Custom(root.join("link/new"))),
            ..ExportSettings::default()
        };
        let confine = confine_to(&root);
        let started = app.export_with(vec![photo], Some(settings), Some(&confine));
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
