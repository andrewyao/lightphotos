// SPDX-License-Identifier: GPL-3.0-or-later

//! Native "choose a folder" dialog — a thin wrapper over `rfd` so the rest of
//! the app just gets an `Option<PathBuf>` back. Used by the landing page's
//! "Choose Folder" button and the `Cmd/Ctrl+O` shortcut (both routed through
//! `App::open_folder_picker`).
//!
//! wasm has its own, asynchronous picker built on the File System Access API
//! (`web_fs::pick_and_list_folder`) and never compiles this module.

use std::path::PathBuf;

/// Show the OS folder picker and return the chosen directory, or `None` if the
/// user cancelled. Blocks the calling thread until the modal closes — call it
/// on the main (event-loop) thread only, which is where `UiAction`s and key
/// handlers run.
pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a folder of photos")
        .pick_folder()
}
