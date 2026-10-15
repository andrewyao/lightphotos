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

use crate::decode::image_decode::is_image_extension;
use crate::navigation::is_listable_subdir;

/// A folder picked via `showDirectoryPicker`, listed one level deep.
/// `dir` is the handle's name used as a label, not a real path. All paths
/// are relative to it.
pub struct PickedFolder {
    pub dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub handles: HashMap<PathBuf, FileSystemFileHandle>,
    /// Directory handles by relative path: the root, its immediate
    /// subdirectories, and any deeper folder the user browses into.
    /// `web_folder_store_fs.rs` needs these to write `.lightphotos` sidecars.
    pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>,
}

/// Show the folder picker, keep the chosen folder for later visits, and
/// list its images and subdirectories. Requests `readwrite` so folder store
/// sidecars can be saved. A cancelled picker and a listing failure both
/// return `Err`.
pub async fn pick_and_list_folder() -> Result<PickedFolder, String> {
    let window = web_sys::window().ok_or("no window")?;
    // tools/web-bench hands over an OPFS folder here, since an automated
    // browser cannot drive the native picker. It is kept like a picked one,
    // so a reload finds it.
    let test_root = js_sys::Reflect::get(&window, &"__lpTestRoot".into())
        .ok()
        .filter(|root| !root.is_undefined());
    let handle: FileSystemDirectoryHandle = match test_root {
        Some(root) => root.unchecked_into(),
        None => {
            let opts = DirectoryPickerOptions::new();
            opts.set_mode(FileSystemPermissionMode::Readwrite);
            JsFuture::from(
                window
                    .show_directory_picker_with_options(&opts)
                    .map_err(|e| js_error_string(&e))?,
            )
            .await
            .map_err(|e| js_error_string(&e))?
            .unchecked_into()
        }
    };
    // The Folders list needs this handle after a reload. Losing it costs
    // only that, so it doesn't fail the pick. The keep runs in IndexedDB
    // while the listing runs: the promise starts as soon as it is made, and
    // `JsFuture` attaches its handlers now, so a keep that fails after a
    // failed listing is no unhandled rejection.
    let kept = JsFuture::from(keep_folder(&handle));
    let listing = list_dir(Path::new(""), &handle).await?;
    let name = match kept.await {
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
    Ok(picked_folder(PathBuf::from(name), handle, listing))
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

/// List `handle` as the root called `name`, which is unique among the
/// kept folders so two folders both called Photos don't share paths.
async fn list_root(
    handle: FileSystemDirectoryHandle,
    name: String,
) -> Result<PickedFolder, String> {
    let listing = list_dir(Path::new(""), &handle).await?;
    Ok(picked_folder(PathBuf::from(name), handle, listing))
}

/// `handle`'s `listing`, taken relative to the folder itself, as the root
/// called `root`.
fn picked_folder(
    root: PathBuf,
    handle: FileSystemDirectoryHandle,
    listing: DirListing,
) -> PickedFolder {
    let mut handles = HashMap::new();
    let mut entries = Vec::with_capacity(listing.images.len());
    for (path, fh) in listing.images {
        let path = root.join(path);
        handles.insert(path.clone(), fh);
        entries.push(path);
    }

    let mut dir_handles = HashMap::new();
    for (p, h) in listing.subdirs {
        dir_handles.insert(root.join(p), h);
    }
    dir_handles.insert(root.clone(), handle);

    PickedFolder {
        dir: root,
        entries,
        handles,
        dir_handles,
    }
}

// A directory handle survives a reload only in IndexedDB, which stores it by
// structured clone. `localStorage` holds only strings. Each kept folder is
// stored under `folder:<name>`; before there were several, the one kept
// folder sat under `root`, which the first read moves. A preset folder is
// kept under `preset:<name>`, apart from the photo folders, so forgetting a
// photo folder never takes a preset folder's handle with it.
#[wasm_bindgen(inline_js = r#"
const STORE = "handles";
const PREFIX = "folder:";
const PRESET = "preset:";
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
// The kept name of preset folder `handle`, keeping it under a new unique
// name if it isn't kept yet.
export async function keepPresetFolder(handle) {
  const [keys, handles] = await run("readonly", (s) =>
    Promise.all([request(s.getAllKeys()), request(s.getAll())]));
  const kept = [];
  keys.forEach((key, i) => {
    if (typeof key === "string" && key.startsWith(PRESET)) {
      kept.push([key.slice(PRESET.length), handles[i]]);
    }
  });
  for (const [name, other] of kept) {
    if (await other.isSameEntry(handle)) return name;
  }
  const name = uniqueName(handle.name, kept.map(([n]) => n));
  await run("readwrite", (s) => { s.put(handle, PRESET + name); });
  return name;
}
// Kept preset folder `name` with read access, or null while the browser
// withholds it. Asking needs a click's user activation.
export async function grantedPresetFolder(name, ask) {
  const handle = await run("readonly", (s) => request(s.get(PRESET + name)));
  if (!handle) throw new Error("no saved preset folder");
  const mode = { mode: "read" };
  if ((await handle.queryPermission(mode)) === "granted") return handle;
  if (ask && (await handle.requestPermission(mode)) === "granted") return handle;
  return null;
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = keepFolder)]
    fn keep_folder(handle: &FileSystemDirectoryHandle) -> js_sys::Promise;
    #[wasm_bindgen(js_name = grantedFolder)]
    fn granted_folder(name: &str, ask: bool) -> js_sys::Promise;
    #[wasm_bindgen(js_name = savedFolderNames)]
    fn saved_folder_names_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = keepPresetFolder)]
    fn keep_preset_folder(handle: &FileSystemDirectoryHandle) -> js_sys::Promise;
    #[wasm_bindgen(js_name = grantedPresetFolder)]
    fn granted_preset_folder(name: &str, ask: bool) -> js_sys::Promise;
}

