// SPDX-License-Identifier: MIT OR Apache-2.0

//! wasm32-only: folder picking, listing, and file reads through the File
//! System Access API. The browser counterpart of `navigation.rs`. A picked
//! folder has no OS path, only directory and file handles, and every
//! operation is async.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wasm_bindgen::prelude::wasm_bindgen;
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

/// Show the folder picker, keep the chosen folder for later visits, and
/// list its images and subdirectories. Requests `readwrite` so catalog
/// sidecars can be saved. A cancelled picker and a listing failure both
/// return `Err`.
pub async fn pick_and_list_folder() -> Result<PickedFolder, String> {
    let window = web_sys::window().ok_or("no window")?;
    // tools/web-bench hands over an OPFS folder here, since an automated
    // browser cannot drive the native picker.
    if let Ok(root) = js_sys::Reflect::get(&window, &"__lpTestRoot".into()) {
        if !root.is_undefined() {
            let handle: FileSystemDirectoryHandle = root.unchecked_into();
            let name = handle.name();
            return list_root(handle, name).await;
        }
    }
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
    // The Folders list needs this handle after a reload. Losing it costs
    // only that, so it doesn't fail the pick.
    let name = match JsFuture::from(keep_folder(&handle)).await {
        Ok(name) => name.as_string().unwrap_or_else(|| handle.name()),
        Err(e) => {
            web_sys::console::warn_1(
                &format!(
                    "[web] could not keep the folder for the next visit: {}",
                    js_error_string(&e)
                )
                .into(),
            );
            handle.name()
        }
    };
    list_root(handle, name).await
}

/// List a kept folder again. When the browser has dropped `readwrite`
/// access, `ask` asks for it again, which needs a click's user activation;
/// without `ask` that fails. Fails too when no folder of that name was kept,
/// access is denied, or the listing fails.
pub async fn reopen_saved_folder(name: String, ask: bool) -> Result<PickedFolder, String> {
    let handle: FileSystemDirectoryHandle = JsFuture::from(granted_folder(&name, ask))
        .await
        .map_err(|e| js_error_string(&e))?
        .unchecked_into();
    list_root(handle, name).await
}

/// The names of the folders kept from earlier visits, each the first
/// component of its photos' paths. Reading them needs no permission.
pub async fn saved_folder_names() -> Result<Vec<PathBuf>, String> {
    let names = JsFuture::from(saved_folder_names_js())
        .await
        .map_err(|e| js_error_string(&e))?;
    Ok(js_sys::Array::from(&names)
        .iter()
        .filter_map(|n| n.as_string())
        .map(PathBuf::from)
        .collect())
}

/// Stop keeping a folder. Its files stay as they are.
pub async fn forget_folder(name: String) {
    if let Err(e) = JsFuture::from(forget_folder_js(&name)).await {
        web_sys::console::warn_1(
            &format!("[web] could not forget the folder: {}", js_error_string(&e)).into(),
        );
    }
}

