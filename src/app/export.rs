use super::*;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;
// Instant comes from `super::*` (app/mod.rs re-exports web_time::Instant,
// not std::time::Instant — see loader.rs's launched_at() doc comment).

#[cfg(not(target_arch = "wasm32"))]
use crate::export::ExportJob;
use crate::export::ExportOutcome;
#[cfg(not(target_arch = "wasm32"))]
use crate::paths;

impl App {
    // ---- Export ----

    /// Export the selected image to a baked JPG (crop/rotation/develop applied)
    /// in the folder's `Exports/` subfolder. Runs in the background.
    pub(super) fn export_selected(&mut self) {
        match self.selected_path() {
            Some(path) => self.start_export(vec![path]),
            None => {
                self.set_status("Export: no image selected".into());
                self.request_redraw();
            }
        }
    }

    /// Export every selected photo to a baked JPG in the folder's `Exports/`
    /// subfolder, in the background.
    pub(super) fn export_selection(&mut self) {
        self.start_export(self.selected_paths());
    }

    /// wasm32 export: the same `bake_jpeg` pipeline native runs, on the Web
    /// Worker pool (`JobKind::Export`), with the JPEG written back through a
    /// File System Access writable stream (`web_export_fs::WebFs`). Source
    /// reads and the collision-free target scan are async, so the whole batch
    /// setup runs in one `spawn_local`; `main.rs`'s frame loop drains
    /// `poll_exports` and drives each write. `on_export_outcomes` (shared with
    /// native) folds results into the progress toast.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        use std::collections::HashSet;

        if paths.is_empty() {
            self.set_status("Export: nothing selected".into());
            self.request_redraw();
            return;
        }
        // Same three rejections as the native arm: one batch at a time, and
        // never before the catalog's sidecar reconciliation has populated the
        // edit mirrors (`edits`/`touchups`/`rotations`) this reads.
        if self.export_progress.is_some() {
            self.set_status("Export already in progress\u{2026}".into());
            self.request_redraw();
            return;
        }
        if self.catalog_load_pending.is_some() {
            self.set_status("Export: catalog still loading, try again in a moment\u{2026}".into());
            self.request_redraw();
            return;
        }

        let folder = self.folder_sel.clone().unwrap_or_default();
        let Some(folder_handle) = self.web_dir_handles.get(&folder).cloned() else {
            self.set_status("Export: no directory handle for the current folder".into());
            self.request_redraw();
            return;
        };
        let dest_dir = folder.join(crate::export::EXPORTS_DIR);
        let output_folder = folder_handle.clone();

        // Gather + serialize each photo's edits up front (cheap, on the main
        // thread) — the worker deserializes them for `bake_jpeg`.
        let jobs: Vec<(PathBuf, bool, String, String, u8)> = paths
            .iter()
            .map(|src| {
                let adj = self.edits.get(src).copied().unwrap_or_default();
                let touchups = self.touchups.get(src).cloned().unwrap_or_default();
                let rot = self.rotations.get(src).copied().unwrap_or(0);
                (
                    src.clone(),
                    crate::image_decode::is_raw_extension(src),
                    serde_json::to_string(&adj).unwrap_or_default(),
                    serde_json::to_string(&touchups).unwrap_or_default(),
                    rot,
                )
            })
            .collect();

