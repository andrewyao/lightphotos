// SPDX-License-Identifier: MIT OR Apache-2.0

//! Lightroom presets read in place from folders the user keeps them in. The
//! files are never copied into the library; each scan reads them again, so a
//! preset edited or added in Lightroom shows up on the next one.
//!
//! The rules both walkers share live here as pure functions. The native
//! walker is [`scan`]; the browser's walks `web_fs` asynchronously and feeds
//! the same [`read_preset`].

use std::path::{Path, PathBuf};

use crate::develop::Adjustments;
use crate::persist::lr_preset::{self, Imported};

/// One preset found under a preset folder.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FolderPreset {
    pub path: PathBuf,
    pub group: String,
    pub name: String,
    pub adj: Adjustments,
    /// What the preset sets that has no slider here, shown on hover.
    pub notes: Vec<String>,
}

/// Bounds on one folder's scan, so a folder pointed at the wrong place (a
/// home directory, a photo archive) costs a bounded walk.
pub(crate) struct Limits {
    /// Directories deeper than this under the root are not read.
    pub depth: usize,
    /// The scan stops after this many preset files.
    pub files: usize,
    /// A larger `.xmp` is skipped unread. Real presets are a few KB.
    pub file_bytes: u64,
}

pub(crate) const LIMITS: Limits = Limits {
    depth: 8,
    files: 5000,
    file_bytes: 1 << 20,
};

/// What one folder held.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Scan {
    /// In group order, then name order.
    pub presets: Vec<FolderPreset>,
    /// `.xmp` files that were too big or did not read as a develop preset.
    pub skipped: usize,
}

impl Scan {
    pub(crate) fn sort(&mut self) {
        self.presets.sort_by_cached_key(|p| {
            (
                p.group.to_lowercase(),
                p.name.to_lowercase(),
                p.path.clone(),
            )
        });
    }
}

/// Whether a file name is a preset, `.xmp` in any case.
pub(crate) fn is_preset_file(name: &str) -> bool {
    Path::new(name)
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("xmp"))
}

/// Dot-folders and catalogs hold no presets worth listing.
pub(crate) fn skips_dir(name: &str) -> bool {
    name.starts_with('.')
        || Path::new(name)
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lightphotos"))
}

/// The preset in `bytes`, found at `rel` under `root`, or `None` when it is
/// not a develop preset. Grouped by its `crs:Group`, else the folder it sits
/// in under the root, else the root's own name.
pub(crate) fn read_preset(root: &Path, rel: &Path, bytes: &[u8]) -> Option<FolderPreset> {
    let imported = lr_preset::parse(std::str::from_utf8(bytes).ok()?).ok()?;
    let notes = notes(&imported);
    let folder = rel
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/")
        })
        .filter(|g| !g.is_empty());
    let group = imported
        .group
        .or(folder)
        .unwrap_or_else(|| display_name(root));
    let name = imported.name.unwrap_or_else(|| {
        rel.file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    });
    Some(FolderPreset {
        path: root.join(rel),
        group,
        name,
        adj: imported.adj,
        notes,
    })
}

/// A folder's last component, or the whole path for a root like `/`.
pub(crate) fn display_name(folder: &Path) -> String {
    folder.file_name().map_or_else(
        || folder.display().to_string(),
        |n| n.to_string_lossy().into_owned(),
    )
}

/// The hover notes for what an imported preset could not carry.
pub(crate) fn notes(imported: &Imported) -> Vec<String> {
    let t = crate::i18n::t();
    let mut notes = Vec::new();
    if !imported.dropped.is_empty() {
        notes.push((t.preset_import_note)(&imported.dropped.join(", ")));
    }
    if imported.monochrome {
        notes.push(t.preset_monochrome_note.to_string());
    }
    notes
}

