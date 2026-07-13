//! Small path helpers shared across the catalog, navigation, and thumbnail
//! layers so path identity is computed the same way everywhere.

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

/// The JPEG export target for `src`: same folder, same file stem, `.jpg`,
/// choosing the first name that does not already exist so an existing file
/// (including `src` itself when it's already a `.jpg`, or a sibling) is never
/// overwritten. Tries `stem.jpg`, then `stem-1.jpg`, `stem-2.jpg`, …
pub fn jpg_export_target(src: &Path) -> PathBuf {
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let stem = src
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "export".into());

    let base = dir.join(format!("{stem}.jpg"));
    if !base.exists() {
        return base;
    }
    let mut n = 1u32;
    loop {
        let candidate = dir.join(format!("{stem}-{n}.jpg"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn export_target_avoids_clobbering_existing_files() {
        let dir = std::env::temp_dir().join(format!("iv-paths-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let src = dir.join("photo.png");
        // No jpg yet → plain stem.jpg.
        assert_eq!(jpg_export_target(&src), dir.join("photo.jpg"));

        // photo.jpg exists → photo-1.jpg.
        std::fs::write(dir.join("photo.jpg"), b"x").unwrap();
        assert_eq!(jpg_export_target(&src), dir.join("photo-1.jpg"));

        // Source is itself a jpg and exists → never overwrite it.
        let jpg_src = dir.join("photo.jpg");
        assert_eq!(jpg_export_target(&jpg_src), dir.join("photo-1.jpg"));

        std::fs::remove_dir_all(&dir).ok();
    }
}
