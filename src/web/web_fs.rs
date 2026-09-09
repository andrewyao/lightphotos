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
//!
//! ## Pipeline position
//! - `pick_and_list_folder` runs once, when the user picks a folder — the
//!   wasm32 entry point into Pipeline 2 (there's no `navigation.rs`
//!   directory walk on this platform, since there's no OS path to walk).
//! - `read_array_buffer`/`read_bytes` run at the start of every wasm32
//!   decode, in both Pipeline 1 and Pipeline 2: `app/web.rs` reads a
//!   file's bytes this way before handing them to
//!   `web_worker_pool.rs::WorkerPoolHandle::submit`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    DirectoryPickerOptions, FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemHandleKind,
    FileSystemPermissionMode,
};

use crate::navigation::{is_image, is_listable_subdir};

/// A folder picked via `showDirectoryPicker`, already listed one level deep.
/// `dir` is a synthetic label (the handle's own `.name()`), not a real
/// filesystem path — nothing on the web side has one. `handles` lets a later
/// decode step actually read a file's bytes (`FileSystemFileHandle::get_file`);
/// listing alone doesn't touch file contents. `dir_handles` carries the root
/// and every immediate-subdirectory handle — `Catalog`'s wasm32 sidecar I/O
/// (`web_catalog_fs.rs`) needs the handle for the current folder to find/create
/// `.lightphotos` inside it, which is why the picker below requests `readwrite`
/// mode up front rather than read-only.
pub struct PickedFolder {
    pub dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub handles: HashMap<PathBuf, FileSystemFileHandle>,
    /// Every directory handle discovered so far, keyed by relative path.
    /// Seeded with the root (`dir`) plus every initially discovered child
    /// directory; extended as the user browses deeper (`web_fs::list_dir` via
    /// `app/web.rs::poll_dir_listing`).
    pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>,
}

/// Ask the user to pick a folder (`showDirectoryPicker`, requesting
/// `readwrite` so rating/edit sidecars can actually be written back into
/// it — see `PickedFolder::dir_handles`'s doc comment), then list its image
/// files and its immediate subdirectories in one `list_dir` pass, returning
/// `dir_handles` keyed by relative path (the root plus every first-level
/// subdirectory handle). One round trip — a cancelled picker or a listing
/// failure both come back as `Err`, so the caller doesn't need to
/// distinguish them (there's nothing more specific to do differently either
/// way: report the message and let the user try again).
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

/// One async `values()` scan of `handle`, split by entry kind. Image files
/// are filtered by `navigation::is_image`; subdirectories by
/// `navigation::is_listable_subdir` (hidden entries and macOS bundles
/// dropped, same as the native `list_subdirs`). Both lists are sorted
/// case-insensitively by file name, matching `Playlist::from_dir` /
/// `list_subdirs` ordering. `FileSystemDirectoryHandle` has no synchronous
/// listing — every browser directory read goes through this iterator.
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

/// `sort_by_name` for a `Vec<(PathBuf, T)>`, keyed on the path.
fn sort_pairs_by_name<T>(v: &mut [(PathBuf, T)]) {
    v.sort_by(|a, b| {
        let an = a.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let bn = b.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        an.cmp(&bn)
    });
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

/// Resolve a handle to its `File` without reading any of its contents — the
/// browser's nearest equivalent of `fs::metadata`. `File` carries `size` and
/// `last_modified`, which is everything `web_thumb_cache` needs to name a
/// photo's cache entry, so a cache hit never touches the source bytes.
pub async fn stat(handle: &FileSystemFileHandle) -> Result<web_sys::File, String> {
    Ok(JsFuture::from(handle.get_file())
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into())
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
