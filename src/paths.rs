// SPDX-License-Identifier: GPL-3.0-or-later

//! Path helpers for file identity and export filenames.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A stable identity for "same file" comparisons and map keys: the canonical
/// path, or the path as given when canonicalization fails (e.g. the file is gone).
pub fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub(crate) fn export_stem(src: &Path) -> String {
    src.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into())
}

/// The first free `<stem>.jpg`, `<stem>-1.jpg`, ... name for `src`, skipping
/// names in `existing` (already in `Exports/`) or `taken` (handed out earlier in
/// this batch). The wasm32 version of [`jpg_export_target`], since the browser
/// has no `Path::exists()`.
#[cfg(any(target_arch = "wasm32", test))]
pub fn jpg_export_name(src: &Path, existing: &HashSet<String>, taken: &HashSet<String>) -> String {
    let stem = export_stem(src);
    let free = |name: &str| !existing.contains(name) && !taken.contains(name);

    let base = format!("{stem}.jpg");
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = format!("{stem}-{n}.jpg");
        if free(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// The first free `stem.jpg`, `stem-1.jpg`, ... path in `dest_dir` for `src`.
/// Skips files on disk and paths in `taken` (handed out earlier in this batch,
/// not yet written), so same-stem sources like `photo.raw` and `photo.jpg`
/// don't collide.
#[cfg(not(target_arch = "wasm32"))]
pub fn jpg_export_target(src: &Path, dest_dir: &Path, taken: &HashSet<PathBuf>) -> PathBuf {
    let stem = export_stem(src);

    let free = |candidate: &Path| !candidate.exists() && !taken.contains(candidate);

    let base = dest_dir.join(format!("{stem}.jpg"));
    if free(&base) {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = dest_dir.join(format!("{stem}-{n}.jpg"));
        if free(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_name_avoids_existing_and_taken() {
        let src = Path::new("/photos/DSC_1000.NEF");
        let mut existing = HashSet::new();
        let taken = HashSet::new();
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000.jpg");

        existing.insert("DSC_1000.jpg".to_string());
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000-1.jpg");

        let mut taken = HashSet::new();
        taken.insert("DSC_1000-1.jpg".to_string());
        assert_eq!(jpg_export_name(src, &existing, &taken), "DSC_1000-2.jpg");
    }

    #[test]
    fn export_name_falls_back_when_stemless() {
        assert_eq!(
            jpg_export_name(Path::new("/x/.."), &HashSet::new(), &HashSet::new()),
            "export.jpg"
        );
    }

    #[test]
    fn export_target_avoids_clobbering_existing_files() {
        let dir = std::env::temp_dir().join(format!("iv-paths-test-{}", std::process::id()));
        let out = dir.join("Exports");
        std::fs::create_dir_all(&out).unwrap();
        let none = HashSet::new();

        let src = dir.join("photo.png");
        assert_eq!(jpg_export_target(&src, &out, &none), out.join("photo.jpg"));

        std::fs::write(out.join("photo.jpg"), b"x").unwrap();
        assert_eq!(
            jpg_export_target(&src, &out, &none),
            out.join("photo-1.jpg")
        );

        // `taken` reserves names that aren't on disk yet.
        let mut taken = HashSet::new();
        let first = jpg_export_target(&src, &out, &taken);
        taken.insert(first.clone());
        let second = jpg_export_target(&src, &out, &taken);
        assert_eq!(first, out.join("photo-1.jpg"));
        assert_eq!(second, out.join("photo-2.jpg"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
