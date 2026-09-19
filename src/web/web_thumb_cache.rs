// SPDX-License-Identifier: GPL-3.0-or-later

//! wasm32-only: the thumbnail cache in `.lightphotos/`, read and written
//! through File System Access handles. Entry names come from `thumbnail.rs`,
//! so the native app and the browser share one cache per folder.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::{OsStr, OsString};
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemWritableFileStream,
};

use crate::web_catalog_fs::{is_not_found, js_error_string, sidecar_dir};

/// Shared by all writes to one directory while that folder is open. One
/// task at a time (`running`) builds the index and deletes old versions.
/// Other writers only enqueue names, so scans and deletes never overlap.
#[derive(Default)]
pub(crate) struct Cleanup {
    running: bool,
    indexed: bool,
    versions: HashMap<OsString, HashSet<String>>,
    pending: VecDeque<String>,
}

/// Cache key from a `File`'s size and modified time. `last_modified` is
/// already in milliseconds, the unit `thumbnail::cache_key` expects.
pub(crate) fn key_for(file: &web_sys::File) -> u64 {
    crate::thumbnail::cache_key(file.last_modified() as u64, file.size() as u64)
}

pub(crate) fn entry_name(filename: &OsStr, key: u64) -> String {
    crate::thumbnail::cache_name(filename, key)
        .to_string_lossy()
        .into_owned()
}

/// Read `root/.lightphotos/<name>`. Every failure returns `None`, because
/// the right response to any of them is to decode the source instead.
pub(crate) async fn load(root: &FileSystemDirectoryHandle, name: &str) -> Option<Vec<u8>> {
    let dir = sidecar_dir(root, false).await.ok()??;
    let handle: FileSystemFileHandle = match JsFuture::from(dir.get_file_handle(name)).await {
        Ok(v) => v.unchecked_into(),
        Err(_) => return None,
    };
    crate::web_fs::read_bytes(&handle).await.ok()
}

/// Write `bytes` to `root/.lightphotos/<name>`, then delete older cache
/// versions of the same photo. Creates `.lightphotos` only on the first
/// write. The write is atomic because the stream swaps the file in on
/// `close()`.
pub(crate) async fn store(
    root: &FileSystemDirectoryHandle,
    name: &str,
    bytes: &[u8],
    cleanup: &RefCell<Cleanup>,
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
    remove_previous_versions(&dir, name, cleanup).await;
    Ok(())
}

/// Call only after the new entry's write succeeded, so a failed write keeps
/// the old entry. The key is parsed from `stored_name`, so this needs no
/// extra metadata read of the source file.
async fn remove_previous_versions(
    dir: &FileSystemDirectoryHandle,
    stored_name: &str,
    cleanup: &RefCell<Cleanup>,
) {
    if crate::thumbnail::parse_cache_name(OsStr::new(stored_name)).is_none() {
        return;
    }
    {
        let mut state = cleanup.borrow_mut();
        state.pending.push_back(stored_name.to_owned());
        if state.running {
            return;
        }
        state.running = true;
    }
    if !cleanup.borrow().indexed {
        let versions = index_versions(dir).await;
        let mut state = cleanup.borrow_mut();
        state.versions = versions;
        state.indexed = true;
    }
    loop {
        let next = cleanup.borrow_mut().pending.pop_front();
        let Some(name) = next else { break };
        let (photo, _) = crate::thumbnail::parse_cache_name(OsStr::new(&name)).unwrap();
        let doomed = {
            let mut state = cleanup.borrow_mut();
            let versions = state.versions.entry(photo.clone()).or_default();
            versions.insert(name.clone());
            versions
                .iter()
                .filter(|old| **old != name)
                .cloned()
                .collect::<Vec<_>>()
        };
        for old in doomed {
            // Failed deletions stay indexed for a later successful write.
            if JsFuture::from(dir.remove_entry(&old)).await.is_ok() {
                cleanup
                    .borrow_mut()
                    .versions
                    .get_mut(&photo)
                    .unwrap()
                    .remove(&old);
            }
        }
    }
    cleanup.borrow_mut().running = false;
}

/// Best-effort. Entries missed after a browser error are collected by the
/// sweep on the next folder open.
async fn index_versions(dir: &FileSystemDirectoryHandle) -> HashMap<OsString, HashSet<String>> {
    let mut versions: HashMap<OsString, HashSet<String>> = HashMap::new();
    let iter = dir.values();
    loop {
        let Ok(promise) = iter.next() else { break };
        let Ok(next) = JsFuture::from(promise).await else {
            break;
        };
        if js_sys::Reflect::get(&next, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(true)
        {
            break;
        }
        let Ok(value) = js_sys::Reflect::get(&next, &"value".into()) else {
            continue;
        };
        let Ok(child) = value.dyn_into::<web_sys::FileSystemHandle>() else {
            continue;
        };
        let name = child.name();
        if let Some((photo, _)) = crate::thumbnail::parse_cache_name(OsStr::new(&name)) {
            versions.entry(photo).or_default().insert(name);
        }
    }
    versions
}

/// Run once when a folder opens. Deletes entries whose photo is not in
/// `live` or whose key no longer matches the photo's size and mtime. Costs
/// one `get_file()` per cached photo. If that fails, the photo's entries
/// stay for a later sweep. A failed delete leaves the file.
///
/// `reads` is the shared `MAX_CONCURRENT_READS` counter from `app/web.rs`.
/// Each `get_file()` holds one slot while it runs, so the sweep takes at
/// most one read from the grid. Too many concurrent reads make Chrome throw
/// `NotReadableError`.
pub(crate) async fn sweep_orphans(
    root: &FileSystemDirectoryHandle,
    live: &HashMap<OsString, FileSystemFileHandle>,
    reads: Rc<Cell<u32>>,
) {
    let Ok(Some(dir)) = sidecar_dir(root, false).await else {
        return;
    };

    let mut doomed: Vec<String> = Vec::new();
    let mut keys: HashMap<OsString, Option<u64>> = HashMap::new();
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
        // Skip sidecars and anything else that is not a cache entry.
        let Some((photo, key)) = crate::thumbnail::parse_cache_name(OsStr::new(&name)) else {
            continue;
        };
        let Some(handle) = live.get(&photo) else {
            doomed.push(name);
            continue;
        };
        let current_key = match keys.get(&photo) {
            Some(key) => *key,
            None => {
                reads.set(reads.get() + 1);
                let key = crate::web_fs::stat(handle).await.ok().map(|f| key_for(&f));
                reads.set(reads.get().saturating_sub(1));
                keys.insert(photo, key);
                key
            }
        };
        if current_key.is_some_and(|current| current != key) {
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
