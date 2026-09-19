// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: folder picking, listing, and file reads through the File
//! System Access API. The browser counterpart of `navigation.rs`. A picked
//! folder has no OS path, only directory and file handles, and every
//! operation is async.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DirectoryPickerOptions, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemHandleKind,
    FileSystemPermissionMode,
};

use crate::navigation::{is_image, is_listable_subdir};

/// A folder picked via `showDirectoryPicker`, listed one level deep.
/// `dir` is the handle's name used as a label, not a real path. All paths
/// are relative to it.
pub struct PickedFolder {
    pub dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub handles: HashMap<PathBuf, FileSystemFileHandle>,
    /// Directory handles by relative path: the root, its immediate
    /// subdirectories, and any deeper folder the user browses into.
    /// `web_catalog_fs.rs` needs these to write `.lightphotos` sidecars.
    pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>,
}

/// Show the folder picker and list the chosen folder's images and
/// subdirectories. Requests `readwrite` so catalog sidecars can be saved.
/// A cancelled picker and a listing failure both return `Err`.
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

    let root = PathBuf::from(handle.name());
    let listing = list_dir(&root, &handle).await?;

    let mut handles = HashMap::new();
    let mut entries = Vec::with_capacity(listing.images.len());
    for (path, fh) in listing.images {
        handles.insert(path.clone(), fh);
        entries.push(path);
    }

    let mut dir_handles = HashMap::new();
    dir_handles.insert(root.clone(), handle);
    for (p, h) in listing.subdirs {
        dir_handles.insert(p, h);
    }

    Ok(PickedFolder {
        dir: root,
        entries,
        handles,
        dir_handles,
    })
}

/// The image files and immediate subdirectories of one directory handle.
/// Paths are relative to the picked root (`base` is this directory's own
/// relative path; children are `base.join(child_name)`).
pub struct DirListing {
    pub images: Vec<(PathBuf, FileSystemFileHandle)>,
    pub subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)>,
}

/// List `handle` with the same filters as the native listing (`is_image`,
/// `is_listable_subdir`). Both lists are sorted case-insensitively by name,
/// matching the native order.
pub async fn list_dir(
    base: &Path,
    handle: &FileSystemDirectoryHandle,
) -> Result<DirListing, String> {
    let mut images: Vec<(PathBuf, FileSystemFileHandle)> = Vec::new();
    let mut subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)> = Vec::new();

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
        let name = child.name();
        let path = base.join(&name);
        match child.kind() {
            FileSystemHandleKind::File => {
                if is_image(&path) {
                    images.push((path, child.unchecked_into()));
                }
            }
            FileSystemHandleKind::Directory => {
                if is_listable_subdir(&name) {
                    subdirs.push((path, child.unchecked_into()));
                }
            }
            _ => {}
        }
    }

    sort_pairs_by_name(&mut images);
    sort_pairs_by_name(&mut subdirs);
    Ok(DirListing { images, subdirs })
}

fn sort_pairs_by_name<T>(v: &mut [(PathBuf, T)]) {
    v.sort_by(|a, b| {
        let an = a.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let bn = b.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        an.cmp(&bn)
    });
}

/// Read a file as a JS `ArrayBuffer` without copying it into wasm memory.
/// Use this when the bytes go straight to a worker via a `postMessage`
/// transfer. A RAW file can be tens of MB, and the extra copy in
/// `read_bytes` caused `RangeError: Array buffer allocation failed`.
pub async fn read_array_buffer(
    handle: &FileSystemFileHandle,
) -> Result<js_sys::ArrayBuffer, String> {
    let file = stat(handle).await?;
    let buf: js_sys::ArrayBuffer = JsFuture::from(file.array_buffer())
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into();
    Ok(buf)
}

/// Resolve a handle to its `File` without reading its contents, like
/// `fs::metadata`. `web_thumb_cache` keys entries on the `File`'s size and
/// modified time, so a cache hit never reads the source bytes.
pub async fn stat(handle: &FileSystemFileHandle) -> Result<web_sys::File, String> {
    Ok(JsFuture::from(handle.get_file())
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into())
}

/// Read a file's full contents into wasm memory.
pub async fn read_bytes(handle: &FileSystemFileHandle) -> Result<Vec<u8>, String> {
    let buf = read_array_buffer(handle).await?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// The `message` of a thrown JS value (usually a `DOMException` such as
/// `AbortError` on picker cancel), or its debug form.
fn js_error_string(e: &JsValue) -> String {
    js_sys::Reflect::get(e, &"message".into())
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
