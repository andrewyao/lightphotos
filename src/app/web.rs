//! wasm32-only file access and decoding. Picks and lists folders through the
//! File System Access API, reads photo bytes on the main thread (only it holds
//! the file handles), and sends them to the Web Worker pool. Results land in
//! `loader.rs`'s caches through its `*_external` methods.

use super::*;
use crate::jobs::thumbnail::THUMB_PX;
use crate::navigation::Playlist;
use crate::web::web_decode::JobKind;
use crate::web::web_fs;

/// Cap on concurrent file reads from the picked folder, shared by thumbnails,
/// the loupe, and the cache sweep. Chrome throws `NotReadableError` when too
/// many reads run at once. 2 is the highest value observed never to trip it;
/// 4 is used anyway, because the retry backoff below recovers a read that does
/// fail and the extra concurrency is worth more than those retries cost. If
/// folder loads stall, lower it.
const MAX_CONCURRENT_READS: u32 = 4;

/// Thumbnails read but not yet decoded, per decode thread. The read cap above
/// does not bound this: an OPFS read takes milliseconds, so without it every
/// missing cell's whole file sat in the queue at once, 25 MB a RAW, and the
/// shared 4 GB wasm heap ran out and killed decode threads. Wasm memory never
/// shrinks, so the high-water mark is what counts.
const THUMBS_IN_FLIGHT_PER_THREAD: usize = 1;

/// Read failures a key tolerates before it is marked failed for good. Chrome's
/// read failures are transient and can last longer than a few seconds.
const MAX_READ_RETRIES: u8 = 10;

/// Delay before retry `attempt` (1-based), drawn uniformly from `[0, cap]`
/// where `cap` doubles from 500 ms up to 10 s ("full jitter"). Keys that fail
/// together would retry together and collide again with a fixed delay. The
/// random spread breaks them apart.
fn retry_backoff(attempt: u8) -> std::time::Duration {
    let cap_ms = 500u64
        .saturating_mul(1u64 << attempt.saturating_sub(1).min(20))
        .min(10_000);
    let jittered_ms = (js_sys::Math::random() * cap_ms as f64).max(50.0);
    std::time::Duration::from_millis(jittered_ms as u64)
}

/// True in a browser on a Mac, where Cmd rather than Ctrl is the command key.
pub(crate) fn browser_is_mac() -> bool {
    thread_local! {
        static MAC: bool = web_sys::window()
            .and_then(|w| w.navigator().platform().ok())
            .is_some_and(|p| p.starts_with("Mac"));
    }
    MAC.with(|mac| *mac)
}

/// Puts text that egui copied or cut on the browser clipboard. egui_winit's
/// clipboard is an in-app string on the web build, so without this a copy
/// reaches nothing outside the page.
pub(crate) fn write_web_clipboard(commands: &[egui::OutputCommand]) {
    let Some(window) = web_sys::window() else {
        return;
    };
    for command in commands {
        if let egui::OutputCommand::CopyText(text) = command {
            let promise = window.navigator().clipboard().write_text(text);
            wasm_bindgen_futures::spawn_local(async move {
                if let Err(e) = wasm_bindgen_futures::JsFuture::from(promise).await {
                    web_sys::console::warn_1(&format!("clipboard write failed: {e:?}").into());
                }
            });
        }
    }
}

/// The metadata the browser's `File` gives without reading the bytes: size,
/// modified time, and the format from the extension.
async fn file_facts(
    path: &Path,
    handle: &web_sys::FileSystemFileHandle,
) -> image_decode::ImageMetadata {
    let mut meta = image_decode::ImageMetadata {
        format: image_decode::format_name(path),
        ..Default::default()
    };
    if let Ok(file) = web_fs::stat(handle).await {
        meta.file_size = Some(file.size() as u64);
        let modified = std::time::SystemTime::UNIX_EPOCH
            + std::time::Duration::from_millis(file.last_modified() as u64);
        meta.modified = Some(image_decode::local_date(modified));
    }
    meta
}

pub(crate) struct Web {
    /// A Reopen Session waiting on its folder's listing, then on each
    /// listing down to its subfolder.
    session_restore: Option<session::Session>,
    /// True while `showDirectoryPicker` and its listing are in flight. Disables
    /// the landing page's "Choose Folder" button so a second picker can't open.
    folder_pending: bool,
    /// Set while `fonts::fetch_full_cjk` is running or once it has succeeded,
    /// so the font downloads at most once a session. A failed fetch clears it.
    full_cjk_requested: Arc<std::sync::atomic::AtomicBool>,
    folder_tx: Sender<Result<crate::web::web_fs::PickedFolder, String>>,
    folder_rx: Receiver<Result<crate::web::web_fs::PickedFolder, String>>,
    /// File handles for the open folder's images, keyed like the playlist
    /// entries. A picked folder has no OS path, so every read goes through these.
    file_handles: HashMap<PathBuf, web_sys::FileSystemFileHandle>,
    /// Directory handles for every folder browsed so far, keyed by relative
    /// path with the picked root's name first. The catalog's sidecar handle
    /// switches to the current folder's entry on each navigation.
    dir_handles: HashMap<PathBuf, web_sys::FileSystemDirectoryHandle>,
    /// Per-folder thumbnail cache index, shared by that folder's async writes.
    thumb_cleanup:
        HashMap<PathBuf, std::rc::Rc<std::cell::RefCell<crate::web::web_thumb_cache::Cleanup>>>,
    /// Async subfolder listings, tagged with the navigation generation that
    /// asked for them. `poll_dir_listing` drops stale generations.
    dirlist_tx: Sender<(u64, PathBuf, Result<crate::web::web_fs::DirListing, String>)>,
    dirlist_rx: Receiver<(u64, PathBuf, Result<crate::web::web_fs::DirListing, String>)>,
    /// `(directory, generation)` pairs with a listing in flight. Stops per-frame
    /// polling from re-requesting, while a newer generation may retry.
    dirlist_inflight: std::collections::HashSet<(PathBuf, u64)>,
    /// Finished browser JPEG writes, drained into `on_export_outcomes`. The
    /// browser counterpart of native `Exporter::poll()`.
    export_tx: Sender<crate::export::ExportOutcome>,
    export_rx: Receiver<crate::export::ExportOutcome>,
    /// Text read from the browser clipboard, fed to egui as a paste on the
    /// next frame. See `request_web_paste`.
    paste_tx: Sender<String>,
    paste_rx: Receiver<String>,
    pending_nav: Option<WebPendingNav>,
    /// Bumped on every tree action. A listing may apply only the navigation
    /// deferred by the latest action.
    nav_generation: u64,
    pending_nav_generation: u64,
    /// Bumped only when a folder pick replaces the handle maps. Thumbnail jobs
    /// carry it so results for an old pick are dropped: browser paths start
    /// with the folder's name, so re-picking a same-named folder would
    /// otherwise match stale jobs. Not `nav_generation`, which bumps on
    /// every tree action and would cancel decodes while arrowing through the tree.
    handle_generation: u64,
    /// Thumbnail decodes in flight on the Web Worker pool. `loader.rs`'s queue
    /// has no workers on wasm32, so its own in-flight set doesn't apply.
    thumb_inflight: HashSet<(PathBuf, u32)>,
    /// Failed cached decodes waiting for a slot to read the source file. Their
    /// keys stay in `thumb_inflight` so normal requests don't retry the
    /// corrupt cache.
    thumb_recovery_pending: Vec<crate::web::web_decode::PoolResult>,
    /// `get_file()` reads in flight across all decode tiers. Chrome throws
    /// `NotReadableError` when too many reads are open against one folder, so
    /// `MAX_CONCURRENT_READS` caps this. A key that can't start this frame
    /// stays out of its in-flight set and is retried next frame.
    /// `Rc<Cell<_>>` so the `spawn_local` task can decrement it without `&mut App`.
    read_inflight: std::rc::Rc<std::cell::Cell<u32>>,
    /// Consecutive failure count and earliest next retry time per key.
    /// Chrome's `NotReadableError` is usually transient, so a key gives up only
    /// after `MAX_READ_RETRIES` failures. Retries a frame apart all fail, so the
    /// deadline backs off per attempt (`retry_backoff`). A decode failure
    /// gives up at once, see `web_decode::Failure`. Cleared on success or
    /// when giving up.
    thumb_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    preview_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    /// Loupe preview decodes. `try_show` asks for the preview every frame, so
    /// `preview_failed` stops a doomed decode from retrying forever.
    preview_inflight: HashSet<(PathBuf, u32)>,
    preview_failed: HashSet<(PathBuf, u32)>,
    /// The loupe's fast screen-fit decode (`JobKind::Speed`). Tracked apart
    /// from `Preview` because both results usually have the same size, so they
    /// can't share `loader.rs`'s preview slot. `poll_web_preview` uploads a
    /// `Speed` result directly instead.
    speed_inflight: HashSet<(PathBuf, u32)>,
    speed_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    speed_failed: HashSet<(PathBuf, u32)>,
    /// The loupe's zoom-triggered full-resolution decode, the wasm32
    /// counterpart of `Loader::request_full`. Results land through
    /// `loader.insert_full_external`, where `try_show` finds them.
    full_inflight: HashSet<(PathBuf, u32)>,
    full_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    full_failed: HashSet<(PathBuf, u32)>,

