// SPDX-License-Identifier: GPL-3.0-or-later

//! The `App`: all viewer state plus the coordinator logic that ties the GPU
//! renderer, the background loader, the ratings/edits catalog, and the egui
//! chrome together — and the keyboard bindings.
//!
//! The winit event loop lives in `main.rs`; it translates raw window events into
//! the `pub(crate)` methods and fields exposed here. Everything that decides
//! *what* to show, what is selected, and how the loupe is transformed lives in
//! this module. Per-domain detail (loupe transform math, browser selection,
//! folder tree, histogram, thumbnail working set) is slated to move into its own
//! module in later steps.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;
// web_time::Instant, not std::time::Instant — see loader.rs's launched_at()
// doc comment for why (no OS clock on bare wasm32/64).
use web_time::Instant;

use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

use crate::burst::BurstMark;
use crate::catalog::Catalog;
use crate::develop::{Adjustments, Crop, TouchUp};
use crate::duplicates::DuplicateMark;
use crate::export::Exporter;
use crate::loader::Loader;
use crate::navigation::{Cmp, Playlist};
use crate::renderer::{EguiPaint, Renderer};
use crate::{image_decode, ui};

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

/// Thumbnail size bounds (longest-side pixels) for the grid/filmstrip.
const THUMB_MIN: u32 = 96;
const THUMB_MAX: u32 = 512;
const THUMB_DEFAULT: u32 = 192;
const THUMB_STEP: u32 = 32;

/// Bounds for the loupe's screen-fit preview decode (longest-side pixels). The
/// lower bound keeps the preview meaningfully sharper than the largest possible
/// thumbnail; the upper bound stops a 5K display from asking for a decode so
/// large it defeats the point of having a preview tier at all.
const PREVIEW_MIN: u32 = 1024;
const PREVIEW_MAX: u32 = 4096;
/// The preview target is rounded up to a multiple of this so that dragging a
/// window edge doesn't request a new decode (and evict the old one) on every
/// single pixel of resize.
const PREVIEW_QUANTUM: u32 = 512;
/// Smallest touch-up radius in source-image pixels.
pub(crate) const TOUCHUP_MIN_PIXELS: f32 = 3.0;
pub(crate) const TOUCHUP_MAX_RADIUS: f32 = 0.15;
/// Fraction of the patch radius used to blend the correction into its edges.
const TOUCHUP_FEATHER: f32 = 1.0;

/// Number of Develop sliders the keyboard cycles through (panel order: temp,
/// tint, exposure, contrast, highlights, shadows, whites, blacks, vibrance,
/// saturation, denoise).
const DEVELOP_SLIDERS: usize = 11;

/// Load macOS fonts at runtime so the binary does not need to embed a large
/// Unicode font. The built-in egui fonts remain after these entries as a
/// fallback for machines where a system font path differs or is unavailable.
fn configure_system_fonts(ctx: &egui::Context) {
    let mut definitions = egui::FontDefinitions::default();
    let candidates = [
        ("macos-ui", "/System/Library/Fonts/SFNS.ttf", 0),
        // Covers CJK characters that are not present in SFNS. TTC files may
        // contain multiple faces; index 0 is the regular face on macOS.
        ("macos-cjk", "/System/Library/Fonts/Hiragino Sans GB.ttc", 0),
        (
            "macos-cjk-fallback",
            "/System/Library/Fonts/AppleSDGothicNeo.ttc",
            0,
        ),
    ];

    let mut loaded = Vec::new();
    for (name, path, index) in candidates {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        definitions.font_data.insert(
            name.to_owned(),
            Arc::new(egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: Default::default(),
            }),
        );
        loaded.push(name.to_owned());
    }

    if loaded.is_empty() {
        return;
    }

    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(fonts) = definitions.families.get_mut(&family) {
            for name in loaded.iter().rev() {
                fonts.insert(0, name.clone());
            }
        }
    }
    ctx.set_fonts(definitions);
}

/// Two top-level views: a thumbnail Grid and a single-image Loupe.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    Loupe,
    /// Side-by-side review of one duplicate group (`survey_members`), entered
    /// from a duplicate-group badge click in the Grid. Reuses `Region::Grid`
    /// for keyboard-focus bookkeeping (see `normalize_focus`) rather than
    /// adding a new `Region` variant, since it's a modal-like screen entered
    /// from and exited back to the Grid, not part of the F6 chrome-cycling
    /// ring.
    Survey,
}

/// One edge of the crop rectangle, in the image's own (texture) space — `Left`
/// is the low-x edge of the *unrotated* image, etc. Grab/hit-testing maps these
/// to on-screen edges through the loupe transform, so they behave correctly even
/// when the image is rotated.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

/// What a crop drag is currently manipulating.
#[derive(Copy, Clone)]
enum CropGrab {
    /// Resizing by moving one edge.
    Edge(CropEdge),
    /// Moving the whole rectangle (size fixed): the pointer's texture-uv at grab
    /// and the rectangle as it was then, so the drag is anchor-relative (no drift).
    Move { anchor: (f32, f32), rect0: Crop },
}

/// Transient crop-mode state (Loupe sub-mode). Present ⇔ crop mode is active.
/// `rect` is the rectangle being edited in normalized texture space (matching
/// [`develop::Crop`]); it is committed into the image's `Adjustments.crop` on
/// exit. While cropping, the GPU shows the full frame (identity crop) and the
/// egui overlay draws the mask, so the whole image stays visible for framing.
pub struct CropDraft {
    /// The crop rectangle under edit (normalized 0..1, texture space).
    rect: Crop,
    /// What the current drag owns (edge resize or whole-rect move); `None` when
    /// no drag is in progress.
    grab: Option<CropGrab>,
    /// Pixel aspect ratio (w/h) captured at grab time, used for Shift-lock.
    aspect: f32,
}

/// A full-frame crop rectangle (the identity crop).
const FULL_CROP: Crop = Crop {
    left: 0.0,
    top: 0.0,
    right: 1.0,
    bottom: 1.0,
};
/// Smallest crop edge separation, in normalized units, so the rect never collapses.
const MIN_CROP: f32 = 0.02;

/// The UI region that currently receives arrow/Enter keys. `Toolbar` and
/// `Filmstrip` are chrome: reachable only via `F6`/`Shift+F6`, which toggles
/// between them and whichever "main chain" region (`Folders`/`Grid`/`Detail`/
/// `Develop`) was last focused — see `cycle_region`. The main chain itself is
/// walked linearly with `Enter` (deeper) / `Escape` (back out), never by F6.
/// `Detail` is the bare enlarged image (Develop panel closed); `Develop` is
/// the same Loupe view with the panel open. Clicking into a panel also moves
/// focus there.
/// (Not `Tab` for region-cycling: egui_winit hardcodes every Tab press as
/// always-consumed to run its own competing widget-focus traversal — see the
/// Tab-stripping comment in `redraw` — so region-cycling deliberately lives
/// on F6 instead; Tab is used for cycling *within* a region.)
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Region {
    Toolbar,
    Folders,
    Grid,
    Detail,
    Filmstrip,
    Develop,
}

/// Chrome regions F6 toggles through, in order (wraps back to the main
/// region). Cycling logic lives in `cycle_region`.
const CHROME_ORDER: [Region; 2] = [Region::Toolbar, Region::Filmstrip];

/// A region's keyboard focus is either just "selected" (F6 landed here; F6
/// again moves to the next region) or "entered" (a specific item/control has
/// the cursor; F6, where meaningful, moves within it instead). Entering
/// happens via `Enter` or, for control-oriented regions, automatically on the
/// first arrow press — Escape pops back to `Selected`. Grid arrows move the
/// image selection without adding a separate focus level, so Escape continues
/// to leave the grid in one press after navigation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FocusLevel {
    Selected,
    Entered,
}

