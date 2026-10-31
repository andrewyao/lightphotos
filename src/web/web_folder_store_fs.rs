// SPDX-License-Identifier: MIT OR Apache-2.0

//! wasm32-only: sidecar I/O through File System Access directory handles.
//! The browser counterpart of the `std::fs` sidecar code in `folder_store.rs`,
//! which still owns the sidecar format.

use std::collections::HashMap;
use std::ffi::{OsStr, OsString};

use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetDirectoryOptions,
    FileSystemGetFileOptions, FileSystemHandleKind, FileSystemWritableFileStream,
};

use crate::persist::folder_store::group_file::{self, GROUPS_DIR};
use crate::persist::folder_store::SidecarLoad;
use crate::sidecar::{ImageRecord, SIDECAR_DIR, SIDECAR_EXT};
use crate::web::web_fs::{is_not_found, js_error_string};

/// The `.lightphotos` directory under `root`, created if `create` is set.
/// Returns `Ok(None)` when it is missing and `create` is false.
/// `web_thumb_cache.rs` stores thumbnails in the same directory.
pub(crate) async fn sidecar_dir(
    root: &FileSystemDirectoryHandle,
    create: bool,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    subdir(root, SIDECAR_DIR, create).await
}

/// `.lightphotos/groups` under `root`, on the same terms as `sidecar_dir`.
async fn groups_dir(
    root: &FileSystemDirectoryHandle,
    create: bool,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    match sidecar_dir(root, create).await? {
        Some(dir) => subdir(&dir, GROUPS_DIR, create).await,
        None => Ok(None),
    }
}