        let total = jobs.len();
        self.export_progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
        });
        self.set_status(format!("Exporting 0/{total}\u{2026}"));
        self.request_redraw();

        let pool = self.web_worker_pool.handle();
        let fs = crate::web_export_fs::WebFs::new(folder_handle, self.web_file_handles.clone());
        let capacity = self.web_worker_pool.export_capacity();
        wasm_bindgen_futures::spawn_local(async move {
            let existing = match fs.existing_export_names().await {
                Ok(existing) => existing,
                Err(e) => {
                    for (src, ..) in jobs {
                        pool.fail_export(
                            src,
                            output_folder.clone(),
                            dest_dir.clone(),
                            String::new(),
                            format!("could not scan Exports: {e}"),
                        );
                    }
                    return;
                }
            };
            let mut taken: HashSet<String> = HashSet::new();
            for (src, is_raw, adj_json, touchups_json, rot) in jobs {
                while pool.export_in_flight() >= capacity {
                    Self::wait_for_export_capacity().await;
                }
                let filename = crate::paths::jpg_export_name(&src, &existing, &taken);
                taken.insert(filename.clone());
                match fs.read_source_array_buffer(&src).await {
                    Ok(bytes) => pool.submit_export(
                        src,
                        output_folder.clone(),
                        dest_dir.clone(),
                        filename,
                        bytes,
                        is_raw,
                        adj_json,
                        touchups_json,
                        rot,
                    ),
                    Err(e) => {
                        pool.fail_export(src, output_folder.clone(), dest_dir.clone(), filename, e)
                    }
                }
            }
        });
    }

    #[cfg(target_arch = "wasm32")]
    async fn wait_for_export_capacity() {
        use wasm_bindgen::JsCast;
        let promise = js_sys::Promise::new(
            &mut |resolve: js_sys::Function, _reject: js_sys::Function| {
                if let Some(window) = web_sys::window() {
                    let _ = window.set_timeout_with_callback_and_timeout_and_arguments_0(
                        resolve.unchecked_ref(),
                        16,
                    );
                }
            },
        );
        let _ = wasm_bindgen_futures::JsFuture::from(promise).await;
    }

    /// Queue `paths` for background export into `<current folder>/Exports/`.
    /// Returns immediately: the heavy decode/bake/encode runs on the exporter's
    /// worker pool, and `on_export_outcomes` reports progress as jobs finish.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.set_status("Export: nothing selected".into());
            self.request_redraw();
            return;
        }
        // One export batch at a time. A second batch launched before the first's
        // files land on disk would re-resolve the same `Exports/stem.jpg` targets
        // (both `.exists()` and the per-call `taken` set see nothing yet) and two
        // workers would race to write the same file — and it would clobber the
        // in-flight progress. Reject the overlap instead.
        if self.export_progress.is_some() {
            self.set_status("Export already in progress\u{2026}".into());
            self.request_redraw();
            return;
        }
        // The catalog's sidecar scan for the active directory runs on a
        // background thread (`app/catalog.rs::request_catalog_load`) and can
        // still be in flight right after opening it. `adj`/`touchups` below
        // read the `self.edits`/`self.touchups` mirrors, which only get
        // populated once that load reconciles — starting an export before
        // then would silently bake with missing edits rather than fail
        // loudly, so refuse instead.
        if self.catalog_load_pending.is_some() {
            self.set_status("Export: catalog still loading, try again in a moment\u{2026}".into());
            self.request_redraw();
            return;
        }
        let Some(exporter) = self.exporter.as_ref() else {
            return;
        };

        // Exports live under the current folder (the one whose images are
        // shown), so they stay together and never clutter the RAW folder.
        let base = self
            .folder_sel
            .clone()
            .or_else(|| paths[0].parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let exports_dir = base.join(crate::export::EXPORTS_DIR);
        if let Err(e) = std::fs::create_dir_all(&exports_dir) {
            self.set_status(format!(
                "Export failed: could not create Exports folder: {e}"
            ));
            self.request_redraw();
            return;
        }

        // Resolve every destination up front (sequential, so the `taken` set
        // dedupes same-stem sources), gather each photo's edits, and hand off a
        // self-contained job. No decode happens here — only cheap bookkeeping.
        let total = paths.len();
        let mut taken: HashSet<PathBuf> = HashSet::new();
        for src in paths {
            let dest = paths::jpg_export_target(&src, &exports_dir, &taken);
            taken.insert(dest.clone());
            // Read from the in-memory mirrors, same as `rot` below, rather
            // than `Catalog` directly — consistent with how rotation was
            // already sourced. The `catalog_load_pending` check above is
            // what actually guarantees these are populated by the time we
            // get here; reading the mirrors alone would not (they're filled
            // by the same background reconciliation `Catalog` itself is).
            let adj = self.edits.get(&src).copied().unwrap_or_default();
            let touchups = self.touchups.get(&src).cloned().unwrap_or_default();
            let rot = self.rotations.get(&src).copied().unwrap_or(0);
            exporter.submit(ExportJob {
                src,
                dest,
                adj,
                touchups,
                rot,
            });
        }

        self.export_progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
        });
        self.set_status(format!("Exporting 0/{total}\u{2026}"));
        self.request_redraw();
    }

    /// Fold a batch of finished exports into the progress toast. When the last
    /// job lands, replace the live counter with a final summary and clear the
    /// in-flight state (which stops the keep-awake redraw loop in `main.rs`).
    pub(crate) fn on_export_outcomes(&mut self, outcomes: Vec<ExportOutcome>) {
        let Some(mut prog) = self.export_progress.take() else {
            return;
        };
        for ExportOutcome { src, result } in outcomes {
            prog.done += 1;
            match result {
                Ok(out) => eprintln!("[lightphotos] exported {}", out.display()),
                Err(e) => {
                    eprintln!("[lightphotos] export failed for {}: {e}", src.display());
                    prog.errors += 1;
                    prog.last_err = Some(e);
                }
            }
        }
        if prog.done >= prog.total {
            let ok = prog.total - prog.errors;
            self.set_status(match prog.last_err {
                None => format!("Exported {ok} photo(s)"),
                Some(e) => format!("Exported {ok}/{} \u{2014} last error: {e}", prog.total),
            });
            // export_progress stays None (taken above) → toast expires normally.
        } else {
            self.set_status(format!("Exporting {}/{}\u{2026}", prog.done, prog.total));
            self.export_progress = Some(prog);
        }
    }

    // ---- Status toast ----

    pub(super) fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// The current status message. While an export is in flight the message is
    /// held without expiry (a slow single decode must not blank the progress
    /// toast mid-run); otherwise it fades after a few seconds.
    pub(crate) fn status_text(&self) -> Option<&str> {
        if self.export_progress.is_some() {
            return self.status.as_ref().map(|(s, _)| s.as_str());
        }
        self.status
            .as_ref()
            .and_then(|(s, t)| (t.elapsed().as_secs_f32() < 3.0).then_some(s.as_str()))
    }
}
