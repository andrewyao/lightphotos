// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: the File System Access implementation of `export::ExportFs`.
//! Sources are read through file handles. JPEGs are written through a
//! writable stream, which swaps the file in atomically on `close()`, the
//! same guarantee native export gets from write-to-temp plus rename.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemWritableFileStream,
};

use crate::export::{ExportFs, EXPORTS_DIR};

/// Built fresh for each export batch, so `file_handles` is a snapshot of
/// `App::web_file_handles` at that moment.
pub(crate) struct WebFs {
    /// The current folder. `Exports/` is created under it.
    folder: FileSystemDirectoryHandle,
    /// Keyed by path relative to the picked root.
    file_handles: HashMap<PathBuf, FileSystemFileHandle>,
}

impl WebFs {
    pub(crate) fn new(
        folder: FileSystemDirectoryHandle,
        file_handles: HashMap<PathBuf, FileSystemFileHandle>,
    ) -> Self {
        Self {
            folder,
            file_handles,
        }
    }

    /// Names already in `Exports/`, so a batch can pick collision-free
    /// targets up front. The API has no synchronous exists check. A missing
    /// `Exports/` gives an empty set. Any other failure is an error, because
    /// a partial scan could miss a collision.
    pub(crate) async fn existing_export_names(&self) -> Result<HashSet<String>, String> {
        let mut names = HashSet::new();
        let opts = FileSystemGetDirectoryOptions::new();
        opts.set_create(false);
        let dir: FileSystemDirectoryHandle = match JsFuture::from(
            self.folder
                .get_directory_handle_with_options(EXPORTS_DIR, &opts),
        )
        .await
        {
            Ok(v) => v.unchecked_into(),
            Err(e) if is_not_found(&e) => return Ok(names),
            Err(e) => return Err(js_error_string(&e)),
        };

        let iter = dir.values();
        loop {
            let next = match iter.next() {
                Ok(promise) => JsFuture::from(promise)
                    .await
                    .map_err(|e| js_error_string(&e))?,
                Err(e) => return Err(js_error_string(&e)),
            };
            let done = js_sys::Reflect::get(&next, &"done".into())
                .map_err(|e| js_error_string(&e))?
                .as_bool()
                .ok_or_else(|| "Exports iterator returned invalid done flag".to_string())?;
            if done {
                break;
            }
            let value =
                js_sys::Reflect::get(&next, &"value".into()).map_err(|e| js_error_string(&e))?;
            let child = value
                .dyn_into::<web_sys::FileSystemHandle>()
                .map_err(|_| "Exports iterator returned an invalid entry".to_string())?;
            names.insert(child.name());
        }
        Ok(names)
    }
}

impl ExportFs for WebFs {
    async fn read_source(&self, src: &Path) -> Result<Vec<u8>, String> {
        let handle = self
            .file_handles
            .get(src)
            .ok_or_else(|| format!("no file handle for {}", src.display()))?;
        crate::web_fs::read_bytes(handle).await
    }

    async fn write_atomic(&self, dest: &Path, bytes: &[u8]) -> Result<(), String> {
        let filename = dest
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("bad export destination: {}", dest.display()))?;

        let dir_opts = FileSystemGetDirectoryOptions::new();
        dir_opts.set_create(true);
        let dir: FileSystemDirectoryHandle = JsFuture::from(
            self.folder
                .get_directory_handle_with_options(EXPORTS_DIR, &dir_opts),
        )
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into();

        let file_opts = FileSystemGetFileOptions::new();
        file_opts.set_create(true);
        let file: FileSystemFileHandle =
            JsFuture::from(dir.get_file_handle_with_options(filename, &file_opts))
                .await
                .map_err(|e| js_error_string(&e))?
                .unchecked_into();

        let writable: FileSystemWritableFileStream = JsFuture::from(file.create_writable())
            .await
            .map_err(|e| js_error_string(&e))?
            .unchecked_into();

        let array = js_sys::Uint8Array::from(bytes);
        JsFuture::from(
            writable
                .write_with_js_u8_array(&array)
                .map_err(|e| js_error_string(&e))?,
        )
        .await
        .map_err(|e| js_error_string(&e))?;

        JsFuture::from(writable.close())
            .await
            .map_err(|e| js_error_string(&e))?;
        Ok(())
    }
}

impl WebFs {
    pub(crate) async fn read_source_array_buffer(
        &self,
        src: &Path,
    ) -> Result<js_sys::ArrayBuffer, String> {
        let handle = self
            .file_handles
            .get(src)
            .ok_or_else(|| format!("no file handle for {}", src.display()))?;
        crate::web_fs::read_array_buffer(handle).await
    }
}

fn is_not_found(e: &JsValue) -> bool {
    js_sys::Reflect::get(e, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
        == Some("NotFoundError")
}

fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
