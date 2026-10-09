// SPDX-License-Identifier: MIT OR Apache-2.0

//! One sidecar per photo group, `<dir>/.lightphotos/groups/<id>.json`, so a
//! group's members and representative change together in one atomic write.
//! The body is `{"v":1,"members":[...],"representative":"..."}`, with no
//! `representative` while none is chosen.

use std::path::{Path, PathBuf};

use crate::persist::groups::{Group, GroupId, SavedGroup};

pub(crate) const GROUPS_DIR: &str = "groups";
const EXT: &str = "json";
const FORMAT: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct GroupFile {
    v: u32,
    members: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    representative: Option<String>,
}

pub(super) fn path(dir: &Path, id: &GroupId) -> PathBuf {
    dir.join(super::SIDECAR_DIR)
        .join(GROUPS_DIR)
        .join(format!("{id}.{EXT}"))
}

/// The id a file named `name` in `groups/` holds, or `None` for a file that
/// is not a group sidecar.
pub(crate) fn id_of(name: &str) -> Option<GroupId> {
    let stem = name.strip_suffix(&format!(".{EXT}"))?;
    GroupId::from_stem(stem.as_ref())
}

/// Only malformed JSON or an unknown version fails. What the members say is
/// checked against the folder later, by `Groups::from_loaded`.
pub(crate) fn parse(bytes: &[u8]) -> Result<SavedGroup, String> {
    let file: GroupFile = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    if file.v != FORMAT {
        return Err(format!("unknown group format version {}", file.v));
    }
    Ok(SavedGroup {
        members: file.members.into_iter().map(Into::into).collect(),
        rep: file.representative.map(Into::into),
    })
}

/// The sidecar body for `group`. `Group::new` admits only UTF-8 names, so
/// the conversion to JSON strings loses nothing.
pub(super) fn to_bytes(group: &Group) -> Result<Vec<u8>, String> {
    let name = |n: &std::ffi::OsString| n.to_string_lossy().into_owned();
    let file = GroupFile {
        v: FORMAT,
        members: group.members().iter().map(name).collect(),
        representative: group.rep().map(name),
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
        let Some(id) = path.file_name().and_then(|n| n.to_str()).and_then(id_of) else {
            eprintln!(
                "[catalog] unreadable group {}: not a group id",
                path.display()
            );
            *skipped += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_json_file_with_a_safe_stem_is_a_group() {
        assert_eq!(
            id_of("g-fixture00001.json").map(|id| id.to_string()),
            Some("g-fixture00001".into())
        );
        assert!(id_of("g-fixture00001.xmp").is_none(), "another extension");
        assert!(id_of("g-1.json.tmp").is_none(), "a swap file");
        assert!(id_of("../g-1.json").is_none(), "a stem that leaves groups/");
        assert!(id_of(".json").is_none(), "an empty stem");
    }
}
