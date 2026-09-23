use super::*;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use crate::export::{ExportDest, ExportJob, ExportTarget, FolderChoice};
use crate::export::{ExportLanding, ExportOutcome, ExportSettings};
#[cfg(not(target_arch = "wasm32"))]
use crate::immich::{Account, ImmichServer};
#[cfg(not(target_arch = "wasm32"))]
use crate::paths;

/// `prefs` key for the last Immich server connected to.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const IMMICH_SERVER_PREF: &str = "immich_server";

/// `prefs` key for the export form's settings.
const EXPORT_SETTINGS_PREF: &str = "export_settings";

/// The saved export settings, or the defaults (full size into `Exports/`,
/// which is what export did before it had settings). A chosen folder that has
/// since gone away falls back to `Exports/` here rather than failing the next
/// export.
#[cfg_attr(test, allow(dead_code))]
pub(super) fn load_export_settings() -> ExportSettings {
    let saved: ExportSettings = crate::prefs::load(EXPORT_SETTINGS_PREF)
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    match &saved.target {
        #[cfg(not(target_arch = "wasm32"))]
        ExportTarget::Folder(FolderChoice::Custom(dir)) if !dir.is_dir() => ExportSettings {
            target: ExportTarget::default(),
            ..saved
        },
        _ => saved,
    }
}

fn save_export_settings(settings: &ExportSettings) {
    let saved = serde_json::to_string(settings)
        .map_err(|e| e.to_string())
        .and_then(|json| crate::prefs::save(EXPORT_SETTINGS_PREF, &json));
    if let Err(e) = saved {
        eprintln!("[export] could not save export settings: {e}");
    }
}

/// Where the app stands with an Immich server.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum ImmichLink {
    /// `url` and `key` are what the form's fields hold. `error` is why the
    /// last Connect failed.
    Disconnected {
        url: String,
        key: String,
        error: Option<String>,
    },
    /// Checking the key off the UI thread. The fields stay as typed, so a
    /// failure can hand them back.
    Connecting {
        url: String,
        key: String,
        rx: Receiver<Result<(ImmichServer, Account), String>>,
    },
    Connected {
        server: Arc<ImmichServer>,
        account: Account,
    },
}

impl App {
    pub(crate) fn export_form_open(&self) -> bool {
        self.export_form_open
    }

    pub(crate) fn export_settings(&self) -> &ExportSettings {
        &self.export_settings
    }

    /// Open the export form, or close it if it is showing. Opening it is what
    /// the toolbar's Export button and `X` do; Export in the form runs it.
    pub(super) fn toggle_export_form(&mut self) {
        self.export_form_open = !self.export_form_open;
        #[cfg(not(target_arch = "wasm32"))]
        if self.export_form_open {
            self.resume_immich();
        }
        self.request_redraw();
    }

    pub(super) fn close_export_form(&mut self) {
        self.export_form_open = false;
        self.request_redraw();
    }

    pub(super) fn set_export_settings(&mut self, settings: ExportSettings) {
        save_export_settings(&settings);
        self.export_settings = settings;
        #[cfg(not(target_arch = "wasm32"))]
        self.resume_immich();
        self.request_redraw();
    }

    /// Why Export can't run right now, shown under the disabled button.
    pub(crate) fn export_blocker(&self) -> Option<&'static str> {
        let t = crate::i18n::t();
        if self.selection_count() == 0 {
            return Some(t.export_nothing_selected);
        }
        // One batch at a time. A second batch would pick the same file names
        // before the first batch's files exist on disk, and overwrite them.
        if self.export_progress.is_some() {
            return Some(t.export_in_progress);
        }
        // The edit maps read below fill in only after the background catalog
        // load finishes. Exporting earlier would silently drop edits.
        if self.catalog_load_pending.is_some() {
            return Some(t.export_catalog_loading);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.export_settings.target == ExportTarget::Immich
            && !matches!(self.immich, ImmichLink::Connected { .. })
        {
            return Some(t.export_needs_immich);
        }
        None
    }

    /// Export in the form: export the selection with the form's settings. The
    /// form closes once the batch is running, and the toast takes over.
    pub(super) fn run_export_form(&mut self) {
        if let Some(why) = self.export_blocker() {
            self.set_status(why.into());
            self.request_redraw();
            return;
        }
        self.start_export(self.selected_paths());
        if self.export_progress.is_some() {
            self.close_export_form();
        }
    }

