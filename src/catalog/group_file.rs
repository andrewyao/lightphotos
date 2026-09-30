// SPDX-License-Identifier: GPL-3.0-or-later

//! One sidecar per photo group, `<dir>/.lightphotos/groups/<id>.json`, so a
//! group's members and representative change together in one atomic write.
//! The body is `{"v":1,"members":[...],"representative":"..."}`.

use std::path::{Path, PathBuf};

use crate::groups::GroupId;
#[cfg(not(target_arch = "wasm32"))]
use crate::groups::{Group, SavedGroup};

pub(super) const GROUPS_DIR: &str = "groups";
const EXT: &str = "json";
#[cfg(not(target_arch = "wasm32"))]
const FORMAT: u32 = 1;

#[cfg(not(target_arch = "wasm32"))]
#[derive(serde::Serialize, serde::Deserialize)]
struct GroupFile {
    v: u32,
    members: Vec<String>,
    representative: String,
}

pub(super) fn path(dir: &Path, id: &GroupId) -> PathBuf {
    dir.join(super::SIDECAR_DIR)
        .join(GROUPS_DIR)
        .join(format!("{id}.{EXT}"))
}

/// Only malformed JSON or an unknown version fails. What the members say is
/// checked against the folder later, by `Groups::from_loaded`.
#[cfg(not(target_arch = "wasm32"))]
fn parse(bytes: &[u8]) -> Result<SavedGroup, String> {
    let file: GroupFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if file.v != FORMAT {
        return Err(format!("unknown group format version {}", file.v));
    }
    Ok(SavedGroup {
        members: file.members.into_iter().map(Into::into).collect(),
        rep: file.representative.into(),
    })
}

/// The sidecar body for `group`. `Group::new` admits only UTF-8 names, so
/// the conversion to JSON strings loses nothing.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn to_bytes(group: &Group) -> Result<Vec<u8>, String> {
    let name = |n: &std::ffi::OsString| n.to_string_lossy().into_owned();
    let file = GroupFile {
        v: FORMAT,
        members: group.members().iter().map(name).collect(),
        representative: name(group.rep()),
    };
    serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())
}

/// Every parseable group sidecar in `dir`. `skipped` counts the rest. A
/// missing `groups/` gives nothing.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load(dir: &Path, skipped: &mut usize) -> Vec<(GroupId, SavedGroup)> {
    let mut files = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir.join(super::SIDECAR_DIR).join(GROUPS_DIR)) else {
        return files;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.extension() != Some(std::ffi::OsStr::new(EXT))
            || !entry.file_type().is_ok_and(|t| t.is_file())
        {
            continue;
        }
        let Some(id) = path.file_stem().and_then(GroupId::from_stem) else {
            continue;
        };
        let parsed = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| parse(&bytes));
        match parsed {
            Ok(saved) => files.push((id, saved)),
            Err(e) => {
                eprintln!("[catalog] unreadable group {}: {e}", path.display());
                *skipped += 1;
            }
        }
    }
    files
}
