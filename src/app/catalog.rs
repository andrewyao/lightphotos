//! The App's catalog: which one opens at launch, switching to another, and
//! the pref that remembers the last one. The catalog itself, its roots and
//! its marks copy, is [`crate::persist::catalog`].

use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
use super::*;
use crate::persist::catalog::Catalog;
#[cfg(not(target_arch = "wasm32"))]
use crate::persist::catalog::{CatalogRef, DEFAULT_NAME};

/// `prefs` key holding the last opened catalog's path.
#[cfg(not(target_arch = "wasm32"))]
const CATALOG_PREF: &str = "catalog";

/// What launch opened, and why it is not the catalog asked for, if so.
pub(super) struct Launch {
    pub(super) catalog: Catalog,
    /// The last catalog could not open, so `catalog` is the default. The
    /// pref keeps naming the last one, so it reopens once it is back.
    pub(super) fallback: Option<String>,
}

impl Launch {
    /// A catalog that is never written. Every test's.
    #[cfg(test)]
    pub(super) fn in_memory() -> Launch {
        Launch {
            catalog: Catalog::in_memory(DEFAULT_NAME),
            fallback: None,
        }
    }

    /// The catalog the last launch left open, or the default one, created
    /// from `legacy` (an older build's Folders list) when it does not exist
    /// yet.
    #[cfg(all(not(test), not(target_arch = "wasm32")))]
    pub(super) fn at_startup(legacy: &[PathBuf]) -> Launch {
        let last = crate::persist::prefs::load(CATALOG_PREF).map(PathBuf::from);
        let default_dir = crate::persist::catalog::native::default_dir();
        launch(last.as_deref(), default_dir.as_deref(), legacy)
    }

    /// The browser's one in-memory catalog, until its store exists.
    #[cfg(all(not(test), target_arch = "wasm32"))]
    pub(super) fn at_startup(_legacy: &[PathBuf]) -> Launch {
        Launch {
            catalog: Catalog::open_default(),
            fallback: None,
        }
    }
}

/// `Launch::at_startup` with its inputs injected. `last` is the pref, which
/// nothing here writes: a last catalog that is not there now may be back on
/// the next launch.
#[cfg(not(target_arch = "wasm32"))]
fn launch(last: Option<&Path>, default_dir: Option<&Path>, legacy: &[PathBuf]) -> Launch {
    let mut fallback = None;
    if let Some(last) = last {
        match CatalogRef::parse(last).and_then(Catalog::open) {
            Ok(catalog) => {
                return Launch {
                    catalog,
                    fallback: None,
                }
            }
            Err(e) => fallback = Some(e.to_string()),
        }
    }
    let default = default_dir
        .ok_or_else(|| "no home directory for the default catalog".to_string())
        .and_then(|dir| CatalogRef::in_dir(dir, DEFAULT_NAME).map_err(|e| e.to_string()))
        .and_then(|at| Catalog::open_or_create(at, legacy.to_vec()).map_err(|e| e.to_string()));
    match default {
        Ok(catalog) => Launch { catalog, fallback },
        // Nothing can be saved, but the user keeps the folders they had.
        Err(e) => {
            let mut catalog = Catalog::in_memory(DEFAULT_NAME);
            for dir in legacy {
                catalog.add_folder(dir);
            }
            Launch {
                catalog,
                fallback: Some(fallback.map_or(e.clone(), |first| format!("{first}; {e}"))),
            }
        }
    }
}

// The browser has one catalog until its store lands, so nothing there
// switches yet.
#[cfg(not(target_arch = "wasm32"))]
impl App {
    /// Leave the folder showing, make `catalog` the one open, and reopen
    /// the last session inside it, or its first folder, or the landing page.
    pub(super) fn switch_catalog(&mut self, catalog: Catalog) {
        self.close_folder();
        self.catalog = catalog;
        self.expanded.clear();
        self.subdirs.clear();
        self.missing_roots = super::folders::missing_among(self.catalog.folders());
        self.remember_catalog();
        self.restore_last_session();
        self.request_redraw();
    }

    /// Open the catalog at `at` and switch to it.
    pub(super) fn open_catalog(&mut self, at: CatalogRef) {
        match Catalog::open(at) {
            Ok(catalog) => self.switch_catalog(catalog),
            Err(e) => {
                self.set_status(
                    StatusKind::Error,
                    (crate::i18n::t().catalog_open_failed)(&e.to_string()),
                );
                self.request_redraw();
            }
        }
    }