    /// Where a folder export would land, for the form to show.
    pub(crate) fn export_folder(&self) -> Option<PathBuf> {
        #[cfg(not(target_arch = "wasm32"))]
        if let ExportTarget::Folder(FolderChoice::Custom(dir)) = &self.export_settings.target {
            return Some(dir.clone());
        }
        self.folder_sel
            .clone()
            .or_else(|| self.selected_path()?.parent().map(PathBuf::from))
            .map(|base| base.join(crate::export::EXPORTS_DIR))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn choose_export_folder(&mut self) {
        if let Some(dir) = crate::dialog::pick_folder() {
            self.set_export_settings(ExportSettings {
                target: ExportTarget::Folder(FolderChoice::Custom(dir)),
                ..self.export_settings.clone()
            });
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn immich(&self) -> &ImmichLink {
        &self.immich
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_immich_fields(&mut self, new_url: Option<String>, new_key: Option<String>) {
        if let ImmichLink::Disconnected { url, key, error } = &mut self.immich {
            if let Some(u) = new_url {
                *url = u;
            }
            if let Some(k) = new_key {
                *key = k;
            }
            *error = None;
            self.request_redraw();
        }
    }

    /// Reconnect with the saved key the first time the form shows Immich, so
    /// a returning user finds it already connected.
    #[cfg(not(target_arch = "wasm32"))]
    fn resume_immich(&mut self) {
        if self.immich_key_looked_up || self.export_settings.target != ExportTarget::Immich {
            return;
        }
        self.immich_key_looked_up = true;
        if let ImmichLink::Disconnected { url, key, .. } = &mut self.immich {
            if url.is_empty() {
                return;
            }
            let Ok(origin) = ImmichServer::normalized(url) else {
                return;
            };
            if let Some(saved) = crate::secret::load_api_key(&origin) {
                *key = saved;
                self.connect_immich();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn connect_immich(&mut self) {
        let ImmichLink::Disconnected { url, key, .. } = &self.immich else {
            return;
        };
        let (url, key) = (url.clone(), key.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let (u, k) = (url.clone(), key.clone());
        let spawned = std::thread::Builder::new()
            .name("immich-connect".into())
            .spawn(move || {
                let _ = tx.send(ImmichServer::connect(&u, &k));
            });
        self.immich = match spawned {
            Ok(_) => ImmichLink::Connecting { url, key, rx },
            Err(e) => ImmichLink::Disconnected {
                url,
                key,
                error: Some(e.to_string()),
            },
        };
        self.request_redraw();
    }

    /// Forget the server and its saved key.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn disconnect_immich(&mut self) {
        if let ImmichLink::Connected { server, .. } = &self.immich {
            crate::secret::delete_api_key(server.origin());
            self.immich = ImmichLink::Disconnected {
                url: server.origin().to_string(),
                key: String::new(),
                error: None,
            };
            self.request_redraw();
        }
    }

    /// Take a finished Connect. Returns true while one is still running, so the
    /// frame loop keeps polling.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn poll_immich_connect(&mut self) -> bool {
        let ImmichLink::Connecting { url, key, rx } = &self.immich else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the connection check stopped".into())
            }
        };
        self.immich = match result {
            Ok((server, account)) => {
                if let Err(e) = crate::prefs::save(IMMICH_SERVER_PREF, server.origin()) {
                    eprintln!("[immich] could not save the server URL: {e}");
                }
                if let Err(e) = crate::secret::save_api_key(server.origin(), key) {
                    eprintln!("[immich] could not save the API key: {e}");
                }
                ImmichLink::Connected {
                    server: Arc::new(server),
                    account,
                }
            }
            Err(e) => ImmichLink::Disconnected {
                url: url.clone(),
                key: key.clone(),
                error: Some(e),
            },
        };
        self.request_redraw();
        false
    }

    /// Web export. Runs the native `bake_jpeg` pipeline on the Web Worker pool
    /// and writes each JPEG through the File System Access API. Reading sources
    /// and listing `Exports/` are async, so the batch setup runs in `spawn_local`.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        use std::collections::HashSet;

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
        let max_px = self.export_settings.size.max_px();
        self.export_progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
            uploading: false,
            duplicates: 0,
            unrated: 0,
            last_rating_err: None,
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
                        max_px,
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

    /// Queue `paths` for background export to wherever the form points.
    /// Returns at once; `on_export_outcomes` reports progress. The caller has
    /// checked `export_blocker`.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        let Some(exporter) = self.exporter.as_ref() else {
            return;
        };
        let settings = &self.export_settings;
        let max_px = settings.size.max_px();
        let job = |src: PathBuf, dest: ExportDest| ExportJob {
            adj: self.edits.get(&src).copied().unwrap_or_default(),
            touchups: self.touchups.get(&src).cloned().unwrap_or_default(),
            rot: self.rotations.get(&src).copied().unwrap_or(0),
            src,
            dest,
            max_px,
        };

        let total = paths.len();
        match &settings.target {
            ExportTarget::Folder(_) => {
                let Some(dir) = self.export_folder() else {
                    return;
                };
                if let Err(e) = std::fs::create_dir_all(&dir) {
                    self.set_status((crate::i18n::t().export_no_folder)(&e.to_string()));
                    self.request_redraw();
                    return;
                }
                // Pick destinations one at a time so `taken` keeps same-stem
                // sources apart.
                let mut taken: HashSet<PathBuf> = HashSet::new();
                for src in paths {
                    let dest = paths::jpg_export_target(&src, &dir, &taken);
                    taken.insert(dest.clone());
                    exporter.submit(job(src, ExportDest::Folder(dest)));
                }
            }
            ExportTarget::Immich => {
                let ImmichLink::Connected { server, .. } = &self.immich else {
                    return;
                };
                for src in paths {
                    let dest = ExportDest::Immich {
                        server: Arc::clone(server),
                        filename: format!("{}.jpg", paths::export_stem(&src)),
                        stars: self.rating_of(&src),
                    };
                    exporter.submit(job(src, dest));
                }
            }
        }

        let uploading = !matches!(settings.target, ExportTarget::Folder(_));
        self.export_progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
            uploading,
            duplicates: 0,
            unrated: 0,
            last_rating_err: None,
        });
        self.set_status(Self::progress_text(uploading, 0, total));
        self.request_redraw();
    }