/// What the loupe currently has uploaded to the GPU. The decode *target* is
/// tracked separately in `App::want`; `try_show` reconciles `want` into `shown`.
/// Folding the old `shown: Option<PathBuf>` + `shown_is_full: bool` pair into one
/// enum makes the "thumbnail vs full" tier part of the path's identity, so a
/// stray `shown_is_full` can't disagree with which path is shown.
enum Shown {
    /// Nothing uploaded yet.
    Nothing,
    /// A thumbnail placeholder, shown instantly while the preview decodes.
    Thumb(PathBuf),
    /// The screen-fit preview — what the loupe shows for all normal viewing.
    /// Carries the target it was decoded for *and* the longest side actually
    /// uploaded. Both are needed: the target catches a window resize asking for
    /// a different size, and the actual size catches the Speed pass being
    /// superseded by the forced decode behind it (same path, same target, more
    /// pixels) — which is the entire RAW fast path.
    Preview(PathBuf, u32, u32),
    /// The full-resolution image, fetched only once the user zooms in.
    Full(PathBuf),
}

impl Shown {
    /// The path shown in any tier, or `None` when nothing is shown.
    fn path(&self) -> Option<&Path> {
        match self {
            Shown::Nothing => None,
            Shown::Thumb(p) | Shown::Preview(p, _, _) | Shown::Full(p) => Some(p),
        }
    }

    /// True when `path` is shown at full resolution.
    fn is_full_of(&self, path: &Path) -> bool {
        matches!(self, Shown::Full(p) if p == path)
    }

    /// True when `path` is already shown as a preview decoded for `target` and
    /// carrying exactly `actual` pixels on its longest side — i.e. re-uploading
    /// would be a no-op.
    fn is_preview_of(&self, path: &Path, target: u32, actual: u32) -> bool {
        matches!(self, Shown::Preview(p, t, a) if p == path && *t == target && *a == actual)
    }
}

/// Native/mac: no wasm32-style Loupe-loading state exists (decode is fast,
/// real multi-core threads via ImageIO/`RawDevelop`), so the image draw
/// never needs blanking while waiting on it. The real implementation is
/// `app/web.rs`'s wasm32-gated `impl App` block (`loupe_is_loading`).
#[cfg(not(target_arch = "wasm32"))]
impl App {
    pub(crate) fn loupe_is_loading(&self) -> bool {
        false
    }
}

/// Progress of an in-flight background export batch. `Some` from the moment
/// jobs are submitted until the last outcome is drained.
pub(crate) struct ExportProgress {
    done: usize,
    total: usize,
    errors: usize,
    last_err: Option<String>,
}

/// A finished subject-segmentation run: the path it was computed for, and the
/// mask or the reason there isn't one.
pub(crate) type SelectionOutcome = (PathBuf, Result<crate::segmentation::Mask, String>);

/// A folder navigation deferred until its subfolder listing lands (wasm32
/// only — see `app/nav.rs`'s request/apply split). `Open` toggles expansion
/// and does native `open_folder`'s pure-container skip; `Load` just swaps
/// the grid like native `load_folder` (used by `folder_move` /
/// `folder_collapse`).
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone)]
pub(crate) enum WebPendingNav {
    Open(PathBuf),
    Load(PathBuf),
}