    /// Where each running export's JPEG goes. The loader's threads bake it;
    /// the destination folder handle cannot leave the main thread.
    exports: crate::web::web_exports::WebExports,
    /// `Preview` and `Speed` results that `poll_web_thumbs` pulled off the
    /// pool's single shared channel. `poll_web_preview` consumes them in the
    /// same frame.
    preview_pending: Vec<crate::web::web_decode::PoolResult>,
    /// `Full` results set aside the same way, for `poll_web_full`.
    full_pending: Vec<crate::web::web_decode::PoolResult>,
}

impl Web {
    pub(super) fn new() -> Self {
        let (folder_tx, folder_rx) = std::sync::mpsc::channel();
        let (dirlist_tx, dirlist_rx) = std::sync::mpsc::channel();
        let (export_tx, export_rx) = std::sync::mpsc::channel();
        let (paste_tx, paste_rx) = std::sync::mpsc::channel();
        Self {
            session_restore: None,
            folder_pending: false,
            full_cjk_requested: Default::default(),
            file_handles: HashMap::new(),
            dir_handles: HashMap::new(),
            thumb_cleanup: HashMap::new(),
            dirlist_inflight: std::collections::HashSet::new(),
            pending_nav: None,
            nav_generation: 0,
            pending_nav_generation: 0,
            handle_generation: 0,
            thumb_inflight: HashSet::new(),
            thumb_recovery_pending: Vec::new(),
            read_inflight: std::rc::Rc::new(std::cell::Cell::new(0)),
            thumb_retries: HashMap::new(),
            preview_retries: HashMap::new(),
            preview_inflight: HashSet::new(),
            preview_failed: HashSet::new(),
            speed_inflight: HashSet::new(),
            speed_retries: HashMap::new(),
            speed_failed: HashSet::new(),
            full_inflight: HashSet::new(),
            full_retries: HashMap::new(),
            full_failed: HashSet::new(),
            exports: Default::default(),
            preview_pending: Vec::new(),
            full_pending: Vec::new(),
            folder_tx,
            folder_rx,
            dirlist_tx,
            dirlist_rx,
            export_tx,
            export_rx,
            paste_tx,
            paste_rx,
        }
    }

    /// The handle of a folder the person picked or opened.
    pub(super) fn dir_handle(&self, path: &Path) -> Option<web_sys::FileSystemDirectoryHandle> {
        self.dir_handles.get(path).cloned()
    }

    pub(super) fn has_dir(&self, path: &Path) -> bool {
        self.dir_handles.contains_key(path)
    }

    pub(super) fn file_handles(&self) -> &HashMap<PathBuf, web_sys::FileSystemFileHandle> {
        &self.file_handles
    }

    /// Drops a trashed photo's handles.
    pub(super) fn forget_handles(&mut self, path: &Path) {
        self.file_handles.remove(path);
        self.dir_handles.remove(path);
    }

    /// File reads in flight, shared with the reads' own tasks.
    pub(super) fn read_inflight(&self) -> std::rc::Rc<std::cell::Cell<u32>> {
        self.read_inflight.clone()
    }

    pub(super) fn exports(&self) -> crate::web::web_exports::WebExports {
        self.exports.clone()
    }

    pub(super) fn folder_pending(&self) -> bool {
        self.folder_pending
    }

    /// Text pasted since the last frame.
    pub(super) fn take_pastes(&self) -> impl Iterator<Item = String> + '_ {
        std::iter::from_fn(|| self.paste_rx.try_recv().ok())
    }
}

impl App {
    /// Invalidate in-flight directory listings and any navigation deferred
    /// behind one. Runs on every tree action. Thumbnails are keyed to the
    /// handle maps instead, see [`App::invalidate_web_thumb_handles`].
    pub(crate) fn supersede_web_pending_nav(&mut self) {
        self.web.nav_generation = self.web.nav_generation.wrapping_add(1);
        self.web.pending_nav = None;
        self.web.session_restore = None;
    }

    /// Invalidate thumbnail work after a pick replaces the handle maps.
    /// `poll_web_thumbs` drops the old results on arrival. Clearing their keys
    /// lets the new pick request the same browser paths again.
    pub(crate) fn invalidate_web_thumb_handles(&mut self) {
        self.web.handle_generation = self.web.handle_generation.wrapping_add(1);
        self.web.thumb_inflight.clear();
        self.web.thumb_recovery_pending.clear();
        self.web.thumb_retries.clear();
    }

    pub(crate) fn defer_web_nav(&mut self, nav: crate::app::WebPendingNav) {
        self.web.pending_nav_generation = self.web.nav_generation;
        self.web.pending_nav = Some(nav);
    }

