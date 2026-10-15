// SPDX-License-Identifier: MIT OR Apache-2.0

//! The browser's catalogs, in IndexedDB through
//! [`crate::web::web_catalog_idb`]: a record per catalog and a marks shard
//! per photo directory. The app reads the record and hands it to
//! `land_record`; this side writes.
//!
//! The page can close at any moment with no exit hook, so each write starts
//! at once. Only a write queued behind one already out for the same key
//! waits, and only the newest such write is kept. The queue is the page's,
//! not the catalog's, so a switch to another catalog drops none of the
//! writes still out.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::Path;

use super::{Catalog, CatalogRef, Record};
use crate::web::web_catalog_idb::{self as idb, Store};

thread_local! {
    static WRITES: RefCell<Writes> = RefCell::new(Writes::default());
}

/// A stored value: the object store and its key.
type Key = (Store, String);

#[derive(Default)]
struct Writes {
    /// Keys with a write out.
    out: HashSet<Key>,
    /// The newest write waiting on each key in `out`: the JSON to put, or
    /// `None` to delete.
    next: HashMap<Key, Option<String>>,
    /// Failed writes since the last `pump`.
    failed: Vec<String>,
}

/// Put `json` at `key`, or delete it on `None`, now or once the write out
/// for `key` lands.
fn write(key: Key, json: Option<String>) {
    let start = WRITES.with_borrow_mut(|w| {
        if w.out.contains(&key) {
            w.next.insert(key.clone(), json.clone());
            false
        } else {
            w.out.insert(key.clone());
            true
        }
    });
    if start {
        wasm_bindgen_futures::spawn_local(drive(key, json));
    }
}

/// Write `key`, then each newer value queued for it while that ran.
async fn drive(key: Key, mut json: Option<String>) {
    loop {
        let (store, id) = (key.0, key.1.as_str());
        let result = match &json {
            Some(json) => idb::put(store, id, json).await,
            None => idb::delete(store, id).await,
        };
        let next = WRITES.with_borrow_mut(|w| {
            if let Err(e) = result {
                w.failed.push(e);
            }
            let next = w.next.remove(&key);
            if next.is_none() {
                w.out.remove(&key);
            }
            next
        });
        match next {
            Some(newer) => json = newer,
            None => return,
        }
    }
}

/// The catalogs store's key for catalog `name`'s preset folders. A catalog
/// name holds no NUL, so no catalog is ever called this, and builds that
/// list the store's keys as catalogs pass over it.
fn presets_key(name: &str) -> String {
    format!("presets\0{name}")
}

/// The marks store's key for `dir` in catalog `name`. A catalog name holds
/// no NUL, so the split is unambiguous.
fn shard_key(name: &str, dir: &Path) -> String {
    format!("{name}\0{}", dir.to_string_lossy())
}

impl Catalog {
    /// The catalog `at`, its record still to come.
    pub(crate) fn pending(at: CatalogRef) -> Catalog {
        let mut catalog = Catalog::empty(at);
        catalog.folders = Record::pending();
        catalog.preset_folders = Record::pending();
        catalog
    }

    pub(super) fn save_record(&mut self) {
        let Record::Loaded(folders) = &self.folders else {
            return;
        };
        match super::record_bytes(folders).and_then(utf8) {
            Ok(json) => write((Store::Catalogs, self.name().to_string()), Some(json)),
            Err(e) => self.note_error(e),
        }
    }

    pub(super) fn save_presets(&mut self) {
        let Record::Loaded(folders) = &self.preset_folders else {
            return;
        };
        match super::record_bytes(folders).and_then(utf8) {
            Ok(json) => write((Store::Catalogs, presets_key(self.name())), Some(json)),
            Err(e) => self.note_error(e),
        }
    }

    pub(super) fn put_shard(&mut self, dir: &Path, bytes: Option<Vec<u8>>) {
        match bytes.map(utf8).transpose() {
            Ok(json) => write((Store::Marks, shard_key(self.name(), dir)), json),
            Err(e) => self.note_error(e),
        }
    }

    /// Surface failed writes. Once per frame.
    pub(crate) fn pump(&mut self) {
        for e in WRITES.with_borrow_mut(|w| std::mem::take(&mut w.failed)) {
            self.note_error(e);
        }
    }

    /// Values not yet stored.
    pub(crate) fn backlog(&self) -> usize {
        WRITES.with_borrow(|w| w.out.len())
    }
}

fn utf8(bytes: Vec<u8>) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|e| e.to_string())
}
