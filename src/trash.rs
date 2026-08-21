// SPDX-License-Identifier: MIT OR Apache-2.0

//! Move files to trash — native NSFileManager on macOS, trash crate elsewhere.

use std::path::Path;

/// Move `path` to the user's Trash. Returns a human-readable error on failure
/// (e.g. the file is gone, or the volume has no Trash). The original file is
/// left untouched on error.
#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    use objc2_foundation::{NSFileManager, NSString, NSURL};

    let path_str = path.to_str().ok_or("path is not valid UTF-8")?;
    let ns_path = NSString::from_str(path_str);
    // `fileURLWithPath:` percent-encodes and resolves the POSIX path for us.
    let url = NSURL::fileURLWithPath(&ns_path);
    let fm = NSFileManager::defaultManager();
    // Pass `None` for the resulting-URL out-param; we don't need the trashed
    // location back.
    fm.trashItemAtURL_resultingItemURL_error(&url, None)
        .map_err(|e| e.localizedDescription().to_string())
}

#[cfg(not(target_os = "macos"))]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trashing_a_missing_file_errors() {
        let path = std::env::temp_dir().join("image-viewer-does-not-exist-xyz.txt");
        assert!(move_to_trash(&path).is_err());
    }
}
