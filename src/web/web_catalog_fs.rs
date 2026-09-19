// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: sidecar I/O through File System Access directory handles.
//! The browser counterpart of the `std::fs` sidecar code in `catalog.rs`,
//! which still owns the sidecar format.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemHandleKind, FileSystemWritableFileStream,
};

use crate::catalog::{ImageRecord, SidecarLoad, SIDECAR_DIR, SIDECAR_EXT};

/// The `.lightphotos` directory under `root`, created if `create` is set.
/// Returns `Ok(None)` when it is missing and `create` is false.
/// `web_thumb_cache.rs` stores thumbnails in the same directory.
pub(crate) async fn sidecar_dir(
    root: &FileSystemDirectoryHandle,
    create: bool,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    let opts = FileSystemGetDirectoryOptions::new();
    opts.set_create(create);
    match JsFuture::from(root.get_directory_handle_with_options(SIDECAR_DIR, &opts)).await {
        Ok(v) => Ok(Some(v.unchecked_into())),
        Err(e) if !create && is_not_found(&e) => Ok(None),
        Err(e) => Err(js_error_string(&e)),
    }
}

fn xmp_name(filename: &OsStr) -> String {
    format!("{}.{SIDECAR_EXT}", filename.to_string_lossy())
}

/// The reverse of `xmp_name`. Strips only the last extension, so
/// `PHOTO1.ARW.xmp` maps back to `PHOTO1.ARW`.
fn strip_xmp(name: &str) -> Option<OsString> {
    name.strip_suffix(&format!(".{SIDECAR_EXT}"))
        .map(OsString::from)
}

/// Load every sidecar in `root/.lightphotos`. Like the native loader, an
/// unreachable directory gives an empty result and a bad file counts toward
/// `skipped` without stopping the scan.
pub(crate) async fn load_sidecars(root: &FileSystemDirectoryHandle) -> SidecarLoad {
    let mut images = HashMap::new();
    let mut skipped = 0usize;

    let dir = match sidecar_dir(root, false).await {
        Ok(Some(d)) => d,
        Ok(None) => return SidecarLoad { images, skipped },
        Err(e) => {
            web_sys::console::error_1(&format!("[web] listing .lightphotos failed: {e}").into());
            return SidecarLoad { images, skipped };
        }
    };

    let iter = dir.values();
    loop {
        let next = match iter.next() {
            Ok(promise) => match JsFuture::from(promise).await {
                Ok(v) => v,
                Err(_) => break,
            },
            Err(_) => break,
        };
        let done = js_sys::Reflect::get(&next, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if done {
            break;
        }
        let Ok(value) = js_sys::Reflect::get(&next, &"value".into()) else {
            continue;
        };
        let Ok(child) = value.dyn_into::<web_sys::FileSystemHandle>() else {
            continue;
        };
        if child.kind() != FileSystemHandleKind::File {
            continue;
        }
        let name = child.name();
        let Some(stem) = strip_xmp(&name) else {
            continue;
        };
        let file_handle: FileSystemFileHandle = child.unchecked_into();
        match crate::web_fs::read_bytes(&file_handle).await {
            Ok(bytes) => match serde_json::from_slice::<ImageRecord>(&bytes) {
                Ok(rec) if !rec.is_empty() => {
                    images.insert(stem, rec);
                }
                Ok(_) => {}
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] unreadable sidecar {name}: {e}").into(),
                    );
                    skipped += 1;
                }
            },
            Err(e) => {
                web_sys::console::error_1(&format!("[web] could not read {name}: {e}").into());
                skipped += 1;
            }
        }
    }

    SidecarLoad { images, skipped }
}

/// Write `bytes` to `root/.lightphotos/<filename>.xmp`, creating the
/// directory if needed. The writable stream writes to a swap file and
/// replaces the real file on `close()`, so the write is atomic.
pub(crate) async fn write_sidecar(
    root: &FileSystemDirectoryHandle,
    filename: &OsStr,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = sidecar_dir(root, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos".to_string())?;

    let opts = FileSystemGetFileOptions::new();
    opts.set_create(true);
    let file_handle: FileSystemFileHandle =
        JsFuture::from(dir.get_file_handle_with_options(&xmp_name(filename), &opts))
            .await
            .map_err(|e| js_error_string(&e))?
            .unchecked_into();

    let writable: FileSystemWritableFileStream = JsFuture::from(file_handle.create_writable())
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

/// Delete `root/.lightphotos/<filename>.xmp`. A missing directory or file
/// is not an error.
pub(crate) async fn delete_sidecar(
    root: &FileSystemDirectoryHandle,
    filename: &OsStr,
) -> Result<(), String> {
    let Some(dir) = sidecar_dir(root, false).await? else {
        return Ok(());
    };
    match JsFuture::from(dir.remove_entry(&xmp_name(filename))).await {
        Ok(_) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(e) => Err(js_error_string(&e)),
    }
}

/// Permanently delete `dir/<filename>`. File System Access has no trash,
/// so this replaces `trash::move_to_trash` on web. A file that is already
/// gone is not an error.
pub(crate) async fn remove_file(
    dir: &FileSystemDirectoryHandle,
    filename: &OsStr,
) -> Result<(), String> {
    match JsFuture::from(dir.remove_entry(&filename.to_string_lossy())).await {
        Ok(_) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(e) => Err(js_error_string(&e)),
    }
}

/// True when a thrown JS value is a `NotFoundError` `DOMException`, the
/// web equivalent of `io::ErrorKind::NotFound`.
pub(crate) fn is_not_found(e: &JsValue) -> bool {
    js_sys::Reflect::get(e, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
        == Some("NotFoundError")
}

pub(crate) fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
