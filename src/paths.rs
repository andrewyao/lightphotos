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