pub(crate) struct App {
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) renderer: Option<Renderer>,
    pub(crate) loader: Option<Loader>,
    /// Background JPEG export worker pool; `None` until the window is created.
    pub(crate) exporter: Option<Exporter>,
    /// Background worker pool for the feature-print refinement pass.
    pub(crate) feature_pool: Option<crate::featureprint::DistancePool>,
    /// Background worker pool for face/eye-openness analysis.
    pub(crate) face_pool: Option<crate::facequality::FacePool>,
    /// In-flight export batch progress, driving the persistent progress toast.
    pub(crate) export_progress: Option<ExportProgress>,

    /// `Renderer::new` is `async` (wgpu's adapter/device acquisition is a
    /// browser Promise under WebGPU) — native wraps it in `pollster::block_on`
    /// inside `resumed()` and never touches this; wasm32 can't block the main
    /// thread at all, so `resumed()` instead spawns the future via
    /// `wasm_bindgen_futures::spawn_local` and this channel carries the
    /// finished `Renderer` back to be polled in `about_to_wait`, same
    /// one-shot-background-job shape as `selection_tx`/`rx` below (subject
    /// segmentation's own one-shot-thread-plus-channel pattern).
    #[cfg(target_arch = "wasm32")]
    pub(crate) renderer_init_tx: Sender<(Renderer, winit::dpi::PhysicalSize<u32>)>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) renderer_init_rx: Receiver<(Renderer, winit::dpi::PhysicalSize<u32>)>,

    playlist: Option<Playlist>,

    /// Path we want shown in the loupe (may still be decoding).
    want: Option<PathBuf>,
    /// What's currently uploaded to the GPU (nothing / thumbnail / full).
    shown: Shown,
    /// A file/dir requested before the window/renderer existed.
    pub(crate) pending_initial: Option<PathBuf>,

    /// True while `showDirectoryPicker` + listing is in flight — drives the
    /// landing page's "Choose Folder" button (disabled/shows a status while
    /// pending, matches `catalog_load_pending`'s role for that other async
    /// one-shot job) and guards against firing a second picker before the
    /// first resolves. wasm32 only: this whole landing-page flow doesn't
    /// exist natively (`main()` requires a CLI arg or delivers one via
    /// AppleEvent, so `self.playlist` is never meaningfully "still empty and
    /// waiting on the user" there the way it legitimately is here — see
    /// `ui::draw`'s landing-page branch).
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_pending: bool,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_tx: Sender<Result<crate::web_fs::PickedFolder, String>>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_rx: Receiver<Result<crate::web_fs::PickedFolder, String>>,
    /// File handles for the currently-open folder's images, keyed the same
    /// way `self.playlist`'s entries are — a picked folder has no real OS
    /// path, only these, so a later decode step (not wired up yet) will need
    /// to look a path back up to its handle before it can read any bytes.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_file_handles: HashMap<PathBuf, web_sys::FileSystemFileHandle>,
    /// Directory handles for every folder the user has browsed into, keyed
    /// by relative path (root's `.name()` as the first component). Seeded
    /// from `PickedFolder::dir_handles` at pick time, extended by
    /// `poll_dir_listing` as subfolders are listed. The catalog's wasm
    /// sidecar handle is swapped to `web_dir_handles[current folder]` on
    /// each navigation.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dir_handles: HashMap<PathBuf, web_sys::FileSystemDirectoryHandle>,
    /// Async subfolder-listing results (`web_fs::list_dir`), same one-shot
    /// channel shape as `web_folder_tx`/`web_folder_rx`. Key is the listed
    /// directory's relative path.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_tx:
        Sender<(PathBuf, Result<crate::web_fs::DirListing, String>)>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_rx:
        Receiver<(PathBuf, Result<crate::web_fs::DirListing, String>)>,
    /// Directories with a `list_dir` in flight — dedupes repeated
    /// `request_dir_listing` calls from per-frame nav polling.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_inflight: std::collections::HashSet<PathBuf>,
    /// wasm32 export: finished JPEG writes (`web_export_fs::WebFs::write_atomic`,
    /// driven from `main.rs`'s frame loop after `poll_exports`) report back
    /// here as `ExportOutcome`s, drained into the shared `on_export_outcomes`
    /// — the browser counterpart of native's `Exporter::poll()`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_export_tx: Sender<crate::export::ExportOutcome>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_export_rx: Receiver<crate::export::ExportOutcome>,
    /// A folder navigation deferred until its listing lands (see
    /// `app/nav.rs`'s request/apply split). `Open` toggles expansion +
    /// pure-container skip like native `open_folder`; `Load` just swaps the
    /// grid like native `load_folder` (used by `folder_move` /
    /// `folder_collapse`).
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_pending_nav: Option<WebPendingNav>,
    /// Thumbnail decodes currently in flight — `loader.rs`'s own
    /// `thumb_inflight` isn't reused here since decode results arrive via
    /// the Web Worker pool (`web_worker_pool.rs`), not `loader.rs`'s own
    /// (unserviced-on-wasm32) worker queue.
    #[cfg(target_arch = "wasm32")]
    web_thumb_inflight: HashSet<(PathBuf, u32)>,
    /// How many `FileSystemFileHandle::get_file()` reads are in flight right
    /// now, shared between `request_web_thumbs` and `request_web_preview`
    /// (both read from the same picked folder). Chrome throws a
    /// `NotReadableError` ("could not be read, typically due to permission
    /// problems...") once too many concurrent reads are open against one
    /// folder — confirmed the hard way: it surfaced only after the Worker
    /// pool (M4) made decode fast enough for a whole grid page's worth of
    /// reads to fire in the same frame. `MAX_CONCURRENT_READS`
    /// (`app/web.rs`) caps this; a key that can't start this frame is simply
    /// left off `web_thumb_inflight`/`web_preview_inflight` so the same
    /// per-frame recomputation that already drives those naturally retries
    /// it once the budget frees up — no separate retry/backoff bookkeeping
    /// needed. `Rc<Cell<_>>` (not a plain `u32`) so the `spawn_local` task
    /// that decrements it on completion doesn't need `&mut App`.
    #[cfg(target_arch = "wasm32")]
    web_read_inflight: std::rc::Rc<std::cell::Cell<u32>>,
    /// Consecutive read/decode failure counts plus the earliest time the
    /// next retry may fire, keyed the same as
    /// `web_thumb_inflight`/`web_preview_inflight`. A `NotReadableError`
    /// from Chrome (confirmed real, hit even after capping concurrent
    /// reads) is very likely transient, so a failure isn't treated as
    /// permanent until it's failed `MAX_READ_RETRIES` times in a row
    /// (`app/web.rs`) — but a *zero-delay* retry (the first version of this)
    /// still failed every single time: 5 retries a frame apart (~16ms each)
    /// all land within the same ~80ms window, no real time for whatever's
    /// transiently exhausted to clear. The `Instant` is a real backoff
    /// deadline (`retry_backoff`, growing per attempt) — `request_web_thumbs`
    /// /`request_web_preview` check it before resubmitting, not just
    /// "is the key currently marked in-flight". Cleared on success or on
    /// giving up permanently.
    #[cfg(target_arch = "wasm32")]
    web_thumb_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_preview_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    /// Loupe preview decode — same shape as `web_thumb_inflight`, at
    /// `preview_px()` instead of `thumb_px`. `web_preview_failed` exists
    /// here (and has no thumbnail counterpart) because `try_show` re-calls
    /// `request_preview` every single frame until something lands; without
    /// a negative cache a RAW file open in the Loupe would retry a doomed
    /// decode forever.
    #[cfg(target_arch = "wasm32")]
    web_preview_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_preview_failed: HashSet<(PathBuf, u32)>,
    /// The Loupe's screen-fit *fast* decode (`JobKind::Speed`) — same shape
    /// as `web_preview_inflight`/`web_preview_retries`/`web_preview_failed`,
    /// tracked separately because a `Speed` result and the real `Preview`
    /// (quality) result for the same `(path, target)` key both land at
    /// (almost always) identical pixel dimensions once resized to fit the
    /// same target, so they can't share `loader.rs`'s preview cache slot —
    /// `try_show`'s "did a sharper tier land" check compares `(target,
    /// actual)`, which wouldn't see a difference. `poll_web_preview` applies
    /// a landed `Speed` result directly via `upload_shown` instead of
    /// routing it through `loader.rs` at all.
    #[cfg(target_arch = "wasm32")]
    web_speed_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_speed_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_speed_failed: HashSet<(PathBuf, u32)>,
    /// The Loupe's zoom-triggered full-resolution decode (`JobKind::Full`) —
    /// wasm32 counterpart of `loader.rs`'s native `Job::Full`/`request_full`
    /// (whose worker queue has no live workers on wasm32). Lands via
    /// `loader.insert_full_external`, so `try_show`'s existing `get_full`
    /// branch picks it up with no further changes there.
    #[cfg(target_arch = "wasm32")]
    web_full_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_full_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_full_failed: HashSet<(PathBuf, u32)>,

    /// Real parallel decode (wasm port plan's M4) — a hand-rolled
    /// `web_sys::Worker` pool, chosen over `wasm-bindgen-rayon` because that
    /// crate's JS-orchestrated init flow doesn't fit this app's binary-crate
    /// entry point and stays off the nightly toolchain. `request_web_thumbs`/
    /// `request_web_preview` still read file bytes on the main thread (an
    /// unavoidable async I/O step against a `FileSystemFileHandle`), then
    /// hand the bytes to a worker via `web_worker_pool::WorkerPoolHandle`;
    /// `poll_web_worker_pool` drains finished decodes into the same
    /// `loader.rs` external-insert methods the old inline-decode path used.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_worker_pool: crate::web_worker_pool::WorkerPool,
    /// Preview-tier results the pool handed back while `poll_web_thumbs`
    /// (which runs first each frame — see `main.rs`'s `about_to_wait`) was
    /// draining the pool's single shared result channel; `poll_web_preview`
    /// consumes these the same frame instead of polling the pool itself, so
    /// a shared channel can't split one frame's results across two draws.
    #[cfg(target_arch = "wasm32")]
    web_preview_pending: Vec<crate::web_worker_pool::PoolResult>,
    /// `Full`-tier results set aside the same way `web_preview_pending` sets
    /// aside `Preview`/`Speed` ones — `poll_web_thumbs` routes them here
    /// directly (see its own routing comment) since `poll_web_full` is a
    /// separate function from `poll_web_preview`.
    #[cfg(target_arch = "wasm32")]
    web_full_pending: Vec<crate::web_worker_pool::PoolResult>,

    // ---- Browser state ----
    /// Grid vs. Loupe.
    pub(crate) mode: ViewMode,
    /// Ratings catalog (persistent) + an in-memory mirror for fast lookups.
    catalog: Catalog,
    /// The directory + request token of the sidecar scan currently running
    /// on a background thread, if any — set by `request_catalog_load`,
    /// cleared once a result carrying the matching token lands (see
    /// `poll_catalog_load`). One-shot-thread-per-request, same shape as
    /// `selection_pending`/`selection_tx`/`selection_rx` below, not a
    /// persistent pool: a directory switch is a single job, not a stream.
    ///
    /// Keyed on a token, not just the directory: revisiting a directory
    /// before its first load lands (e.g. rapid A→B→A folder navigation)
    /// starts a second background load for `A` while the first is still in
    /// flight. Without a token, whichever of the two lands first would clear
    /// `catalog_load_pending` by directory-equality alone, stopping the
    /// tight poll cadence before the second (real, still-in-flight) load for
    /// the now-active directory has actually reconciled.
    catalog_load_pending: Option<(PathBuf, u64)>,
    /// Monotonic counter minted by `request_catalog_load`, one per call —
    /// see `catalog_load_pending`.
    catalog_load_token: u64,
    catalog_load_tx: Sender<(PathBuf, u64, crate::catalog::SidecarLoad)>,
    catalog_load_rx: Receiver<(PathBuf, u64, crate::catalog::SidecarLoad)>,
    ratings: HashMap<PathBuf, u8>,
    /// Per-image non-destructive develop edits (persistent, mirrored in-memory).
    /// Only non-identity edits are stored to keep the map small.
    edits: HashMap<PathBuf, Adjustments>,
    touchups: HashMap<PathBuf, Vec<TouchUp>>,
    touchup_active: bool,
    touchup_radius: f32,
    touchup_selected: Option<usize>,
    /// Whether the loupe's right-hand Develop panel is open.
    develop_open: bool,
    /// A small downsampled LINEAR-light RGB grid of the shown image, used to
    /// recompute the live histogram cheaply when adjustments change. Rebuilt
    /// whenever a new full image is uploaded.
    // Row-major, `hist_dw` × `hist_dh` — a real 2D grid (not a flat list of
    // isolated samples) so `recompute_histogram` can look up actual neighbor
    // cells for denoise, and can derive each cell's normalized (u, v)
    // position analytically to drop cells outside the active crop rect.
    hist_sample: Vec<[f32; 3]>,
    /// Format of the image that produced `hist_sample`; RAW linear samples
    /// need the RAW shader's real sRGB transfer and preview boost.
    hist_pixel_format: image_decode::PixelFormat,
    /// `hist_sample`'s grid dimensions (0×0 when no image is loaded).
    hist_dw: usize,
    hist_dh: usize,
    /// Cached per-channel (R/G/B) display-space histogram bins for the panel.
    /// Float bins: samples are splatted fractionally across neighbouring buckets
    /// so a tone-curve stretch doesn't re-quantize into a comb of empty bins.
    /// `None` when no image is shown.
    histogram: Option<[[f32; 256]; 3]>,
    /// Set when `histogram` needs recomputing (new image, or an edit changed).
    hist_dirty: bool,
    /// Active star filter (`None` = show all).
    filter: Option<(Cmp, u8)>,
    /// Comparator the toolbar applies when a star level is clicked (≥ / = / ≤).
    /// Remembered across "All" so the mode is sticky.
    filter_cmp: Cmp,
    /// Indices into `playlist.entries()` that pass the current filter.
    visible: Vec<usize>,
    /// Position *within `visible`* of the current selection, or `None` in the
    /// Grid's browse-first state (no selection until the user clicks or arrows).
    /// The Loupe always has a selection. `None` makes the "no selection" state
    /// unrepresentable as a stray index — there is no separate active flag.
    sel: Option<usize>,
    /// The multi-selection: positions *within `visible`*. Empty in the
    /// browse-first state. When non-empty it always contains `sel` (the
    /// primary/active cell). Bulk operations act on this set.
    selected: BTreeSet<usize>,
    /// Anchor position (within `visible`) for Shift range-selection.
    anchor: Option<usize>,
    /// Accumulated mouse-wheel delta over the filmstrip, not yet enough to
    /// cross the one-photo step threshold. See [`App::scroll_filmstrip`].
    filmstrip_scroll_accum: f32,
    /// Thumbnail longest-side pixels for the grid + filmstrip.
    thumb_px: u32,
    /// Columns the grid actually laid out last frame (for Up/Down row moves).
    grid_cols: usize,
    /// Visible cell range `[start, end)` the grid scrolled into view last frame.
    /// Drives thumbnail virtualization so huge folders don't load every image.
    grid_range: (usize, usize),
    /// Whether the egui Grid ScrollArea should return to its origin next frame.
    grid_scroll_reset: bool,
    /// Visible cell range `[start, end)` the loupe filmstrip scrolled into view
    /// last frame. The horizontal equivalent of `grid_range`.
    strip_range: (usize, usize),
    /// egui textures for thumbnails, keyed by (path, thumb_px, edit_signature).
    /// The edit signature makes an edit change (crop/tone/rotation) mint a new key,
    /// so `sync_thumb_textures` drops the stale texture and re-bakes. Rebuilt as
    /// thumbnails arrive; pruned to the current working set each frame.
    thumb_tex: HashMap<(PathBuf, u32, u64), egui::TextureHandle>,

    // ---- Best-of-burst state ----
    /// Whether burst detection is active (badges + dimming). Mutually exclusive
    /// with the star filter; `bursts_on` implies `filter.is_none()`.
    bursts_on: bool,
    /// Cached capture times per path (EXIF/mtime). `Some(None)` records a read
    /// that yielded no time, so we don't re-request it. Survives toggling off.
    capture_times: HashMap<PathBuf, Option<SystemTime>>,
    /// Cached sharpness scores per path. Survives toggling off.
    sharpness: HashMap<PathBuf, f64>,
    /// Derived burst marks, indexed by playlist entry index (not visible pos).
    /// Empty when bursts are off. Rebuilt when caches or the toggle change.
    burst_marks: Vec<Option<BurstMark>>,

    // ---- Content-duplicate grouping state (dHash) ----
    /// Whether content-duplicate detection is active (badges). Independent of
    /// `bursts_on`: a photo can be in both a time-burst and a content-duplicate
    /// group at once — these are separate underlying computations, unified only
    /// at the UI badge layer.
    dupes_on: bool,
    /// Cached dHash per path, computed off thumbnail arrival (whole folder, not
    /// gated on any prior grouping). Survives toggling off.
    phashes: HashMap<PathBuf, u64>,
    /// Raw dHash grouping (pre feature-print refinement), indexed by playlist
    /// entry index. Kept separately from `dup_marks` so `request_feature_prints`
    /// can identify each group's anchor/candidates without recomputing it.
    dup_groups: Vec<u32>,
    /// Refined grouping (post feature-print split), indexed by playlist entry
    /// index. `dup_marks` only keeps Best/Sibling per entry, discarding which
    /// entries share a group — this is what Survey Mode needs to gather a
    /// clicked badge's group members.
    dup_refined: Vec<u32>,
    /// Vision feature-print distances keyed by (dHash group anchor, member).
    /// Survives toggling off. Anchor-relative (not all-pairs) — see
    /// `duplicates::refine_by_feature_print`.
    feature_distances: HashMap<(PathBuf, PathBuf), f32>,
    /// Terminally failed comparisons, keyed by (anchor, member), so corrupt or
    /// unsupported files do not get resubmitted every frame.
    feature_failed: HashSet<(PathBuf, PathBuf)>,
    /// Outstanding feature-print comparisons keyed by (anchor, member), so
    /// `request_feature_prints` doesn't resubmit every frame.
    feature_pending: HashSet<(PathBuf, PathBuf)>,
    /// Derived duplicate marks (post feature-print refinement), indexed by
    /// playlist entry index (not visible pos). Empty when dupes are off.
    /// Rebuilt when caches or the toggle change.
    dup_marks: Vec<Option<DuplicateMark>>,

    // ---- Face / eyes-closed state ----
    /// Cached per-path face signals (face count + worst eye openness). Survives
    /// toggling off, like `sharpness` and `phashes`. Filled only for photos that
    /// are already in a burst or a duplicate group — Vision decodes the file at
    /// full resolution to find faces, far heavier than the thumbnail-based blur
    /// and dHash passes, so it is not worth spending on a whole folder.
    face_quality: HashMap<PathBuf, crate::facequality::FaceQuality>,
    /// Outstanding analyses, so `request_face_quality` doesn't resubmit a path
    /// every frame while its worker is still running.
    face_pending: HashSet<PathBuf>,
    /// Terminally failed analyses (corrupt or unsupported files), so they are
    /// not retried forever.
    face_failed: HashSet<PathBuf>,
    // ---- Subject-selection overlay state (Loupe only, transient) ----
    /// Whether the Loupe is drawing the subject-selection overlay.
    selection_on: bool,
    /// Whether the overlay highlights the background instead of the subject.
    selection_invert: bool,
    /// The mask for the photo currently in the Loupe, tagged with the path it
    /// was computed from so a stale result can be recognized and dropped.
    ///
    /// Deliberately transient — derived data, not a user edit, so it is never
    /// written to the catalog — and deliberately one photo deep: there is no
    /// per-folder pass, because segmentation is far too heavy to run over
    /// anything but the picture actually on screen.
    current_selection: Option<(PathBuf, crate::segmentation::Mask)>,
    /// The path whose mask is being computed right now, if any, so the request
    /// isn't fired again every frame while its thread runs.
    selection_pending: Option<PathBuf>,
    /// Result channel for those one-shot worker threads.
    selection_tx: Sender<SelectionOutcome>,
    selection_rx: Receiver<SelectionOutcome>,

    /// Whether the grid is narrowed to photos with a detected blink. Stacks on
    /// top of the star filter rather than replacing it (they're different
    /// questions), and reads only the cache — a photo the face pass hasn't
    /// reached is not "eyes open", it's unjudged, and stays hidden.
    eyes_filter: bool,

    // ---- Survey Mode state (one duplicate group at a time) ----
    /// Paths of the duplicate group currently under review. Empty outside
    /// `ViewMode::Survey`.
    survey_members: Vec<PathBuf>,
    /// The group's best member as of `open_survey` (from `dup_marks`), so
    /// `keep_best_reject_rest` stays consistent with the grid badge instead of
    /// re-deriving "best" independently.
    survey_best: Option<PathBuf>,
    /// Index into `survey_members` that rating hotkeys/arrow-keys apply to.
    survey_focus: usize,

    // ---- Loupe view state ----
    zoom: f32,
    pub(crate) pan: (f32, f32), // screen-space pixel coords of the image's top-left corner
    pub(crate) win_size: (f32, f32),
    /// True while the view is auto-fit to the window (so a resize re-fits).
    pub(crate) fitted: bool,
    // TEMPORARY DEBUG — see `app/thumbs.rs::set_tier_debug`. Prefixed onto
    // the window/tab title (`update_window_title`) since the clear-color
    // tint alone is invisible whenever the fitted image fills the Loupe
    // viewport edge-to-edge (no letterbox margin left for the color to show
    // in). Remove alongside the color tint once the zoom-refit fix is
    // verified.
    pub(crate) debug_tier_label: &'static str,
    /// Per-image rotation, in 90° clockwise steps (0..=3).
    rotations: HashMap<PathBuf, u8>,
    /// The image viewport rect (physical px) the loupe drew into last frame, if any.
    loupe_viewport: Option<(u32, u32, u32, u32)>,
    /// Transient crop-mode state; `Some` while the user is editing a crop.
    crop_edit: Option<CropDraft>,
    /// True while the White Balance gray-picker is armed: the next click on
    /// the Loupe image samples that pixel and solves temp/tint to neutralize
    /// it, then clears back to `false`.
    wb_picker: bool,
    /// Before/after compare mode (Loupe only): the image is drawn twice, the
    /// left half with identity tone (but crop + rotation), the right with the
    /// full develop edits.
    compare: bool,
    /// Cached camera/lens/exposure metadata per path, for the Loupe info
    /// panel. In-memory only (never persisted, unlike `catalog.rs`'s ratings
    /// and edits) — read live from the file on demand as each image is shown.
    exif_cache: HashMap<PathBuf, image_decode::ImageMetadata>,

    /// True pixel dimensions (display orientation) of the photo in `want`, when
    /// known. All loupe zoom/pan/crop math is expressed against *this*, not the
    /// size of whatever texture happens to be uploaded — the loupe deliberately
    /// shows a thumbnail, then a downscaled preview, then (only if the user
    /// zooms in) the full-resolution decode, and "100% zoom" or a crop rectangle
    /// must mean the same thing throughout. Filled from the image properties by
    /// `on_exif_info` without decoding anything; `None` until that lands.
    source_size: Option<(u32, u32)>,

    /// A bulk action awaiting confirmation. `Some` while the confirm modal is up.
    pending_bulk: Option<ui::BulkKind>,

    /// Copied develop settings (tone only, no crop) plus the source file's path,
    /// for pasting onto other selected photos. `None` until the user copies.
    copied_settings: Option<(PathBuf, Adjustments)>,

    /// Whether the keyboard-shortcut help overlay is showing (toggled by `?`).
    show_help: bool,

    /// Whether the quit-confirmation modal is showing (Esc in the grid).
    pending_quit: bool,
    /// Set once the user confirms quit; `main.rs` exits the event loop on it.
    pub(crate) quit_requested: bool,

    /// A short-lived status message (e.g. an export result), with the time it was
    /// set; shown as a toast for a few seconds, then ignored.
    status: Option<(String, Instant)>,

    /// True while winit reports the window as occluded (hidden/minimized/behind
    /// another window). We pause redraw retries while occluded.
    pub(crate) occluded: bool,

    // ---- Folder-tree sidebar state ----
    /// Top of the tree (the opened folder, or an opened file's parent).
    folder_root: Option<PathBuf>,
    /// The folder whose images are shown in the grid. Doubles as the folder
    /// tree's keyboard cursor: like a standard single-select tree (Finder's
    /// list view, VS Code's explorer), there's no separate "browse without
    /// loading" state — arrow-key movement in the tree selects and loads a
    /// folder in the same step.
    folder_sel: Option<PathBuf>,
    /// Folders currently expanded in the tree.
    expanded: HashSet<PathBuf>,
    /// Lazily-cached immediate subdirectories, one `list_subdirs` per folder.
    subdirs: HashMap<PathBuf, Vec<PathBuf>>,

    // ---- Keyboard focus state ----
    /// Which UI region currently receives arrow/Enter keys. Set by clicks, F6
    /// cycling, and module changes.
    focus: Region,
    /// The last main-chain region (`Folders`/`Grid`/`Detail`/`Develop`) that
    /// had focus — i.e. `focus` itself whenever it isn't chrome. F6/Escape use
    /// this to jump back out of `Toolbar`/`Filmstrip`.
    main_focus: Region,
    /// Whether `focus` is just "selected" (F6 landed here) or "entered" (a
    /// specific item/control has the cursor). See [`FocusLevel`].
    focus_level: FocusLevel,
    /// Index of the keyboard-focused Develop slider (0..=7, panel order).
    develop_focus: usize,
    /// Index of the keyboard-focused Toolbar control (panel order).
    toolbar_focus: usize,

    // ---- Input state ----
    pub(crate) cursor: (f64, f64),
    pub(crate) modifiers: ModifiersState,
    pub(crate) space_down: bool,
    pub(crate) dragging: bool,
    pub(crate) last_drag: (f64, f64),

    // ---- egui chrome ----
    pub(crate) egui_ctx: egui::Context,
    pub(crate) egui_state: Option<egui_winit::State>,
}