/// Walks `root` for presets. `Err` only when the root itself cannot be read;
/// a bad file or subfolder below it is skipped, never an error. Symlink loops
/// are cut by remembering each directory's canonical path.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn scan(root: &Path, limits: &Limits) -> Result<Scan, String> {
    use std::collections::{HashSet, VecDeque};

    std::fs::read_dir(root).map_err(|e| e.to_string())?;
    let mut out = Scan::default();
    let mut seen = HashSet::new();
    let mut files = 0usize;
    // Breadth first, in name order, so a folder reached two ways is grouped
    // by its shallowest path every time.
    let mut queue = VecDeque::from([(root.to_path_buf(), 0usize)]);
    'walk: while let Some((dir, depth)) = queue.pop_front() {
        let Ok(canonical) = std::fs::canonicalize(&dir) else {
            continue;
        };
        if !seen.insert(canonical) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut entries: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        entries.sort();
        for path in entries {
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            // `metadata` follows a symlink, so a linked folder is walked and
            // the seen-set stops it coming round again.
            let Ok(meta) = std::fs::metadata(&path) else {
                continue;
            };
            if meta.is_dir() {
                if depth < limits.depth && !skips_dir(&name) {
                    queue.push_back((path, depth.saturating_add(1)));
                }
                continue;
            }
            if !is_preset_file(&name) {
                continue;
            }
            if files >= limits.files {
                break 'walk;
            }
            files = files.saturating_add(1);
            let preset = read_capped(&path, limits.file_bytes).and_then(|bytes| {
                let rel = path.strip_prefix(root).ok()?;
                read_preset(root, rel, &bytes)
            });
            match preset {
                Some(p) => out.presets.push(p),
                None => out.skipped = out.skipped.saturating_add(1),
            }
        }
    }
    out.sort();
    Ok(out)
}