/// Show the folder picker for a folder of Lightroom presets and keep it for
/// later visits. Returns its kept name, which the catalog lists in place of
/// a path. Read access is all a scan needs.
pub async fn pick_preset_folder() -> Result<PathBuf, String> {
    let window = web_sys::window().ok_or("no window")?;
    let opts = DirectoryPickerOptions::new();
    opts.set_mode(FileSystemPermissionMode::Read);
    let handle: FileSystemDirectoryHandle = JsFuture::from(
        window
            .show_directory_picker_with_options(&opts)
            .map_err(|e| js_error_string(&e))?,
    )
    .await
    .map_err(|e| js_error_string(&e))?
    .unchecked_into();
    let name = JsFuture::from(keep_preset_folder(&handle))
        .await
        .map_err(|e| js_error_string(&e))?;
    Ok(PathBuf::from(
        name.as_string().unwrap_or_else(|| handle.name()),
    ))
}

/// Kept preset folder `name`, or `None` while the browser withholds read
/// access. `ask` asks for it, which needs a click's user activation.
pub async fn reopen_preset_folder(
    name: &str,
    ask: bool,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    let handle = JsFuture::from(granted_preset_folder(name, ask))
        .await
        .map_err(|e| js_error_string(&e))?;
    Ok((!handle.is_null()).then(|| handle.unchecked_into()))
}

/// Walks preset folder `root` through `handle`, by the rules and limits the
/// native walker uses. The browser has no symlinks, so the depth limit is
/// the only bound a walk needs.
pub async fn scan_preset_folder(
    root: &Path,
    handle: FileSystemDirectoryHandle,
    cancel: &std::sync::atomic::AtomicBool,
) -> crate::persist::preset_folders::Scan {
    use crate::persist::preset_folders::{is_preset_file, read_preset, skips_dir, Scan, LIMITS};
    let mut out = Scan::default();
    let mut files = 0usize;
    let mut queue = std::collections::VecDeque::from([(PathBuf::new(), handle, 0usize)]);
    'walk: while let Some((rel, dir, depth)) = queue.pop_front() {
        let Ok(children) = children(&dir).await else {
            continue;
        };
        for (name, child) in children {
            if cancel.load(std::sync::atomic::Ordering::Relaxed) {
                break 'walk;
            }
            let path = rel.join(&name);
            match child.kind() {
                FileSystemHandleKind::Directory => {
                    if depth < LIMITS.depth && !skips_dir(&name) {
                        queue.push_back((path, child.unchecked_into(), depth.saturating_add(1)));
                    }
                }
                FileSystemHandleKind::File if is_preset_file(&name) => {
                    if files >= LIMITS.files {
                        break 'walk;
                    }
                    files = files.saturating_add(1);
                    let file = child.unchecked_into::<FileSystemFileHandle>();
                    let preset = match stat(&file).await {
                        Ok(f) if f.size() <= LIMITS.file_bytes as f64 => file_bytes(&f)
                            .await
                            .ok()
                            .and_then(|bytes| read_preset(root, &path, &bytes)),
                        _ => None,
                    };
                    match preset {
                        Some(p) => out.presets.push(p),
                        None => out.skipped = out.skipped.saturating_add(1),
                    }
                }
                _ => {}
            }
        }
    }
    out.sort();
    out
}

/// Every entry of `handle` by name, in name order.
async fn children(
    handle: &FileSystemDirectoryHandle,
) -> Result<Vec<(String, web_sys::FileSystemHandle)>, String> {
    let mut out = Vec::new();
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
        out.push((child.name(), child));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// The image files and immediate subdirectories of one directory handle.
/// Paths are relative to the picked root (`base` is this directory's own
/// relative path; children are `base.join(child_name)`).
pub struct DirListing {
    pub images: Vec<(PathBuf, FileSystemFileHandle)>,
    pub subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)>,
}

/// List `handle` with the same filters as the native listing (`is_image_extension`,
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
            FileSystemHandleKind::File if is_image_extension(&path) => {
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
    file_bytes(&stat(handle).await?).await
}

/// The contents of a `File` already resolved with [`stat`].
pub async fn file_bytes(file: &web_sys::File) -> Result<Vec<u8>, String> {
    let buf = JsFuture::from(file.array_buffer())
        .await
        .map_err(|e| js_error_string(&e))?;
    Ok(js_sys::Uint8Array::new(&buf).to_vec())
}

/// The `message` of a thrown JS value (usually a `DOMException` such as
/// `AbortError` on picker cancel), or its debug form.
pub(crate) fn js_error_string(e: &JsValue) -> String {
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
