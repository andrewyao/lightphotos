// SPDX-License-Identifier: MIT OR Apache-2.0

//! The browser's catalog, until an IndexedDB store exists: one in-memory
//! catalog named `default` that is never written. Roots still come from the
//! kept folder handles in `web_fs`. The store that replaces this fills the
//! same four functions and hands the loaded record to `load_record`.

use std::path::Path;

use super::{Catalog, CatalogRef, DEFAULT_NAME};

impl Catalog {
    /// The one catalog the browser has.
    pub(crate) fn open_default() -> Catalog {
        let at = CatalogRef {
            name: DEFAULT_NAME.to_string(),
        };
        let mut catalog = Catalog::empty(at);
        catalog.folders = super::Record::Pending(Vec::new());
        catalog.load_record(Vec::new());
        catalog
    }

    pub(super) fn save_record(&mut self) {}

    pub(super) fn put_shard(&mut self, _dir: &Path, _bytes: Option<Vec<u8>>) {}

    pub(crate) fn pump(&mut self) {}

    pub(crate) fn backlog(&self) -> usize {
        0
    }
}
