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

/// What `--drive --models-pick` answers the ComfyUI models-folder picker
/// with, in place of cancelling it.
static MODELS_PICK: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Answer the ComfyUI models-folder picker of a `--drive` run with `dir`.
/// Set once, before the app starts.
pub(crate) fn answer_models_picker(dir: PathBuf) {
    let _ = MODELS_PICK.set(dir);
}

/// The folder picker for ComfyUI's models folder, opened in `start` when it
/// exists. `None` when the user cancelled. Blocks until the dialog closes,
/// so call it only on the main thread.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_models_folder(start: Option<PathBuf>) -> Option<PathBuf> {
    if HEADLESS.load(Ordering::Relaxed) {
        let pick = MODELS_PICK.get().cloned();
        eprintln!("[drive] the models-folder picker was asked for; answering {pick:?}");
        return pick;
    }
    let mut dialog = rfd::FileDialog::new().set_title(crate::i18n::t().comfy_models_picker_title);
    if let Some(start) = start.filter(|s| s.is_dir()) {
        dialog = dialog.set_directory(start);
    }
    dialog.pick_folder()
}

/// What `--drive --gps-pick` answers the GPS-track picker with, in place of
/// cancelling it.
static GPS_PICKS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

/// Answer every GPS-track picker of a `--drive` run with `paths`. Set once,
/// before the app starts.
pub(crate) fn answer_gps_pickers(paths: Vec<PathBuf>) {
    let _ = GPS_PICKS.set(paths);
}

/// The picker for GPX tracks: on macOS one panel that takes files and
/// folders together, elsewhere the files or the folder `kind` names. Empty
/// when the user cancelled. Blocks until it closes, so call it only on the
/// main thread.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_gps_tracks(kind: crate::app::PickKind) -> Vec<PathBuf> {
    use crate::app::PickKind;
    if HEADLESS.load(Ordering::Relaxed) {
        let picks = GPS_PICKS.get().cloned().unwrap_or_default();
        eprintln!("[drive] the GPS-track picker was asked for; answering {picks:?}");
        return picks;
    }
    let t = crate::i18n::t();
    let dialog = || {
        rfd::FileDialog::new()
            .set_title(t.gps_picker_title)
            .add_filter(t.gpx_filter_name, &["gpx", "GPX"])
    };
    match kind {
        #[cfg(target_os = "macos")]
        PickKind::Any => pick_files_and_folders(t.gps_picker_title, &["gpx"]),
        #[cfg(not(target_os = "macos"))]
        PickKind::Any => dialog().pick_files().unwrap_or_default(),
        PickKind::Files => dialog().pick_files().unwrap_or_default(),
        PickKind::Folder => dialog().pick_folders().unwrap_or_default(),
    }
}

/// NSOpenPanel taking files with `extensions` and folders together, several
/// at once, which `rfd` can't ask for.
#[cfg(target_os = "macos")]
fn pick_files_and_folders(title: &str, extensions: &[&str]) -> Vec<PathBuf> {
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSModalResponseOK, NSOpenPanel};
    use objc2_foundation::{NSArray, NSString};
    use objc2_uniform_type_identifiers::UTType;
    let Some(mtm) = MainThreadMarker::new() else {
        eprintln!("[lightphotos] the GPS-track picker needs the main thread");
        return Vec::new();
    };
    let panel = NSOpenPanel::openPanel(mtm);
    panel.setCanChooseFiles(true);
    panel.setCanChooseDirectories(true);
    panel.setAllowsMultipleSelection(true);
    panel.setTitle(Some(&NSString::from_str(title)));
    panel.setMessage(Some(&NSString::from_str(title)));
    let types: Vec<_> = extensions
        .iter()
        .filter_map(|e| UTType::typeWithFilenameExtension(&NSString::from_str(e)))
        .collect();
    if !types.is_empty() {
        panel.setAllowedContentTypes(&NSArray::from_retained_slice(&types));
    }
    if panel.runModal() != NSModalResponseOK {
        return Vec::new();
    }
    panel
        .URLs()
        .iter()
        .filter_map(|url| Some(PathBuf::from(url.path()?.to_string())))
        .collect()
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