mod nav;
mod keys;
mod accessors;
mod catalog;
mod adjust;
mod crop;
mod export;
mod histogram;
mod loupe;
mod thumbs;
#[cfg(target_arch = "wasm32")]
mod web;

impl App {
    pub(crate) fn new(initial: Option<PathBuf>) -> Self {
        let catalog = Catalog::new();
        let egui_ctx = egui::Context::default();
        configure_system_fonts(&egui_ctx);
        let (selection_tx, selection_rx) = std::sync::mpsc::channel();
        let (catalog_load_tx, catalog_load_rx) = std::sync::mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let (renderer_init_tx, renderer_init_rx) = std::sync::mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let (web_folder_tx, web_folder_rx) = std::sync::mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let (web_dirlist_tx, web_dirlist_rx) = std::sync::mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let (web_export_tx, web_export_rx) = std::sync::mpsc::channel();
        #[cfg(target_arch = "wasm32")]
        let web_worker_pool =
            crate::web_worker_pool::WorkerPool::new(crate::web_worker_pool::worker_count());
        Self {
            window: None,
            renderer: None,
            loader: None,
            exporter: None,
            feature_pool: None,
            face_pool: None,
            export_progress: None,
            playlist: None,
            want: None,
            shown: Shown::Nothing,
            pending_initial: initial,
            #[cfg(target_arch = "wasm32")]
            web_folder_pending: false,
            #[cfg(target_arch = "wasm32")]
            web_folder_tx,
            #[cfg(target_arch = "wasm32")]
            web_folder_rx,
            #[cfg(target_arch = "wasm32")]
            web_file_handles: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_dir_handles: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_dirlist_tx,
            #[cfg(target_arch = "wasm32")]
            web_dirlist_rx,
            #[cfg(target_arch = "wasm32")]
            web_dirlist_inflight: std::collections::HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_export_tx,
            #[cfg(target_arch = "wasm32")]
            web_export_rx,
            #[cfg(target_arch = "wasm32")]
            web_pending_nav: None,
            #[cfg(target_arch = "wasm32")]
            web_thumb_inflight: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_read_inflight: std::rc::Rc::new(std::cell::Cell::new(0)),
            #[cfg(target_arch = "wasm32")]
            web_thumb_retries: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_preview_retries: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_preview_inflight: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_preview_failed: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_speed_inflight: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_speed_retries: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_speed_failed: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_full_inflight: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_full_retries: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_full_failed: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_worker_pool,
            #[cfg(target_arch = "wasm32")]
            web_preview_pending: Vec::new(),
            #[cfg(target_arch = "wasm32")]
            web_full_pending: Vec::new(),
            mode: ViewMode::Grid,
            catalog,
            catalog_load_pending: None,
            catalog_load_token: 0,
            catalog_load_tx,
            catalog_load_rx,
            ratings: HashMap::new(),
            edits: HashMap::new(),
            touchups: HashMap::new(),
            touchup_active: false,
            // Start with a small spot; the effective minimum is three source
            // pixels once an image is loaded.
            touchup_radius: 0.001,
            touchup_selected: None,
            develop_open: true,
            hist_sample: Vec::new(),
            hist_pixel_format: image_decode::PixelFormat::Srgb8,
            hist_dw: 0,
            hist_dh: 0,
            histogram: None,
            hist_dirty: false,
            filter: None,
            filter_cmp: Cmp::Gte,
            visible: Vec::new(),
            sel: None,
            selected: BTreeSet::new(),
            anchor: None,
            filmstrip_scroll_accum: 0.0,
            thumb_px: THUMB_DEFAULT,
            grid_cols: 1,
            grid_range: (0, 0),
            grid_scroll_reset: true,
            strip_range: (0, 0),
            thumb_tex: HashMap::new(),
            bursts_on: false,
            capture_times: HashMap::new(),
            sharpness: HashMap::new(),
            burst_marks: Vec::new(),
            dupes_on: false,
            phashes: HashMap::new(),
            dup_groups: Vec::new(),
            dup_refined: Vec::new(),
            feature_distances: HashMap::new(),
            feature_failed: HashSet::new(),
            feature_pending: HashSet::new(),
            dup_marks: Vec::new(),
            face_quality: HashMap::new(),
            face_pending: HashSet::new(),
            face_failed: HashSet::new(),
            eyes_filter: false,
            selection_on: false,
            selection_invert: false,
            current_selection: None,
            selection_pending: None,
            selection_tx,
            selection_rx,
            #[cfg(target_arch = "wasm32")]
            renderer_init_tx,
            #[cfg(target_arch = "wasm32")]
            renderer_init_rx,
            survey_members: Vec::new(),
            survey_best: None,
            survey_focus: 0,
            zoom: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            debug_tier_label: "",
            rotations: HashMap::new(),
            loupe_viewport: None,
            crop_edit: None,
            wb_picker: false,
            compare: false,
            exif_cache: HashMap::new(),
            source_size: None,
            pending_bulk: None,
            copied_settings: None,
            show_help: false,
            pending_quit: false,
            quit_requested: false,
            status: None,
            occluded: false,
            folder_root: None,
            folder_sel: None,
            expanded: HashSet::new(),
            subdirs: HashMap::new(),
            focus: Region::Folders,
            main_focus: Region::Folders,
            focus_level: FocusLevel::Selected,
            develop_focus: 0,
            toolbar_focus: 0,
            cursor: (0.0, 0.0),
            modifiers: ModifiersState::empty(),
            space_down: false,
            dragging: false,
            last_drag: (0.0, 0.0),
            egui_ctx,
            egui_state: None,
        }
    }

