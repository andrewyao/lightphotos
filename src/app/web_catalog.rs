//! wasm32-only: the browser's catalogs from the App's side. Which one is
//! open, reading its record from IndexedDB, making one by name and
//! switching. The record is async, so a catalog opens `Pending`: root
//! changes merge in memory, and the session restore waits for the record.
//!
//! A catalog lists folders by kept name. Its roots that this browser still
//! keeps a handle for open as usual; the rest show as missing, like a native
//! root on an unplugged drive.

use std::sync::mpsc::{Receiver, Sender};

use super::*;
use crate::persist::catalog::{web_fallback, Catalog, CatalogRef};
use crate::web::{web_catalog_idb as idb, web_fs};

/// What opening a catalog read, all at once so its roots and missing roots
/// land together.
pub(super) struct CatalogLoad {
    generation: u64,
    record: Result<Option<Vec<u8>>, String>,
    /// The kept folder handles' names.
    kept: Result<Vec<PathBuf>, String>,
    /// Every stored catalog's name.
    names: Result<Vec<String>, String>,
    /// The catalog a switch left, to go back to if this one won't load.
    /// `None` for a launch, which falls back to the default instead, and
    /// whose missing record is made from `kept`: the first visit since
    /// catalogs, whose folders were the kept ones.
    came_from: Option<CatalogRef>,
}

pub(super) struct State {
    tx: Sender<CatalogLoad>,
    rx: Receiver<CatalogLoad>,
    /// Counts the loads. A switch drops a slower load's result.
    generation: u64,
    /// Every catalog in this browser, by name. `None` until the first load
    /// lists them, which New Catalog waits for so it never writes over one.
    known: Option<Vec<CatalogRef>>,
    /// The session restore asked before the record landed.
    restore_waiting: bool,
    /// New Catalog's name prompt, while it is open.
    name_edit: Option<String>,
    /// Roots removed while the record was out. Whether their kept handles
    /// go is decided against the landed record.
    forget_after_load: Vec<PathBuf>,
}

impl State {
    pub(super) fn new() -> State {
        let (tx, rx) = std::sync::mpsc::channel();
        State {
            tx,
            rx,
            generation: 0,
            known: None,
            restore_waiting: false,
            name_edit: None,
            forget_after_load: Vec::new(),
        }
    }
}

impl App {
    /// Read the open catalog's record, the kept folders and the catalog
    /// list. `came_from` is the catalog to go back to if it won't load;
    /// `None` at launch.
    pub(crate) fn start_catalog_load(&mut self, came_from: Option<CatalogRef>) {
        let state = &mut self.web.catalogs;
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        let tx = state.tx.clone();
        let name = self.catalog.name().to_string();
        wasm_bindgen_futures::spawn_local(async move {
            let record = idb::get_record(&name).await;
            let kept = web_fs::saved_folder_names().await;
            let names = idb::catalog_names().await;
            let _ = tx.send(CatalogLoad {
                generation,
                record,
                kept,
                names,
                came_from,
            });
        });
    }

    /// Land a finished load. Returns true while the record is still out.
    pub(super) fn poll_catalog_load(&mut self) -> bool {
        while let Ok(load) = self.web.catalogs.rx.try_recv() {
            if load.generation == self.web.catalogs.generation {
                self.land_catalog(load);
            }
        }
        self.catalog.loading()
    }

    fn land_catalog(&mut self, load: CatalogLoad) {
        let kept = load.kept.unwrap_or_else(|e| {
            web_sys::console::warn_1(
                &format!("[catalog] could not read the kept folders: {e}").into(),
            );
            Vec::new()
        });
        match load.names {
            Ok(names) => {
                let known = names
                    .iter()
                    // A key that isn't a name this build would make stays
                    // unlisted; switching to it would make a new catalog.
                    .filter_map(|n| CatalogRef::named(n).ok().filter(|at| at.name() == n))
                    .collect();
                self.web.catalogs.known = Some(known);
            }
            Err(e) => web_sys::console::warn_1(
                &format!("[catalog] could not list the catalogs: {e}").into(),
            ),
        }
        let launch = load.came_from.is_none();
        let seed = if launch { kept.as_slice() } else { &[] };
        let removed = std::mem::take(&mut self.web.catalogs.forget_after_load);
        match self.catalog.land_record(load.record, seed) {
            Ok(()) => {
                for root in removed {
                    self.forget_kept_handle(&root);
                }
            }
            Err(e) => {
                let t = crate::i18n::t();
                let toast = if launch {
                    t.catalog_fallback
                } else {
                    t.catalog_open_failed
                };
                self.set_status(StatusKind::Error, toast(&e));
                let came_from = load.came_from.as_ref().map(CatalogRef::name);
                let back = web_fallback(self.catalog.name(), came_from)
                    .and_then(|name| CatalogRef::named(name).ok());
                if let Some(back) = back {
                    // A launch leaves the pref naming the last catalog, as
                    // natively, so it opens again once it can. A switch
                    // never happened, so the pref goes back too.
                    if !launch {
                        self.remember_web_catalog(back.name());
                    }
                    self.enter_web_catalog(Catalog::pending(back), None);
                    return;
                }
            }
        }
        self.note_known_catalog();
        // A root picked while the record was out has handles before its
        // kept name reaches `kept`.
        self.missing_roots = self
            .catalog
            .folders()
            .iter()
            .filter(|f| !kept.contains(f) && !self.web_root_allowed(f))
            .cloned()
            .collect();
        if std::mem::take(&mut self.web.catalogs.restore_waiting) {
            self.restore_last_session();
        }
        self.request_redraw();
    }

