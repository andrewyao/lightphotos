// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: File System Access counterpart to `catalog.rs`'s std::fs-
//! based sidecar I/O (`load_sidecars`/`write_sidecar_file`/`delete_sidecar`'s
//! native arm). A picked folder has no real OS path for `std::fs` to use —
//! everything here goes through the folder's root `FileSystemDirectoryHandle`
//! instead, same as `web_fs.rs`'s read-only listing/decode path.
//!
//! Reuses `catalog.rs`'s own `SIDECAR_DIR`/`SIDECAR_EXT` constants and
//! `ImageRecord::is_empty` rather than re-deriving the sidecar convention
//! here — this module only supplies the browser-specific transport, not a
//! second definition of what a sidecar is.
//!
//! ## Pipeline position
//! - Not part of Pipeline 1/2/3 (decode/thumbnail/export) — this is
//!   ratings-and-edits persistence, read once when a folder opens
//!   (`load_sidecars`) and written whenever the user rates or edits a photo
//!   (`write_sidecar`/`delete_sidecar`).
//! - Runs alongside the pipelines rather than inside them: `edits`/
//!   `rotations` loaded here are what Pipeline 2's thumbnail bake
//!   (`image_ops::bake_edited`) and Pipeline 1's live shader adjustments
//!   read from.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemHandleKind, FileSystemWritableFileStream,
};

use crate::catalog::{ImageRecord, SidecarLoad, SIDECAR_DIR, SIDECAR_EXT};

/// Get the `.lightphotos` subdirectory under `root`, creating it if `create`
/// and it doesn't exist yet. `Ok(None)` (not an error) when it's missing and
/// `create` is false — mirrors `catalog::load_sidecars`' "a missing
/// `.lightphotos` directory is not an error, just an empty catalog".
async fn sidecar_dir(
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

/// `<filename>.<SIDECAR_EXT>` for the sidecar of a photo named `filename`.
fn xmp_name(filename: &OsStr) -> String {
    format!("{}.{SIDECAR_EXT}", filename.to_string_lossy())
}

/// The reverse of `xmp_name`: strip exactly the trailing `.<SIDECAR_EXT>`,
/// same as native's `Path::file_stem()` on a `SIDECAR_EXT`-suffixed path —
/// correctly preserving a name like `PHOTO1.ARW` that itself contains a dot.
/// `None` for a browser entry that isn't actually a sidecar (shouldn't
/// happen given the directory-listing filter below, but cheap to check).
fn strip_xmp(name: &str) -> Option<OsString> {
    name.strip_suffix(&format!(".{SIDECAR_EXT}"))
        .map(OsString::from)
}

/// Scan `root/.lightphotos/*.xmp` — the async, handle-based counterpart of
/// `catalog::load_sidecars`. Errors reaching the directory itself (not
/// found, permission) degrade to an empty result (same as native); a
/// per-file read/parse failure increments `skipped` instead of aborting the
/// whole scan, same as native.
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
            continue; // not a sidecar file — shouldn't appear under .lightphotos, but ignore rather than miscount
        };
        let file_handle: FileSystemFileHandle = child.unchecked_into();
        match crate::web_fs::read_bytes(&file_handle).await {
            Ok(bytes) => match serde_json::from_slice::<ImageRecord>(&bytes) {
                Ok(rec) if !rec.is_empty() => {
                    images.insert(stem, rec);
                }
                Ok(_) => {} // an empty record on disk: nothing to cache
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

/// Write `rec` as pretty-printed JSON to `root/.lightphotos/<filename>.xmp`,
/// creating `.lightphotos` as needed (mirrors native's lazy creation).
/// `FileSystemWritableFileStream::close()` performs an atomic swap-in on
/// supporting browsers (the underlying implementation writes to a hidden
/// swap file and only replaces the real one on close) — the same atomicity
/// native's explicit tmp-file-plus-rename gets, here for free.
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

/// Delete `root/.lightphotos/<filename>.xmp`. A missing `.lightphotos`
/// directory, or a missing sidecar within it, is not an error — matches
/// native's `NotFound => Ok(())` arm.
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

/// Permanently delete `dir/<filename>` — the wasm32 stand-in for
/// `trash::move_to_trash` (File System Access has no trash/recycle
/// primitive, only `remove_entry`). A file that's already gone is not an
/// error, same as native's `NotFound => Ok(())`. Called from
/// `app/catalog.rs`'s wasm `run_delete`, which issues it fire-and-forget and
/// prunes the UI optimistically.
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

/// True when a thrown JS value is a `DOMException` named `NotFoundError` —
/// the File System Access equivalent of native's
/// `std::io::ErrorKind::NotFound`.
fn is_not_found(e: &JsValue) -> bool {
    js_sys::Reflect::get(e, &"name".into())
        .ok()
        .and_then(|v| v.as_string())
        .as_deref()
        == Some("NotFoundError")
}

/// Same extraction `web_fs.rs::js_error_string` uses — duplicated rather
/// than imported since `web_fs::js_error_string` is private to that module
/// (both are one-liners around `Reflect::get(e, "message")`, not worth a
/// shared-visibility change for).
fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