    /// Open a path: a directory → Grid (selection at 0); a file → Loupe (start
    /// on that file). Builds the playlist, seeds ratings, computes the visible
    /// view, and kicks off thumbnail/full requests.
    pub(crate) fn open(&mut self, path: PathBuf) {
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        eprintln!(
            "[lightphotos] open {}: {}",
            if is_dir { "dir" } else { "file" },
            path.display()
        );

        if is_dir {
            // Grid: the opened dir is the tree root, expanded with its children.
            self.folder_root = Some(path.clone());
            self.expanded = HashSet::from([path.clone()]);
            self.ensure_subdirs(&path);
            self.load_folder(path);
            self.mode = ViewMode::Grid;
            self.normalize_focus();
            self.request_redraw();
        } else {
            // File → Loupe (unchanged flow), but populate the tree from the
            // parent so switching to the grid shows a sidebar.
            if let Some(parent) = path.parent() {
                let parent = parent.to_path_buf();
                self.folder_root = Some(parent.clone());
                self.folder_sel = Some(parent.clone());
                self.expanded = HashSet::from([parent.clone()]);
                self.ensure_subdirs(&parent);
            }

            let playlist = Playlist::from_file(&path);
            self.seed_mirrors(&playlist);
            let start_index = playlist.position();
            self.playlist = Some(playlist);
            self.reset_burst_state();
            self.reset_dup_state();
            self.recompute_visible();
            self.sel = Some(
                self.visible
                    .iter()
                    .position(|&i| i == start_index)
                    .unwrap_or(0),
            );
            self.collapse_selection();
            self.mode = ViewMode::Loupe;
            self.develop_open = true;
            self.focus = Region::Detail;
            self.focus_level = FocusLevel::Selected;
            self.normalize_focus();
            self.load_selected();
            self.request_neighbors();
            self.request_redraw();
        }
    }

