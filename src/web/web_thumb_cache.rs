// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: File System Access counterpart to `thumbnail.rs`'s std::fs-
//! backed `ThumbCache`. A picked folder has no real OS path, so everything
//! here goes through the folder's `FileSystemDirectoryHandle`, exactly as
//! `web_catalog_fs.rs` does for ratings sidecars — and into the same
//! `.lightphotos/` directory, which that module's `sidecar_dir` resolves.
//!
//! The filenames come from `thumbnail.rs`'s own `cache_key`/`cache_name`, not
//! from a second convention defined here, so a folder cached by the native
//! app populates instantly in the browser and the reverse. This module only
//! supplies the browser-specific transport.
//!
//! ## Pipeline position
//! - Pipeline 2 (Grid/filmstrip). `app/web.rs`'s `request_web_thumbs` calls
//!   [`load`] before reading a photo at all; on a hit the multi-megabyte
//!   source is never touched and a ~45 KB JPEG is decoded instead.
//! - On a miss the worker decodes the source and encodes the JPEG alongside
//!   it (`wasm_worker.rs`), and `poll_web_thumbs` hands those bytes to
//!   [`store`]. Encoding in the worker keeps it off the single main thread.

use std::ffi::OsStr;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemWritableFileStream,
};

use crate::web_catalog_fs::{is_not_found, js_error_string, sidecar_dir};

/// The metadata a cache lookup keys on: a `File`'s size and last-modified
/// time, the browser's equivalent of native's `fs::metadata`. Obtained from
/// `FileSystemFileHandle::get_file()`, which resolves the file object without
/// reading a single byte of its contents.
pub(crate) fn key_for(file: &web_sys::File) -> u64 {
    // `last_modified` is milliseconds since the epoch, which is exactly what
    // `thumbnail::cache_key` hashes — native truncates its own nanosecond
    // mtime to match. See that function's doc comment on why.
    crate::thumbnail::cache_key(file.last_modified() as u64, file.size() as u64)
}

/// The cache entry name for the photo `filename` at `key` — `thumbnail.rs`'s
/// naming, lossily converted, the same way `web_catalog_fs::xmp_name` handles
/// sidecar names.
pub(crate) fn entry_name(filename: &OsStr, key: u64) -> String {
    crate::thumbnail::cache_name(filename, key)
        .to_string_lossy()
        .into_owned()
}

/// Read `root/.lightphotos/<entry_name>` if it exists. `None` covers both a
/// missing `.lightphotos` and a missing entry within it — a cache miss is not
/// an error, same as native's "no file, decode the source".
///
/// Errors are swallowed to `None` deliberately: every failure mode here
/// (permission lost, corrupt entry, quota) has the same correct response,
/// which is to decode the source instead.
pub(crate) async fn load(root: &FileSystemDirectoryHandle, name: &str) -> Option<Vec<u8>> {
    let dir = sidecar_dir(root, false).await.ok()??;
    let handle: FileSystemFileHandle = match JsFuture::from(dir.get_file_handle(name)).await {
        Ok(v) => v.unchecked_into(),
        Err(_) => return None,
    };
    crate::web_fs::read_bytes(&handle).await.ok()
}

/// Write `bytes` to `root/.lightphotos/<name>`, creating `.lightphotos` if
/// this is the folder's first entry — the same lazy creation native does, so
/// browsing a folder without ever filling the grid leaves no trace.
///
/// `FileSystemWritableFileStream::close()` swaps the file in atomically on
/// supporting browsers, which is the same guarantee native gets from its
/// explicit temp-file-plus-rename.
pub(crate) async fn store(
    root: &FileSystemDirectoryHandle,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = sidecar_dir(root, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos".to_string())?;

    let opts = FileSystemGetFileOptions::new();
    opts.set_create(true);
    let handle: FileSystemFileHandle =
        JsFuture::from(dir.get_file_handle_with_options(name, &opts))
            .await
            .map_err(|e| js_error_string(&e))?
            .unchecked_into();

    let writable: FileSystemWritableFileStream = JsFuture::from(handle.create_writable())
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

/// Delete every entry in `root/.lightphotos/` whose photo is no longer in
/// `live` — the browser half of `thumbnail::sweep_orphans`, run once when a
/// folder opens.
///
/// Narrower than native's sweep, which also drops entries whose photo changed:
/// deciding that needs each photo's size and mtime, and on File System Access
/// that is a `get_file()` per photo rather than a `stat`. Photos are never
/// modified in place through the browser build, so the case native catches
/// barely arises here; a folder edited natively and then opened on the web
/// keeps a stale entry until it is opened natively again.
///
/// Best-effort throughout: a failed delete just leaves the file.
pub(crate) async fn sweep_orphans(root: &FileSystemDirectoryHandle, live: &[String]) {
    let Ok(Some(dir)) = sidecar_dir(root, false).await else {
        return; // no .lightphotos yet — nothing was ever cached here
    };

    let mut doomed: Vec<String> = Vec::new();
    let iter = dir.values();
    loop {
        let Ok(promise) = iter.next() else { break };
        let Ok(next) = JsFuture::from(promise).await else {
            break;
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
        let name = child.name();
        // Anything `parse_cache_name` rejects is not ours — sidecars above
        // all — and is left alone.
        let Some((photo, _)) = crate::thumbnail::parse_cache_name(OsStr::new(&name)) else {
            continue;
        };
        if !live.iter().any(|p| OsStr::new(p) == photo) {
            doomed.push(name);
        }
    }

    for name in doomed {
        if let Err(e) = JsFuture::from(dir.remove_entry(&name)).await {
            if !is_not_found(&e) {
                web_sys::console::error_1(
                    &format!("[web] could not evict {name}: {}", js_error_string(&e)).into(),
                );
            }
        }
    }
}
