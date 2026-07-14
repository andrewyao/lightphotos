// SPDX-License-Identifier: MIT OR Apache-2.0

//! Move files to the macOS Trash via `NSFileManager` — native, matching the
//! project's objc2/ImageIO approach (no third-party crate).

use std::path::Path;

use objc2_foundation::{NSFileManager, NSString, NSURL};

/// Move `path` to the user's Trash. Returns a human-readable error on failure
/// (e.g. the file is gone, or the volume has no Trash). The original file is
/// left untouched on error.
pub fn move_to_trash(path: &Path) -> Result<(), String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trashes_an_existing_file() {
        // Create a temp file, trash it, and confirm it left its original spot.
        // (It lands in the user's Trash — harmless.)
        let path = std::env::temp_dir()
            .join(format!("image-viewer-trash-test-{}.txt", std::process::id()));
        std::fs::write(&path, b"trash me").unwrap();
        assert!(path.exists());
        move_to_trash(&path).expect("trash should succeed");
        assert!(!path.exists(), "file should be gone from its original location");
    }

    #[test]
    fn trashing_a_missing_file_errors() {
        let path = std::env::temp_dir().join("image-viewer-does-not-exist-xyz.txt");
        assert!(move_to_trash(&path).is_err());
    }
}
