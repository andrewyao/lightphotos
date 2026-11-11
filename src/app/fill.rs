// SPDX-License-Identifier: MIT OR Apache-2.0

//! What the app hands a background bake for a photo's generative fills.

use std::path::Path;

use crate::develop::fill::FillInput;

use super::App;

impl App {
    /// What a background bake of `path` carries. A native worker reads the
    /// patch files itself, in a read-only folder too, since reading writes
    /// nothing.
    pub(crate) fn fill_input(&self, path: &Path) -> FillInput {
        fill_input_of(&self.folder_store, path)
    }
}

/// [`App::fill_input`] as a free function, so a caller holding the loader
/// can still build one. Nothing in the browser reads patch files yet, so
/// there a bake draws no fills.
pub(crate) fn fill_input_of(
    folder_store: &crate::persist::folder_store::FolderStore,
    path: &Path,
) -> FillInput {
    #[cfg(not(target_arch = "wasm32"))]
    {
        FillInput::OnDisk(folder_store.fills(path).to_vec())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (folder_store, path);
        FillInput::none()
    }
}
