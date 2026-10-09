use super::*;
#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashSet;
use std::path::PathBuf;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::Receiver;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use crate::export::immich::{Account, Album, ImmichServer};
#[cfg(not(target_arch = "wasm32"))]
use crate::export::{AlbumChoice, ExportDest, ExportJob, ExportTarget, FolderChoice};
use crate::export::{ExportLanding, ExportOutcome, ExportSettings};
#[cfg(not(target_arch = "wasm32"))]
use crate::paths;

/// `prefs` key for the last Immich server connected to.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const IMMICH_SERVER_PREF: &str = "immich_server";

/// `prefs` key for the export form's settings.
const EXPORT_SETTINGS_PREF: &str = "export_settings";

/// How long a status toast shows after it was set or last kept alive.
const STATUS_SECS: f32 = 3.0;

/// The saved export settings, or the defaults (full size into `Exports/`,
/// which is what export did before it had settings). A chosen folder that has
/// since gone away falls back to `Exports/` here rather than failing the next
/// export.
#[cfg_attr(test, allow(dead_code))]
pub(super) fn load_export_settings() -> ExportSettings {
    let saved: ExportSettings = crate::persist::prefs::load(EXPORT_SETTINGS_PREF)
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
    // A test must never write the developer's own settings.
    if cfg!(test) {
        return;
    }
    let saved = serde_json::to_string(settings)
        .map_err(|e| e.to_string())
        .and_then(|json| crate::persist::prefs::save(EXPORT_SETTINGS_PREF, &json));
    if let Err(e) = saved {
        eprintln!("[export] could not save export settings: {e}");
    }
}

/// A checked server, its account, and the account's albums.
#[cfg(not(target_arch = "wasm32"))]
type Connection = (ImmichServer, Account, Result<Vec<Album>, String>);

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
        rx: Receiver<Result<Connection, String>>,
    },
    Connected {
        server: Arc<ImmichServer>,
        account: Account,
        /// The account's albums, or why they couldn't be listed. A failed
        /// listing still leaves "No album" and "New album" to pick.
        albums: Result<Vec<Album>, String>,
    },
}

/// The export form and the exports it runs. App's other modules see it
/// only through the methods below and App's export methods.
pub(crate) struct Exports {
    /// `None` until the window is created.
    exporter: Option<Exporter>,
    progress: Option<ExportProgress>,
    /// What the export form is set to, remembered across launches.
    settings: ExportSettings,
    /// The export form is showing in the right-hand panel.
    form_open: bool,
    /// The Immich server export uploads to, and the form's sign-in fields.
    #[cfg(not(target_arch = "wasm32"))]
    immich: ImmichLink,
    /// Whether the saved API key has been looked up yet. The lookup waits for
    /// the first time the form shows Immich, so someone who never uses it never
    /// sees a Keychain prompt.
    #[cfg(not(target_arch = "wasm32"))]
    key_looked_up: bool,
    /// The album add that ends an Immich batch.
    #[cfg(not(target_arch = "wasm32"))]
    album_add: Option<AlbumAdd>,
}

/// An album add in flight, with the batch's summary and status kind to
/// finish the toast with.
#[cfg(not(target_arch = "wasm32"))]
struct AlbumAdd {
    rx: Receiver<Result<crate::export::immich::Album, String>>,
    summary: String,
    kind: StatusKind,
}

impl Exports {
    pub(super) fn new() -> Self {
        Self {
            exporter: None,
            progress: None,
            // A test must never read the developer's own settings.
            #[cfg(test)]
            settings: Default::default(),
            #[cfg(not(test))]
            settings: load_export_settings(),
            form_open: false,
            #[cfg(not(target_arch = "wasm32"))]
            immich: ImmichLink::Disconnected {
                #[cfg(test)]
                url: String::new(),
                #[cfg(not(test))]
                url: crate::persist::prefs::load(IMMICH_SERVER_PREF).unwrap_or_default(),
                key: String::new(),
                error: None,
            },
            #[cfg(not(target_arch = "wasm32"))]
            key_looked_up: cfg!(test),
            #[cfg(not(target_arch = "wasm32"))]
            album_add: None,
        }
    }

    /// Hides the form without a redraw, for a page switch that redraws anyway.
    pub(super) fn close_form(&mut self) {
        self.form_open = false;
    }
}

impl App {
    pub(crate) fn export_form_open(&self) -> bool {
        self.exports.form_open
    }

    /// Starts the export workers. The window's setup calls it, so a test's App
    /// has none.
    pub(crate) fn start_exporter(&mut self) {
        self.exports.exporter = Some(Exporter::new());
    }