    /// Save the open catalog as the one to reopen next launch.
    fn remember_catalog(&mut self) {
        // A test must never write the developer's own settings.
        if cfg!(test) {
            return;
        }
        let path = self.catalog.at().path().to_string_lossy();
        if let Err(e) = crate::persist::prefs::save(CATALOG_PREF, &path) {
            eprintln!("[catalog] could not save the catalog pref: {e}");
        }
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;
    use crate::persist::catalog::EXT;

    fn default_at(dir: &Path) -> PathBuf {
        dir.join(format!("{DEFAULT_NAME}.{EXT}"))
    }

    #[test]
    fn a_legacy_session_seeds_the_default_catalog_once() {
        let dir = temp_folder("catalog-seed");
        let legacy = vec![dir.join("b"), dir.join("a")];
        let first = launch(None, Some(&dir), &legacy);
        assert!(first.fallback.is_none());
        assert_eq!(first.catalog.at().path(), default_at(&dir));
        assert_eq!(first.catalog.folders(), &[dir.join("a"), dir.join("b")]);
        drop(first);

        let later = launch(None, Some(&dir), &[dir.join("newer")]);
        assert_eq!(
            later.catalog.folders(),
            &[dir.join("a"), dir.join("b")],
            "a second launch opens the default as it is, with no reseed"
        );
        assert!(std::fs::read(default_at(&dir).join("catalog.json")).is_ok());
    }

    #[test]
    fn the_last_catalog_reopens_from_the_pref() {
        let dir = temp_folder("catalog-last");
        let trip = CatalogRef::in_dir(&dir, "trip").unwrap();
        drop(Catalog::create_at(trip.clone(), vec![dir.join("t")]).unwrap());
        let launched = launch(Some(trip.path()), Some(&dir), &[dir.join("legacy")]);
        assert!(launched.fallback.is_none());
        assert_eq!(launched.catalog.at(), &trip);
        assert_eq!(launched.catalog.folders(), &[dir.join("t")]);
        assert!(
            !default_at(&dir).exists(),
            "reopening the last catalog does not create the default"
        );
    }

    #[test]
    fn a_missing_last_catalog_falls_back_to_default_and_says_so() {
        let dir = temp_folder("catalog-missing");
        let gone = dir.join(format!("gone.{EXT}"));
        let launched = launch(Some(&gone), Some(&dir), &[dir.join("legacy")]);
        let why = launched.fallback.expect("the fallback is reported");
        assert!(why.contains("gone"), "{why}");
        assert_eq!(launched.catalog.at().path(), default_at(&dir));
        assert_eq!(launched.catalog.folders(), &[dir.join("legacy")]);

        let bad_name = dir.join("not-a-catalog");
        let launched = launch(Some(&bad_name), Some(&dir), &[]);
        assert!(launched.fallback.is_some());
        assert_eq!(launched.catalog.at().path(), default_at(&dir));
    }

    /// With no home and no default, the app still starts with the folders
    /// it had, and says nothing will be saved.
    #[test]
    fn without_a_default_dir_the_catalog_is_in_memory() {
        let launched = launch(None, None, &[PathBuf::from("/kept")]);
        assert!(launched.fallback.is_some());
        assert_eq!(launched.catalog.folders(), &[PathBuf::from("/kept")]);
    }

    #[test]
    fn switching_catalogs_swaps_the_roots_and_reopens_inside_the_new_one() {
        let dir = temp_folder("catalog-switch");
        let (a, b) = (dir.join("a"), dir.join("b"));
        for d in [&a, &b] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("x.jpg"), b"").unwrap();
        }
        let mut app = App::new(None);
        app.open(a.clone());
        app.expanded.insert(a.clone());
        assert_eq!(app.folder_roots(), std::slice::from_ref(&a));

        let other = CatalogRef::in_dir(&dir, "other").unwrap();
        let other = Catalog::create_at(other, vec![b.clone()]).unwrap();
        app.switch_catalog(other);
        assert_eq!(app.catalog.name(), "other");
        assert_eq!(app.folder_roots(), std::slice::from_ref(&b));
        assert_eq!(
            app.folder_sel.as_deref(),
            Some(b.as_path()),
            "the session's folder is not in the new catalog, so its first opens"
        );
        assert!(!app.expanded.contains(&a));

        let empty = Catalog::in_memory("empty");
        app.switch_catalog(empty);
        assert!(app.folder_roots().is_empty());
        assert!(
            app.playlist.is_none(),
            "an empty catalog shows the landing page"
        );
    }

    /// `lightphotos trip.lightphotos` opens the catalog rather than adding
    /// it as a photo folder.
    #[test]
    fn opening_a_catalog_path_switches_to_it() {
        let dir = temp_folder("catalog-open-path");
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        let at = CatalogRef::in_dir(&dir, "trip").unwrap();
        drop(Catalog::create_at(at.clone(), vec![photos.clone()]).unwrap());
        let mut app = App::new(None);
        app.open(at.path().to_path_buf());
        assert_eq!(app.catalog.at(), &at);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&photos));
        assert!(
            !app.folder_roots().contains(&at.path().to_path_buf()),
            "a catalog is never a photo root"
        );
    }

    #[test]
    fn a_catalog_that_will_not_open_leaves_the_current_one_with_a_toast() {
        let dir = temp_folder("catalog-open-bad");
        let at = CatalogRef::in_dir(&dir, "nope").unwrap();
        let mut app = App::new(None);
        app.catalog.add_folder(&dir);
        app.open_catalog(at);
        assert_eq!(app.catalog.name(), DEFAULT_NAME);
        assert_eq!(app.folder_roots(), std::slice::from_ref(&dir));
        assert!(app.status().is_some());
    }
}