    fn progress_text(uploading: bool, done: usize, total: usize) -> String {
        let t = crate::i18n::t();
        if uploading {
            (t.uploading)(done, total)
        } else {
            (t.exporting)(done, total)
        }
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
                Ok(landing) => {
                    match landing {
                        ExportLanding::File(out) => {
                            eprintln!("[lightphotos] exported {}", out.display())
                        }
                        #[cfg(not(target_arch = "wasm32"))]
                        ExportLanding::Asset {
                            id,
                            duplicate,
                            rating_error,
                        } => {
                            prog.duplicates += usize::from(duplicate);
                            eprintln!("[lightphotos] uploaded {} as {id}", src.display());
                            if let Some(e) = rating_error {
                                eprintln!("[lightphotos] rating not set on {id}: {e}");
                                prog.unrated += 1;
                                prog.last_rating_err = Some(e);
                            }
                        }
                    }
                    #[cfg(target_arch = "wasm32")]
                    crate::analytics::event("photo_exported");
                }
                Err(e) => {
                    eprintln!("[lightphotos] export failed for {}: {e}", src.display());
                    #[cfg(target_arch = "wasm32")]
                    crate::analytics::property("export_failed", "reason", "export_pipeline");
                    prog.errors += 1;
                    prog.last_err = Some(e);
                }
            }
        }
        if prog.done >= prog.total {
            let ok = prog.total - prog.errors;
            let t = crate::i18n::t();
            let summary = match (prog.last_err, prog.uploading) {
                (None, false) => (t.exported)(ok),
                (None, true) => (t.uploaded)(ok, prog.duplicates),
                (Some(e), false) => (t.exported_partial)(ok, prog.total, &e),
                (Some(e), true) => (t.uploaded_partial)(ok, prog.total, &e),
            };
            self.set_status(match prog.last_rating_err {
                Some(e) => (t.ratings_not_set)(&summary, prog.unrated, &e),
                None => summary,
            });
        } else {
            self.set_status(Self::progress_text(prog.uploading, prog.done, prog.total));
            self.export_progress = Some(prog);
        }
    }

    pub(super) fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// True while a long operation owns the status line. One predicate rather
    /// than a condition that grows an `||` per feature.
    pub(crate) fn batch_running(&self) -> bool {
        self.export_progress.is_some()
            || self.bulk_delete.is_some()
            || !self.autotone_pending.is_empty()
            || self.catalog.backlog() > 0
    }

    /// The current status message. It expires after 3 seconds, except while a
    /// batch is running, so a slow decode can't blank the progress toast.
    pub(crate) fn status_text(&self) -> Option<&str> {
        if self.batch_running() {
            return self.status.as_ref().map(|(s, _)| s.as_str());
        }
        self.status
            .as_ref()
            .and_then(|(s, t)| (t.elapsed().as_secs_f32() < 3.0).then_some(s.as_str()))
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod status_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_tmp_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "lightphotos-status-test-{}-{}",
            std::process::id(),
            n
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// An App whose status was set longer ago than the 3-second expiry.
    fn app_with_a_stale_status() -> App {
        let mut app = App::new(None);
        app.set_status("Rated 20000 photos".to_string());
        let (msg, _) = app.status.take().unwrap();
        app.status = Some((msg, Instant::now() - std::time::Duration::from_secs(4)));
        app
    }

    /// Uploads two photos through the path the form's Export button takes, to
    /// the server in `LIGHTPHOTOS_IMMICH_URL` with the key in
    /// `LIGHTPHOTOS_IMMICH_KEY`, then uploads them again. Works against Immich
    /// or Gumnut:
    ///
    /// `LIGHTPHOTOS_IMMICH_URL=https://immich.gumnut.ai LIGHTPHOTOS_IMMICH_KEY=...
    ///  cargo test immich_round_trip -- --ignored --nocapture`
    #[test]
    #[ignore = "needs an Immich server"]
    fn immich_round_trip() {
        use crate::export::{ExportSize, ExportTarget};
        let url = std::env::var("LIGHTPHOTOS_IMMICH_URL").expect("LIGHTPHOTOS_IMMICH_URL");
        let key = std::env::var("LIGHTPHOTOS_IMMICH_KEY").expect("LIGHTPHOTOS_IMMICH_KEY");

        let dir = unique_tmp_dir();
        // Content unique to this run, so a real server sees new assets.
        let seed = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_micros()
            % 200) as u8;
        for (i, name) in ["a.jpg", "b.jpg"].iter().enumerate() {
            let rgba: Vec<u8> = (0..300 * 200)
                .flat_map(|p| [(p % 251) as u8, seed, 40 * i as u8, 255])
                .collect();
            crate::image_encode::encode_jpeg(&dir.join(name), 300, 200, &rgba).unwrap();
        }
        // A camera time in UTC-7: the server should see 2024-06-02T01:00Z.
        let a = dir.join("a.jpg");
        let stamp = crate::image_decode::CaptureStamp::new("2024:06:01 18:00:00", Some("-07:00"));
        let tagged =
            crate::image_encode::with_exif(&std::fs::read(&a).unwrap(), 300, 200, stamp.as_ref());
        std::fs::write(&a, tagged).unwrap();

        let mut app = App::new(None);
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.recompute_visible();
        app.selected.insert(0);
        app.apply_rating_to_selection(4);
        app.selected.insert(1);
        app.exporter = Some(crate::export::Exporter::new());
        app.export_settings = ExportSettings {
            target: ExportTarget::Immich,
            size: ExportSize::LongEdge(120),
        };
        // Connected directly: `connect_immich` would save the key to the
        // developer's own Keychain.
        let (server, account) = ImmichServer::connect(&url, &key).expect("connect");
        eprintln!("connected as {} <{}>", account.name, account.email);
        app.immich = ImmichLink::Connected {
            server: Arc::new(server),
            account,
        };

        let run = |app: &mut App| {
            app.toggle_export_form();
            app.run_export_form();
            assert!(
                !app.export_form_open(),
                "the form closes once the batch runs"
            );
            let deadline = Instant::now() + std::time::Duration::from_secs(120);
            while app.export_progress.is_some() {
                assert!(Instant::now() < deadline, "the batch finished in time");
                let outcomes = app.exporter.as_ref().unwrap().poll();
                if !outcomes.is_empty() {
                    app.on_export_outcomes(outcomes);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let status = app.status.as_ref().unwrap().0.clone();
            eprintln!("{status}");
            status
        };
        assert_eq!(run(&mut app), "Uploaded 2 photo(s)");
        assert_eq!(
            run(&mut app),
            "Uploaded 2 photo(s), 2 already on the server",
            "the same bytes again converge on the existing assets"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_status_expires_when_nothing_is_running() {
        let app = app_with_a_stale_status();
        assert_eq!(app.status_text(), None);
    }

    /// A batch that outlives the 3-second expiry would otherwise watch its own
    /// progress toast blank out halfway through.
    #[test]
    fn a_stale_status_survives_while_sidecars_are_still_being_written() {
        let dir = unique_tmp_dir();
        let mut app = app_with_a_stale_status();
        for i in 0..64 {
            app.catalog.set(&dir.join(format!("p{i}.jpg")), 3);
        }
        assert!(app.catalog.backlog() > 0);
        assert_eq!(
            app.status_text(),
            Some("Rated 20000 photos"),
            "the toast must outlive its expiry while writes are still draining"
        );

        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert_eq!(app.catalog.backlog(), 0);
        assert_eq!(
            app.status_text(),
            None,
            "once the writes land the toast expires as usual"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_stale_status_survives_while_a_delete_is_running() {
        let dir = unique_tmp_dir();
        std::fs::write(dir.join("a.jpg"), b"").unwrap();

        let mut app = app_with_a_stale_status();
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.recompute_visible();
        app.selected.insert(0);
        app.delete_selection();
        // `delete_selection` sets its own progress toast; age it past expiry.
        let (msg, _) = app.status.take().unwrap();
        app.status = Some((
            msg.clone(),
            Instant::now() - std::time::Duration::from_secs(4),
        ));

        assert!(app.bulk_delete.is_some());
        assert_eq!(
            app.status_text(),
            Some(msg.as_str()),
            "a running delete must keep its progress toast on screen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
