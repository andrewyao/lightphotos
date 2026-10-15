// SPDX-License-Identifier: MIT OR Apache-2.0

//! The native folder and Lightroom-preset pickers, a thin wrapper over `rfd`.
//! The wasm build uses its own async picker in `web_fs` instead.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

static HEADLESS: AtomicBool = AtomicBool::new(false);

pub(crate) fn cancel_all_pickers() {
    HEADLESS.store(true, Ordering::Relaxed);
}

fn headless(what: &str) -> bool {
    let on = HEADLESS.load(Ordering::Relaxed);
    if on {
        eprintln!("[drive] the {what} picker was asked for; answering cancelled");
    }
    on
}

/// Show the OS folder picker. Returns `None` if the user cancelled. Blocks
/// until the dialog closes, so call it only on the main thread.
pub fn pick_folder() -> Option<PathBuf> {
    if headless("folder") {
        return None;
    }
    rfd::FileDialog::new()
        .set_title(crate::i18n::t().picker_title)
        .pick_folder()
}

/// The save dialog for a new catalog, in `dir` (made if missing) with
/// `Untitled.lightphotos` filled in. `None` when the user cancelled.
/// Blocks until the dialog closes, so call it only on the main thread.
pub fn save_catalog(dir: Option<PathBuf>) -> Option<PathBuf> {
    if headless("catalog save") {
        return None;
    }
    let t = crate::i18n::t();
    let ext = crate::persist::catalog::EXT;
    let mut dialog = rfd::FileDialog::new()
        .set_title(t.catalog_save_title)
        .set_file_name(format!("{}.{ext}", t.catalog_untitled))
        .add_filter(t.catalog_filter_name, &[ext]);
    if let Some(dir) = dir {
        let _ = std::fs::create_dir_all(&dir);
        dialog = dialog.set_directory(dir);
    }
    dialog.save_file()
}

/// The folder picker, asking for a `.lightphotos` folder. `None` when the
/// user cancelled. Blocks until the dialog closes, so call it only on the
/// main thread.
pub fn pick_catalog() -> Option<PathBuf> {
    if headless("catalog") {
        return None;
    }
    rfd::FileDialog::new()
        .set_title(crate::i18n::t().catalog_picker_title)
        .pick_folder()
}

/// The folder picker for a folder of Lightroom presets. `None` when the user
/// cancelled. Blocks until the dialog closes, so call it only on the main
/// thread.
pub fn pick_preset_folder() -> Option<PathBuf> {
    if headless("preset folder") {
        return None;
    }
    rfd::FileDialog::new()
        .set_title(crate::i18n::t().preset_folder_picker_title)
        .pick_folder()
}

/// Show the OS file picker for Lightroom presets. Empty when the user
/// cancelled. Blocks until the dialog closes, so call it only on the main
/// thread.
pub fn pick_xmp_files() -> Vec<PathBuf> {
    if headless("preset") {
        return Vec::new();
    }
    rfd::FileDialog::new()
        .set_title(crate::i18n::t().xmp_picker_title)
        .add_filter(crate::i18n::t().xmp_filter_name, &["xmp"])
        .pick_files()
        .unwrap_or_default()
}
