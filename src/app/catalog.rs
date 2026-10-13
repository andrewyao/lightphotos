//! The App's catalog: which one opens at launch, switching to another, and
//! the pref that remembers the last one. The catalog itself, its roots and
//! its marks copy, is [`crate::persist::catalog`].

use std::path::PathBuf;

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

impl App {
    /// Bring the catalog's copy of the open folder's marks up to date with
    /// the folder store. Once per frame, and before the folder store or
    /// the catalog is replaced. Idempotent: an unchanged revision, or a
    /// folder still loading, does nothing.
    pub(crate) fn sync_marks(&mut self) {
        let revision = self.folder_store.marks_revision();
        if revision == self.marks_synced {
            return;
        }
        let Some((dir, marks)) = self.folder_store.dir_marks() else {
            return;
        };
        let dir = dir.to_path_buf();
        self.catalog.record_marks(&dir, &marks);
        self.marks_synced = revision;
    }
}

// The browser has one catalog until its store lands, so nothing there
// switches yet.
#[cfg(not(target_arch = "wasm32"))]
impl App {
    /// Leave the folder showing, make `catalog` the one open, and reopen
    /// the last session inside it, or its first folder, or the landing page.
    /// The catalog left behind gets the folder's marks first; dropping it
    /// finishes its writes.
    pub(super) fn switch_catalog(&mut self, catalog: Catalog) {
        self.sync_marks();
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

    use crate::app::test_support::load_folder;
    use crate::persist::catalog::{DirMarks, PhotoMarks};
    use crate::persist::folder_store::FolderStore;
    use crate::sidecar::{ColorLabel, Flag};
    use std::time::Duration;

    /// An App over an on-disk catalog listing `photos`, a folder of
    /// `a.jpg`, `b.jpg` and `c.jpg`, with nothing loaded yet.
    fn disk_app(tag: &str) -> (App, PathBuf) {
        let dir = temp_folder(tag);
        let photos = dir.join("photos");
        std::fs::create_dir_all(&photos).unwrap();
        for name in ["a.jpg", "b.jpg", "c.jpg"] {
            std::fs::write(photos.join(name), b"").unwrap();
        }
        let at = CatalogRef::in_dir(&dir, "t").unwrap();
        let mut app = App::new(None);
        app.catalog = Catalog::create_at(at, vec![photos.clone()]).unwrap();
        (app, photos)
    }

    /// Project and wait for the shard to land.
    fn synced(app: &mut App) {
        app.sync_marks();
        app.catalog.flush_blocking(Duration::from_secs(10));
        assert!(app.catalog.take_error().is_none());
    }

    fn marks(entries: &[(&str, PhotoMarks)]) -> DirMarks {
        entries
            .iter()
            .map(|(name, m)| (name.to_string(), *m))
            .collect()
    }

    #[test]
    fn rating_flagging_and_labelling_land_in_the_shard_as_the_sidecars_say() {
        let (mut app, photos) = disk_app("marks-shard");
        load_folder(&mut app, &photos);
        app.folder_store.set(&photos.join("a.jpg"), 4);
        app.folder_store
            .set_flag(&photos.join("b.jpg"), Some(Flag::Pick));
        app.folder_store
            .set_label(&photos.join("c.jpg"), Some(ColorLabel::Red));
        app.folder_store.set_rotation(&photos.join("c.jpg"), 1);
        synced(&mut app);

        let shard = app.catalog.shard_path(&photos);
        let json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&shard).unwrap()).unwrap();
        assert_eq!(json["dir"], photos.to_string_lossy().as_ref());
        assert_eq!(json["photos"]["a.jpg"], serde_json::json!({"rating": 4}));
        assert_eq!(json["photos"]["b.jpg"], serde_json::json!({"flag": "pick"}));
        assert_eq!(
            json["photos"]["c.jpg"],
            serde_json::json!({"label": "red"}),
            "a rotation is an edit, not a mark"
        );
        let want = marks(&[
            (
                "a.jpg",
                PhotoMarks {
                    rating: Some(4),
                    ..Default::default()
                },
            ),
            (
                "b.jpg",
                PhotoMarks {
                    flag: Some(Flag::Pick),
                    ..Default::default()
                },
            ),
            (
                "c.jpg",
                PhotoMarks {
                    label: Some(ColorLabel::Red),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(app.catalog.load_shard(&photos), Some(want));
    }

    #[test]
    fn clearing_the_last_mark_deletes_the_shard() {
        let (mut app, photos) = disk_app("marks-clear");
        load_folder(&mut app, &photos);
        let a = photos.join("a.jpg");
        app.folder_store.set(&a, 2);
        app.folder_store.set_flag(&a, Some(Flag::Reject));
        synced(&mut app);
        let shard = app.catalog.shard_path(&photos);
        assert!(shard.exists());

        app.folder_store.set(&a, 0);
        synced(&mut app);
        assert!(shard.exists(), "the flag still marks the photo");
        app.folder_store.set_flag(&a, None);
        synced(&mut app);
        assert!(!shard.exists());
    }

    #[test]
    fn a_stale_shard_is_rewritten_when_its_folder_loads() {
        let (mut app, photos) = disk_app("marks-stale");
        let mut truth = FolderStore::with_dir(photos.clone());
        truth.set(&photos.join("a.jpg"), 3);
        truth.flush_blocking(Duration::from_secs(10));
        drop(truth);
        let shard = app.catalog.shard_path(&photos);
        crate::paths::write_atomic_in_dir(
            &shard,
            format!(
                r#"{{"v":1,"dir":{},"photos":{{"zzz.jpg":{{"rating":5}},"a.jpg":{{"rating":1}}}}}}"#,
                serde_json::to_string(&photos).unwrap()
            )
            .as_bytes(),
        )
        .unwrap();

        load_folder(&mut app, &photos);
        synced(&mut app);
        let want = marks(&[(
            "a.jpg",
            PhotoMarks {
                rating: Some(3),
                ..Default::default()
            },
        )]);
        assert_eq!(app.catalog.load_shard(&photos), Some(want));
    }

    /// A load is a background read; a mark made before it lands must wait
    /// for it, or the shard would hold one photo and lose the rest.
    #[test]
    fn a_rating_made_while_the_folder_loads_waits_for_the_load() {
        use crate::app::test_support::wait_for_folder_store;
        let (mut app, photos) = disk_app("marks-loading");
        let mut truth = FolderStore::with_dir(photos.clone());
        truth.set(&photos.join("b.jpg"), 5);
        truth.flush_blocking(Duration::from_secs(10));
        drop(truth);

        app.load_playlist(Playlist::from_dir(&photos), photos.clone());
        app.folder_store.set(&photos.join("a.jpg"), 2);
        synced(&mut app);
        let shard = app.catalog.shard_path(&photos);
        assert!(!shard.exists(), "nothing is written while the load is out");

        wait_for_folder_store(&mut app);
        synced(&mut app);
        let want = marks(&[
            (
                "a.jpg",
                PhotoMarks {
                    rating: Some(2),
                    ..Default::default()
                },
            ),
            (
                "b.jpg",
                PhotoMarks {
                    rating: Some(5),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(app.catalog.load_shard(&photos), Some(want));
    }

    #[test]
    fn a_bulk_delete_drops_the_photos_from_the_shard() {
        let (mut app, photos) = disk_app("marks-delete");
        load_folder(&mut app, &photos);
        let (a, b) = (photos.join("a.jpg"), photos.join("b.jpg"));
        app.folder_store.set(&a, 1);
        app.folder_store.set(&b, 2);
        synced(&mut app);

        app.folder_store.forget_photos(std::slice::from_ref(&a));
        synced(&mut app);
        let want = marks(&[(
            "b.jpg",
            PhotoMarks {
                rating: Some(2),
                ..Default::default()
            },
        )]);
        assert_eq!(app.catalog.load_shard(&photos), Some(want));
    }

    #[test]
    fn a_photo_outside_every_catalog_folder_writes_nothing() {
        let (mut app, photos) = disk_app("marks-outside");
        let elsewhere = photos.parent().unwrap().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("x.jpg"), b"").unwrap();
        app.catalog.remove_folder(&photos);
        load_folder(&mut app, &elsewhere);
        app.folder_store.set(&elsewhere.join("x.jpg"), 5);
        synced(&mut app);
        assert!(
            !app.catalog.shard_path(&elsewhere).exists(),
            "a folder the catalog does not list gets no shard"
        );
        assert!(
            !app.catalog
                .shard_path(&elsewhere)
                .parent()
                .unwrap()
                .exists(),
            "not even the marks directory"
        );
    }

    /// The sidecars are the truth; the catalog copy is rebuilt from them,
    /// so a mark made in one catalog is seen by another that lists the
    /// folder once it is visited there.
    #[test]
    fn switching_catalogs_projects_the_folder_into_the_one_left_and_the_one_entered() {
        let (mut app, photos) = disk_app("marks-switch");
        load_folder(&mut app, &photos);
        app.folder_store.set(&photos.join("a.jpg"), 4);
        let first = app.catalog.shard_path(&photos);

        let other = CatalogRef::in_dir(photos.parent().unwrap(), "other").unwrap();
        let other = Catalog::create_at(other, vec![photos.clone()]).unwrap();
        app.switch_catalog(other);
        assert!(first.exists(), "the catalog left behind got the rating");
        crate::app::test_support::wait_for_folder_store(&mut app);
        synced(&mut app);
        let want = marks(&[(
            "a.jpg",
            PhotoMarks {
                rating: Some(4),
                ..Default::default()
            },
        )]);
        assert_eq!(app.catalog.load_shard(&photos), Some(want));
    }
}
