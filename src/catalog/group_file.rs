// SPDX-License-Identifier: GPL-3.0-or-later

//! One sidecar per photo group, `<dir>/.lightphotos/groups/<id>.json`, so a
//! group's members and representative change together in one atomic write.
//! The body is `{"v":1,"members":[...],"representative":"..."}`.

use std::path::{Path, PathBuf};

use crate::groups::GroupId;
#[cfg(not(target_arch = "wasm32"))]
use crate::groups::{Group, Groups};

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

#[cfg(not(target_arch = "wasm32"))]
fn parse(bytes: &[u8]) -> Result<Group, String> {
    let file: GroupFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if file.v != FORMAT {
        return Err(format!("unknown group format version {}", file.v));
    }
    let members = file.members.into_iter().map(Into::into).collect();
    Group::new(members, file.representative.into()).ok_or_else(|| {
        "a group needs two or more distinct members, one of them the representative".to_string()
    })
}

/// The sidecar body for `group`. JSON strings are UTF-8, so a member whose
/// name is not fails the write instead of being saved under a lossy name.
#[cfg(not(target_arch = "wasm32"))]
pub(super) fn to_bytes(group: &Group) -> Result<Vec<u8>, String> {
    let name = |n: &std::ffi::OsString| {
        n.to_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("{} is not a UTF-8 file name", n.to_string_lossy()))
    };
    let file = GroupFile {
        v: FORMAT,
        members: group.members().iter().map(name).collect::<Result<_, _>>()?,
        representative: name(group.rep())?,
    };
    serde_json::to_vec_pretty(&file).map_err(|e| e.to_string())
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn load(dir: &Path, skipped: &mut usize) -> Groups {
    let Ok(entries) = std::fs::read_dir(dir.join(super::SIDECAR_DIR).join(GROUPS_DIR)) else {
        return Groups::default();
    };
    let mut files = Vec::new();
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
            Ok(group) => files.push((id, group)),
            Err(e) => {
                eprintln!("[catalog] unreadable group {}: {e}", path.display());
                *skipped += 1;
            }
        }
    }
    if files.is_empty() {
        return Groups::default();
    }
    let listing: std::collections::HashSet<std::ffi::OsString> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(|e| e.ok()).map(|e| e.file_name()).collect())
        .unwrap_or_default();
    Groups::from_loaded(files, &listing)
}
