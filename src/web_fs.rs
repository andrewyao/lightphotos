// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: File System Access folder picking + listing. The browser
//! equivalent of `navigation.rs`'s `std::fs::read_dir`-based folder
//! enumeration — both async (a real user-permission prompt, then an async
//! directory-handle iterator) and handle-based rather than path-based (a
//! picked folder has no real OS path string available to Rust at all, only
//! `FileSystemDirectoryHandle`/`FileSystemFileHandle` objects).
//!
//! `read_bytes` below also handles the actual file read for a decode step —
//! full decode-at-size (embedded previews, RAW, WebCodecs) is still ahead,
//! see the wasm port plan's M1; this only gets far enough for a naive
//! full-decode-then-downscale JPEG thumbnail (`app/web.rs`).

use std::collections::HashMap;
use std::path::PathBuf;

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DirectoryPickerOptions, FileSystemDirectoryHandle, FileSystemFileHandle,
    FileSystemHandleKind, FileSystemPermissionMode,
};

use crate::navigation::{is_image, sort_by_name};

/// A folder picked via `showDirectoryPicker`, already listed. `dir` is a
/// synthetic label (the handle's own `.name()`), not a real filesystem path
/// — nothing on the web side has one. `handles` lets a later decode step
/// actually read a file's bytes (`FileSystemFileHandle::get_file`); listing
/// alone doesn't touch file contents. `dir_handle` is the folder's own root
/// handle — `Catalog`'s wasm32 sidecar I/O (`web_catalog_fs.rs`) needs it to
/// find/create `.lightphotos` inside this folder, which is why the picker
/// below requests `readwrite` mode up front rather than read-only.
pub struct PickedFolder {
    pub dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub handles: HashMap<PathBuf, FileSystemFileHandle>,
    pub dir_handle: FileSystemDirectoryHandle,
}

/// Ask the user to pick a folder (`showDirectoryPicker`, requesting
/// `readwrite` so rating/edit sidecars can actually be written back into
/// it — see `PickedFolder::dir_handle`'s doc comment), then list its image
/// files. One round trip — a cancelled picker or a listing failure both
/// come back as `Err`, so the caller doesn't need to distinguish them
/// (there's nothing more specific to do differently either way: report the
/// message and let the user try again).
pub async fn pick_and_list_folder() -> Result<PickedFolder, String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = DirectoryPickerOptions::new();
    opts.set_mode(FileSystemPermissionMode::Readwrite);
    let handle: FileSystemDirectoryHandle = JsFuture::from(
        window
            .show_directory_picker_with_options(&opts)
            .map_err(|e| js_error_string(&e))?,
    )
    .await
    .map_err(|e| js_error_string(&e))?
    .unchecked_into();

    list_images(&handle).await
}

/// Enumerate `handle`'s direct children via its async `values()` iterator
/// (`FileSystemDirectoryHandle` has no synchronous listing at all — every
/// browser directory read goes through this), keeping only files whose name
/// looks like an image (`navigation::is_image` — the same extension check
/// the native/non-mac folder listing uses). Doesn't recurse into
/// subdirectories — matches `Playlist::from_dir`'s own "images directly in
/// this folder" scope.
async fn list_images(handle: &FileSystemDirectoryHandle) -> Result<PickedFolder, String> {
    let dir_name = handle.name();
    let mut entries = Vec::new();
    let mut handles = HashMap::new();

    let iter = handle.values();
    loop {
        let next: JsValue = JsFuture::from(iter.next().map_err(|e| js_error_string(&e))?)
            .await
            .map_err(|e| js_error_string(&e))?;
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
            continue; // one level deep only, per this function's doc comment
        }
        let name = child.name();
        let path = PathBuf::from(&name);
        if !is_image(&path) {
            continue;
        }
        let file_handle: FileSystemFileHandle = child.unchecked_into();
        handles.insert(path.clone(), file_handle);
        entries.push(path);
    }

    sort_by_name(&mut entries);
    Ok(PickedFolder {
        dir: PathBuf::from(dir_name),
        entries,
        handles,
        dir_handle: handle.clone(),
    })
}

/// Read a file's contents as a raw JS `ArrayBuffer` — no copy into Rust/wasm
/// memory. `read_bytes` below is this plus one copy (`Uint8Array::to_vec()`)
/// for the common case where a caller actually wants owned Rust bytes;
/// `web_worker_pool.rs`'s job dispatch calls this one directly instead,
/// specifically to avoid that copy — the buffer goes straight into a
/// `postMessage` transfer list to a worker, so main-thread ownership of the
/// bytes is momentary either way, and a RAW file can be tens of MB.
/// Skipping the copy roughly halves the main thread's peak footprint per
/// in-flight file — real memory pressure on wasm32's constrained heap,
/// confirmed by an actual `RangeError: Array buffer allocation failed`
/// crash before this existed (not the 4GB ceiling itself, just avoidable
/// double-buffering pushing real allocations to fail well short of it).
pub async fn read_array_buffer(handle: &FileSystemFileHandle) -> Result<js_sys::ArrayBuffer, String> {
    let file: web_sys::File = JsFuture::from(handle.get_file())
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into();
    let buf: js_sys::ArrayBuffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into();
    Ok(buf)
}

/// Read a file's full contents into an owned `Vec<u8>`. `FileSystemFileHandle
/// ::get_file()` (a `File`/`Blob`) then `Blob::array_buffer()` — the only way
/// to get bytes out of a browser-picked file at all; there's no
/// `std::fs::read` for something with no real OS path.
pub async fn read_bytes(handle: &FileSystemFileHandle) -> Result<Vec<u8>, String> {
    let buf = read_array_buffer(handle).await?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// Best-effort human-readable message from a thrown JS value (usually a
/// `DOMException`, e.g. `AbortError` when the user cancels the picker, or
/// `NotAllowedError` if permission is denied).
fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
