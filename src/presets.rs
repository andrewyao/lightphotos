// SPDX-License-Identifier: GPL-3.0-or-later

//! The named-look library. A preset is the tone half of an edit under a name,
//! saved once and applied to any photo afterwards.
//!
//! Presets are global to the app, not per folder, so they live in
//! [`crate::prefs`] rather than in a catalog sidecar: `Catalog` is scoped to
//! one active directory and keys its records by filename.

use serde::{Deserialize, Serialize};

use crate::develop::Adjustments;

/// `prefs` key holding the serialized library.
const KEY: &str = "presets";

/// Envelope version. `Adjustments` needs none of its own, because every field
/// defaults and the worst case is one photo's edit. A preset library is
/// user-authored data with no second copy anywhere, so the envelope is the
/// cheap place for a later tone-block change to branch a migration.
const VERSION: u32 = 1;

/// A named look: the tone half of an edit, with no crop, rotation or touch-ups.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Preset {
    pub id: u64,
    pub name: String,
    pub adjustments: Adjustments,
    /// What an import could not represent, shown as the row's hover text. A
    /// status line lasts three seconds; this is the durable channel for it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The stored document. Unknown keys and a higher `version` still load, since
/// every field defaults.
#[derive(Serialize, Deserialize)]
struct Document {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    presets: Vec<Preset>,
}

/// The whole library, in display order.
pub struct PresetStore {
    /// Sorted by [`sort_key`], which is the order rows render in.
    presets: Vec<Preset>,
    /// `max(id) + 1`, computed at load. Ids are never reused, so a rename
    /// can't make an `ApplyPreset` act on a different look.
    next_id: u64,
    /// Set when the stored document would not parse. Every write is refused
    /// while it holds, so a corrupt but recoverable document is never
    /// overwritten with an empty library.
    corrupt: bool,
    /// Whether mutations are written back through `prefs`. False for a store a
    /// test built, which must never touch the real library.
    persistent: bool,
    last_error: Option<String>,
}

impl PresetStore {
    /// A library that is never written anywhere.
    pub fn in_memory() -> PresetStore {
        PresetStore {
            presets: Vec::new(),
            next_id: 1,
            corrupt: false,
            persistent: false,
            last_error: None,
        }
    }

    /// The saved library. A document that will not parse yields an empty list
    /// with every write refused, so the user can fix or move the file instead
    /// of losing it. `App::new` substitutes [`PresetStore::in_memory`] under
    /// test, so no test writes the developer's own library.
    #[cfg_attr(test, allow(dead_code))]
    pub fn load() -> PresetStore {
        let mut store = PresetStore {
            persistent: true,
            ..PresetStore::in_memory()
        };
        let Some(text) = crate::prefs::load(KEY) else {
            return store;
        };
        if text.trim().is_empty() {
            return store;
        }
        match parse(&text) {
            Ok(mut presets) => {
                store.next_id = presets.iter().map(|p| p.id + 1).max().unwrap_or(1);
                presets.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
                store.presets = presets;
            }
            Err(cause) => {
                store.corrupt = true;
                store.last_error = Some((crate::i18n::t().presets_load_failed)(&cause));
            }
        }
        store
    }

    /// Rows in display order.
    pub fn presets(&self) -> &[Preset] {
        &self.presets
    }

    pub fn get(&self, id: u64) -> Option<&Preset> {
        self.presets.iter().find(|p| p.id == id)
    }