/// List `handle` as the root called `name`, which is unique among the
/// kept folders so two folders both called Photos don't share paths.
async fn list_root(
    handle: FileSystemDirectoryHandle,
    name: String,
) -> Result<PickedFolder, String> {
    let root = PathBuf::from(name);
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

// A directory handle survives a reload only in IndexedDB, which stores it by
// structured clone. `localStorage` holds only strings. Each kept folder is
// stored under `folder:<name>`; before there were several, the one kept
// folder sat under `root`, which the first read moves.
#[wasm_bindgen(inline_js = r#"
const STORE = "handles";
const PREFIX = "folder:";
const LEGACY = "root";
function openDb() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open("lightphotos-folders", 1);
    req.onupgradeneeded = () => req.result.createObjectStore(STORE);
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
function request(req) {
  return new Promise((resolve, reject) => {
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}
// One transaction. `body` must issue its requests without awaiting anything
// else, or the transaction commits under it.
async function run(mode, body) {
  const db = await openDb();
  try {
    const tx = db.transaction(STORE, mode);
    const done = new Promise((resolve, reject) => {
      tx.oncomplete = () => resolve();
      tx.onerror = () => reject(tx.error);
      tx.onabort = () => reject(tx.error);
    });
    const out = await body(tx.objectStore(STORE));
    await done;
    return out;
  } finally {
    db.close();
  }
}
function uniqueName(base, taken) {
  if (!taken.includes(base)) return base;
  for (let n = 2; ; n++) {
    const name = `${base} (${n})`;
    if (!taken.includes(name)) return name;
  }
}
// Every kept folder as [name, handle], after moving the legacy entry.
async function keptFolders() {
  const [keys, handles] = await run("readonly", (s) =>
    Promise.all([request(s.getAllKeys()), request(s.getAll())]));
  const kept = [];
  let legacy = null;
  keys.forEach((key, i) => {
    if (key === LEGACY) legacy = handles[i];
    else if (typeof key === "string" && key.startsWith(PREFIX)) {
      kept.push([key.slice(PREFIX.length), handles[i]]);
    }
  });
  if (legacy) {
    const name = uniqueName(legacy.name, kept.map(([n]) => n));
    await run("readwrite", (s) => {
      s.put(legacy, PREFIX + name);
      s.delete(LEGACY);
    });
    kept.push([name, legacy]);
  }
  return kept;
}
export async function savedFolderNames() {
  return (await keptFolders()).map(([name]) => name);
}
// The kept name of `handle`'s folder, keeping it under a new unique name
// if it isn't kept yet.
export async function keepFolder(handle) {
  const kept = await keptFolders();
  for (const [name, other] of kept) {
    if (await other.isSameEntry(handle)) return name;
  }
  const name = uniqueName(handle.name, kept.map(([n]) => n));
  await run("readwrite", (s) => { s.put(handle, PREFIX + name); });
  return name;
}
export async function grantedFolder(name, ask) {
  await keptFolders();
  const handle = await run("readonly", (s) => request(s.get(PREFIX + name)));
  if (!handle) throw new Error("no saved folder");
  const mode = { mode: "readwrite" };
  if ((await handle.queryPermission(mode)) !== "granted"
      && (!ask || (await handle.requestPermission(mode)) !== "granted")) {
    throw new Error("folder access denied");
  }
  return handle;
}
export async function forgetFolder(name) {
  await run("readwrite", (s) => { s.delete(PREFIX + name); });
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = keepFolder)]
    fn keep_folder(handle: &FileSystemDirectoryHandle) -> js_sys::Promise;
    #[wasm_bindgen(js_name = grantedFolder)]
    fn granted_folder(name: &str, ask: bool) -> js_sys::Promise;
    #[wasm_bindgen(js_name = savedFolderNames)]
    fn saved_folder_names_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = forgetFolder)]
    fn forget_folder_js(name: &str) -> js_sys::Promise;
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
            FileSystemHandleKind::File if is_image(&path) => {
                images.push((path, child.unchecked_into()));
            }
            FileSystemHandleKind::Directory if is_listable_subdir(&name) => {
                subdirs.push((path, child.unchecked_into()));
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

/// The URL directory the app's own wasm was served from, with no trailing
/// slash. The site serves the app under `/app/`, so the origin alone is
/// wrong. It comes from the `<link rel="modulepreload">` trunk emits, and
/// falls back to the origin.
pub(crate) fn asset_dir() -> String {
    let Some(window) = web_sys::window() else {
        return String::new();
    };
    let origin = window.location().origin().unwrap_or_default();
    let href = window
        .document()
        .and_then(|d| d.query_selector("link[rel=modulepreload]").ok().flatten())
        .and_then(|el| el.get_attribute("href"));
    match href.as_deref().and_then(|h| h.rfind('/').map(|i| &h[..i])) {
        Some(dir) => format!("{origin}{dir}"),
        None => origin,
    }
}
