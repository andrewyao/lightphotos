use super::*;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;


use crate::export::{ExportJob, ExportOutcome};
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

    /// Queue `paths` for background export into `<current folder>/Exports/`.
    /// Returns immediately: the heavy decode/bake/encode runs on the exporter's
    /// worker pool, and `on_export_outcomes` reports progress as jobs finish.
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
        let exports_dir = base.join("Exports");
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
            let adj = self.catalog.adjustments(&src);
            let touchups = self.catalog.touchups(&src);
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
