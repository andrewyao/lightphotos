// SPDX-License-Identifier: MIT OR Apache-2.0

//! wasm32-only: the browser's catalogs in IndexedDB, the counterpart of the
//! native `<name>.lightphotos` directories. DB `lightphotos-catalogs` holds
//! two stores: `catalogs`, a catalog's name to `{v, folders}` and
//! `presets\0<name>` to its preset folders' `{v, folders}`, and `marks`,
//! `<name>\0<dir>` to `{v, dir, photos}`. The values are the same JSON the
//! native catalog writes, put as objects. Rust parses what it reads back, so
//! a value edited by hand is an error there, not here.

use std::path::PathBuf;

use wasm_bindgen::prelude::wasm_bindgen;
use wasm_bindgen_futures::JsFuture;

use crate::web::web_fs::js_error_string;

/// One of the catalog database's object stores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Store {
    Catalogs,
    Marks,
}

impl Store {
    fn js(self) -> &'static str {
        match self {
            Store::Catalogs => "catalogs",
            Store::Marks => "marks",
        }
    }
}

// `forgetFolderIfUnlisted` and `forgetPresetFolderIfUnlisted` reach into
// `web_fs`'s `lightphotos-folders` database, so its name, store and
// `folder:` and `preset:` key prefixes must match that module's.
#[wasm_bindgen(inline_js = r#"
const DB = "lightphotos-catalogs";
const CATALOGS = "catalogs";
const MARKS = "marks";
function open(name, stores) {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(name, 1);
    req.onupgradeneeded = () => {
      for (const store of stores) req.result.createObjectStore(store);
    };
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
// One transaction on `store` of the open `db`. `body` must issue its
// requests without awaiting anything else, or the transaction commits
// under it.
async function txn(db, store, mode, body) {
  const tx = db.transaction(store, mode);
  const done = new Promise((resolve, reject) => {
    tx.oncomplete = () => resolve();
    tx.onerror = () => reject(tx.error);
    tx.onabort = () => reject(tx.error);
  });
  const [out] = await Promise.all([body(tx.objectStore(store)), done]);
  return out;
}
async function run(store, mode, body) {
  const db = await open(DB, [CATALOGS, MARKS]);
  try {
    return await txn(db, store, mode, body);
  } finally {
    db.close();
  }
}
export async function catalogNames() {
  const keys = await run(CATALOGS, "readonly", (s) => request(s.getAllKeys()));
  return keys.filter((k) => typeof k === "string");
}
// The stored record as JSON, or null when there is none.
export async function getCatalog(name) {
  const value = await run(CATALOGS, "readonly", (s) => request(s.get(name)));
  return value === undefined ? null : String(JSON.stringify(value));
}
export async function putValue(store, key, json) {
  const value = JSON.parse(json);
  await run(store, "readwrite", (s) => request(s.put(value, key)));
}
export async function deleteValue(store, key) {
  await run(store, "readwrite", (s) => request(s.delete(key)));
}
// Forget the kept handle of `folder` unless a catalog other than `name`
// lists it. True when it was forgotten. The check and the forget run here
// together, so nothing on the Rust side can come between them.
export async function forgetFolderIfUnlisted(name, folder) {
  const [keys, values] = await run(CATALOGS, "readonly", (s) =>
    Promise.all([request(s.getAllKeys()), request(s.getAll())]));
  const lists = (v) => Array.isArray(v?.folders) && v.folders.some((f) =>
    typeof f === "string" && (f === folder || f.startsWith(folder + "/")));
  if (keys.some((k, i) => k !== name && !k.includes("\0") && lists(values[i]))) return false;
  const db = await open("lightphotos-folders", ["handles"]);
  try {
    await txn(db, "handles", "readwrite", (s) => request(s.delete("folder:" + folder)));
  } finally {
    db.close();
  }
  return true;
}
export async function forgetPresetFolderIfUnlisted(name, folder) {
  const [keys, values] = await run(CATALOGS, "readonly", (s) =>
    Promise.all([request(s.getAllKeys()), request(s.getAll())]));
  const mine = "presets\0" + name;
  const lists = (v) => Array.isArray(v?.folders) && v.folders.includes(folder);
  if (keys.some((k, i) => typeof k === "string" && k.startsWith("presets\0")
      && k !== mine && lists(values[i]))) return false;
  const db = await open("lightphotos-folders", ["handles"]);
  try {
    await txn(db, "handles", "readwrite", (s) => request(s.delete("preset:" + folder)));
  } finally {
    db.close();
  }
  return true;
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = catalogNames)]
    fn catalog_names_js() -> js_sys::Promise;
    #[wasm_bindgen(js_name = getCatalog)]
    fn get_catalog_js(name: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = putValue)]
    fn put_value_js(store: &str, key: &str, json: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = deleteValue)]
    fn delete_value_js(store: &str, key: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = forgetFolderIfUnlisted)]
    fn forget_folder_if_unlisted_js(name: &str, folder: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = forgetPresetFolderIfUnlisted)]
    fn forget_preset_folder_if_unlisted_js(name: &str, folder: &str) -> js_sys::Promise;
}

async fn settle(promise: js_sys::Promise) -> Result<wasm_bindgen::JsValue, String> {
    JsFuture::from(promise)
        .await
        .map_err(|e| js_error_string(&e))
}

/// Every stored catalog's name, unsorted.
pub(crate) async fn catalog_names() -> Result<Vec<String>, String> {
    let names = settle(catalog_names_js()).await?;
    Ok(js_sys::Array::from(&names)
        .iter()
        .filter_map(|n| n.as_string())
        .collect())
}

/// Catalog `name`'s stored record as JSON, `None` when there is none.
pub(crate) async fn get_record(name: &str) -> Result<Option<Vec<u8>>, String> {
    Ok(settle(get_catalog_js(name))
        .await?
        .as_string()
        .map(String::into_bytes))
}

/// Store `json` at `key`, replacing what was there.
pub(crate) async fn put(store: Store, key: &str, json: &str) -> Result<(), String> {
    settle(put_value_js(store.js(), key, json)).await.map(drop)
}

pub(crate) async fn delete(store: Store, key: &str) -> Result<(), String> {
    settle(delete_value_js(store.js(), key)).await.map(drop)
}

/// Stop keeping preset folder `folder`'s handle unless a catalog other than
/// `name` lists it. Its files stay as they are.
pub(crate) async fn forget_preset_folder_if_unlisted(name: String, folder: PathBuf) {
    let folder = folder.to_string_lossy();
    if let Err(e) = settle(forget_preset_folder_if_unlisted_js(&name, &folder)).await {
        web_sys::console::warn_1(
            &format!("[catalog] could not forget the preset folder {folder}: {e}").into(),
        );
    }
}

/// Stop keeping `folder`'s handle unless a catalog other than `name` lists
/// it. Its files stay as they are.
pub(crate) async fn forget_folder_if_unlisted(name: String, folder: PathBuf) {
    let folder = folder.to_string_lossy();
    match settle(forget_folder_if_unlisted_js(&name, &folder)).await {
        Ok(forgot) if forgot.as_bool() == Some(false) => web_sys::console::log_1(
            &format!("[catalog] another catalog lists {folder}, so its access is kept").into(),
        ),
        Ok(_) => {}
        Err(e) => web_sys::console::warn_1(
            &format!("[catalog] could not forget the folder {folder}: {e}").into(),
        ),
    }
}