    /// Writes each JPEG a decode thread finished baking, then reports the
    /// completed writes.
    pub(crate) fn land_web_exports(&mut self) {
        let baked = self
            .loader
            .as_mut()
            .map(|l| l.take_web_exports())
            .unwrap_or_default();
        for r in self.web.exports.land(baked) {
            let crate::web::web_exports::ExportResult {
                path,
                folder,
                dest_dir,
                filename,
                result,
            } = r;
            let tx = self.web.export_tx.clone();
            match result {
                Ok(jpeg) => {
                    let file_handles = self.web.file_handles.clone();
                    let dest = dest_dir.join(&filename);
                    wasm_bindgen_futures::spawn_local(async move {
                        let result = crate::web::web_export_fs::WebFs::new(folder, file_handles)
                            .write_atomic(&dest, &jpeg)
                            .await
                            .map(|()| crate::export::ExportLanding::File(dest.clone()));
                        let _ = tx.send(crate::export::ExportOutcome { src: path, result });
                    });
                }
                Err(e) => {
                    let _ = tx.send(crate::export::ExportOutcome {
                        src: path,
                        result: Err(e),
                    });
                }
            }
        }
        let export_outcomes: Vec<_> =
            std::iter::from_fn(|| self.web.export_rx.try_recv().ok()).collect();
        if !export_outcomes.is_empty() {
            self.on_export_outcomes(export_outcomes);
            self.request_redraw();
        }
    }

    /// Makes Cmd egui's command key in a Mac browser. egui_winit picks the
    /// command key at compile time and wasm is never `target_os = "macos"`, so
    /// it takes Ctrl: Cmd+C didn't copy and Cmd+V typed a "v". Call after
    /// egui_winit handles `ModifiersChanged`, which overwrites these fields.
    pub(crate) fn use_mac_command_key(modifiers: &mut egui::Modifiers, held: ModifiersState) {
        if browser_is_mac() {
            modifiers.mac_cmd = held.super_key();
            modifiers.command = held.super_key();
        }
    }

    /// Reads the browser clipboard on Cmd/Ctrl+V. egui_winit's paste carries
    /// only its in-app clipboard on wasm. The read is async, so the text
    /// reaches egui a frame later through `web_paste_rx`.
    pub(crate) fn request_web_paste(&mut self, code: KeyCode) {
        let command = if browser_is_mac() {
            self.modifiers.super_key()
        } else {
            self.modifiers.control_key()
        };
        if code != KeyCode::KeyV || !command {
            return;
        }
        let Some(browser) = web_sys::window() else {
            return;
        };
        let promise = browser.navigator().clipboard().read_text();
        let tx = self.web.paste_tx.clone();
        let window = self.window.clone();
        wasm_bindgen_futures::spawn_local(async move {
            match wasm_bindgen_futures::JsFuture::from(promise).await {
                Ok(text) => {
                    let text = text.as_string().unwrap_or_default().replace("\r\n", "\n");
                    if !text.is_empty() {
                        let _ = tx.send(text);
                        if let Some(w) = window {
                            w.request_redraw();
                        }
                    }
                }
                Err(e) => web_sys::console::warn_1(&format!("clipboard read failed: {e:?}").into()),
            }
        });
    }

    /// Sizes the thumbnail cache for the visible grid plus pending Auto Tone
    /// photos, so an Auto Tone thumbnail is not evicted before
    /// `poll_auto_tone` reads it. Call before draining worker results.
    pub(crate) fn prepare_web_thumb_cache(&mut self) {
        let visible = self.working_thumb_keys().len();
        // The window, not the whole batch. Sizing this to `autotone_pending`
        // told the cache to hold the entire selection, which on a 32-bit heap
        // that never shrinks is how a large folder ran the tab out of memory.
        let capacity = visible + self.autotone.window_len();
        if let Some(loader) = &mut self.loader {
            loader.set_thumb_working_set_size(capacity);
        }
    }

