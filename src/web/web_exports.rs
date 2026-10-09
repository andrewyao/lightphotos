// SPDX-License-Identifier: MIT OR Apache-2.0

//! wasm32: the main thread's half of a batch export. The loader's threads
//! bake each JPEG, but the folder it goes to is a File System Access handle,
//! a JS object that cannot cross to another thread. So each export's
//! destination waits here under the job's id until its JPEG comes back.
//! Cloning shares the table, so `spawn_local` futures can submit.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use web_sys::FileSystemDirectoryHandle;

use crate::develop::{Adjustments, TouchUp};
use crate::loader::WebDecoder;

/// Most exports in flight at once. Every thread shares one wasm heap of at
/// most 4 GB, and a full-resolution RAW bake peaks somewhere near 1 GB, so
/// this is kept below what the old pool of separate heaps allowed.
const MAX_IN_FLIGHT: usize = 2;

/// A finished or failed export and where its JPEG goes.
pub struct ExportResult {
    pub path: PathBuf,
    pub folder: FileSystemDirectoryHandle,
    pub dest_dir: PathBuf,
    pub filename: String,
    pub result: Result<Vec<u8>, String>,
}

struct Dest {
    folder: FileSystemDirectoryHandle,
    dest_dir: PathBuf,
    filename: String,
}

#[derive(Clone, Default)]
pub struct WebExports {
    dests: Rc<RefCell<HashMap<u64, Dest>>>,
    next_id: Rc<Cell<u64>>,
    /// Exports that failed before they reached a thread.
    failed: Rc<RefCell<Vec<ExportResult>>>,
}

pub struct Export {
    pub path: PathBuf,
    pub folder: FileSystemDirectoryHandle,
    pub dest_dir: PathBuf,
    pub filename: String,
    pub bytes: js_sys::ArrayBuffer,
    pub is_raw: bool,
    pub adj: Adjustments,
    pub touchups: Vec<TouchUp>,
    pub rot: u8,
    pub max_px: u32,
}

impl WebExports {
    pub fn in_flight(&self) -> usize {
        self.dests.borrow().len()
    }

    /// How many exports may run at once. One thread is left for the grid
    /// and the Loupe.
    pub fn capacity(&self, decoder: &WebDecoder) -> usize {
        decoder.threads().saturating_sub(1).clamp(1, MAX_IN_FLIGHT)
    }

    pub fn submit(&self, decoder: &WebDecoder, export: Export) {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        self.dests.borrow_mut().insert(
            id,
            Dest {
                folder: export.folder,
                dest_dir: export.dest_dir,
                filename: export.filename,
            },
        );
        decoder.submit_export(crate::web::web_decode::WebExportJob {
            id,
            path: export.path,
            bytes: js_sys::Uint8Array::new(&export.bytes).to_vec(),
            is_raw: export.is_raw,
            adj: export.adj,
            touchups: export.touchups,
            rot: export.rot,
            max_px: export.max_px,
        });
    }

    /// Report an export that failed before it was submitted, such as a
    /// failed source read.
    pub fn fail(
        &self,
        path: PathBuf,
        folder: FileSystemDirectoryHandle,
        dest_dir: PathBuf,
        filename: String,
        error: String,
    ) {
        self.failed.borrow_mut().push(ExportResult {
            path,
            folder,
            dest_dir,
            filename,
            result: Err(error),
        });
    }

    /// Pairs the loader's finished exports with their destinations, plus
    /// any that failed before submit.
    pub fn land(
        &self,
        finished: Vec<(u64, PathBuf, Result<Vec<u8>, String>)>,
    ) -> Vec<ExportResult> {
        let mut out = std::mem::take(&mut *self.failed.borrow_mut());
        let mut dests = self.dests.borrow_mut();
        for (id, path, result) in finished {
            let Some(Dest {
                folder,
                dest_dir,
                filename,
            }) = dests.remove(&id)
            else {
                continue;
            };
            out.push(ExportResult {
                path,
                folder,
                dest_dir,
                filename,
                result,
            });
        }
        out
    }
}