    /// Exports finished since the last call.
    pub(crate) fn poll_exporter(&self) -> Vec<ExportOutcome> {
        self.exports
            .exporter
            .as_ref()
            .map(|e| e.poll())
            .unwrap_or_default()
    }

    /// Whether an export batch is running.
    pub(crate) fn export_running(&self) -> bool {
        self.exports.progress.is_some()
    }

    pub(crate) fn export_settings(&self) -> &ExportSettings {
        &self.exports.settings
    }

    /// Open the export form, or close it if it is showing. Opening it is what
    /// the toolbar's Export button and `X` do; Export in the form runs it.
    pub(super) fn toggle_export_form(&mut self) {
        self.exports.form_open = !self.exports.form_open;
        if self.exports.form_open {
            self.info_open = false;
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.exports.form_open {
            self.resume_immich();
        }
        self.request_redraw();
    }

    pub(super) fn close_export_form(&mut self) {
        self.exports.form_open = false;
        self.request_redraw();
    }

    pub(super) fn set_export_settings(&mut self, settings: ExportSettings) {
        save_export_settings(&settings);
        self.exports.settings = settings;
        #[cfg(not(target_arch = "wasm32"))]
        self.resume_immich();
        self.request_redraw();
    }

    /// Why Export can't run right now, shown under the disabled button.
    pub(crate) fn export_blocker(&self) -> Option<&'static str> {
        let t = crate::i18n::t();
        if self.action_count() == 0 {
            return Some(t.export_nothing_selected);
        }
        // One batch at a time. A second batch would pick the same file names
        // before the first batch's files exist on disk, and overwrite them.
        if self.exports.progress.is_some() {
            return Some(t.export_in_progress);
        }
        // The album add that ends an Immich batch counts as part of it: a
        // batch started meanwhile would create a second new album.
        #[cfg(not(target_arch = "wasm32"))]
        if self.exports.album_add.is_some() {
            return Some(t.export_in_progress);
        }
        // The edit maps read below fill in only after the background catalog
        // load finishes. Exporting earlier would silently drop edits.
        if self.catalog_loading() {
            return Some(t.export_catalog_loading);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.exports.settings.target == ExportTarget::Immich
            && !matches!(self.exports.immich, ImmichLink::Connected { .. })
        {
            return Some(t.export_needs_immich);
        }
        #[cfg(not(target_arch = "wasm32"))]
        if self.exports.settings.target == ExportTarget::Immich
            && matches!(&self.exports.settings.album, AlbumChoice::New(name) if name.trim().is_empty())
        {
            return Some(t.album_name_needed);
        }
        #[cfg(target_arch = "wasm32")]
        if self.exports.settings.target == crate::export::ExportTarget::Immich {
            return Some(t.immich_native_only);
        }
        None
    }

    /// Export in the form: export the selection with the form's settings. The
    /// form closes once the batch is running, and the toast takes over.
    pub(super) fn run_export_form(&mut self) {
        if let Some(why) = self.export_blocker() {
            self.set_status(StatusKind::Error, why.into());
            self.request_redraw();
            return;
        }
        self.start_export(self.action_paths());
        if self.exports.progress.is_some() {
            self.close_export_form();
        }
    }

    /// Where a folder export would land, for the form to show.
    pub(crate) fn export_folder(&self) -> Option<PathBuf> {
        #[cfg(not(target_arch = "wasm32"))]
        if let ExportTarget::Folder(FolderChoice::Custom(dir)) = &self.exports.settings.target {
            return Some(dir.clone());
        }
        self.folder_sel
            .clone()
            .or_else(|| self.selected_path()?.parent().map(PathBuf::from))
            .map(|base| base.join(crate::export::EXPORTS_DIR))
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn choose_export_folder(&mut self) {
        if let Some(dir) = crate::shell::dialog::pick_folder() {
            self.set_export_settings(ExportSettings {
                target: ExportTarget::Folder(FolderChoice::Custom(dir)),
                ..self.exports.settings.clone()
            });
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn immich(&self) -> &ImmichLink {
        &self.exports.immich
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn set_immich_fields(&mut self, new_url: Option<String>, new_key: Option<String>) {
        if let ImmichLink::Disconnected { url, key, error } = &mut self.exports.immich {
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
        if self.exports.key_looked_up || self.exports.settings.target != ExportTarget::Immich {
            return;
        }
        self.exports.key_looked_up = true;
        if let ImmichLink::Disconnected { url, key, .. } = &mut self.exports.immich {
            if url.is_empty() {
                return;
            }
            let Ok(origin) = ImmichServer::normalized(url) else {
                return;
            };
            if let Some(saved) = crate::persist::secret::load_api_key(&origin) {
                *key = saved;
                self.connect_immich();
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn connect_immich(&mut self) {
        let ImmichLink::Disconnected { url, key, .. } = &self.exports.immich else {
            return;
        };
        let (url, key) = (url.clone(), key.clone());
        let (tx, rx) = std::sync::mpsc::channel();
        let (u, k) = (url.clone(), key.clone());
        let spawned = std::thread::Builder::new()
            .name("immich-connect".into())
            .spawn(move || {
                let _ = tx.send(ImmichServer::connect(&u, &k).map(|(server, account)| {
                    let albums = server.albums();
                    (server, account, albums)
                }));
            });
        self.exports.immich = match spawned {
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
        if let ImmichLink::Connected { server, .. } = &self.exports.immich {
            crate::persist::secret::delete_api_key(server.origin());
            self.exports.immich = ImmichLink::Disconnected {
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
        let ImmichLink::Connecting { url, key, rx } = &self.exports.immich else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                Err("the connection check stopped".into())
            }
        };
        self.exports.immich = match result {
            Ok((server, account, albums)) => {
                if let Err(e) = crate::persist::prefs::save(IMMICH_SERVER_PREF, server.origin()) {
                    eprintln!("[immich] could not save the server URL: {e}");
                }
                if let Err(e) = crate::persist::secret::save_api_key(server.origin(), key) {
                    eprintln!("[immich] could not save the API key: {e}");
                }
                ImmichLink::Connected {
                    server: Arc::new(server),
                    account,
                    albums,
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

    /// Web export. Runs the native `bake_jpeg` pipeline on the loader's decode
    /// threads and writes each JPEG through the File System Access API. Reading sources
    /// and listing `Exports/` are async, so the batch setup runs in `spawn_local`.
    #[cfg(target_arch = "wasm32")]
    pub(super) fn start_export(&mut self, paths: Vec<PathBuf>) {
        use std::collections::HashSet;

        let folder = self.folder_sel.clone().unwrap_or_default();
        let Some(folder_handle) = self.web.dir_handle(&folder) else {
            self.set_status(StatusKind::Error, crate::i18n::t().export_no_handle.into());
            self.request_redraw();
            return;
        };
        let dest_dir = folder.join(crate::export::EXPORTS_DIR);
        let output_folder = folder_handle.clone();

        let Some(decoder) = self.loader.as_ref().map(|l| l.web_decoder()) else {
            return;
        };
        let jobs: Vec<(
            PathBuf,
            bool,
            crate::develop::Adjustments,
            Vec<crate::develop::TouchUp>,
            u8,
        )> = paths
            .iter()
            .map(|src| {
                (
                    src.clone(),
                    crate::decode::image_decode::is_raw_extension(src),
                    self.edits.get(src).copied().unwrap_or_default(),
                    self.touchups.get(src).cloned().unwrap_or_default(),
                    self.rotations.get(src).copied().unwrap_or(0),
                )
            })
            .collect();

        let total = jobs.len();
        let max_px = self.exports.settings.size.max_px();
        self.exports.progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
            uploading: false,
            duplicates: 0,
            unrated: 0,
            last_rating_err: None,
        });
        self.set_status(StatusKind::Progress, (crate::i18n::t().exporting)(0, total));
        self.request_redraw();

        let pool = self.web.exports();
        let fs =
            crate::web::web_export_fs::WebFs::new(folder_handle, self.web.file_handles().clone());
        wasm_bindgen_futures::spawn_local(async move {
            let existing = match fs.existing_export_names().await {
                Ok(existing) => existing,
                Err(e) => {
                    for (src, ..) in jobs {
                        pool.fail(
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
            for (src, is_raw, adj, touchups, rot) in jobs {
                // Each export in flight holds its source's bytes and a
                // full-resolution decode in the one shared heap.
                while pool.in_flight() >= pool.capacity(&decoder) {
                    Self::wait_for_export_capacity().await;
                }
                let filename = crate::paths::jpg_export_name(&src, &existing, &taken);
                taken.insert(filename.clone());
                match fs.read_source_array_buffer(&src).await {
                    Ok(bytes) => pool.submit(
                        &decoder,
                        crate::web::web_exports::Export {
                            path: src,
                            folder: output_folder.clone(),
                            dest_dir: dest_dir.clone(),
                            filename,
                            bytes,
                            is_raw,
                            adj,
                            touchups,
                            rot,
                            max_px,
                        },
                    ),
                    Err(e) => pool.fail(src, output_folder.clone(), dest_dir.clone(), filename, e),
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
    fn start_export(&mut self, paths: Vec<PathBuf>) {
        let Some(exporter) = self.exports.exporter.as_ref() else {
            return;
        };
        let settings = &self.exports.settings;
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
                    self.set_status(
                        StatusKind::Error,
                        (crate::i18n::t().export_no_folder)(&e.to_string()),
                    );
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
                let ImmichLink::Connected { server, .. } = &self.exports.immich else {
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
        let album = match uploading {
            true => settings.album.clone(),
            false => AlbumChoice::None,
        };
        self.exports.progress = Some(ExportProgress {
            done: 0,
            total,
            errors: 0,
            last_err: None,
            uploading,
            duplicates: 0,
            unrated: 0,
            last_rating_err: None,
            album,
            asset_ids: Vec::new(),
        });
        self.set_status(
            StatusKind::Progress,
            Self::progress_text(uploading, 0, total),
        );
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
        let Some(mut prog) = self.exports.progress.take() else {
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
                            prog.asset_ids.push(id.clone());
                            eprintln!("[lightphotos] uploaded {} as {id}", src.display());
                            if let Some(e) = rating_error {
                                eprintln!("[lightphotos] rating not set on {id}: {e}");
                                prog.unrated += 1;
                                prog.last_rating_err = Some(e);
                            }
                        }
                    }
                    #[cfg(target_arch = "wasm32")]
                    crate::web::analytics::event("photo_exported");
                }
                Err(e) => {
                    eprintln!("[lightphotos] export failed for {}: {e}", src.display());
                    #[cfg(target_arch = "wasm32")]
                    crate::web::analytics::property("export_failed", "reason", "export_pipeline");
                    prog.errors += 1;
                    prog.last_err = Some(e);
                }
            }
        }
        if prog.done >= prog.total {
            let ok = prog.total - prog.errors;
            let t = crate::i18n::t();
            let kind = if prog.last_err.is_some() || prog.last_rating_err.is_some() {
                StatusKind::Error
            } else {
                StatusKind::Success
            };
            let summary = match (prog.last_err, prog.uploading) {
                (None, false) => (t.exported)(ok),
                (None, true) => (t.uploaded)(ok, prog.duplicates),
                (Some(e), false) => (t.exported_partial)(ok, prog.total, &e),
                (Some(e), true) => (t.uploaded_partial)(ok, prog.total, &e),
            };
            let summary = match prog.last_rating_err {
                Some(e) => (t.ratings_not_set)(&summary, prog.unrated, &e),
                None => summary,
            };
            #[cfg(not(target_arch = "wasm32"))]
            let (kind, summary) =
                match self.start_album_add(prog.album, prog.asset_ids, summary, kind) {
                    Ok(adding) => (StatusKind::Progress, adding),
                    Err(summary) => (kind, summary),
                };
            self.set_status(kind, summary);
        } else {
            self.set_status(
                StatusKind::Progress,
                Self::progress_text(prog.uploading, prog.done, prog.total),
            );
            self.exports.progress = Some(prog);
        }
    }

    /// Put a finished batch's uploads into its album, off the UI thread,
    /// creating the album first when it is new. Returns the toast to show
    /// meanwhile, or hands `summary` back when there is nothing to add.
    #[cfg(not(target_arch = "wasm32"))]
    fn start_album_add(
        &mut self,
        album: AlbumChoice,
        ids: Vec<String>,
        summary: String,
        kind: StatusKind,
    ) -> Result<String, String> {
        let ImmichLink::Connected { server, .. } = &self.exports.immich else {
            return Err(summary);
        };
        // `existing` is `None` for an album still to be created.
        let (name, existing) = match album {
            AlbumChoice::None => return Err(summary),
            _ if ids.is_empty() => return Err(summary),
            AlbumChoice::Existing { id, name } => (name.clone(), Some(Album { id, name })),
            AlbumChoice::New(name) => (name, None),
        };
        let server = Arc::clone(server);
        let new_name = name.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("immich-album".into())
            .spawn(move || {
                let result = match existing {
                    Some(album) => Ok(album),
                    None => server.create_album(new_name.trim()),
                }
                .and_then(|album| server.add_to_album(&album.id, &ids).map(|()| album));
                let _ = tx.send(result);
            });
        if let Err(e) = spawned {
            return Err((crate::i18n::t().album_failed)(&summary, &e.to_string()));
        }
        let adding = (crate::i18n::t().adding_to_album)(&summary, &name);
        self.exports.album_add = Some(AlbumAdd { rx, summary, kind });
        Ok(adding)
    }

    /// Take a finished album add. Returns true while one is still running. A
    /// new album becomes the form's `Existing` choice, so the next batch goes
    /// into the same album rather than a second one of the same name.
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn poll_album_add(&mut self) -> bool {
        let Some(AlbumAdd { rx, .. }) = &self.exports.album_add else {
            return false;
        };
        let result = match rx.try_recv() {
            Ok(result) => result,
            Err(std::sync::mpsc::TryRecvError::Empty) => return true,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err("the album add stopped".into()),
        };
        let Some(AlbumAdd {
            summary,
            kind: export_kind,
            ..
        }) = self.exports.album_add.take()
        else {
            return false;
        };
        let summary = &summary;
        let t = crate::i18n::t();
        let (kind, status) = match result {
            Ok(album) => {
                if let ImmichLink::Connected {
                    albums: Ok(list), ..
                } = &mut self.exports.immich
                {
                    if !list.iter().any(|a| a.id == album.id) {
                        list.push(album.clone());
                        list.sort_by_key(|a| a.name.to_lowercase());
                    }
                }
                if matches!(self.exports.settings.album, AlbumChoice::New(_)) {
                    self.set_export_settings(ExportSettings {
                        album: AlbumChoice::Existing {
                            id: album.id,
                            name: album.name.clone(),
                        },
                        ..self.exports.settings.clone()
                    });
                }
                (export_kind, (t.added_to_album)(summary, &album.name))
            }
            Err(e) => (StatusKind::Error, (t.album_failed)(summary, &e)),
        };
        self.set_status(kind, status);
        self.request_redraw();
        false
    }

    pub(super) fn set_status(&mut self, kind: StatusKind, msg: String) {
        self.status = Some((kind, msg, Instant::now()));
    }

    /// True while a long operation owns the status line. One predicate rather
    /// than a condition that grows an `||` per feature.
    pub(crate) fn batch_running(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        let adding_to_album = self.exports.album_add.is_some();
        #[cfg(target_arch = "wasm32")]
        let adding_to_album = false;
        self.exports.progress.is_some()
            || adding_to_album
            || self.bulk_delete.is_some()
            || self.autotone.is_running()
            || self.score_progress().is_some()
            || self.catalog.backlog() > 0
    }

    /// Restart a showing toast's 3-second clock while a batch runs, so a slow
    /// decode can't blank the progress toast. Called once per event-loop turn.
    /// A toast that already expired stays expired: a slider drag queues a
    /// sidecar write too, and must not bring back an old "Auto Tone applied".
    pub(crate) fn keep_status_alive(&mut self) {
        if !self.batch_running() {
            return;
        }
        if let Some((_, _, at)) = self.status.as_mut() {
            if at.elapsed().as_secs_f32() < STATUS_SECS {
                *at = Instant::now();
            }
        }
    }

    /// The current status message and its kind, until it expires.
    pub(crate) fn status(&self) -> Option<(StatusKind, &str)> {
        let (kind, text, at) = self.status.as_ref()?;
        (at.elapsed().as_secs_f32() < STATUS_SECS).then_some((*kind, text.as_str()))
    }

    #[cfg(test)]
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status().map(|(_, text)| text)
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod status_tests {
    use super::*;
    use crate::app::test_support::temp_folder;

    /// An App whose status was set longer ago than the 3-second expiry.
    fn app_with_a_stale_status() -> App {
        let mut app = App::new(None);
        app.set_status(StatusKind::Success, "Rated 20000 photos".to_string());
        let (kind, msg, _) = app.status.take().unwrap();
        app.status = Some((
            kind,
            msg,
            Instant::now() - std::time::Duration::from_secs(4),
        ));
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

        let dir = temp_folder("status-test");
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
            crate::decode::image_encode::encode_jpeg(
                &dir.join(name),
                300,
                200,
                &rgba,
                crate::decode::image_encode::JpegQuality::Export,
            )
            .unwrap();
        }
        // A camera time in UTC-7: the server should see 2024-06-02T01:00Z.
        let a = dir.join("a.jpg");
        let stamp =
            crate::decode::image_decode::CaptureStamp::new("2024:06:01 18:00:00", Some("-07:00"));
        let tagged = crate::decode::image_encode::with_exif(
            &std::fs::read(&a).unwrap(),
            300,
            200,
            stamp.as_ref(),
        );
        std::fs::write(&a, tagged).unwrap();

        let mut app = App::new(None);
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.recompute_visible();
        app.selected.insert(0);
        app.apply_rating_to_selection(4);
        app.selected.insert(1);
        app.exports.exporter = Some(crate::export::Exporter::new());
        app.exports.settings = ExportSettings {
            target: ExportTarget::Immich,
            size: ExportSize::LongEdge(120),
            album: AlbumChoice::None,
        };
        // Connected directly: `connect_immich` would save the key to the
        // developer's own Keychain.
        let (server, account) = ImmichServer::connect(&url, &key).expect("connect");
        eprintln!("connected as {} <{}>", account.name, account.email);
        let albums = server.albums();
        app.exports.immich = ImmichLink::Connected {
            server: Arc::new(server),
            account,
            albums,
        };

        let run = |app: &mut App| {
            app.toggle_export_form();
            app.run_export_form();
            assert!(
                !app.export_form_open(),
                "the form closes once the batch runs"
            );
            let deadline = Instant::now() + std::time::Duration::from_secs(120);
            while app.exports.progress.is_some() {
                assert!(Instant::now() < deadline, "the batch finished in time");
                let outcomes = app.exports.exporter.as_ref().unwrap().poll();
                if !outcomes.is_empty() {
                    app.on_export_outcomes(outcomes);
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            let status = app.status.as_ref().unwrap().1.clone();
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

    /// Move the toast's clock back, as if `secs` had passed.
    fn age_status(app: &mut App, secs: u64) {
        let (_, _, at) = app.status.as_mut().unwrap();
        *at -= std::time::Duration::from_secs(secs);
    }

    /// A batch that outlives the 3-second expiry would otherwise watch its own
    /// progress toast blank out halfway through.
    #[test]
    fn a_status_survives_while_sidecars_are_still_being_written() {
        let dir = temp_folder("status-test");
        let mut app = App::new(None);
        app.catalog.open_dir(&dir);
        app.set_status(StatusKind::Success, "Rated 20000 photos".to_string());
        for i in 0..64 {
            app.catalog.set(&dir.join(format!("p{i}.jpg")), 3);
        }
        assert!(app.catalog.backlog() > 0);
        for _ in 0..3 {
            age_status(&mut app, 2);
            app.keep_status_alive();
        }
        assert_eq!(
            app.status_text(),
            Some("Rated 20000 photos"),
            "the toast must outlive its expiry while writes are still draining"
        );

        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        assert_eq!(app.catalog.backlog(), 0);
        age_status(&mut app, 4);
        app.keep_status_alive();
        assert_eq!(
            app.status_text(),
            None,
            "once the writes land the toast expires as usual"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A slider edit queues a sidecar write. That must not bring back a toast
    /// that already expired, such as an earlier "Auto Tone applied".
    #[test]
    fn a_later_write_does_not_revive_an_expired_status() {
        let dir = temp_folder("status-test");
        let mut app = app_with_a_stale_status();
        app.catalog.open_dir(&dir);
        app.catalog.set(&dir.join("p.jpg"), 3);
        assert!(app.catalog.backlog() > 0);
        app.keep_status_alive();
        assert_eq!(app.status_text(), None);

        app.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_status_survives_while_a_delete_is_running() {
        let dir = temp_folder("status-test");
        std::fs::write(dir.join("a.jpg"), b"").unwrap();

        let mut app = App::new(None);
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.recompute_visible();
        app.selected.insert(0);
        app.delete_selection();
        // `delete_selection` sets its own progress toast; run it past expiry.
        let msg = app.status_text().unwrap().to_string();
        for _ in 0..3 {
            age_status(&mut app, 2);
            app.keep_status_alive();
        }

        assert!(app.bulk_delete.is_some());
        assert_eq!(
            app.status_text(),
            Some(msg.as_str()),
            "a running delete must keep its progress toast on screen"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn connected(albums: Vec<Album>) -> ImmichLink {
        ImmichLink::Connected {
            server: Arc::new(ImmichServer::offline("https://immich.test")),
            account: Account {
                name: "Ada".into(),
                email: "ada@example.com".into(),
            },
            albums: Ok(albums),
        }
    }

    /// A batch into a new album ends with the album in the list and chosen
    /// as `Existing`, so the next batch doesn't create a second one.
    #[test]
    fn a_created_album_becomes_the_remembered_choice() {
        let mut app = App::new(None);
        app.exports.immich = connected(vec![]);
        app.exports.settings.target = ExportTarget::Immich;
        app.exports.settings.album = AlbumChoice::New("Trip".into());
        let (tx, rx) = std::sync::mpsc::channel();
        app.exports.album_add = Some(AlbumAdd {
            rx,
            summary: "Uploaded 2".into(),
            kind: StatusKind::Success,
        });
        assert!(app.poll_album_add(), "still waiting before a reply");
        assert!(
            app.batch_running(),
            "the toast stays up while the album add runs"
        );

        let album = Album {
            id: "a1".into(),
            name: "Trip".into(),
        };
        tx.send(Ok(album.clone())).unwrap();
        assert!(!app.poll_album_add());
        assert_eq!(
            app.exports.settings.album,
            AlbumChoice::Existing {
                id: "a1".into(),
                name: "Trip".into()
            }
        );
        let ImmichLink::Connected { albums, .. } = &app.exports.immich else {
            panic!("still connected");
        };
        assert_eq!(albums.as_ref().unwrap(), &vec![album]);
        assert_eq!(
            app.status_text(),
            Some((crate::i18n::t().added_to_album)("Uploaded 2", "Trip").as_str())
        );
    }

    #[test]
    fn a_new_album_needs_a_name_before_export() {
        let dir = temp_folder("status-test");
        std::fs::write(dir.join("a.jpg"), b"").unwrap();
        let mut app = App::new(None);
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.recompute_visible();
        app.selected.insert(0);
        app.exports.immich = connected(vec![]);
        app.exports.settings.target = ExportTarget::Immich;
        app.exports.settings.album = AlbumChoice::New("  ".into());
        assert_eq!(
            app.export_blocker(),
            Some(crate::i18n::t().album_name_needed)
        );
        app.exports.settings.album = AlbumChoice::New("Trip".into());
        assert_eq!(app.export_blocker(), None);

        let (_tx, rx) = std::sync::mpsc::channel();
        app.exports.album_add = Some(AlbumAdd {
            rx,
            summary: "Uploaded 1".into(),
            kind: StatusKind::Success,
        });
        assert_eq!(
            app.export_blocker(),
            Some(crate::i18n::t().export_in_progress),
            "the last batch's album add must finish first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A failed export stays red after its album add succeeds, and a failed
    /// album add turns a clean export red.
    #[test]
    fn an_album_add_keeps_or_raises_the_export_error() {
        let mut app = App::new(None);
        app.exports.immich = connected(vec![]);
        let album = || Album {
            id: "a1".into(),
            name: "Trip".into(),
        };
        let cases = [
            (StatusKind::Success, Ok(album()), StatusKind::Success),
            (StatusKind::Error, Ok(album()), StatusKind::Error),
            (
                StatusKind::Success,
                Err("offline".to_string()),
                StatusKind::Error,
            ),
        ];
        for (export_kind, result, expected) in cases {
            let (tx, rx) = std::sync::mpsc::channel();
            app.exports.album_add = Some(AlbumAdd {
                rx,
                summary: "Uploaded 1".into(),
                kind: export_kind,
            });
            tx.send(result).unwrap();
            assert!(!app.poll_album_add());
            assert_eq!(app.status().map(|(kind, _)| kind), Some(expected));
        }
    }

    /// Nothing to add, or no album chosen, leaves the summary as it was.
    #[test]
    fn an_album_add_starts_only_with_an_album_and_uploads() {
        let mut app = App::new(None);
        app.exports.immich = connected(vec![]);
        let none = app.start_album_add(
            AlbumChoice::None,
            vec!["x".into()],
            "s".into(),
            StatusKind::Success,
        );
        assert_eq!(none, Err("s".into()));
        let empty = app.start_album_add(
            AlbumChoice::New("Trip".into()),
            vec![],
            "s".into(),
            StatusKind::Success,
        );
        assert_eq!(empty, Err("s".into()));
        assert!(app.exports.album_add.is_none());
    }
}

#[cfg(test)]
mod form_tests {
    use super::*;

    fn folder_app(photos: usize) -> (App, Vec<PathBuf>) {
        let (mut app, _, paths) = crate::app::test_support::folder_app("export-form", photos);
        app.focus = Region::Grid;
        app.sel = Some(0);
        (app, paths)
    }

    /// Export's two sections, Destination and Output, set each label over
    /// its value, flush with the page's left edge.
    #[test]
    fn export_lays_out_as_a_form() {
        use crate::app::test_support::settled;
        use crate::i18n::t;

        let (mut app, _) = folder_app(2);
        app.exports.form_open = true;
        let painted = settled(&mut app);
        let destination = painted.pos_of(t().export_destination);
        let output = painted.pos_of(t().export_output);
        let size = painted.pos_of(t().export_size);
        let folder = painted.pos_of(t().export_folder);

        assert!(destination.y < folder.y && folder.y < output.y && output.y < size.y);
        assert_eq!(folder.x, size.x, "labels in both sections share one edge");
        let full = painted.pos_of(t().export_size_full);
        assert!(full.y > size.y, "the value sits under its label");
        let folder_tab = painted.pos_of(t().export_to_folder);
        assert!(
            destination.y < folder_tab.y && folder_tab.y < folder.y,
            "the Folder/Immich choice sits between the header and the rows"
        );
    }

    /// Typing a server URL and an API key into the Immich rows enables
    /// Connect, and clicking it asks to connect.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn immich_connect_enables_once_both_fields_are_typed() {
        use crate::app::test_support::{click, frame, settled};
        use crate::export::ExportTarget;
        use crate::i18n::t;
        use crate::ui::UiAction;

        let (mut app, _) = folder_app(2);
        app.sel = Some(0);
        app.exports.form_open = true;
        app.exports.settings.target = ExportTarget::Immich;
        let painted = settled(&mut app);
        let connect = painted.pos_of(t().immich_connect);
        let (actions, _) = click(&mut app, connect);
        assert!(
            actions.is_empty(),
            "Connect is disabled while both fields are empty"
        );

        let type_into = |app: &mut App, at: egui::Pos2, text: &str| {
            let (actions, _) = click(app, at);
            app.apply_ui_actions(actions);
            let (actions, _) = frame(app, vec![egui::Event::Text(text.into())]);
            app.apply_ui_actions(actions);
            settled(app)
        };
        let field_x = painted.pos_of(t().immich_url_example).x + 20.0;
        // Each field sits under its label, the URL's between it and the example.
        let url_label = painted.pos_of(t().immich_server_url).y;
        let example = painted.pos_of(t().immich_url_example).y;
        let below_label = (example - url_label) / 2.0;
        let url_at = egui::pos2(field_x, url_label + below_label);
        let key_at = egui::pos2(field_x, painted.pos_of(t().immich_api_key).y + below_label);
        assert!(
            url_at.y < example && example < key_at.y,
            "the example sits under the URL field"
        );
        type_into(&mut app, url_at, "https://example.org");
        let painted = type_into(&mut app, key_at, "secret");
        match app.immich() {
            ImmichLink::Disconnected { url, key, .. } => {
                assert_eq!(
                    (url.as_str(), key.as_str()),
                    ("https://example.org", "secret")
                )
            }
            _ => panic!("still disconnected"),
        }
        let (actions, _) = click(&mut app, painted.pos_of(t().immich_connect));
        assert_eq!(actions, vec![UiAction::ConnectImmich]);
    }

    /// Once connected, the Album dropdown lists the account's albums after
    /// "No album" and "New album", and picking New asks for its name.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_album_dropdown_lists_albums_and_new_asks_for_a_name() {
        use crate::app::test_support::{click, settled};
        use crate::export::immich::{Account, Album, ImmichServer};
        use crate::export::{AlbumChoice, ExportTarget};
        use crate::i18n::t;
        use crate::ui::UiAction;

        let (mut app, _) = folder_app(2);
        app.sel = Some(0);
        app.exports.form_open = true;
        app.exports.settings.target = ExportTarget::Immich;
        let album = |id: &str, name: &str| Album {
            id: id.into(),
            name: name.into(),
        };
        app.exports.immich = ImmichLink::Connected {
            server: std::sync::Arc::new(ImmichServer::offline("https://immich.test")),
            account: Account {
                name: "Ada".into(),
                email: "ada@example.com".into(),
            },
            albums: Ok(vec![album("1", "Beach"), album("2", "Wedding")]),
        };
        let painted = settled(&mut app);
        let (_, painted) = click(&mut app, painted.pos_of(t().album_none));
        assert!(
            painted.has("Beach") && painted.has("Wedding"),
            "{:?}",
            painted.texts()
        );

        let (actions, _) = click(&mut app, painted.pos_of("Wedding"));
        let [UiAction::SetExportSettings(picked)] = &actions[..] else {
            panic!("{actions:?}");
        };
        assert_eq!(
            picked.album,
            AlbumChoice::Existing {
                id: "2".into(),
                name: "Wedding".into()
            }
        );

        app.exports.settings.album = AlbumChoice::New(String::new());
        let painted = settled(&mut app);
        assert!(painted.has(t().album_name), "{:?}", painted.texts());
        assert_eq!(app.export_blocker(), Some(t().album_name_needed));
        assert!(
            painted.has(t().album_name_needed),
            "the blocker shows under Export"
        );
    }
}
