// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: the File System Access implementation of `export::ExportFs`.
//! Native export (`export::NativeFs`) reads with `std::fs` and does a
//! tmp-write + atomic rename; in the browser a picked folder has no OS path,
//! so source reads go through a `FileSystemFileHandle` and the JPEG is
//! written through a File System Access writable stream (which performs its
//! own atomic swap on `close()` — the same guarantee native's tmp+rename
//! gets). The transport mirrors `web_catalog_fs::write_sidecar`.
//!
//! ## Pipeline position
//! - This is wasm32's tail of Pipeline 3 (export): `app/export.rs`'s wasm
//!   `start_export` builds a `WebFs`, the worker pool runs `export::bake_jpeg`
//!   (`wasm_worker.rs`), and `main.rs`'s frame loop hands each finished
//!   JPEG's bytes to `WebFs::write_atomic`.
//! - `existing_export_names` is the browser counterpart of native
//!   `paths::jpg_export_target`'s `Path::exists()` collision check — the FSA
//!   API has no synchronous existence test, so `start_export` pre-scans the
//!   `Exports/` directory once per batch instead.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemWritableFileStream,
};

use crate::export::{ExportFs, EXPORTS_DIR};

/// File System Access backing for `export::ExportFs`. Built fresh per export
/// batch in `app/export.rs`'s wasm `start_export`, so its handle map is a
/// snapshot of `App::web_file_handles` at that moment.
pub(crate) struct WebFs {
    /// The currently-shown folder's directory handle — `Exports/` is created
    /// under this.
    folder: FileSystemDirectoryHandle,
    /// Picked-folder-relative source path → its `FileSystemFileHandle`
    /// (cloned from `App::web_file_handles`).
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

    /// Names already present in `Exports/` (any entry kind), so
    /// `start_export` can resolve collision-free targets without a
    /// per-candidate FSA round trip. An absent `Exports/` yields an empty
    /// set — nothing to collide with yet. All other access and iteration
    /// failures are returned: a partial scan is unsafe for collision checks.
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

/// Same extraction `web_fs`/`web_catalog_fs` use — duplicated for the same
/// reason they duplicate it (each is a one-liner around `Reflect::get(e,
/// "message")`, not worth a shared-visibility change).
fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