    /// Populate `subdirs[dir]` (the folder's immediate children) if not cached.
    fn ensure_subdirs(&mut self, dir: &Path) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.subdirs.contains_key(dir) {
                self.subdirs
                    .insert(dir.to_path_buf(), crate::navigation::list_subdirs(dir));
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if !self.subdirs.contains_key(dir) {
                self.request_dir_listing(dir);
            }
        }
    }

    /// Seed the in-memory ratings + edits + rotations mirrors from the catalog
    /// for every image in `playlist`. Shared by `open` (single file → Loupe) and
    /// `load_folder` (grid) so both entry points restore the same persisted state
    /// — notably rotations, which `open` previously skipped.
    ///
    /// `Catalog`'s sidecar scan runs on a background thread
    /// (`request_catalog_load`, in `app/catalog.rs`), so the reconcile loop
    /// below runs once here against the (just-cleared, still-empty) cache —
    /// safely inserting nothing — and again from `poll_catalog_load` once the
    /// real data lands, so first paint is never blocked on sidecar count.
    fn seed_mirrors(&mut self, playlist: &Playlist) {
        self.request_catalog_load(playlist.dir());
        self.reconcile_catalog_mirrors(playlist);
    }

    /// Load `dir`'s images into the grid (browse-first): rebuild the playlist,
    /// seed ratings, recompute the visible view, reset selection to nothing
    /// selected, mark `dir` as the selected folder, and request thumbnails.
    #[cfg(not(target_arch = "wasm32"))]
    fn load_folder(&mut self, dir: PathBuf) {
        self.load_playlist(Playlist::from_dir(&dir), dir);
    }

    #[cfg(target_arch = "wasm32")]
    fn load_folder(&mut self, _dir: PathBuf) {
        debug_assert!(
            false,
            "load_folder must not run on wasm32 — use nav_to_folder / the async open path"
        );
    }

    /// `load_folder`'s body, minus building the `Playlist` itself — shared
    /// with wasm32's File System Access folder picker (`app/web.rs`), which
    /// builds one via `Playlist::from_entries` instead of `from_dir` (no
    /// `std::fs::read_dir` on a browser-picked folder; see that module).
    fn load_playlist(&mut self, playlist: Playlist, dir: PathBuf) {
        self.seed_mirrors(&playlist);
        self.playlist = Some(playlist);
        self.reset_burst_state();
        self.reset_dup_state();
        self.recompute_visible();
        self.sel = None;
        self.selected.clear();
        self.anchor = None;
        self.folder_sel = Some(dir);
        // A new folder starts scrolled to the top, but `grid_range` otherwise
        // keeps whatever the *previous* folder's scroll position left it at.
        // `redraw` syncs textures against `grid_range` before this frame's
        // layout pass gets a chance to correct it (see the ordering comment
        // there), so a stale range here would sync against leftover scroll
        // depth from the old folder for one frame — wiping textures for what's
        // actually on screen. Reset it so that stale read always assumes "top
        // of the new folder" instead.
        self.grid_range = (0, 0);
        self.grid_scroll_reset = true;
        self.request_working_thumbs();
        self.request_redraw();
    }

    /// Paint one frame: run egui to build the chrome, then hand the image pass
    /// (confined to the loupe viewport in Loupe mode) + egui paint jobs to the
    /// renderer for a single wgpu submission.
    pub(crate) fn redraw(&mut self) {
        // Make sure thumbnails for the working set are uploaded before egui
        // references them.
        self.sync_thumb_textures();

        // Refresh the histogram if an image loaded or an adjustment changed. The
        // sample is tiny so this is sub-millisecond; only do it when the panel is
        // open and actually showing.
        if self.hist_dirty && self.develop_open {
            self.recompute_histogram();
        }

        // Kick off a background metadata read for the Loupe info panel when the
        // current image isn't cached yet. The loader dedups in-flight requests,
        // so this is cheap to call every frame until the result lands.
        if self.mode == ViewMode::Loupe {
            if let Some(path) = self.selected_path() {
                if !self.exif_cache.contains_key(&path) {
                    if let Some(loader) = &mut self.loader {
                        loader.request_exif(path);
                    }
                }
            }
        }

        let (Some(window), Some(mut state)) = (self.window.clone(), self.egui_state.take()) else {
            if let Some(r) = &mut self.renderer {
                r.render(None, None, None);
            }
            return;
        };

        let mut raw_input = state.take_egui_input(&*window);
        // Tab is deliberately unbound in this app (region-cycling is on F6
        // instead, precisely to avoid this) — but egui still has its own
        // built-in Tab-driven widget-focus traversal, which would otherwise
        // move its own internal focus and draw its own focus-ring outline on
        // a stray Tab press for no reason. This app has no `TextEdit` or
        // other widget that wants egui's own Tab handling, so just drop the
        // event before egui ever sees it.
        raw_input.events.retain(|e| {
            !matches!(
                e,
                egui::Event::Key {
                    key: egui::Key::Tab,
                    ..
                }
            )
        });

        // Run the UI, collecting the central image rect (Loupe) and any actions.
        let mut out = ui::FrameOutput::default();
        let full_output = self.egui_ctx.clone().run_ui(raw_input, |ui| {
            out = ui::draw(ui, self);
        });

        state.handle_platform_output(&*window, full_output.platform_output);
        self.egui_state = Some(state);

        // Apply actions the UI produced (clicks, double-clicks, slider, filter).
        self.apply_ui_actions(out.actions);

        // Surface any catalog write failure from this frame's mutations (rating,
        // develop edit, rotation, delete) as a toast — otherwise the change is
        // silently lost on quit.
        if let Some(msg) = self.catalog.take_error() {
            self.set_status(msg);
        }

        let pixels_per_point = self.egui_ctx.pixels_per_point();
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);

        let size = window.inner_size();
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size.width.max(1), size.height.max(1)],
            pixels_per_point,
        };

        // In Loupe, confine the image to the central rect egui left for it,
        // converting logical→physical pixels. In Grid, draw no image (the
        // opaque grid panel covers the surface).
        let image_viewport = match self.mode {
            ViewMode::Loupe => out.loupe_rect.map(|r| {
                let x = (r.min.x * pixels_per_point).round().max(0.0) as u32;
                let y = (r.min.y * pixels_per_point).round().max(0.0) as u32;
                let w = (r.width() * pixels_per_point).round().max(0.0) as u32;
                let h = (r.height() * pixels_per_point).round().max(0.0) as u32;
                (x, y, w, h)
            }),
            // Both are egui-only chrome (no GPU-rendered loupe image).
            ViewMode::Grid | ViewMode::Survey => Some((0, 0, 0, 0)),
        };

        // If the loupe viewport changed, re-fit so the image stays centered in it.
        if self.mode == ViewMode::Loupe {
            if image_viewport != self.loupe_viewport {
                self.loupe_viewport = image_viewport;
                if self.fitted {
                    // While cropping, keep the whole image (all 4 edges) visible.
                    if self.crop_edit.is_some() {
                        self.fit_for_crop();
                    } else {
                        self.fit_to_window();
                    }
                } else {
                    self.push_transform();
                }
            }
        }

        // Before/after compare (Loupe): split the central rect into two equal
        // halves, set up the two-uniform draw, and render the image twice.
        // An odd spare pixel becomes a one-pixel divider so both viewports use
        // the same transform dimensions.
        let mut primary_vp = image_viewport;
        let mut compare_vp = None;
        if self.compare && self.mode == ViewMode::Loupe {
            if let Some((x, y, w, h)) = image_viewport {
                if w >= 2 && h > 0 {
                    let half = w / 2;
                    let gap = w % 2;
                    self.push_compare();
                    primary_vp = Some((x, y, half, h));
                    compare_vp = Some((x + half + gap, y, half, h));
                }
            }
        }

        // wasm32 RAW loading (see `App::loupe_is_loading`): don't draw the
        // image quad at all while the real decode is in flight, so the
        // previously-shown photo can't show through underneath — the Loupe
        // just goes blank until the sharp image lands. Overriding
        // `primary_vp` here rather than `image_viewport` itself keeps this
        // out of `self.loupe_viewport`'s own change-detection above — a
        // fake zero-size viewport would otherwise trigger a spurious
        // `fit_to_window()`/`push_transform()` call on both the way into
        // and out of loading. Same zero-size precedent Grid/Survey already
        // establish (`image_viewport`'s own match arm, above) —
        // `renderer.rs`'s `draw_into` early-returns before ever
        // binding/drawing, so `image_bind`/the uploaded texture is left
        // completely untouched, nothing to restore once loading ends.
        if self.loupe_is_loading() {
            primary_vp = Some((0, 0, 0, 0));
            compare_vp = None;
        }

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let egui_paint = EguiPaint {
            textures_delta: full_output.textures_delta,
            paint_jobs,
            screen_descriptor,
        };
        let presented = renderer.render(primary_vp, compare_vp, Some(egui_paint));
        // If the surface wasn't presentable (e.g. the window opened occluded /
        // behind another window), keep retrying so we draw as soon as it's
        // revealed — unless winit has told us it's genuinely occluded, in which
        // case we wait for the Occluded(false) event instead of busy-looping.
        if !presented && !self.occluded {
            self.request_redraw();
        }
    }

    /// Apply the actions egui emitted this frame.
    fn apply_ui_actions(&mut self, actions: Vec<ui::UiAction>) {
        for action in actions {
            match action {
                ui::UiAction::Select(pos) => {
                    if pos < self.visible.len() {
                        self.select_single(pos);
                        // In the loupe, selecting a filmstrip cell must also show
                        // it (selection == shown). In the grid, selecting is just
                        // focus — Enter/double-click opens the loupe.
                        if self.mode == ViewMode::Loupe {
                            self.load_selected();
                            self.request_neighbors();
                        }
                        self.request_redraw();
                    }
                }
                ui::UiAction::SelectToggle(pos) => {
                    if pos < self.visible.len() {
                        self.select_toggle(pos);
                        self.request_redraw();
                    }
                }
                ui::UiAction::SelectRange(pos) => {
                    if pos < self.visible.len() {
                        self.select_range(pos);
                        self.request_redraw();
                    }
                }
                ui::UiAction::CopySettings => self.copy_settings(),
                ui::UiAction::ToggleHelp => {
                    self.show_help = !self.show_help;
                    self.request_redraw();
                }
                ui::UiAction::EnterLoupe => self.enter_loupe(),
                ui::UiAction::EnterGrid => self.enter_grid(),
                ui::UiAction::ConfirmQuit => self.quit_requested = true,
                ui::UiAction::CancelQuit => {
                    self.pending_quit = false;
                    self.request_redraw();
                }
                ui::UiAction::RequestBulk(kind) => self.request_bulk(kind),
                ui::UiAction::ConfirmBulk => {
                    if let Some(kind) = self.pending_bulk.take() {
                        self.run_bulk(kind);
                    }
                    self.request_redraw();
                }
                ui::UiAction::CancelBulk => {
                    self.pending_bulk = None;
                    self.request_redraw();
                }
                ui::UiAction::OpenLoupe(pos) => {
                    if pos < self.visible.len() {
                        self.select_single(pos);
                        self.enter_loupe();
                    }
                }
                ui::UiAction::SetThumbPx(px) => {
                    self.thumb_px = px.clamp(THUMB_MIN, THUMB_MAX);
                    self.request_redraw();
                }
                ui::UiAction::SetFilter(f) => self.set_filter(f),
                ui::UiAction::SetFilterCmp(cmp) => self.set_filter_cmp(cmp),
                ui::UiAction::SetRating(stars) => self.set_rating(stars),
                ui::UiAction::ScrollFilmstrip(delta) => self.scroll_filmstrip(delta),
                ui::UiAction::ToggleBursts => self.toggle_bursts(),
                ui::UiAction::ToggleDupes => self.toggle_dupes(),
                ui::UiAction::ToggleEyesClosed => self.toggle_eyes_filter(),
                ui::UiAction::ToggleSelection => self.toggle_selection(),
                ui::UiAction::ToggleSelectionInvert => self.toggle_selection_invert(),
                ui::UiAction::OpenSurvey(pos) => self.open_survey(pos),
                ui::UiAction::CloseSurvey => self.close_survey(),
                ui::UiAction::KeepBestRejectRest => self.keep_best_reject_rest(),
                ui::UiAction::FocusSurveyMember(i) => self.set_survey_focus(i),
                ui::UiAction::OpenFolder(p) => {
                    // The folder row is one unit: clicking it focuses the tree,
                    // loads the folder, and toggles its expansion — same as Enter.
                    self.set_focus(Region::Folders, FocusLevel::Entered);
                    self.open_folder(p);
                }
                #[cfg(target_arch = "wasm32")]
                ui::UiAction::PickFolder => self.request_folder_pick(),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::PickFolder => {}
                ui::UiAction::Focus(region) => {
                    self.focus = region;
                    self.focus_level = FocusLevel::Entered;
                    self.normalize_focus();
                    self.on_focus_changed();
                    self.request_redraw();
                }
                ui::UiAction::FocusToolbar(idx) => {
                    self.set_focus(Region::Toolbar, FocusLevel::Entered);
                    self.toolbar_focus = idx;
                    self.request_redraw();
                }
                ui::UiAction::FocusDevelop(idx) => {
                    self.set_focus(Region::Develop, FocusLevel::Entered);
                    self.develop_focus = idx;
                    self.request_redraw();
                }
                ui::UiAction::CropGrab(edge) => self.crop_grab(edge),
                ui::UiAction::CropGrabMove(u, v) => self.crop_grab_move(u, v),
                ui::UiAction::CropDragTo(u, v) => self.crop_drag_to(u, v),
                ui::UiAction::CropRelease => self.crop_release(),
                ui::UiAction::ToggleWbPicker => self.toggle_wb_picker(),
                ui::UiAction::PickWhiteBalance(u, v) => self.pick_white_balance(u, v),
                ui::UiAction::ToggleTouchUp => {
                    self.touchup_active = !self.touchup_active;
                    self.wb_picker = false;
                    self.request_redraw();
                }
                ui::UiAction::SetTouchUpRadius(r) => {
                    self.set_touchup_radius(r);
                    self.request_redraw();
                }
                ui::UiAction::TouchUpClick(u, v) => self.add_touchup(u, v),
                ui::UiAction::SelectTouchUp(i) => {
                    if i < self.current_touchups().len() {
                        self.touchup_selected = Some(i);
                    }
                    self.request_redraw();
                }
                ui::UiAction::DeleteTouchUp => self.delete_selected_touchup(),
                ui::UiAction::UndoTouchUp => {
                    if self.touchup_selected.is_none() && !self.current_touchups().is_empty() {
                        self.touchup_selected = Some(self.current_touchups().len() - 1);
                    }
                    self.delete_selected_touchup();
                }
                ui::UiAction::SetAdjustments(adj) => self.apply_adjustments(adj),
                ui::UiAction::ResetAdjustments => {
                    let Some(path) = self.shown.path().map(Path::to_path_buf) else {
                        continue;
                    };
                    self.edits.remove(&path);
                    self.touchups.remove(&path);
                    self.catalog.set_adjustments(&path, &Adjustments::default());
                    self.catalog.set_touchups(&path, &[]);
                    self.touchup_selected = None;
                    self.push_adjustments();
                    self.hist_dirty = true;
                    self.request_redraw();
                }
            }
        }
    }
}