    /// Adds a look under a free name, returning the stored name. `None` when
    /// the name is blank or the library is not writable.
    pub fn add(
        &mut self,
        name: &str,
        adjustments: Adjustments,
        notes: Vec<String>,
    ) -> Option<String> {
        let name = self.unique_name(name, None)?;
        if !self.writable() {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.presets.push(Preset {
            id,
            name: name.clone(),
            adjustments,
            notes,
        });
        self.presets.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        self.flush();
        Some(name)
    }

    /// Renames one preset, suffixing the name if another already has it. A
    /// blank name, an unknown id, or a non-writable library changes nothing.
    pub fn rename(&mut self, id: u64, name: &str) {
        let Some(name) = self.unique_name(name, Some(id)) else {
            return;
        };
        if !self.writable() {
            return;
        }
        let Some(p) = self.presets.iter_mut().find(|p| p.id == id) else {
            return;
        };
        p.name = name;
        self.presets.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        self.flush();
    }

    pub fn remove(&mut self, id: u64) {
        if !self.writable() {
            return;
        }
        let before = self.presets.len();
        self.presets.retain(|p| p.id != id);
        if self.presets.len() != before {
            self.flush();
        }
    }

    /// The latest failure, drained into a toast once per frame alongside the
    /// catalog's.
    pub fn take_error(&mut self) -> Option<String> {
        self.last_error.take()
    }

    /// `name`, or `name 2`, `name 3`, ... when another preset already has it.
    /// The collision idiom `paths::jpg_export_name` uses for export filenames,
    /// which beats a "name taken" error state and is the only policy that also
    /// works for a non-interactive import of thirty files at once. `keep` is
    /// the preset being renamed, which does not collide with itself.
    fn unique_name(&self, name: &str, keep: Option<u64>) -> Option<String> {
        let name = name.trim();
        if name.is_empty() {
            return None;
        }
        let free = |candidate: &str| {
            let key = candidate.to_lowercase();
            !self
                .presets
                .iter()
                .any(|p| Some(p.id) != keep && p.name.to_lowercase() == key)
        };
        if free(name) {
            return Some(name.to_string());
        }
        let mut n = 2u32;
        loop {
            let candidate = format!("{name} {n}");
            if free(&candidate) {
                return Some(candidate);
            }
            n += 1;
        }
    }

    fn writable(&mut self) -> bool {
        if self.corrupt {
            self.last_error = Some(crate::i18n::t().presets_locked.to_string());
            return false;
        }
        true
    }

    /// Writes the whole document. It is single-digit KB and every mutation is
    /// a single human click, so a full rewrite beats diffing and is idempotent.
    fn flush(&mut self) {
        if !self.persistent {
            return;
        }
        let document = Document {
            version: VERSION,
            presets: self.presets.clone(),
        };
        // A failure leaves the change in memory and reports itself. The next
        // mutation rewrites the whole document, which is the retry.
        let written = serde_json::to_string_pretty(&document)
            .map_err(|e| e.to_string())
            .and_then(|text| crate::prefs::save(KEY, &text));
        if let Err(cause) = written {
            self.last_error = Some((crate::i18n::t().presets_save_failed)(&cause));
        }
    }

    #[cfg(test)]
    fn document(&self) -> String {
        serde_json::to_string_pretty(&Document {
            version: VERSION,
            presets: self.presets.clone(),
        })
        .unwrap()
    }

    #[cfg(test)]
    fn from_document(text: &str) -> PresetStore {
        let mut store = PresetStore::in_memory();
        match parse(text) {
            Ok(mut presets) => {
                store.next_id = presets.iter().map(|p| p.id + 1).max().unwrap_or(1);
                presets.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
                store.presets = presets;
            }
            Err(cause) => {
                store.corrupt = true;
                store.last_error = Some((crate::i18n::t().presets_load_failed)(&cause));
            }
        }
        store
    }
}

fn parse(text: &str) -> Result<Vec<Preset>, String> {
    serde_json::from_str::<Document>(text)
        .map(|d| d.presets)
        .map_err(|e| e.to_string())
}

/// Rows render in case-insensitive name order, with the id breaking ties so
/// two presets of the same name keep a stable order.
fn sort_key(p: &Preset) -> (String, u64) {
    (p.name.to_lowercase(), p.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(exposure: f32) -> Adjustments {
        Adjustments {
            exposure,
            ..Default::default()
        }
    }

    #[test]
    fn a_saved_library_round_trips_with_identity_and_cjk_names() {
        let mut store = PresetStore::in_memory();
        store
            .add("\u{6696}\u{8c03}", tone(0.4), Vec::new())
            .unwrap();
        store
            .add(
                "Untouched",
                Adjustments::default(),
                vec!["no tone curve".into()],
            )
            .unwrap();

        let reloaded = PresetStore::from_document(&store.document());
        assert_eq!(reloaded.presets(), store.presets());
        let identity = reloaded
            .presets()
            .iter()
            .find(|p| p.name == "Untouched")
            .unwrap();
        assert!(
            identity.adjustments.is_identity(),
            "an identity preset survives the round trip"
        );
        assert_eq!(identity.notes, ["no tone curve"]);
        let cjk = reloaded.presets().iter().find(|p| p.id == 1).unwrap();
        assert_eq!(cjk.name, "\u{6696}\u{8c03}");
        assert_eq!(cjk.adjustments.exposure, 0.4);
    }

    #[test]
    fn a_corrupt_document_loads_empty_and_refuses_every_write() {
        let mut store = PresetStore::from_document("{\"presets\": [ truncated");
        assert!(store.presets().is_empty());
        assert!(store.take_error().is_some(), "the failure is reported");

        assert_eq!(store.add("New", tone(1.0), Vec::new()), None);
        assert!(store.take_error().is_some(), "the refusal is reported");
        store.rename(1, "Other");
        store.remove(1);
        // Every mutation checks `writable` before `flush`, so refusing the
        // mutation is what keeps the corrupt document from being replaced by an
        // empty one.
        assert!(store.presets().is_empty(), "no mutation lands");
    }

    #[test]
    fn an_unknown_key_and_a_future_version_still_load() {
        let store = PresetStore::from_document(
            "{\"version\": 9, \"mood\": \"golden\", \"presets\": [\
             {\"id\": 3, \"name\": \"Golden\", \"adjustments\": {\"exposure\": 0.5}, \"future\": 1}]}",
        );
        assert_eq!(store.presets().len(), 1);
        assert_eq!(store.presets()[0].adjustments.exposure, 0.5);
        assert!(store.presets()[0].notes.is_empty());
        assert_eq!(store.next_id, 4, "next_id follows the highest stored id");
    }

    #[test]
    fn a_repeated_name_is_suffixed_each_time() {
        let mut store = PresetStore::in_memory();
        assert_eq!(
            store.add("Golden", tone(0.1), vec![]).as_deref(),
            Some("Golden")
        );
        assert_eq!(
            store.add("Golden", tone(0.2), vec![]).as_deref(),
            Some("Golden 2")
        );
        assert_eq!(
            store.add("golden", tone(0.3), vec![]).as_deref(),
            Some("golden 3"),
            "collisions ignore case, since rows sort that way"
        );
        assert_eq!(
            store.add("  Golden  ", tone(0.4), vec![]).as_deref(),
            Some("Golden 4"),
            "the name is trimmed before it is compared"
        );
        assert_eq!(
            store.add("   ", tone(0.5), vec![]),
            None,
            "a blank name is refused"
        );
        assert_eq!(store.presets().len(), 4);
    }

    #[test]
    fn next_id_survives_deleting_the_highest_id() {
        let mut store = PresetStore::in_memory();
        store.add("A", tone(0.1), vec![]).unwrap();
        store.add("B", tone(0.2), vec![]).unwrap();
        let b = store.presets().iter().find(|p| p.name == "B").unwrap().id;
        store.remove(b);
        store.add("C", tone(0.3), vec![]).unwrap();
        let c = store.presets().iter().find(|p| p.name == "C").unwrap().id;
        assert_ne!(c, b, "a deleted id is not handed out again");
    }

    #[test]
    fn insertion_and_rename_keep_case_insensitive_name_order() {
        let mut store = PresetStore::in_memory();
        for name in ["zebra", "Alpha", "mango"] {
            store.add(name, tone(0.1), vec![]).unwrap();
        }
        assert_eq!(names(&store), ["Alpha", "mango", "zebra"]);

        let zebra = store
            .presets()
            .iter()
            .find(|p| p.name == "zebra")
            .unwrap()
            .id;
        store.rename(zebra, "beta");
        assert_eq!(names(&store), ["Alpha", "beta", "mango"]);

        store.rename(zebra, "   ");
        assert_eq!(
            names(&store),
            ["Alpha", "beta", "mango"],
            "a blank rename is refused"
        );

        store.rename(zebra, "beta");
        assert_eq!(
            names(&store),
            ["Alpha", "beta", "mango"],
            "renaming a preset to its own name is not a collision"
        );

        store.rename(zebra, "Mango");
        assert_eq!(names(&store), ["Alpha", "mango", "Mango 2"]);
    }

    fn names(store: &PresetStore) -> Vec<String> {
        store.presets().iter().map(|p| p.name.clone()).collect()
    }
}
