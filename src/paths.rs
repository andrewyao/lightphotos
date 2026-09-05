// SPDX-License-Identifier: GPL-3.0-or-later

//! Small path helpers shared across the navigation and thumbnail layers so
//! path identity is computed the same way everywhere. `catalog.rs` no longer
//! uses `normalize()` — sidecar identity is filename-within-directory, not a
//! canonicalized absolute path — but `migrate_legacy_dir` below still backs
//! its one-time app-support directory rename.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// Normalize a path to a stable identity: prefer the canonical absolute path,
/// but fall back to the path as-given when canonicalization fails (e.g. the
/// file doesn't exist or was moved). Never panics.
///
/// Use this anywhere two paths must be compared for "same file" or used as a
/// map/cache key. The fallback means a missing file still yields a usable
/// (if non-canonical) key rather than an error.
pub fn normalize(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// One-time migration of a renamed app-support/cache directory: if `new` does
/// not yet exist but `legacy` does, move `legacy` → `new` so data written under
/// the old name (e.g. `com.imageviewer` → `com.lightphotos`) survives a rename.
/// Best-effort — any error just leaves both paths untouched.
pub fn migrate_legacy_dir(new: &Path, legacy: &Path) {
    if new.exists() || !legacy.exists() {
        return;
    }
    // Ensure the parent exists, then rename the whole directory in one move.
    if let Some(parent) = new.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::rename(legacy, new);
}

/// The JPEG export target for `src`, placed in `dest_dir` (the `Exports/`
/// subfolder), keeping `src`'s file stem with a `.jpg` extension. Chooses the
/// first name that neither already exists on disk nor appears in `taken` — the
/// set of targets already handed out for this batch but not yet written — so an
/// existing file is never overwritten and two same-stem sources exported
/// together (e.g. `photo.raw` + `photo.jpg`) don't collide. Tries `stem.jpg`,
/// then `stem-1.jpg`, `stem-2.jpg`, …
fn export_stem(src: &Path) -> String {
    src.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into())
}

/// The bare `<stem>.jpg` (or `<stem>-N.jpg`) export filename for `src`,
/// skipping any name in `existing` (already in the `Exports/` folder) or
/// `taken` (handed out earlier in this batch, not yet written). The
/// filesystem-free core of [`jpg_export_target`] — wasm32 export has no
/// `Path::exists()`, so it pre-scans `Exports/` into `existing`
/// (`web_export_fs::WebFs::existing_export_names`) and calls this.
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
        // The source folder and the Exports subfolder the caller creates.
        let out = dir.join("Exports");
        std::fs::create_dir_all(&out).unwrap();
        let none = HashSet::new();

        let src = dir.join("photo.png");
        // No jpg yet → plain stem.jpg in the Exports dir.
        assert_eq!(jpg_export_target(&src, &out, &none), out.join("photo.jpg"));

        // photo.jpg exists on disk → photo-1.jpg.
        std::fs::write(out.join("photo.jpg"), b"x").unwrap();
        assert_eq!(
            jpg_export_target(&src, &out, &none),
            out.join("photo-1.jpg")
        );

        // `taken` reserves names not yet written: two same-stem sources handed
        // out in sequence resolve to distinct targets even before either exists.
        let mut taken = HashSet::new();
        let first = jpg_export_target(&src, &out, &taken); // photo-1.jpg (photo.jpg on disk)
        taken.insert(first.clone());
        let second = jpg_export_target(&src, &out, &taken);
        assert_eq!(first, out.join("photo-1.jpg"));
        assert_eq!(second, out.join("photo-2.jpg"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