/// The 0..=9 digit a key code represents, if it's a top-row or numpad digit.
fn digit_of(code: KeyCode) -> Option<u8> {
    use KeyCode::*;
    Some(match code {
        Digit0 | Numpad0 => 0,
        Digit1 | Numpad1 => 1,
        Digit2 | Numpad2 => 2,
        Digit3 | Numpad3 => 3,
        Digit4 | Numpad4 => 4,
        Digit5 | Numpad5 => 5,
        Digit6 | Numpad6 => 6,
        Digit7 | Numpad7 => 7,
        Digit8 | Numpad8 => 8,
        Digit9 | Numpad9 => 9,
        _ => return None,
    })
}

/// A file's display name (final path component), for toolbars/status messages.
fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// The inclusive set of positions between `anchor` and `pos` (order-agnostic).
/// Used for Shift range-selection.
fn range_set(anchor: usize, pos: usize) -> BTreeSet<usize> {
    let (lo, hi) = if anchor <= pos {
        (anchor, pos)
    } else {
        (pos, anchor)
    };
    (lo..=hi).collect()
}

/// Remap a multi-selection (given as *playlist* indices) onto positions in a new
/// `visible` list, dropping any index the filter removed. Keeps the selection
/// pinned to the same photos across a re-filter.
fn remap_positions(selected_pl: &[usize], new_visible: &[usize]) -> BTreeSet<usize> {
    selected_pl
        .iter()
        .filter_map(|&i| new_visible.iter().position(|&v| v == i))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[usize]) -> BTreeSet<usize> {
        items.iter().copied().collect()
    }

    #[test]
    fn range_set_is_inclusive_and_order_agnostic() {
        assert_eq!(range_set(2, 5), set(&[2, 3, 4, 5]));
        assert_eq!(range_set(5, 2), set(&[2, 3, 4, 5])); // same range, anchor after
        assert_eq!(range_set(3, 3), set(&[3])); // single cell
    }

    #[test]
    fn remap_keeps_surviving_photos_and_drops_filtered() {
        // Old visible = playlist indices [10, 11, 12, 13]; selection was positions
        // {1, 3} → playlist indices [11, 13]. After a re-filter the new visible is
        // [11, 20, 13] (11 → pos 0, 13 → pos 2; 12 dropped).
        let selected_pl = [11usize, 13];
        let new_visible = [11usize, 20, 13];
        assert_eq!(remap_positions(&selected_pl, &new_visible), set(&[0, 2]));
    }

    #[test]
    fn remap_drops_everything_when_all_filtered_out() {
        let selected_pl = [11usize, 13];
        let new_visible = [20usize, 21]; // none of the selected survive
        assert_eq!(remap_positions(&selected_pl, &new_visible), set(&[]));
    }
}