async fn subdir(
    parent: &FileSystemDirectoryHandle,
    name: &str,
    create: bool,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    let opts = FileSystemGetDirectoryOptions::new();
    opts.set_create(create);
    match JsFuture::from(parent.get_directory_handle_with_options(name, &opts)).await {
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

/// The files directly in `dir`, by name. Subdirectories are left out.
async fn files(dir: &FileSystemDirectoryHandle) -> Vec<(String, FileSystemFileHandle)> {
    let mut out = Vec::new();
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
        if child.kind() == FileSystemHandleKind::File {
            out.push((child.name(), child.unchecked_into()));
        }
    }
    out
}

/// Load every sidecar in `root/.lightphotos` and every group in its
/// `groups/`. Like the native loader, an unreachable directory gives an
/// empty result and a bad file counts toward `skipped` without stopping the
/// scan.
pub(crate) async fn load_sidecars(root: &FileSystemDirectoryHandle) -> SidecarLoad {
    let mut load = SidecarLoad {
        images: HashMap::new(),
        groups: Vec::new(),
        skipped: 0,
    };
    let dir = match sidecar_dir(root, false).await {
        Ok(Some(d)) => d,
        Ok(None) => return load,
        Err(e) => {
            web_sys::console::error_1(&format!("[web] listing .lightphotos failed: {e}").into());
            return load;
        }
    };

    for (name, file_handle) in files(&dir).await {
        let Some(stem) = strip_xmp(&name) else {
            continue;
        };
        match read_record(&file_handle).await {
            Ok(rec) => {
                if !rec.is_empty() {
                    load.images.insert(stem, rec);
                }
            }
            Err(e) => {
                web_sys::console::error_1(&format!("[web] unreadable sidecar {name}: {e}").into());
                load.skipped += 1;
            }
        }
    }

    let groups = match subdir(&dir, GROUPS_DIR, false).await {
        Ok(Some(g)) => g,
        Ok(None) => return load,
        Err(e) => {
            web_sys::console::error_1(&format!("[web] listing groups failed: {e}").into());
            return load;
        }
    };
    for (name, file_handle) in files(&groups).await {
        if !name.ends_with(".json") {
            continue;
        }
        let parsed = match group_file::id_of(&name) {
            Some(id) => crate::web::web_fs::read_bytes(&file_handle)
                .await
                .and_then(|bytes| group_file::parse(&bytes))
                .map(|saved| (id, saved)),
            None => Err("not a group id".to_string()),
        };
        match parsed {
            Ok(group) => load.groups.push(group),
            Err(e) => {
                web_sys::console::error_1(&format!("[web] unreadable group {name}: {e}").into());
                load.skipped += 1;
            }
        }
    }
    load
}

/// Write `bytes` to `dir/<name>`. The writable stream writes to a swap file
/// and replaces the real file on `close()`, so the write is atomic.
async fn write_file(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let opts = FileSystemGetFileOptions::new();
    opts.set_create(true);
    let file_handle: FileSystemFileHandle =
        JsFuture::from(dir.get_file_handle_with_options(name, &opts))
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

/// Delete `dir/<name>`. A file that is already gone is not an error.
async fn remove_name(dir: &FileSystemDirectoryHandle, name: &str) -> Result<(), String> {
    match JsFuture::from(dir.remove_entry(name)).await {
        Ok(_) => Ok(()),
        Err(e) if is_not_found(&e) => Ok(()),
        Err(e) => Err(js_error_string(&e)),
    }
}

/// Read and parse one sidecar file.
async fn read_record(file_handle: &FileSystemFileHandle) -> Result<ImageRecord, String> {
    let file = crate::web::web_fs::stat(file_handle).await?;
    // Checked before reading, so an oversized file is never loaded.
    if file.size() > crate::sidecar::MAX_SIDECAR_BYTES as f64 {
        return Err(format!("{} bytes, too large for a sidecar", file.size()));
    }
    let bytes = crate::web::web_fs::file_bytes(&file).await?;
    // Milliseconds; `as` saturates, so a bad value can't wrap.
    crate::sidecar::parse_record(&bytes, || Some((file.last_modified() / 1000.0) as u64))
}

/// Apply `change` to the record in `root/.lightphotos/<filename>.xmp`, or to
/// an empty one when there is none, and save the result.
///
/// A sidecar that can't be opened or read is an error, so its record is
/// never written over. One that reads but isn't a record we understand (it
/// doesn't parse, or is too large) is copied aside to
/// `<sidecar>.unreadable-<n>`, never deleted, and `change` goes onto
/// `fallback`, the cache's copy (or an empty record). That returns the
/// notice to give the user.
pub(crate) async fn update_sidecar(
    root: &FileSystemDirectoryHandle,
    filename: &OsStr,
    fallback: Option<ImageRecord>,
    change: impl FnOnce(&mut ImageRecord) -> Result<(), String>,
) -> Result<Option<String>, String> {
    let mut rec = ImageRecord::default();
    let mut notice = None;
    if let Some(dir) = sidecar_dir(root, false).await? {
        let name = xmp_name(filename);
        match JsFuture::from(dir.get_file_handle(&name)).await {
            Ok(handle) => {
                let handle: FileSystemFileHandle = handle.unchecked_into();
                let file = crate::web::web_fs::stat(&handle).await?;
                let bytes = crate::web::web_fs::file_bytes(&file).await?;
                // Milliseconds; `as` saturates, so a bad value can't wrap.
                let saved_at = || Some((file.last_modified() / 1000.0) as u64);
                match crate::sidecar::parse_record(&bytes, saved_at) {
                    Ok(read) => rec = read,
                    Err(why) => {
                        // Without the cache's copy there is nothing to save
                        // instead; the file is left as it is.
                        let Some(copy) = fallback else {
                            return Err(format!(
                                "its sidecar could not be read ({why}) and was left as it is"
                            ));
                        };
                        // The copy aside is written before the sidecar is.
                        let aside = keep_aside(&dir, &name, &bytes).await?;
                        rec = copy;
                        notice = Some(format!(
                            "{}'s sidecar could not be read ({why}); it was kept as {aside} and saved anew",
                            filename.to_string_lossy()
                        ));
                    }
                }
            }
            Err(e) if is_not_found(&e) => {}
            Err(e) => return Err(js_error_string(&e)),
        }
    }
    change(&mut rec)?;
    if rec.is_empty() {
        delete_sidecar(root, filename).await?;
    } else {
        let bytes = serde_json::to_vec_pretty(&rec).map_err(|e| e.to_string())?;
        write_sidecar(root, filename, &bytes).await?;
    }
    Ok(notice)
}

/// Copy `bytes`, an unreadable sidecar `name`, to the first free
/// `<name>.unreadable-<n>` in `dir`, and return that name.
async fn keep_aside(
    dir: &FileSystemDirectoryHandle,
    name: &str,
    bytes: &[u8],
) -> Result<String, String> {
    for n in 1..=1000u32 {
        let aside = format!("{name}.unreadable-{n}");
        match JsFuture::from(dir.get_file_handle(&aside)).await {
            Ok(_) => continue,
            Err(e) if is_not_found(&e) => {
                write_file(dir, &aside, bytes).await?;
                return Ok(aside);
            }
            Err(e) => return Err(js_error_string(&e)),
        }
    }
    Err(format!("could not keep {name} aside: no free name"))
}

/// Write `bytes` to `root/.lightphotos/<filename>.xmp`, creating the
/// directory if needed.
pub(crate) async fn write_sidecar(
    root: &FileSystemDirectoryHandle,
    filename: &OsStr,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = sidecar_dir(root, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos".to_string())?;
    write_file(&dir, &xmp_name(filename), bytes).await
}

/// Delete `root/.lightphotos/<filename>.xmp`. A missing directory or file
/// is not an error.
pub(crate) async fn delete_sidecar(
    root: &FileSystemDirectoryHandle,
    filename: &OsStr,
) -> Result<(), String> {
    match sidecar_dir(root, false).await? {
        Some(dir) => remove_name(&dir, &xmp_name(filename)).await,
        None => Ok(()),
    }
}

/// Write `bytes` to `root/.lightphotos/groups/<file>`, `file` being the
/// group file's own name, creating the directories if needed.
pub(crate) async fn write_group(
    root: &FileSystemDirectoryHandle,
    file: &OsStr,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = groups_dir(root, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos/groups".to_string())?;
    write_file(&dir, &file.to_string_lossy(), bytes).await
}

/// Delete `root/.lightphotos/groups/<file>`. A missing directory or file is
/// not an error.
pub(crate) async fn delete_group(
    root: &FileSystemDirectoryHandle,
    file: &OsStr,
) -> Result<(), String> {
    match groups_dir(root, false).await? {
        Some(dir) => remove_name(&dir, &file.to_string_lossy()).await,
        None => Ok(()),
    }
}

/// `.lightphotos/masks` under `root`, or `None` when it is missing.
async fn masks_dir(
    root: &FileSystemDirectoryHandle,
) -> Result<Option<FileSystemDirectoryHandle>, String> {
    match sidecar_dir(root, false).await? {
        Some(dir) => subdir(&dir, crate::develop::coverage::COVERAGE_DIR, false).await,
        None => Ok(None),
    }
}

/// The bytes of `root/.lightphotos/masks/<photo>.<id>.png`, or `Ok(None)`
/// when there is no such file. A file over the coverage size limit is
/// refused before it is read, like a sidecar.
pub(crate) async fn read_coverage(
    root: &FileSystemDirectoryHandle,
    photo: &OsStr,
    id: crate::develop::RasterId,
) -> Result<Option<Vec<u8>>, String> {
    let Some(dir) = masks_dir(root).await? else {
        return Ok(None);
    };
    let name = crate::develop::coverage::file_name(photo, id);
    let handle: FileSystemFileHandle =
        match JsFuture::from(dir.get_file_handle(&name.to_string_lossy())).await {
            Ok(v) => v.unchecked_into(),
            Err(e) if is_not_found(&e) => return Ok(None),
            Err(e) => return Err(js_error_string(&e)),
        };
    let file = crate::web::web_fs::stat(&handle).await?;
    if file.size() > crate::develop::coverage::MAX_COVERAGE_FILE_BYTES as f64 {
        return Err(format!(
            "{} bytes, too large for a coverage file",
            file.size()
        ));
    }
    crate::web::web_fs::file_bytes(&file).await.map(Some)
}

/// Write `bytes` to `root/.lightphotos/masks/<photo>.<id>.png`, creating
/// the directories if needed. Content-addressed, so a file already there
/// holds the same bytes and is left alone.
pub(crate) async fn write_coverage(
    root: &FileSystemDirectoryHandle,
    photo: &OsStr,
    id: crate::develop::RasterId,
    bytes: &[u8],
) -> Result<(), String> {
    let dir = sidecar_dir(root, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos".to_string())?;
    let dir = subdir(&dir, crate::develop::coverage::COVERAGE_DIR, true)
        .await?
        .ok_or_else(|| "could not create .lightphotos/masks".to_string())?;
    let name = crate::develop::coverage::file_name(photo, id);
    let name = name.to_string_lossy();
    if JsFuture::from(dir.get_file_handle(&name)).await.is_ok() {
        return Ok(());
    }
    write_file(&dir, &name, bytes).await
}

/// Permanently delete `dir/<filename>`. File System Access has no trash,
/// so this replaces `trash::move_to_trash` on web. A file that is already
/// gone is not an error.
pub(crate) async fn remove_file(
    dir: &FileSystemDirectoryHandle,
    filename: &OsStr,
) -> Result<(), String> {
    remove_name(dir, &filename.to_string_lossy()).await
}
