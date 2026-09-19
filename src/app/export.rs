use super::*;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::path::PathBuf;

#[cfg(not(target_arch = "wasm32"))]
use crate::export::ExportJob;
use crate::export::ExportOutcome;
#[cfg(not(target_arch = "wasm32"))]
use crate::paths;

impl App {
    /// Export the selected image as a JPG with all edits applied, into the
    /// folder's `Exports/` subfolder. Runs in the background.
    pub(super) fn export_selected(&mut self) {
        match self.selected_path() {
            Some(path) => self.start_export(vec![path]),
            None => {
                self.set_status(crate::i18n::t().export_no_image.into());
                self.request_redraw();
            }
        }
    }

    pub(super) fn export_selection(&mut self) {
        self.start_export(self.selected_paths());
    }

    /// Web export. Runs the native `bake_jpeg` pipeline on the Web Worker pool
    /// and writes each JPEG through the File System Access API. Reading sources
    /// and listing `Exports/` are async, so the batch setup runs in `spawn_local`.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        use std::collections::HashSet;

        if paths.is_empty() {
            self.set_status(crate::i18n::t().export_nothing_selected.into());
            self.request_redraw();
            return;
        }
        // Same guards as the native `start_export`.
        if self.export_progress.is_some() {
            self.set_status(crate::i18n::t().export_in_progress.into());
            self.request_redraw();
            return;
        }
        if self.catalog_load_pending.is_some() {
            self.set_status(crate::i18n::t().export_catalog_loading.into());
            self.request_redraw();
            return;
        }

        let folder = self.folder_sel.clone().unwrap_or_default();
        let Some(folder_handle) = self.web_dir_handles.get(&folder).cloned() else {
            self.set_status(crate::i18n::t().export_no_handle.into());
            self.request_redraw();
            return;
        };
        let dest_dir = folder.join(crate::export::EXPORTS_DIR);
        let output_folder = folder_handle.clone();

        // Workers receive each photo's edits as JSON.
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
        self.set_status((crate::i18n::t().exporting)(0, total));
        self.request_redraw();

        let pool = self.web_worker_pool.handle();
        let fs = crate::web_export_fs::WebFs::new(folder_handle, self.web_file_handles.clone());
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
                // Capacity counts ready workers and can drop to zero while
                // workers restart. Still allow one job in flight then, so the
                // pool either runs it later or fails it and the batch finishes.
                loop {
                    let in_flight = pool.export_in_flight();
                    let capacity = pool.export_capacity();
                    if in_flight == 0 || (capacity > 0 && in_flight < capacity) {
                        break;
                    }
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
    /// Returns at once; `on_export_outcomes` reports progress.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        if paths.is_empty() {
            self.set_status(crate::i18n::t().export_nothing_selected.into());
            self.request_redraw();
            return;
        }
        // One batch at a time. A second batch would pick the same file names
        // before the first batch's files exist on disk, and overwrite them.
        if self.export_progress.is_some() {
            self.set_status(crate::i18n::t().export_in_progress.into());
            self.request_redraw();
            return;
        }
        // The edit maps read below fill in only after the background catalog
        // load finishes. Exporting earlier would silently drop edits.
        if self.catalog_load_pending.is_some() {
            self.set_status(crate::i18n::t().export_catalog_loading.into());
            self.request_redraw();
            return;
        }
        let Some(exporter) = self.exporter.as_ref() else {
            return;
        };

        let base = self
            .folder_sel
            .clone()
            .or_else(|| paths[0].parent().map(Path::to_path_buf))
            .unwrap_or_else(|| PathBuf::from("."));
        let exports_dir = base.join(crate::export::EXPORTS_DIR);
        if let Err(e) = std::fs::create_dir_all(&exports_dir) {
            self.set_status((crate::i18n::t().export_no_folder)(&e.to_string()));
            self.request_redraw();
            return;
        }

        // Pick destinations one at a time so `taken` keeps same-stem sources apart.
        let total = paths.len();
        let mut taken: HashSet<PathBuf> = HashSet::new();
        for src in paths {
            let dest = paths::jpg_export_target(&src, &exports_dir, &taken);
            taken.insert(dest.clone());
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
        self.set_status((crate::i18n::t().exporting)(0, total));
        self.request_redraw();
    }

    /// Add finished exports to the progress toast. After the last one, show a
    /// summary and clear `export_progress`.
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
            let t = crate::i18n::t();
            self.set_status(match prog.last_err {
                None => (t.exported)(ok),
                Some(e) => (t.exported_partial)(ok, prog.total, &e),
            });
        } else {
            self.set_status((crate::i18n::t().exporting)(prog.done, prog.total));
            self.export_progress = Some(prog);
        }
    }

    pub(super) fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// The current status message. It expires after 3 seconds, except during
    /// an export, so a slow decode can't blank the progress toast.
    pub(crate) fn status_text(&self) -> Option<&str> {
        if self.export_progress.is_some() {
            return self.status.as_ref().map(|(s, _)| s.as_str());
        }
        self.status
            .as_ref()
            .and_then(|(s, t)| (t.elapsed().as_secs_f32() < 3.0).then_some(s.as_str()))
    }
}