    /// Stop keeping `root`'s handle unless a catalog still lists it. While
    /// the record is out it may list `root`, so the check waits for it.
    pub(super) fn forget_kept_handle(&mut self, root: &Path) {
        if self.catalog.loading() {
            self.web.catalogs.forget_after_load.push(root.to_path_buf());
            return;
        }
        if self.catalog.folders().iter().any(|f| f == root) {
            return;
        }
        wasm_bindgen_futures::spawn_local(idb::forget_folder_if_unlisted(
            self.catalog.name().to_string(),
            root.to_path_buf(),
        ));
    }

    /// Hold the session restore until the record lands. True when it waits.
    pub(super) fn restore_waits_for_catalog(&mut self) -> bool {
        let waits = self.catalog.loading();
        self.web.catalogs.restore_waiting |= waits;
        waits
    }

    /// List the open catalog among the known ones, sorted by name.
    fn note_known_catalog(&mut self) {
        let at = self.catalog.at().clone();
        if let Some(known) = &mut self.web.catalogs.known {
            if !known.contains(&at) {
                known.push(at);
            }
            known.sort_by_cached_key(|c| c.name().to_lowercase());
        }
    }

    /// Every catalog in this browser, for the Folders heading.
    pub(super) fn known_catalogs(&self) -> &[CatalogRef] {
        self.web.catalogs.known.as_deref().unwrap_or(&[])
    }

    /// Leave the open catalog for `catalog`, and save it as the one to
    /// reopen. If it won't load, the one left opens again.
    fn switch_web_catalog(&mut self, catalog: Catalog) {
        let left = self.catalog.at().clone();
        self.remember_web_catalog(catalog.name());
        self.enter_web_catalog(catalog, Some(left));
    }

    /// Leave the folder showing and make `catalog` the open one. Its roots
    /// and the session restore follow once its record lands.
    fn enter_web_catalog(&mut self, catalog: Catalog, came_from: Option<CatalogRef>) {
        self.sync_marks();
        self.close_folder();
        self.supersede_web_pending_nav();
        self.catalog = catalog;
        self.web.catalogs.forget_after_load.clear();
        self.expanded.clear();
        self.subdirs.clear();
        self.missing_roots.clear();
        self.note_known_catalog();
        if self.catalog.loading() {
            self.start_catalog_load(came_from);
        }
        self.restore_last_session();
        self.request_redraw();
    }

    fn remember_web_catalog(&self, name: &str) {
        if let Err(e) = crate::persist::prefs::save(super::catalog::CATALOG_PREF, name) {
            web_sys::console::warn_1(
                &format!("[catalog] could not save the catalog pref: {e}").into(),
            );
        }
    }

    /// Switch to the catalog `at` from the Folders heading.
    pub(super) fn open_catalog(&mut self, at: CatalogRef) {
        if &at != self.catalog.at() {
            self.switch_web_catalog(Catalog::pending(at));
        }
    }

    /// `catalog.new`: on the web the path is the new catalog's name.
    pub(super) fn create_catalog_at(&mut self, path: &Path) -> Result<(), String> {
        let name = path.to_str().ok_or("a catalog name must be text")?;
        self.create_catalog_named(name)
    }

    /// Make an empty catalog called `name` and switch to it. Refused while
    /// the catalog list is still loading, so it never writes over one.
    fn create_catalog_named(&mut self, name: &str) -> Result<(), String> {
        let at = CatalogRef::named(name)?;
        let known = self
            .web
            .catalogs
            .known
            .as_ref()
            .ok_or("the catalogs are still loading")?;
        if known.contains(&at) {
            return Err(format!("a catalog named {} already exists", at.name()));
        }
        let mut catalog = Catalog::pending(at);
        catalog.land_record(Ok(None), &[])?;
        self.switch_web_catalog(catalog);
        Ok(())
    }

    pub(super) fn open_catalog_at(&mut self, _path: &Path) -> Result<(), String> {
        Err("the browser keeps its catalogs inside the browser, by name: use catalog.switch".into())
    }

    /// `catalog.switch`: a catalog in this browser, by name.
    pub(super) fn switch_catalog_named(&mut self, name: &str) -> Result<(), String> {
        let at = CatalogRef::named(name)?;
        if &at == self.catalog.at() {
            return Ok(());
        }
        if !self.known_catalogs().contains(&at) {
            return Err(format!("this browser has no catalog named {}", at.name()));
        }
        self.switch_web_catalog(Catalog::pending(at));
        Ok(())
    }

    /// New Catalog…: the name prompt.
    pub(super) fn new_catalog_dialog(&mut self) {
        self.web.catalogs.name_edit = Some(String::new());
        self.request_redraw();
    }

    pub(crate) fn catalog_name_edit(&self) -> Option<&str> {
        self.web.catalogs.name_edit.as_deref()
    }

    pub(super) fn set_catalog_name_text(&mut self, text: String) {
        if let Some(buffer) = &mut self.web.catalogs.name_edit {
            *buffer = text;
        }
        self.request_redraw();
    }

    /// A blank name leaves the prompt open, as the preset prompt does. A
    /// refused name keeps it open too, with the reason.
    pub(super) fn commit_catalog_name(&mut self) {
        let Some(name) = self.web.catalogs.name_edit.clone() else {
            return;
        };
        if name.trim().is_empty() {
            return;
        }
        match self.create_catalog_named(&name) {
            Ok(()) => self.web.catalogs.name_edit = None,
            Err(e) => self.set_status(
                StatusKind::Error,
                (crate::i18n::t().catalog_create_failed)(&e),
            ),
        }
        self.request_redraw();
    }

    pub(super) fn cancel_catalog_name(&mut self) {
        self.web.catalogs.name_edit = None;
        self.request_redraw();
    }
}