    /// Opens the browser's folder picker unless one is already open.
    pub(crate) fn request_folder_pick(&mut self) {
        if self.web.folder_pending {
            return;
        }
        self.web.folder_pending = true;
        self.web.session_restore = None;
        let tx = self.web.folder_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::pick_and_list_folder().await;
            let _ = tx.send(result);
        });
        self.request_redraw();
    }

    /// Reopen Session: list the last picked folder again, or show the picker
    /// if it can't be. `poll_folder_pick` restores the rest of `session`.
    /// Must run inside the click's user activation, which the browser's
    /// permission prompt and the fallback picker both need.
    pub(crate) fn request_session_reopen(&mut self, session: super::session::Session) {
        if self.web.folder_pending {
            return;
        }
        self.web.folder_pending = true;
        self.web.session_restore = Some(session);
        let tx = self.web.folder_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = match web_fs::reopen_saved_folder().await {
                Ok(picked) => Ok(picked),
                Err(e) => {
                    web_sys::console::warn_1(
                        &format!("[web] Reopen Session falls back to the picker: {e}").into(),
                    );
                    web_fs::pick_and_list_folder().await
                }
            };
            let _ = tx.send(result);
        });
        self.request_redraw();
    }

    /// Loads a finished pick through `load_playlist`, like native
    /// `load_folder`. Returns true while a pick is still open.
    pub(crate) fn poll_folder_pick(&mut self) -> bool {
        if let Ok(result) = self.web.folder_rx.try_recv() {
            self.web.folder_pending = false;
            let restore = self.web.session_restore.take();
            match result {
                Ok(picked) => {
                    // A new folder invalidates pending listings, deferred
                    // navigation, and thumbnail decodes using the old handles.
                    self.supersede_web_pending_nav();
                    self.invalidate_web_thumb_handles();
                    self.fetch_cjk_font_for(picked.handles.keys().chain(picked.dir_handles.keys()));
                    self.web.file_handles = picked.handles;
                    self.web.dir_handles = picked.dir_handles;
                    self.web.thumb_cleanup.clear();
                    self.subdirs.clear();

                    let root = picked.dir.clone();
                    let mut first_level: Vec<PathBuf> = self
                        .web
                        .dir_handles
                        .keys()
                        .filter(|p| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect();
                    crate::navigation::sort_by_name(&mut first_level);
                    self.subdirs.insert(root.clone(), first_level);

                    // Must precede `load_playlist`, whose catalog load reads
                    // sidecars through this handle.
                    let root_handle = self.web.dir_handles.get(&root).cloned();
                    self.catalog.set_wasm_dir_handle(root_handle);
                    let playlist = Playlist::from_entries(root.clone(), picked.entries);
                    self.load_playlist(playlist, root.clone());
                    // Show the folder tree only once the playlist is loaded.
                    self.folder_root = Some(root.clone());
                    self.expanded = std::collections::HashSet::from([root.clone()]);
                    self.mode = ViewMode::Grid;
                    // The picker fallback may have opened a different folder.
                    self.web.session_restore = restore.filter(|s| s.root == root);
                    self.continue_web_session_restore();
                }
                Err(e) => {
                    // Usually a cancelled picker, but a permission or listing
                    // failure lands here too, so always show it.
                    self.set_status(
                        StatusKind::Error,
                        (crate::i18n::t().open_folder_failed)(&e.to_string()),
                    );
                }
            }
            self.request_redraw();
        }
        self.web.folder_pending
    }

    /// The wasm32 version of `request_working_thumbs`. Reads each missing
    /// thumbnail's bytes (or its cached JPEG) and sends them to the Web Worker
    /// pool. Returns true if any thumbnail is still missing.
    pub(crate) fn request_web_thumbs(&mut self) -> bool {
        let px = THUMB_PX;
        let mut keys: Vec<PathBuf> = self
            .working_thumb_keys()
            .into_iter()
            .map(|(p, _, _)| p)
            .collect();
        // Auto Tone needs thumbnails for its photos even off screen, and
        // `loader.rs`'s queue has no workers on wasm32. They go after the
        // visible grid so the grid reads first.
        keys.extend(self.autotone.pending().cloned());
        self.prepare_web_thumb_cache();
        let Some(decoder) = self.loader.as_ref().map(|l| l.web_decoder()) else {
            return false;
        };

        // A queued thumbnail whose cell scrolled away would still decode
        // ahead of the cells now on screen, so it leaves the queue and its
        // key goes back to missing.
        let wanted: std::collections::HashSet<&Path> = keys.iter().map(PathBuf::as_path).collect();
        if let Some(loader) = &mut self.loader {
            for key in loader.retain_web_thumbs(|p| wanted.contains(p)) {
                self.web.thumb_inflight.remove(&key);
            }
        }

        let max_in_flight = decoder.threads() * THUMBS_IN_FLIGHT_PER_THREAD + 2;
        let mut any_missing = false;
        for path in keys {
            let key = (path.clone(), px);
            let already_have = self
                .loader
                .as_ref()
                .is_some_and(|l| l.get_thumb(&path, px).is_some() || l.thumb_failed(&path, px));
            if already_have || self.web.thumb_inflight.contains(&key) {
                continue;
            }
            any_missing = true;
            if self
                .web
                .thumb_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
            {
                continue;
            }
            // Out of read slots, or enough bytes already waiting on the
            // threads. The key stays off `web_thumb_inflight`, so a later
            // frame tries it again, and in on-screen order.
            if self.web.read_inflight.get() >= MAX_CONCURRENT_READS
                || self.web.thumb_inflight.len() >= max_in_flight
            {
                continue;
            }
            let Some(handle) = self.web.file_handles.get(&path).cloned() else {
                // An Auto Tone photo deleted mid-batch has no handle. Mark it
                // failed so `poll_auto_tone` drops it instead of waiting.
                if let Some(loader) = &mut self.loader {
                    loader.mark_thumb_failed_external(path.clone(), px);
                }
                continue;
            };
            // The folder holding this photo's `.lightphotos/` cache. If it is
            // missing, the photo decodes without the cache.
            let cache_dir = path
                .parent()
                .and_then(|d| self.web.dir_handles.get(d))
                .cloned();
            let generation = self.web.handle_generation;
            self.web.thumb_inflight.insert(key);
            self.web.read_inflight.set(self.web.read_inflight.get() + 1);
            let read_inflight = self.web.read_inflight.clone();
            let pool = decoder.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let is_raw = crate::decode::image_decode::is_raw_extension(&path);

                // The cache entry is named by size and mtime, which `stat`
                // reads without reading the file's bytes.
                let entry = match (web_fs::stat(&handle).await, path.file_name()) {
                    (Ok(file), Some(name)) => Some(crate::web::web_thumb_cache::entry_name(
                        name,
                        crate::web::web_thumb_cache::key_for(&file),
                    )),
                    _ => None,
                };

                // A cache hit decodes a ~45 KB JPEG and never reads the
                // multi-megabyte source.
                if let (Some(name), Some(root)) = (&entry, &cache_dir) {
                    if let Some(cached) = crate::web::web_thumb_cache::load(root, name).await {
                        read_inflight.set(read_inflight.get().saturating_sub(1));
                        let buf = js_sys::Uint8Array::from(cached.as_slice()).buffer();
                        pool.submit_thumb(path, px, buf, false, entry, generation, true);
                        return;
                    }
                }

                let result = web_fs::read_array_buffer(&handle).await;
                read_inflight.set(read_inflight.get().saturating_sub(1));
                match result {
                    // On a miss the worker also encodes a JPEG, and
                    // `poll_web_thumbs` stores it.
                    Ok(bytes) => {
                        pool.submit_thumb(path, px, bytes, is_raw, entry, generation, false)
                    }
                    Err(e) => {
                        web_sys::console::error_1(
                            &format!("[web] reading bytes failed for {}: {e}", path.display())
                                .into(),
                        );
                        // Report through the decoder so `poll_web_thumbs` clears
                        // the in-flight key like any decode failure.
                        pool.fail(path, px, JobKind::Thumb, Some(generation), e);
                    }
                }
            });
        }
        any_missing
    }

    /// Writes a worker-encoded thumbnail JPEG into the photo's `.lightphotos/`
    /// folder in the background. Failures are logged, not toasted: they only
    /// cost a re-decode next session, and a full disk would toast once per
    /// photo.
    fn store_web_thumb(&mut self, path: &Path, name: String, bytes: Vec<u8>) {
        let Some(root) = path
            .parent()
            .and_then(|d| self.web.dir_handles.get(d))
            .cloned()
        else {
            return;
        };
        let cleanup = self
            .web
            .thumb_cleanup
            .entry(path.parent().unwrap().to_path_buf())
            .or_default()
            .clone();
        let display = path.to_path_buf();
        wasm_bindgen_futures::spawn_local(async move {
            if let Err(e) = crate::web::web_thumb_cache::store(&root, &name, &bytes, &cleanup).await
            {
                web_sys::console::warn_1(
                    &format!(
                        "[web] could not cache thumbnail for {}: {e}",
                        display.display()
                    )
                    .into(),
                );
            }
        });
    }

    /// Drains the Worker pool's results. Thumbnails go into `loader.rs`'s
    /// cache; loupe results are set aside for `poll_web_preview` and
    /// `poll_web_full`. Returns the `(path, max_px)` keys that arrived.
    pub(crate) fn poll_web_thumbs(&mut self) -> Vec<(PathBuf, u32)> {
        let mut arrived = Vec::new();
        let pending = std::mem::take(&mut self.web.thumb_recovery_pending);
        let landed = self
            .loader
            .as_mut()
            .map(|l| l.take_web_results())
            .unwrap_or_default();
        for r in pending.into_iter().chain(landed) {
            if r.kind == JobKind::Full {
                self.web.full_pending.push(r);
                continue;
            }
            if r.kind != JobKind::Thumb {
                self.web.preview_pending.push(r);
                continue;
            }
            // Browser paths start at the picked folder's name, so two picks of
            // same-named folders share paths. Drop results from before the
            // last pick so they cannot resolve the new folder's handles.
            if r.generation != Some(self.web.handle_generation) {
                continue;
            }
            let recover_source = r.needs_source_decode();
            // A corrupt cache entry needs a source read. With no read slot
            // free, park the result for next frame without using a retry.
            if recover_source && self.web.read_inflight.get() >= MAX_CONCURRENT_READS {
                self.web.thumb_recovery_pending.push(r);
                continue;
            }
            let crate::web::web_decode::PoolResult {
                path,
                target,
                result,
                jpeg,
                cache_name,
                generation,
                ..
            } = r;
            let key = (path.clone(), target);
            // Re-decode from the source, keeping the key in flight and not
            // using a retry. With no file handle this falls through to the
            // failure arm, whose retry limit eventually marks it failed.
            if recover_source {
                if let (Some(handle), Some(pool)) = (
                    self.web.file_handles.get(&path).cloned(),
                    self.loader.as_ref().map(|l| l.web_decoder()),
                ) {
                    let read_inflight = self.web.read_inflight.clone();
                    read_inflight.set(read_inflight.get() + 1);
                    wasm_bindgen_futures::spawn_local(async move {
                        let source = web_fs::read_array_buffer(&handle).await;
                        read_inflight.set(read_inflight.get().saturating_sub(1));
                        match source {
                            Ok(bytes) => {
                                let is_raw = crate::decode::image_decode::is_raw_extension(&path);
                                pool.submit_thumb(
                                    path,
                                    target,
                                    bytes,
                                    is_raw,
                                    cache_name,
                                    generation.expect("validated thumbnail generation"),
                                    false,
                                );
                            }
                            Err(e) => pool.fail(path, target, JobKind::Thumb, generation, e),
                        }
                    });
                    continue;
                }
            }
            self.web.thumb_inflight.remove(&key);
            // Ignore results for files deleted while they decoded.
            if !self
                .playlist
                .as_ref()
                .is_some_and(|playlist| playlist.entries().contains(&path))
                || !self.web.file_handles.contains_key(&path)
            {
                continue;
            }
            match result {
                Ok(img) => {
                    self.web.thumb_retries.remove(&key);
                    // `jpeg` is `Some` only on a cache miss.
                    if let (Some(bytes), Some(name)) = (jpeg, cache_name) {
                        self.store_web_thumb(&path, name, bytes);
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_thumb_external(
                            path.clone(),
                            target,
                            std::sync::Arc::new(img),
                        );
                    }
                    arrived.push((path, target));
                }
                Err(e) => {
                    // Log with `web_sys::console`: `eprintln!` goes nowhere on
                    // wasm32.
                    let entry = self
                        .web
                        .thumb_retries
                        .entry(key)
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if e.is_read() && retries <= MAX_READ_RETRIES {
                        // `request_web_thumbs` retries once this deadline
                        // passes.
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] thumbnail decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] thumbnail decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web.thumb_retries.remove(&(path.clone(), target));
                        crate::web::analytics::decode_failed(&path, "thumbnail");
                        if let Some(loader) = &mut self.loader {
                            loader.mark_thumb_failed_external(path.clone(), target);
                        }
                        arrived.push((path, target));
                    }
                }
            }
        }
        if !arrived.is_empty() {
            self.request_redraw();
        }
        arrived
    }

    /// The wasm32 version of `loader.request_preview`: reads the wanted photo
    /// and asks the Worker pool for a `Preview` decode at `preview_px()`.
    ///
    /// For RAW it also sends a fast `Speed` decode of the same bytes, unless
    /// something is already shown for this photo. Whichever lands first
    /// paints, and the `Preview` replaces a `Speed` result. The same read also
    /// parses the photo's metadata if it is missing, see `WebExifJob`.
    /// Then, with whatever read budget the wanted photo left, it requests a
    /// `Preview` for each tile on the group's shown page that has none yet.
    /// Returns true if a read started.
    pub(crate) fn request_web_preview(&mut self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        // The grid's selection is kept too: its metadata read for the info
        // panel is not a stale Loupe decode.
        let selected = self.selected_path();
        let tiles: Vec<PathBuf> = self
            .compare_tiles()
            .map_or(Vec::new(), |t| t.page_paths().to_vec());
        if let Some(loader) = &mut self.loader {
            let keep = |p: &Path| {
                p == path || selected.as_deref() == Some(p) || tiles.iter().any(|t| t == p)
            };
            for (kind, p, t) in loader.retain_web_loupe(keep) {
                match kind {
                    JobKind::Speed => self.web.speed_inflight.remove(&(p, t)),
                    _ => self.web.preview_inflight.remove(&(p, t)),
                };
            }
        }
        let (quality_needed, speed_needed) = self.web_loupe_reads(&path);
        let mut started = (quality_needed || speed_needed)
            && self.start_web_preview_read(path.clone(), quality_needed, speed_needed);
        let missing: Vec<PathBuf> = self
            .compare_tiles()
            .into_iter()
            .flat_map(|t| t.page_paths().iter().zip(&t.members))
            .filter(|(p, slot)| slot.is_none() && **p != path)
            .map(|(p, _)| p.clone())
            .collect();
        for p in missing {
            if self.web.read_inflight.get() >= MAX_CONCURRENT_READS {
                break;
            }
            if self.web_loupe_reads(&p).0 {
                started |= self.start_web_preview_read(p, true, false);
            }
        }
        started
    }

    /// Reads `path` and submits the `Preview` and `Speed` decodes asked for.
    /// Returns true if a read started.
    fn start_web_preview_read(
        &mut self,
        path: PathBuf,
        quality_needed: bool,
        speed_needed: bool,
    ) -> bool {
        let target = self.preview_px();
        let key = (path.clone(), target);
        let is_raw = crate::decode::image_decode::is_raw_extension(&path);
        // Shares the read budget with thumbnails. Callers retry every frame.
        if self.web.read_inflight.get() >= MAX_CONCURRENT_READS {
            return false;
        }
        let Some(handle) = self.web.file_handles.get(&path).cloned() else {
            return false;
        };
        let Some(loader) = self.loader.as_mut() else {
            return false;
        };
        let pool = loader.web_decoder();
        let exif_needed = !self.exif_cache.contains_key(&path) && loader.begin_web_exif(&path);
        if quality_needed {
            self.web.preview_inflight.insert(key.clone());
        }
        if speed_needed {
            self.web.speed_inflight.insert(key);
        }
        self.web.read_inflight.set(self.web.read_inflight.get() + 1);
        let read_inflight = self.web.read_inflight.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let meta = if exif_needed {
                Some(file_facts(&path, &handle).await)
            } else {
                None
            };
            let result = web_fs::read_bytes(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match result {
                Ok(bytes) => {
                    let bytes = std::sync::Arc::new(bytes);
                    if let Some(meta) = meta {
                        pool.submit_exif(crate::web::web_decode::WebExifJob {
                            path: path.clone(),
                            bytes: bytes.clone(),
                            is_raw,
                            meta,
                        });
                    }
                    if speed_needed {
                        pool.submit(path.clone(), target, bytes.clone(), is_raw, JobKind::Speed);
                    }
                    if quality_needed {
                        pool.submit(path, target, bytes, is_raw, JobKind::Preview);
                    }
                }
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] reading bytes failed for {}: {e}", path.display()).into(),
                    );
                    if let Some(meta) = meta {
                        pool.finish_exif(path.clone(), meta);
                    }
                    if quality_needed {
                        pool.fail(path.clone(), target, JobKind::Preview, None, e.clone());
                    }
                    if speed_needed {
                        pool.fail(path, target, JobKind::Speed, None, e);
                    }
                }
            }
        });
        true
    }

    /// Which of the `Preview` and `Speed` decodes `path` still needs, as
    /// `(preview, speed)`, once `request_web_preview` can start a read.
    fn web_loupe_reads(&self, path: &Path) -> (bool, bool) {
        let target = self.preview_px();
        let key = (path.to_path_buf(), target);
        let is_raw = crate::decode::image_decode::is_raw_extension(path);
        let already_have = self
            .loader
            .as_ref()
            .is_some_and(|l| l.get_full(path).is_some() || l.get_preview(path, target).is_some());
        let quality_needed = !already_have
            && !self.web.preview_inflight.contains(&key)
            && !self.web.preview_failed.contains(&key)
            && !self
                .web
                .preview_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at);

        let speed_needed = is_raw
            && self.shown.path() != Some(path)
            && !self.web.speed_inflight.contains(&key)
            && !self.web.speed_failed.contains(&key)
            && !self
                .web
                .speed_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at);
        (quality_needed, speed_needed)
    }

    /// Read `path`'s metadata: size and modified time from the browser's
    /// `File`, the rest parsed from its bytes on a decode thread (see
    /// `WebExifJob`). The result arrives through `Loader::poll_all`. A
    /// failed read still reports the file facts, so the photo is not re-read
    /// every frame. For the Loupe's photo, `request_web_preview`'s read
    /// carries the parse instead, so it rides with the photo's decode.
    pub(crate) fn request_web_exif(&mut self, path: PathBuf) {
        if self.want.as_ref() == Some(&path) {
            let (quality, speed) = self.web_loupe_reads(&path);
            if quality || speed {
                return;
            }
        }
        if self.web.read_inflight.get() >= MAX_CONCURRENT_READS {
            return;
        }
        let Some(handle) = self.web.file_handles.get(&path).cloned() else {
            return;
        };
        let Some(loader) = self.loader.as_mut() else {
            return;
        };
        if !loader.begin_web_exif(&path) {
            return;
        }
        let decoder = loader.web_decoder();
        self.web.read_inflight.set(self.web.read_inflight.get() + 1);
        let read_inflight = self.web.read_inflight.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let meta = file_facts(&path, &handle).await;
            let read = web_fs::read_bytes(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match read {
                Ok(bytes) => decoder.submit_exif(crate::web::web_decode::WebExifJob {
                    is_raw: image_decode::is_raw_extension(&path),
                    path,
                    bytes: std::sync::Arc::new(bytes),
                    meta,
                }),
                Err(e) => {
                    web_sys::console::warn_1(
                        &format!("[web] metadata read failed for {}: {e}", path.display()).into(),
                    );
                    decoder.finish_exif(path, meta);
                }
            }
        });
    }

    /// Handles the `Preview` and `Speed` results `poll_web_thumbs` set aside.
    /// `Preview` goes into `loader.rs`'s preview cache. `Speed` skips the
    /// cache and is uploaded only if nothing is shown for the photo yet,
    /// because it can land after the sharper `Preview`. Keys that run out of
    /// retries go in `web_preview_failed` or `web_speed_failed`.
    pub(crate) fn poll_web_preview(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web.preview_pending);
        for crate::web::web_decode::PoolResult {
            kind,
            path,
            target,
            result,
            ..
        } in pending
        {
            let key = (path.clone(), target);
            match kind {
                JobKind::Speed => {
                    self.web.speed_inflight.remove(&key);
                    match result {
                        Ok(img) => {
                            self.web.speed_retries.remove(&key);
                            if self.want.as_deref() == Some(path.as_path())
                                && self.shown.path() != Some(path.as_path())
                            {
                                self.upload_shown(
                                    &path,
                                    &img,
                                    Shown::Preview(path.clone(), target, img.width.max(img.height)),
                                );
                                landed = true;
                            }
                        }
                        Err(e) => {
                            // No status toast: the `Preview` decode is still
                            // running independently.
                            let entry = self
                                .web
                                .speed_retries
                                .entry(key.clone())
                                .or_insert((0, Instant::now()));
                            entry.0 += 1;
                            let retries = entry.0;
                            if e.is_read() && retries <= MAX_READ_RETRIES {
                                entry.1 = Instant::now() + retry_backoff(retries);
                                web_sys::console::warn_1(
                                    &format!(
                                        "[web] speed decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                        path.display(),
                                        retries
                                    )
                                    .into(),
                                );
                            } else {
                                web_sys::console::warn_1(
                                    &format!(
                                        "[web] speed decode failed permanently for {}: {e}",
                                        path.display()
                                    )
                                    .into(),
                                );
                                self.web.speed_retries.remove(&key);
                                if self.want.as_deref() == Some(path.as_path()) {
                                    crate::web::analytics::decode_failed(&path, "speed");
                                }
                                self.web.speed_failed.insert(key);
                            }
                        }
                    }
                    continue;
                }
                // `Full` goes to `poll_web_full`, so it never reaches here.
                JobKind::Full => continue,
                JobKind::Preview | JobKind::Thumb => {}
            }
            self.web.preview_inflight.remove(&key);
            match result {
                Ok(img) => {
                    // Web metadata reads carry no pixel size, so take the
                    // source size from the decode and re-fit, as
                    // `on_exif_info` does natively.
                    let real_size = Some((img.width, img.height));
                    if self.want.as_deref() == Some(path.as_path()) && self.source_size != real_size
                    {
                        self.source_size = real_size;
                        if self.fitted {
                            self.fit_to_window();
                        }
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_preview_external(
                            path.clone(),
                            target,
                            std::sync::Arc::new(img),
                        );
                    }
                    self.web.preview_retries.remove(&key);
                    landed = true;
                }
                Err(e) => {
                    let entry = self
                        .web
                        .preview_retries
                        .entry(key.clone())
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if e.is_read() && retries <= MAX_READ_RETRIES {
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] preview decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] preview decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web.preview_retries.remove(&key);
                        self.web.preview_failed.insert(key);
                        if self.want.as_deref() == Some(path.as_path()) {
                            crate::web::analytics::decode_failed(&path, "preview");
                            self.set_status(
                                StatusKind::Error,
                                (crate::i18n::t().preview_failed)(&path.display().to_string()),
                            );
                        }
                    }
                }
            }
        }
        if landed {
            self.try_show();
            self.request_redraw();
        }
        landed
    }

    /// The wasm32 version of `loader.request_full`: reads the wanted photo and
    /// asks the Worker pool for a full decode at the GPU's max texture size.
    /// Returns true if a read started.
    pub(crate) fn request_web_full(&mut self) -> bool {
        // Called every frame, so it needs the same zoom check as
        // `ensure_full_for_zoom`, or every opened photo gets a full decode.
        if !self.full_wanted_for_zoom() {
            return false;
        }
        let Some(path) = self.want.clone() else {
            return false;
        };
        let Some(target) = self.renderer.as_ref().map(|r| r.max_dim) else {
            return false;
        };
        let key = (path.clone(), target);
        let already_have = self
            .loader
            .as_ref()
            .is_some_and(|l| l.get_full(&path).is_some());
        if already_have
            || self.web.full_inflight.contains(&key)
            || self.web.full_failed.contains(&key)
            || self
                .web
                .full_retries
                .get(&key)
                .is_some_and(|(_, retry_at)| Instant::now() < *retry_at)
        {
            return false;
        }
        if self.web.read_inflight.get() >= MAX_CONCURRENT_READS {
            return false;
        }
        let Some(handle) = self.web.file_handles.get(&path).cloned() else {
            return false;
        };
        let Some(pool) = self.loader.as_ref().map(|l| l.web_decoder()) else {
            return false;
        };
        self.web.full_inflight.insert(key);
        self.web.read_inflight.set(self.web.read_inflight.get() + 1);
        let read_inflight = self.web.read_inflight.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let is_raw = crate::decode::image_decode::is_raw_extension(&path);
            let result = web_fs::read_bytes(&handle).await;
            read_inflight.set(read_inflight.get().saturating_sub(1));
            match result {
                Ok(bytes) => pool.submit(
                    path,
                    target,
                    std::sync::Arc::new(bytes),
                    is_raw,
                    JobKind::Full,
                ),
                Err(e) => {
                    web_sys::console::error_1(
                        &format!("[web] reading bytes failed for {}: {e}", path.display()).into(),
                    );
                    pool.fail(path, target, JobKind::Full, None, e);
                }
            }
        });
        true
    }

    /// Moves the `Full` results `poll_web_thumbs` set aside into `loader.rs`'s
    /// full-resolution cache, where `try_show` picks them up.
    pub(crate) fn poll_web_full(&mut self) -> bool {
        let mut landed = false;
        let pending = std::mem::take(&mut self.web.full_pending);
        for crate::web::web_decode::PoolResult {
            path,
            target,
            result,
            ..
        } in pending
        {
            let key = (path.clone(), target);
            self.web.full_inflight.remove(&key);
            match result {
                Ok(img) => {
                    self.web.full_retries.remove(&key);
                    // `source_size` came from the preview, which is capped at
                    // `preview_px()`, so zoom and "100%" were using preview
                    // pixels. The full decode has the true size for any image
                    // that fits in a texture.
                    let real_size = Some((img.width, img.height));
                    if self.want.as_deref() == Some(path.as_path()) && self.source_size != real_size
                    {
                        self.source_size = real_size;
                        if self.fitted {
                            self.fit_to_window();
                        }
                    }
                    if let Some(loader) = &mut self.loader {
                        loader.insert_full_external(path, std::sync::Arc::new(img));
                    }
                    landed = true;
                }
                Err(e) => {
                    let entry = self
                        .web
                        .full_retries
                        .entry(key.clone())
                        .or_insert((0, Instant::now()));
                    entry.0 += 1;
                    let retries = entry.0;
                    if e.is_read() && retries <= MAX_READ_RETRIES {
                        entry.1 = Instant::now() + retry_backoff(retries);
                        web_sys::console::warn_1(
                            &format!(
                                "[web] full-resolution decode failed for {} (retry {}/{MAX_READ_RETRIES}): {e}",
                                path.display(),
                                retries
                            )
                            .into(),
                        );
                    } else {
                        web_sys::console::error_1(
                            &format!(
                                "[web] full-resolution decode failed permanently for {}: {e}",
                                path.display()
                            )
                            .into(),
                        );
                        self.web.full_retries.remove(&key);
                        if self.want.as_deref() == Some(path.as_path()) {
                            crate::web::analytics::decode_failed(&path, "full");
                        }
                        self.web.full_failed.insert(key);
                    }
                }
            }
        }
        if landed {
            self.try_show();
            self.request_redraw();
        }
        landed
    }

    /// True while any Web Worker decode is outstanding. The event loop keeps
    /// polling while this is true, because a worker's `postMessage` reply
    /// does not wake winit. `loader.has_pending_image` is always false on
    /// wasm32.
    pub(crate) fn web_decode_pending(&self) -> bool {
        !self.web.thumb_inflight.is_empty()
            || !self.web.preview_inflight.is_empty()
            || !self.web.speed_inflight.is_empty()
            || !self.web.full_inflight.is_empty()
    }

    /// True while the wanted RAW has nothing to show yet. Stays true during
    /// backoff and after permanent failure, so the previous photo does not
    /// show through.
    pub(crate) fn loupe_is_loading(&self) -> bool {
        let Some(path) = self.want.clone() else {
            return false;
        };
        if !crate::decode::image_decode::is_raw_extension(&path) {
            return false;
        }
        let target = self.preview_px();
        let key = (path.clone(), target);
        // A landed `Speed` result is only visible through `shown`, because it
        // skips `loader.rs`'s cache.
        let has_preview = self.shown.path() == Some(path.as_path())
            || self.loader.as_ref().is_some_and(|loader| {
                loader.get_full(&path).is_some() || loader.get_preview(&path, target).is_some()
            });
        !has_preview
            && (self.web.preview_inflight.contains(&key)
                || self.web.preview_retries.contains_key(&key)
                || self.web.preview_failed.contains(&key)
                || self.web.file_handles.contains_key(&path))
    }

    /// Starts listing `dir` unless it is cached in `subdirs` or already in
    /// flight. `poll_dir_listing` receives the result. A missing handle is
    /// treated as an empty folder.
    pub(crate) fn request_dir_listing(&mut self, dir: &Path) {
        let generation = self.web.nav_generation;
        let dir = dir.to_path_buf();
        if self.subdirs.contains_key(&dir)
            || self
                .web
                .dirlist_inflight
                .contains(&(dir.clone(), generation))
        {
            return;
        }
        let Some(handle) = self.web.dir_handles.get(&dir).cloned() else {
            web_sys::console::error_1(
                &format!("[web] no directory handle for {}", dir.display()).into(),
            );
            self.subdirs.insert(dir.clone(), Vec::new());
            self.set_status(
                StatusKind::Error,
                (crate::i18n::t().folder_handle_missing)(&dir.display().to_string()),
            );
            // No result will arrive, so a navigation waiting on this listing
            // would never run. Cancel pending navigation.
            self.supersede_web_pending_nav();
            return;
        };
        self.web.dirlist_inflight.insert((dir.clone(), generation));
        let base = dir;
        let tx = self.web.dirlist_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::list_dir(&base, &handle).await;
            let _ = tx.send((generation, base, result));
        });
        self.request_redraw();
    }

    /// Merges finished folder listings into the handle maps and `subdirs`, then
    /// runs any navigation that was waiting on that folder. Returns true while
    /// a listing is still in flight.
    pub(crate) fn poll_dir_listing(&mut self) -> bool {
        while let Ok((generation, dir, result)) = self.web.dirlist_rx.try_recv() {
            self.web.dirlist_inflight.remove(&(dir.clone(), generation));

            // Superseded by a newer navigation.
            if generation != self.web.nav_generation {
                continue;
            }

            let listing_succeeded = result.is_ok();
            match result {
                Ok(listing) => {
                    let mut subdir_paths = Vec::with_capacity(listing.subdirs.len());
                    for (path, handle) in listing.subdirs {
                        subdir_paths.push(path.clone());
                        self.web.dir_handles.insert(path, handle);
                    }
                    self.fetch_cjk_font_for(
                        listing.images.iter().map(|(p, _)| p).chain(&subdir_paths),
                    );
                    for (path, handle) in listing.images {
                        self.web.file_handles.insert(path, handle);
                    }
                    self.subdirs.insert(dir.clone(), subdir_paths);
                }
                Err(e) => {
                    self.set_status(
                        StatusKind::Error,
                        (crate::i18n::t().list_folder_failed)(
                            &dir.display().to_string(),
                            &e.to_string(),
                        ),
                    );
                    // Not cached, so navigating here again retries.
                }
            }

            if self.web.pending_nav_generation != self.web.nav_generation {
                self.web.pending_nav = None;
            }
            match self.web.pending_nav.clone() {
                Some(WebPendingNav::Open(p)) if p == dir => {
                    self.web.pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_open_folder(p);
                    }
                }
                Some(WebPendingNav::Load(p)) if p == dir => {
                    self.web.pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                    }
                }
                Some(WebPendingNav::LoadAfterOpen(p)) if p == dir => {
                    self.web.pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                        self.mode = ViewMode::Grid;
                        self.update_window_title();
                        self.normalize_focus();
                    }
                }
                _ => {}
            }
            if listing_succeeded {
                self.continue_web_session_restore();
            } else {
                self.web.session_restore = None;
            }
            self.request_redraw();
        }
        !self.web.dirlist_inflight.is_empty()
    }

    /// Walk a reopened session down to its subfolder, one listing at a time,
    /// then select its photo. `poll_dir_listing` calls back as each listing
    /// lands. A tree action or a failed listing drops the rest of the restore.
    fn continue_web_session_restore(&mut self) {
        let Some(session) = self.web.session_restore.clone() else {
            return;
        };
        let chain = session.folder_chain();
        if let Some(unlisted) = chain.iter().find(|d| !self.subdirs.contains_key(*d)) {
            self.request_dir_listing(&unlisted.clone());
            return;
        }
        self.web.session_restore = None;
        let dir = chain.last().expect("the chain starts at the root").clone();
        if chain.len() > 1 {
            self.expanded.extend(chain);
            self.apply_web_load_folder(dir);
        }
        self.restore_session_view(&session);
    }

    /// Fetch the full Chinese font the first time a listed name has Chinese
    /// characters the loaded fonts can't draw. Every name the UI shows reaches
    /// it through a listing first.
    fn fetch_cjk_font_for<'a>(&mut self, paths: impl IntoIterator<Item = &'a PathBuf>) {
        use std::sync::atomic::Ordering;
        if self.web.full_cjk_requested.load(Ordering::Relaxed) {
            return;
        }
        let font = egui::FontId::proportional(14.0);
        let missing = self.egui_ctx.fonts_mut(|fonts| {
            paths.into_iter().any(|path| {
                path.to_string_lossy()
                    .chars()
                    .any(|c| super::fonts::is_cjk(c) && !fonts.has_glyph(&font, c))
            })
        });
        if missing {
            self.web.full_cjk_requested.store(true, Ordering::Relaxed);
            super::fonts::fetch_full_cjk(
                self.egui_ctx.clone(),
                self.window.clone(),
                self.web.full_cjk_requested.clone(),
            );
        }
    }

    /// The wasm32 end of `open_folder`: toggles the folder's expansion, and if
    /// it has subfolders but no photos, opens its first subfolder instead.
    /// Expects `dir`'s listing to be in `subdirs`.
    pub(crate) fn apply_web_open_folder(&mut self, dir: PathBuf) {
        let subdirs = self.subdirs.get(&dir).cloned().unwrap_or_default();
        let has_own_images = self
            .web
            .file_handles
            .keys()
            .any(|p| p.parent() == Some(dir.as_path()));
        let pure_container = !subdirs.is_empty() && !has_own_images;

        if !subdirs.is_empty() {
            if self.expanded.contains(&dir) {
                if !pure_container {
                    self.expanded.remove(&dir);
                }
            } else {
                self.expanded.insert(dir.clone());
            }
        }

        let target = if pure_container {
            subdirs.into_iter().next().unwrap_or_else(|| dir.clone())
        } else {
            dir.clone()
        };

        if target != dir && !self.subdirs.contains_key(&target) {
            // Once the child is listed, only load it. `Open` would also toggle
            // the child and could skip down another level.
            self.defer_web_nav(WebPendingNav::LoadAfterOpen(target.clone()));
            self.request_dir_listing(&target);
            self.request_redraw();
            return;
        }

        self.apply_web_load_folder(target);
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
    }

    /// The wasm32 version of `load_folder`: shows `dir`'s photos in the grid
    /// without changing expansion. Lists `dir` first if needed.
    pub(crate) fn apply_web_load_folder(&mut self, dir: PathBuf) {
        if !self.subdirs.contains_key(&dir) {
            self.defer_web_nav(WebPendingNav::Load(dir.clone()));
            self.request_dir_listing(&dir);
            self.request_redraw();
            return;
        }
        // Must precede `load_playlist`. Set even when `None`, so sidecar
        // writes never go to the previous folder.
        let dir_handle = self.web.dir_handles.get(&dir).cloned();
        self.catalog.set_wasm_dir_handle(dir_handle);
        let mut entries: Vec<PathBuf> = self
            .web
            .file_handles
            .keys()
            .filter(|p| p.parent() == Some(dir.as_path()))
            .cloned()
            .collect();
        crate::navigation::sort_by_name(&mut entries);
        let playlist = crate::navigation::Playlist::from_entries(dir.clone(), entries);
        self.load_playlist(playlist, dir);
        self.request_redraw();
    }
}
