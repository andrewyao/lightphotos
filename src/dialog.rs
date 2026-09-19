// SPDX-License-Identifier: GPL-3.0-or-later

//! Native folder picker, a thin wrapper over `rfd`. The wasm build uses its
//! own async picker in `web_fs` instead.

use std::path::PathBuf;

/// Show the OS folder picker. Returns `None` if the user cancelled. Blocks
/// until the dialog closes, so call it only on the main thread.
pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a folder of photos")
        .pick_folder()
}