/// The file's bytes, or `None` when it is unreadable or over `max`.
#[cfg(not(target_arch = "wasm32"))]
fn read_capped(path: &Path, max: u64) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(max.saturating_add(1))
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() as u64 <= max).then_some(bytes)
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::app::test_support::temp_folder;

    fn preset(name: Option<&str>, group: Option<&str>, exposure: &str) -> String {
        let name = name.map_or(String::new(), |n| {
            format!("<crs:Name><rdf:Alt><rdf:li xml:lang=\"x-default\">{n}</rdf:li></rdf:Alt></crs:Name>")
        });
        let group = group.map_or(String::new(), |g| {
            format!("<crs:Group><rdf:Alt><rdf:li xml:lang=\"x-default\">{g}</rdf:li></rdf:Alt></crs:Group>")
        });
        format!(
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\">\
             <rdf:RDF xmlns:rdf=\"http://www.w3.org/1999/02/22-rdf-syntax-ns#\">\
             <rdf:Description rdf:about=\"\" \
             xmlns:crs=\"http://ns.adobe.com/camera-raw-settings/1.0/\" \
             crs:PresetType=\"Normal\" crs:Exposure2012=\"{exposure}\">\
             {name}{group}</rdf:Description></rdf:RDF></x:xmpmeta>"
        )
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn listed(scan: &Scan) -> Vec<(&str, &str)> {
        scan.presets
            .iter()
            .map(|p| (p.group.as_str(), p.name.as_str()))
            .collect()
    }

    /// Catches a group taken from the wrong place, a name with no stem
    /// fallback, and an unstable order.
    #[test]
    fn presets_group_by_crs_group_then_subfolder_then_root() {
        let root = temp_folder("presets-groups").join("Root");
        write(&root, "Warm.xmp", &preset(Some("Warm"), None, "+0.3"));
        write(&root, "Film/Kodak.xmp", &preset(None, None, "+0.1"));
        write(
            &root,
            "Film/Color/Fuji.xmp",
            &preset(Some("Fuji"), None, "0"),
        );
        write(
            &root,
            "Misc/Night.xmp",
            &preset(Some("Night"), Some("Moody Looks"), "-1"),
        );

        let scan = scan(&root, &LIMITS).unwrap();

        assert_eq!(
            listed(&scan),
            [
                ("Film", "Kodak"),
                ("Film/Color", "Fuji"),
                ("Moody Looks", "Night"),
                ("Root", "Warm"),
            ]
        );
        assert_eq!(scan.skipped, 0);
        let warm = scan.presets.iter().find(|p| p.name == "Warm").unwrap();
        assert_eq!(warm.adj.exposure, 0.3);
        assert_eq!(warm.path, root.join("Warm.xmp"));
        let _ = std::fs::remove_dir_all(root.parent().unwrap());
    }

    /// Catches a walk that skips upper-case extensions, reads non-preset
    /// files, or dies on one bad file instead of counting it.
    #[test]
    fn bad_files_are_counted_and_the_rest_still_read() {
        let root = temp_folder("presets-bad");
        write(&root, "LOUD.XMP", &preset(Some("Loud"), None, "+1"));
        write(&root, "notes.txt", &preset(Some("Not a preset"), None, "0"));
        write(
            &root,
            "broken.xmp",
            "<x:xmpmeta xmlns:x=\"adobe:ns:meta/\"><rdf:RD",
        );
        write(&root, "binary.xmp", "\u{0}\u{1}\u{2}");
        std::fs::write(root.join("latin1.xmp"), [0xff, 0xfe, 0x00, 0x41]).unwrap();
        let profile = preset(Some("Vivid"), None, "0").replace("Normal", "Look");
        write(&root, "profile.xmp", &profile);
        let huge = format!(
            "{}{}",
            preset(Some("Huge"), None, "0"),
            " ".repeat(LIMITS.file_bytes as usize)
        );
        write(&root, "huge.xmp", &huge);
        write(
            &root,
            ".hidden/Secret.xmp",
            &preset(Some("Secret"), None, "0"),
        );
        write(
            &root,
            "Old.lightphotos/Inner.xmp",
            &preset(Some("Inner"), None, "0"),
        );

        let scan = scan(&root, &LIMITS).unwrap();

        assert_eq!(
            listed(&scan).iter().map(|(_, n)| *n).collect::<Vec<_>>(),
            ["Loud"]
        );
        assert_eq!(
            scan.skipped, 5,
            "broken, binary, latin1, the profile and the oversize file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Catches an unbounded walk: a folder nine levels down is past the cap,
    /// eight is not.
    #[test]
    fn the_walk_stops_at_the_depth_limit() {
        let root = temp_folder("presets-depth");
        let deep = |n: usize| {
            (1..=n)
                .map(|i| format!("d{i}"))
                .collect::<Vec<_>>()
                .join("/")
        };
        write(
            &root,
            &format!("{}/Eight.xmp", deep(8)),
            &preset(Some("Eight"), Some("G"), "0"),
        );
        write(
            &root,
            &format!("{}/Nine.xmp", deep(9)),
            &preset(Some("Nine"), Some("G"), "0"),
        );

        let scan = scan(&root, &LIMITS).unwrap();

        assert_eq!(listed(&scan), [("G", "Eight")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn the_walk_stops_at_the_file_limit() {
        let root = temp_folder("presets-count");
        for i in 0..5 {
            write(&root, &format!("{i}.xmp"), &preset(None, None, "0"));
        }
        let limits = Limits { files: 3, ..LIMITS };

        let scan = scan(&root, &limits).unwrap();

        assert_eq!(scan.presets.len() + scan.skipped, 3);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A link back to an ancestor would walk forever, or until the depth cap
    /// listed every preset eight times, without the seen-set.
    #[cfg(unix)]
    #[test]
    fn a_symlink_loop_lists_each_preset_once() {
        let root = temp_folder("presets-loop");
        write(&root, "A/Warm.xmp", &preset(Some("Warm"), None, "0"));
        std::os::unix::fs::symlink(&root, root.join("A/back")).unwrap();
        std::os::unix::fs::symlink(root.join("A"), root.join("again")).unwrap();

        let scan = scan(&root, &LIMITS).unwrap();

        assert_eq!(listed(&scan), [("A", "Warm")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_missing_root_is_an_error_not_an_empty_folder() {
        let root = temp_folder("presets-missing").join("gone");
        assert!(scan(&root, &LIMITS).is_err());
    }
}
