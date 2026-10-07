// SPDX-License-Identifier: GPL-3.0-or-later

//! The `App`: all viewer state, plus the logic that ties together the GPU
//! renderer, the background loader, the catalog, the egui chrome, and the
//! keyboard bindings. `main.rs` runs the winit event loop and calls into the
//! `pub(crate)` methods and fields here.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::SystemTime;
// std::time::Instant panics on wasm32, which has no OS clock.
use web_time::Instant;

use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

use crate::catalog::Catalog;
use crate::develop::{Adjustments, Crop, TouchUp};
use crate::export::Exporter;
use crate::loader::Loader;
use crate::navigation::{Cmp, FlagFilter, Playlist};
use crate::renderer::{EguiPaint, Renderer};
use crate::{image_decode, ui};

/// The closest manual zoom, in screen pixels per source pixel. The farthest
/// is the fit, so zooming out never shrinks the photo below the window.
const MAX_ZOOM: f32 = 64.0;

/// Side of a grid cell, in egui points. Smaller than
/// [`crate::thumbnail::THUMB_PX`] so HiDPI displays get real pixels to draw.
pub(crate) const GRID_CELL_PT: f32 = 192.0;

/// Whether the Grid toolbar shows the Eyes-closed filter. Off because no real
/// photo set has checked `CLOSED_EYE_RATIO`.
pub(crate) const SHOW_EYES_FILTER: bool = false;

/// Whether presets are reachable: the Develop panel's Presets block, the
/// selection bar's Preset dropdown, and Cmd+Shift+P. Off until the feature is
/// ready. Saved presets stay on disk either way.
pub(crate) const SHOW_PRESETS: bool = false;

/// Whether Settings shows Auto Tone's centering choice. Hidden for now; the
/// saved choice still applies.
pub(crate) const SHOW_AUTOTONE_CENTERING: bool = false;

/// Whether the Loupe's info bar shows Show Selection and Invert. The O keys
/// work either way.
pub(crate) const SHOW_SELECTION_BUTTONS: bool = false;

/// Longest-side bounds for the loupe's screen-fit preview decode. The minimum
/// keeps it sharper than a thumbnail. The maximum stops a 5K display from
/// asking for a decode nearly as costly as the full image.
const PREVIEW_MIN: u32 = 1024;
const PREVIEW_MAX: u32 = 4096;
/// The preview target rounds up to a multiple of this, so resizing the window
/// doesn't request a new decode on every pixel.
const PREVIEW_QUANTUM: u32 = 512;
/// Smallest touch-up radius in source-image pixels.
pub(crate) const TOUCHUP_MIN_PIXELS: f32 = 3.0;
pub(crate) const TOUCHUP_MAX_RADIUS: f32 = 0.15;
/// Default brush feather: the fraction of the patch radius used to blend the
/// correction into its edges.
const TOUCHUP_FEATHER: f32 = 1.0;
/// Both renderers clamp feather to this, so the brush stops here too.
pub(crate) const TOUCHUP_MIN_FEATHER: f32 = 0.02;

#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    Grid,
    Loupe,
}

/// One edge of the crop rectangle in texture space: `Left` is the low-x edge
/// of the unrotated image. Hit-testing maps edges to the screen through the
/// loupe transform, so they work on rotated images.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CropEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Copy, Clone)]
enum CropGrab {
    Edge(CropEdge),
    /// Moving the whole rectangle. Stores the pointer's texture uv and the
    /// rectangle at grab time, so the drag is anchor-relative and doesn't drift.
    Move {
        anchor: (f32, f32),
        rect0: Crop,
    },
}

/// Crop-mode state, present only while crop mode is active. `rect` is in
/// normalized texture space and saves to `Adjustments.crop` as each edit
/// lands.
/// While cropping, the GPU draws the full frame and egui draws the mask.
pub struct CropDraft {
    rect: Crop,
    /// `None` when no drag is in progress.
    grab: Option<CropGrab>,
    /// Texture-space pixel aspect ratio (w/h) captured at grab time, for
    /// Shift-lock under `CropAspect::Custom`.
    grab_aspect: f32,
    aspect: CropAspect,
    /// The draft's shape on screen. Only fixed, non-square ratios read it.
    orientation: CropOrientation,
    /// Degrees the photo turns before `rect` applies. `rect` is in the
    /// straightened canvas, so it stays inside the turned photo.
    straighten: f32,
    straighten_tool: StraightenTool,
    /// The Develop tab to go back to when crop mode ends.
    return_tab: DevelopTab,
}

/// The Straighten tool inside crop mode. A line is drawn in the draft's
/// canvas uv, so it reads the angle the photo shows on screen.
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum StraightenTool {
    Off,
    /// On, waiting for the first drag.
    Ready,
    Line {
        from: (f32, f32),
        to: (f32, f32),
    },
}

const FULL_CROP: Crop = Crop {
    left: 0.0,
    top: 0.0,
    right: 1.0,
    bottom: 1.0,
};
/// Smallest crop edge separation, in normalized units, so the rect never collapses.
const MIN_CROP: f32 = 0.02;

/// The UI region that receives arrow and Enter keys. `Toolbar` and `Filmstrip`
/// are chrome, reached only with F6 / Shift+F6, which toggles between them and
/// the last focused main region. The main chain (`Folders` > `Grid` > `Detail`
/// > `Develop`) is walked with Enter and Escape. `Detail` is the Loupe with the
/// Develop panel closed. Region cycling uses F6, not Tab, because egui_winit
/// always consumes Tab for its own widget focus.
#[derive(Copy, Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Region {
    Toolbar,
    Folders,
    Grid,
    Detail,
    Filmstrip,
    Develop,
}

/// The Grid's order. Session only; every launch starts on `Name`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum GridSort {
    /// The folder's file-name order.
    #[default]
    Name,
    /// Oldest capture time first, photos without one last in name order.
    Time,
    /// Highest quality score first, unscored photos last in name order.
    Quality,
}

/// Which view fills the left panel. Session only; every launch starts on
/// `Folders`.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum LeftTab {
    #[default]
    Folders,
    Info,
}

/// Chrome regions F6 steps through before wrapping back to the main region.
const CHROME_ORDER: [Region; 2] = [Region::Toolbar, Region::Filmstrip];

/// `Selected` means F6 landed on the region. `Entered` means a specific
/// control has the cursor. Enter, or the first arrow press in a control
/// region, enters. Escape returns to `Selected`. Grid arrows move the
/// selection without entering, so one Escape still leaves the grid.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FocusLevel {
    Selected,
    Entered,
}

/// What the loupe has uploaded to the GPU, and at which tier. `App::want` is
/// the path we want; `try_show` moves `shown` toward it.
enum Shown {
    Nothing,
    /// Placeholder shown while the preview decodes.
    Thumb(PathBuf),
    /// The screen-fit preview: `(path, target, actual longest side)`. The
    /// target detects a resize that wants a new size. The actual size detects
    /// the full-quality decode replacing the fast Speed pass at the same target.
    Preview(PathBuf, u32, u32),
    /// Full resolution, fetched only after the user zooms in.
    Full(PathBuf),
}

impl Shown {
    fn path(&self) -> Option<&Path> {
        match self {
            Shown::Nothing => None,
            Shown::Thumb(p) | Shown::Preview(p, _, _) | Shown::Full(p) => Some(p),
        }
    }

    fn is_full_of(&self, path: &Path) -> bool {
        matches!(self, Shown::Full(p) if p == path)
    }

    /// True when re-uploading this preview would change nothing.
    fn is_preview_of(&self, path: &Path, target: u32, actual: u32) -> bool {
        matches!(self, Shown::Preview(p, t, a) if p == path && *t == target && *a == actual)
    }
}

/// Native decode is fast enough that the loupe never blanks while loading.
/// The wasm32 version lives in `app/web.rs`.
#[cfg(not(target_arch = "wasm32"))]
impl App {
    fn loupe_is_loading(&self) -> bool {
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
    /// The batch goes to an Immich server, so the toast says "Uploading".
    uploading: bool,
    /// Uploads the server already had.
    duplicates: usize,
    /// Uploads whose star rating the server refused, and why the last one was.
    unrated: usize,
    last_rating_err: Option<String>,
    /// The album the batch goes into, and the assets it uploaded so far.
    #[cfg(not(target_arch = "wasm32"))]
    album: crate::export::AlbumChoice,
    #[cfg(not(target_arch = "wasm32"))]
    asset_ids: Vec<String>,
}

/// A finished subject-segmentation run: the path it was computed for, and the
/// mask or the reason there isn't one.
pub(crate) type SelectionOutcome = (PathBuf, Result<crate::segmentation::Mask, String>);

/// An action waiting on its confirm dialog. While one is open it owns the
/// keyboard, and only one can be open at a time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PendingConfirm {
    Bulk(ui::BulkKind),
    DeletePreset(u64),
    DeleteGroup {
        focus: Option<ui::Role>,
    },
    /// Trash the members picked in the Loupe's Compare pane.
    DeletePicks,
}

/// What a status toast reports, which sets its colors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StatusKind {
    Success,
    Error,
    Progress,
    /// Neither a result nor progress, such as a batch the user stopped.
    Info,
}

/// A wasm32 folder navigation waiting on its async subfolder listing.
/// `Open` toggles expansion and, for a folder with subfolders but no photos,
/// skips to its first child, like native `open_folder`. `Load` swaps the grid,
/// like native `load_folder`. `LoadAfterOpen` finishes that skip without
/// opening the child.
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone)]
pub(crate) enum WebPendingNav {
    Open(PathBuf),
    Load(PathBuf),
    LoadAfterOpen(PathBuf),
}

/// Which module the Develop panel shows under the histogram.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum DevelopTab {
    /// Tone, color, and detail sliders.
    Sliders,
    /// Crop mode: the tab shows exactly while `crop_edit` is set.
    Crop,
    /// Touch Up and future local adjustments.
    Masks,
}

/// An icon on the Loupe's right-edge rail: a Develop page, or the group's
/// Compare pane. At most one is lit, and clicking the lit one turns it off.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum RailItem {
    Develop(DevelopTab),
    GroupCompare,
    Export,
}

impl From<DevelopTab> for RailItem {
    fn from(tab: DevelopTab) -> Self {
        RailItem::Develop(tab)
    }
}

/// What a click on the Loupe image does, besides panning.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LoupeTool {
    None,
    /// The next click solves temp and tint to make that pixel neutral.
    WbPicker,
    /// Clicks add spot-heal touch-ups.
    TouchUp,
}

/// A grid thumbnail on the GPU. The size travels with the id because the
/// renderer does not keep one, and every cell needs it to letterbox the photo
/// in its square.
#[derive(Clone, Copy)]
pub(crate) struct ThumbTexture {
    pub id: egui::TextureId,
    pub width: u32,
    pub height: u32,
    /// The edit signature baked into it. A cell keeps drawing a texture
    /// whose edit is out of date until the new bake replaces it.
    pub sig: u64,
}

/// A finished background sidecar load: its directory, the navigation token it
/// was requested under, its place in the sidecar write order, and the records.
pub(crate) type CatalogLoadResult = (
    PathBuf,
    u64,
    crate::catalog::LoadMark,
    crate::catalog::SidecarLoad,
);

/// A folder's signal cache as loaded off the UI thread, with what it knows
/// about each playlist photo whose file has not changed since.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) type SignalLoad = (
    crate::signalcache::SignalCache,
    Vec<(PathBuf, crate::signalcache::PhotoSignals)>,
);

pub(crate) struct App {
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) renderer: Option<Renderer>,
    /// One mipped texture per group member, keyed on the members and
    /// the preview size, so a resize or another group re-uploads the set.
    pub(crate) compare_tiles: Option<CompareTiles>,
    /// Center of the zoomed square in uv, shared by every member.
    pub(crate) compare_center: egui::Pos2,
    /// The pointer was over the zoom marker last frame.
    pub(crate) compare_marker_hovered: bool,
    /// The pointer is over the shown photo in Compare, square or not.
    pub(crate) compare_photo_hovered: bool,
    /// The zoomed square's side as a fraction of the photo's short
    /// side. `LIGHTPHOTOS_COMPARE_SIDE` sets the start, 0.15 by default.
    pub(crate) compare_side: f32,
    /// Whether a grouped photo's Loupe shows the group pane. Kept across
    /// photos for the session, not saved.
    group_view: GroupView,
    tile_fidelity: TileFidelity,
    /// The members tile clicks picked in the Compare pane, waiting for Set
    /// as representative or Delete.
    picks: GroupPicks,
    /// Last adjustments handed to the GPU. Tests run without a renderer, so
    /// this is the only way to assert what the loupe would actually show.
    #[cfg(test)]
    pub(crate) pushed_adj: Option<Adjustments>,
    pub(crate) loader: Option<Loader>,
    /// `None` until the window is created, and on targets that cannot spawn
    /// threads.
    pub(crate) score_pool: Option<crate::score::ScorePool>,
    /// The photos a "Score photos" run has left. `None` when none is running.
    score_job: Option<crate::score::ScoreJob>,
    /// The export form, its settings and the exports it started.
    exports: export::Exports,
    /// The running bulk delete, if any. `pub(crate)` because the frame loop
    /// polls it.
    pub(crate) bulk_delete: Option<bulk_delete::BulkDelete>,

    /// `Renderer::new` is async because WebGPU device setup is a browser
    /// Promise. wasm32 can't block the main thread, so `resumed()` spawns it
    /// and this channel returns the renderer to `about_to_wait`. Native blocks
    /// with `pollster` instead.
    #[cfg(target_arch = "wasm32")]
    pub(crate) renderer_init_tx: Sender<(Renderer, winit::dpi::PhysicalSize<u32>)>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) renderer_init_rx: Receiver<(Renderer, winit::dpi::PhysicalSize<u32>)>,

    playlist: Option<Playlist>,

    /// Path we want shown in the loupe (may still be decoding).
    want: Option<PathBuf>,
    shown: Shown,
    /// A file or folder requested before the window and renderer existed.
    pub(crate) pending_initial: Option<PathBuf>,
    /// The last session saved, which the landing page's Reopen Session
    /// button restores.
    session: Option<session::Session>,
    /// A Reopen Session waiting on its folder's listing, then on each
    /// listing down to its subfolder.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_session_restore: Option<session::Session>,

    /// True while `showDirectoryPicker` and its listing are in flight. Disables
    /// the landing page's "Choose Folder" button so a second picker can't open.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_pending: bool,
    /// Set while `fonts::fetch_full_cjk` is running or once it has succeeded,
    /// so the font downloads at most once a session. A failed fetch clears it.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_full_cjk_requested: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_tx: Sender<Result<crate::web_fs::PickedFolder, String>>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_rx: Receiver<Result<crate::web_fs::PickedFolder, String>>,
    /// File handles for the open folder's images, keyed like the playlist
    /// entries. A picked folder has no OS path, so every read goes through these.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_file_handles: HashMap<PathBuf, web_sys::FileSystemFileHandle>,
    /// Directory handles for every folder browsed so far, keyed by relative
    /// path with the picked root's name first. The catalog's sidecar handle
    /// switches to the current folder's entry on each navigation.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dir_handles: HashMap<PathBuf, web_sys::FileSystemDirectoryHandle>,
    /// Per-folder thumbnail cache index, shared by that folder's async writes.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_thumb_cleanup:
        HashMap<PathBuf, std::rc::Rc<std::cell::RefCell<crate::web_thumb_cache::Cleanup>>>,
    /// Async subfolder listings, tagged with the navigation generation that
    /// asked for them. `poll_dir_listing` drops stale generations.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_tx: Sender<(u64, PathBuf, Result<crate::web_fs::DirListing, String>)>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_rx: Receiver<(u64, PathBuf, Result<crate::web_fs::DirListing, String>)>,
    /// `(directory, generation)` pairs with a listing in flight. Stops per-frame
    /// polling from re-requesting, while a newer generation may retry.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_inflight: std::collections::HashSet<(PathBuf, u64)>,
    /// Finished browser JPEG writes, drained into `on_export_outcomes`. The
    /// browser counterpart of native `Exporter::poll()`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_export_tx: Sender<crate::export::ExportOutcome>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_export_rx: Receiver<crate::export::ExportOutcome>,
    /// Text read from the browser clipboard, fed to egui as a paste on the
    /// next frame. See `request_web_paste`.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_paste_tx: Sender<String>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_paste_rx: Receiver<String>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_pending_nav: Option<WebPendingNav>,
    /// Bumped on every tree action. A listing may apply only the navigation
    /// deferred by the latest action.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_nav_generation: u64,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_pending_nav_generation: u64,
    /// Bumped only when a folder pick replaces the handle maps. Thumbnail jobs
    /// carry it so results for an old pick are dropped: browser paths start
    /// with the folder's name, so re-picking a same-named folder would
    /// otherwise match stale jobs. Not `web_nav_generation`, which bumps on
    /// every tree action and would cancel decodes while arrowing through the tree.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_handle_generation: u64,
    /// Thumbnail decodes in flight on the Web Worker pool. `loader.rs`'s queue
    /// has no workers on wasm32, so its own in-flight set doesn't apply.
    #[cfg(target_arch = "wasm32")]
    web_thumb_inflight: HashSet<(PathBuf, u32)>,
    /// Failed cached decodes waiting for a slot to read the source file. Their
    /// keys stay in `web_thumb_inflight` so normal requests don't retry the
    /// corrupt cache.
    #[cfg(target_arch = "wasm32")]
    web_thumb_recovery_pending: Vec<crate::web_decode::PoolResult>,
    /// `get_file()` reads in flight across all decode tiers. Chrome throws
    /// `NotReadableError` when too many reads are open against one folder, so
    /// `MAX_CONCURRENT_READS` caps this. A key that can't start this frame
    /// stays out of its in-flight set and is retried next frame.
    /// `Rc<Cell<_>>` so the `spawn_local` task can decrement it without `&mut App`.
    #[cfg(target_arch = "wasm32")]
    web_read_inflight: std::rc::Rc<std::cell::Cell<u32>>,
    /// Consecutive failure count and earliest next retry time per key.
    /// Chrome's `NotReadableError` is usually transient, so a key gives up only
    /// after `MAX_READ_RETRIES` failures. Retries a frame apart all fail, so the
    /// deadline backs off per attempt (`retry_backoff`). A decode failure
    /// gives up at once, see `web_decode::Failure`. Cleared on success or
    /// when giving up.
    #[cfg(target_arch = "wasm32")]
    web_thumb_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_preview_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    /// Loupe preview decodes. `try_show` asks for the preview every frame, so
    /// `web_preview_failed` stops a doomed decode from retrying forever.
    #[cfg(target_arch = "wasm32")]
    web_preview_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_preview_failed: HashSet<(PathBuf, u32)>,
    /// The loupe's fast screen-fit decode (`JobKind::Speed`). Tracked apart
    /// from `Preview` because both results usually have the same size, so they
    /// can't share `loader.rs`'s preview slot. `poll_web_preview` uploads a
    /// `Speed` result directly instead.
    #[cfg(target_arch = "wasm32")]
    web_speed_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_speed_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_speed_failed: HashSet<(PathBuf, u32)>,
    /// The loupe's zoom-triggered full-resolution decode, the wasm32
    /// counterpart of `Loader::request_full`. Results land through
    /// `loader.insert_full_external`, where `try_show` finds them.
    #[cfg(target_arch = "wasm32")]
    web_full_inflight: HashSet<(PathBuf, u32)>,
    #[cfg(target_arch = "wasm32")]
    web_full_retries: HashMap<(PathBuf, u32), (u8, Instant)>,
    #[cfg(target_arch = "wasm32")]
    web_full_failed: HashSet<(PathBuf, u32)>,

    /// Where each running export's JPEG goes. The loader's threads bake it;
    /// the destination folder handle cannot leave the main thread.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_exports: crate::web_exports::WebExports,
    /// `Preview` and `Speed` results that `poll_web_thumbs` pulled off the
    /// pool's single shared channel. `poll_web_preview` consumes them in the
    /// same frame.
    #[cfg(target_arch = "wasm32")]
    web_preview_pending: Vec<crate::web_decode::PoolResult>,
    /// `Full` results set aside the same way, for `poll_web_full`.
    #[cfg(target_arch = "wasm32")]
    web_full_pending: Vec<crate::web_decode::PoolResult>,

    pub(crate) mode: ViewMode,
    /// `pub(crate)` because the frame loop drives its write queue directly.
    pub(crate) catalog: Catalog,
    /// Directory and token of the background sidecar scan in flight, if any.
    /// Cleared only when the result with the matching token lands. A token is
    /// needed because fast A to B to A navigation runs two loads for A, and the
    /// first one landing must not clear the second.
    catalog_load_pending: Option<(PathBuf, u64)>,
    /// One per `request_catalog_load` call.
    catalog_load_token: u64,
    catalog_load_tx: Sender<CatalogLoadResult>,
    catalog_load_rx: Receiver<CatalogLoadResult>,
    ratings: HashMap<PathBuf, u8>,
    /// Per-image develop edits. Holds only non-identity edits.
    edits: HashMap<PathBuf, Adjustments>,
    touchups: HashMap<PathBuf, Vec<TouchUp>>,
    /// Loupe click tool. Crop mode (`crop_edit`) turns it off.
    tool: LoupeTool,
    touchup_radius: f32,
    touchup_feather: f32,
    touchup_opacity: f32,
    touchup_selected: Option<usize>,
    /// O hides the spot circles in Touch Up to judge the fix unobstructed.
    touchup_spots_hidden: bool,
    /// Per-image spot lists as they were before each Touch Up add or delete,
    /// newest last, so Undo can step back through both.
    touchup_undo: HashMap<PathBuf, Vec<Vec<TouchUp>>>,
    develop_open: bool,
    develop_tab: DevelopTab,
    /// The shown photo's pixel sample and the histogram binned from it.
    hist: histogram::Histogram,
    /// Photo whose edit is not yet written to its sidecar. See
    /// `save_edit_unless_dragging`.
    unsaved_edit: Option<PathBuf>,
    #[cfg(target_arch = "wasm32")]
    unsaved_edit_kind: &'static str,
    /// Active star filter. `None` shows all.
    filter: Option<(Cmp, u8)>,
    /// The grid's flag filter, applied with the star filter.
    flag_filter: FlagFilter,
    /// The Compare pane's own flag filter over the shown group's tiles.
    compare_flag_filter: FlagFilter,
    /// Comparator used when a star level is clicked. Stays set across "All".
    filter_cmp: Cmp,
    grid_sort: GridSort,
    /// Indices into `playlist.entries()` that pass the current filter.
    visible: Vec<usize>,
    /// The photos behind `visible`, a group's cell counting each member.
    shown_photos: usize,
    /// Position within `visible` of the current selection. `None` in the Grid
    /// before the user clicks or arrows. The Loupe always has a selection.
    sel: Option<usize>,
    /// Multi-selection, as positions within `visible`. When non-empty it
    /// contains `sel`. Bulk operations act on this set.
    selected: BTreeSet<usize>,
    /// Shift range-selection anchor, as a position within `visible`.
    anchor: Option<usize>,
    /// Wheel delta over the filmstrip not yet large enough to step one photo.
    filmstrip_scroll_accum: f32,
    /// Columns the grid laid out last frame, for Up/Down moves.
    grid_cols: usize,
    /// Cell range `[start, end)` visible in the grid last frame. Only these
    /// load thumbnails, so huge folders stay cheap.
    grid_range: (usize, usize),
    grid_scroll_reset: bool,
    /// Where the Grid or the filmstrip drew each cell last frame.
    cell_rects: Vec<(usize, egui::Rect)>,
    /// The filmstrip's equivalent of `grid_range`.
    strip_range: (usize, usize),
    /// Thumbnail textures keyed by (path, THUMB_PX), each carrying the edit
    /// signature baked into it. An edit changes the signature, which queues a
    /// re-bake; the stale texture stays on screen until the bake replaces it.
    /// Pruned to the working set each frame.
    ///
    /// The renderer owns the GPU side, so dropping an entry here is not enough;
    /// `sync_thumb_textures` hands every pruned id back to `Renderer::free_thumb`.
    thumb_tex: HashMap<(PathBuf, u32), ThumbTexture>,

    /// The folder's derived signals as they were left last session. Seeded
    /// into the four maps below when a folder opens, and written back as new
    /// ones are computed. A cache, never user data: see `src/signalcache.rs`
    /// for why it is not part of `ImageRecord`.
    pub(crate) signals: crate::signalcache::SignalCache,
    /// The folder's cache file loading on another thread, with the entries it
    /// holds for the playlist already checked against the files. While this is
    /// `Some`, `signals` is a detached stand-in. See `adopt_signal_cache`.
    #[cfg(not(target_arch = "wasm32"))]
    signal_load_rx: Option<Receiver<SignalLoad>>,

    /// Capture time per path, from EXIF or mtime. `Some(None)` means the read
    /// found no time, so it isn't requested again.
    capture_times: HashMap<PathBuf, Option<SystemTime>>,
    /// Group Bursts' photos while their capture times are read.
    burst_scan: Option<Vec<PathBuf>>,

    /// The running Auto Tone batch and the centering it analyses with.
    autotone: autotone::AutoTone,

    /// Face analysis of grouped photos, and the eyes-closed filter it feeds.
    faces: faces::Faces,
    selection_on: bool,
    /// Highlight the background instead of the subject.
    selection_invert: bool,
    /// Subject mask for the photo in the Loupe, tagged with its path so a stale
    /// result can be dropped. Never saved to the catalog. Only the photo on
    /// screen gets one, since segmentation is too heavy to run per folder.
    current_selection: Option<(PathBuf, crate::segmentation::Mask)>,
    selection_pending: Option<PathBuf>,
    selection_tx: Sender<SelectionOutcome>,
    selection_rx: Receiver<SelectionOutcome>,

    /// Zoom as a multiple of the fit scale (`1.0` is fitted). Fit-relative so
    /// swapping decode tiers leaves the on-screen transform unchanged.
    zoom_rel: f32,
    pub(crate) pan: (f32, f32), // screen-space pixel coords of the image's top-left corner
    pub(crate) win_size: (f32, f32),
    /// True while the view is auto-fit, so a resize re-fits.
    pub(crate) fitted: bool,
    /// Per-image rotation, in 90° clockwise steps (0..=3).
    rotations: HashMap<PathBuf, u8>,
    /// The viewport rect (physical px) the loupe drew into last frame.
    loupe_viewport: Option<(u32, u32, u32, u32)>,
    crop_edit: Option<CropDraft>,
    /// Kept across crops, like Lightroom's overlay choice.
    crop_overlay: CropOverlay,
    /// Before/after split. The left half keeps crop and rotation but no tone edits.
    compare: bool,
    /// Camera and exposure metadata for the info panel. Memory only, read from
    /// the file as each image is shown.
    exif_cache: HashMap<PathBuf, image_decode::ImageMetadata>,

    /// True pixel size (display orientation) of the photo in `want`. All loupe
    /// zoom, pan, and crop math uses this, not the uploaded texture's size, so
    /// "100%" means the same thing across thumbnail, preview, and full tiers.
    /// Read from image properties by `on_exif_info`; `None` until then.
    source_size: Option<(u32, u32)>,

    pending_confirm: Option<PendingConfirm>,

    /// Copied tone settings (no crop) and the path they came from.
    copied_settings: Option<(PathBuf, Adjustments)>,

    /// The named-look library, global to the app rather than per folder.
    presets: crate::presets::PresetStore,
    /// Open name prompt: the edit buffer, and which preset it renames (`None`
    /// is a new one).
    preset_name_edit: Option<(String, Option<u64>)>,

    show_help: bool,
    show_settings: bool,
    left_tab: LeftTab,

    /// Toast message and when it was set.
    status: Option<(StatusKind, String, Instant)>,

    /// Redraw retries pause while the window is hidden or minimized.
    pub(crate) occluded: bool,
    /// When egui next wants a frame, from the last frame's repaint delay.
    /// `about_to_wait` redraws once it passes, so an animation (a modal's
    /// fade, a collapsing header) finishes with no input to wake the loop.
    pub(crate) repaint_at: Option<Instant>,

    /// Top of the folder tree: the opened folder, or an opened file's parent.
    folder_root: Option<PathBuf>,
    /// The folder shown in the grid, and also the tree's keyboard cursor.
    /// Moving in the tree loads the folder in the same step.
    folder_sel: Option<PathBuf>,
    expanded: HashSet<PathBuf>,
    /// Immediate subdirectories, listed once per folder on demand.
    subdirs: HashMap<PathBuf, Vec<PathBuf>>,

    /// The region that receives arrow and Enter keys.
    focus: Region,
    /// The last focused main-chain region. F6 and Escape return here from chrome.
    main_focus: Region,
    focus_level: FocusLevel,
    /// Keyboard-focused Develop slider, an index into `develop::SLIDERS`.
    develop_focus: usize,
    toolbar_focus: usize,

    pub(crate) cursor: (f64, f64),
    pub(crate) modifiers: ModifiersState,
    pub(crate) space_down: bool,
    /// Whether the current Space hold started a drag, so its release is not a tap.
    pub(crate) space_panned: bool,
    pub(crate) dragging: bool,
    pub(crate) last_drag: (f64, f64),

    pub(crate) egui_ctx: egui::Context,
    pub(crate) egui_state: Option<egui_winit::State>,
}

mod accessors;
mod adjust;
mod autotone;
mod bulk_delete;
mod bursts;
mod catalog;
mod crop;
pub(crate) use accessors::FlagCoverage;
pub(crate) use catalog::flag_name;
pub(crate) use crop::{CropAspect, CropOrientation, CropOverlay};
use group_compare::GroupPicks;
pub(crate) use group_compare::{compare_zoom_uv, GroupView, PickHow, Square, Tile, TileFidelity};
mod export;
mod faces;
mod fonts;
mod group_compare;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) use export::ImmichLink;
mod histogram;
mod keys;
mod loupe;
#[cfg(target_os = "macos")]
mod menu;
mod nav;
mod presets;
mod score;
mod session;
#[cfg(test)]
pub(crate) mod test_support;
mod thumbs;
#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub(crate) use web::browser_is_mac;

#[cfg(all(feature = "hotpath", not(target_arch = "wasm32")))]
pub(crate) use thumbs::{grid_working_range, load_order, strip_working_range};

impl App {
    pub(crate) fn new(initial: Option<PathBuf>) -> Self {
        let catalog = Catalog::new();
        let egui_ctx = egui::Context::default();
        fonts::configure(&egui_ctx, None);
        crate::ui::font_size::init(&egui_ctx);
        crate::ui::theme::init(&egui_ctx);
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
        let (web_paste_tx, web_paste_rx) = std::sync::mpsc::channel();
        Self {
            window: None,
            renderer: None,
            #[cfg(test)]
            pushed_adj: None,
            loader: None,
            score_pool: None,
            score_job: None,
            exports: export::Exports::new(),
            bulk_delete: None,
            playlist: None,
            want: None,
            shown: Shown::Nothing,
            pending_initial: initial,
            // A test must never read the developer's own session.
            #[cfg(test)]
            session: None,
            #[cfg(not(test))]
            session: session::Session::load(),
            #[cfg(target_arch = "wasm32")]
            web_session_restore: None,
            #[cfg(target_arch = "wasm32")]
            web_folder_pending: false,
            #[cfg(target_arch = "wasm32")]
            web_full_cjk_requested: Default::default(),
            #[cfg(target_arch = "wasm32")]
            web_folder_tx,
            #[cfg(target_arch = "wasm32")]
            web_folder_rx,
            #[cfg(target_arch = "wasm32")]
            web_file_handles: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_dir_handles: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_thumb_cleanup: HashMap::new(),
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
            web_paste_tx,
            #[cfg(target_arch = "wasm32")]
            web_paste_rx,
            #[cfg(target_arch = "wasm32")]
            web_pending_nav: None,
            #[cfg(target_arch = "wasm32")]
            web_nav_generation: 0,
            #[cfg(target_arch = "wasm32")]
            web_pending_nav_generation: 0,
            #[cfg(target_arch = "wasm32")]
            web_handle_generation: 0,
            #[cfg(target_arch = "wasm32")]
            web_thumb_inflight: HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_thumb_recovery_pending: Vec::new(),
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
            web_exports: Default::default(),
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
            tool: LoupeTool::None,
            // Clamped up to TOUCHUP_MIN_PIXELS once an image is loaded.
            touchup_radius: 0.001,
            touchup_feather: TOUCHUP_FEATHER,
            touchup_opacity: 1.0,
            touchup_selected: None,
            touchup_spots_hidden: false,
            touchup_undo: HashMap::new(),
            develop_open: true,
            develop_tab: DevelopTab::Sliders,
            hist: histogram::Histogram::new(),
            unsaved_edit: None,
            #[cfg(target_arch = "wasm32")]
            unsaved_edit_kind: "adjustment",
            filter: None,
            flag_filter: FlagFilter::All,
            compare_flag_filter: FlagFilter::NotRejected,
            filter_cmp: Cmp::Gte,
            grid_sort: GridSort::Name,
            visible: Vec::new(),
            shown_photos: 0,
            sel: None,
            selected: BTreeSet::new(),
            anchor: None,
            filmstrip_scroll_accum: 0.0,
            grid_cols: 1,
            grid_range: (0, 0),
            cell_rects: Vec::new(),
            grid_scroll_reset: true,
            strip_range: (0, 0),
            thumb_tex: HashMap::new(),
            signals: crate::signalcache::SignalCache::empty(),
            #[cfg(not(target_arch = "wasm32"))]
            signal_load_rx: None,
            capture_times: HashMap::new(),
            burst_scan: None,
            // A test must never read the developer's own settings.
            #[cfg(test)]
            autotone: autotone::AutoTone::new(Default::default()),
            #[cfg(not(test))]
            autotone: autotone::AutoTone::new(crate::autotone::Centering::load()),
            faces: faces::Faces::new(),
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
            zoom_rel: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            rotations: HashMap::new(),
            loupe_viewport: None,
            compare_tiles: None,
            compare_center: egui::pos2(0.5, 0.5),
            compare_marker_hovered: false,
            compare_photo_hovered: false,
            compare_side: std::env::var("LIGHTPHOTOS_COMPARE_SIDE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.15),
            group_view: GroupView::default(),
            tile_fidelity: TileFidelity::default(),
            picks: GroupPicks::default(),
            crop_edit: None,
            crop_overlay: CropOverlay::default(),
            compare: false,
            exif_cache: HashMap::new(),
            source_size: None,
            pending_confirm: None,
            copied_settings: None,
            // A test must never write the developer's own preset library.
            #[cfg(test)]
            presets: crate::presets::PresetStore::in_memory(),
            #[cfg(not(test))]
            presets: crate::presets::PresetStore::load(),
            preset_name_edit: None,
            show_help: false,
            show_settings: false,
            left_tab: LeftTab::Folders,
            status: None,
            occluded: false,
            repaint_at: None,
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
            space_panned: false,
            dragging: false,
            last_drag: (0.0, 0.0),
            egui_ctx,
            egui_state: None,
        }
    }

    /// Open a directory in the Grid with nothing selected, or a file in the
    /// Loupe with its folder as the playlist.
    pub(crate) fn open(&mut self, path: PathBuf) {
        self.teardown_loupe_state();
        #[cfg(not(target_arch = "wasm32"))]
        let is_dir = std::fs::metadata(&path)
            .map(|m| m.is_dir())
            .unwrap_or(false);
        #[cfg(target_arch = "wasm32")]
        let is_dir = self.web_dir_handles.contains_key(&path) || self.subdirs.contains_key(&path);
        eprintln!(
            "[lightphotos] open {}: {}",
            if is_dir { "dir" } else { "file" },
            path.display()
        );

        if is_dir {
            self.folder_root = Some(path.clone());
            self.expanded = HashSet::from([path.clone()]);
            self.ensure_subdirs(&path);
            #[cfg(not(target_arch = "wasm32"))]
            self.load_folder(path);
            #[cfg(target_arch = "wasm32")]
            {
                // Browser paths are synthetic, backed by directory handles
                // whose listing is async, so `Playlist::from_dir` can't run.
                if !self.subdirs.contains_key(&path) {
                    self.supersede_web_pending_nav();
                    self.defer_web_nav(WebPendingNav::Load(path.clone()));
                    self.request_dir_listing(&path);
                    self.mode = ViewMode::Grid;
                    self.normalize_focus();
                    self.request_redraw();
                    return;
                }
                self.apply_web_load_folder(path);
            }
            self.mode = ViewMode::Grid;
            self.normalize_focus();
            self.request_redraw();
        } else {
            // Root the tree at the parent so the Grid has a sidebar.
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
            let start = playlist.entry(start_index).map(Path::to_path_buf);
            self.playlist = Some(playlist);
            self.reset_eyes_filter();
            self.recompute_visible();
            self.sel = match self.place_of(start_index) {
                nav::Place::Cell(p) => Some(p),
                nav::Place::Hidden(_) => {
                    self.want = start;
                    None
                }
                nav::Place::Gone => Some(0),
            };
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

    /// Open a folder picker and load the choice. On the web the picker is
    /// async and `poll_folder_pick` receives the result. Native uses a modal dialog.
    fn open_folder_picker(&mut self) {
        #[cfg(target_arch = "wasm32")]
        self.request_folder_pick();
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = crate::dialog::pick_folder() {
            self.open(path);
        }
    }

    /// Leave any Loupe editing mode. Call before navigating, so input isn't
    /// captured by a mode meant for the previous image.
    fn teardown_loupe_state(&mut self) {
        if let Some(draft) = self.crop_edit.take() {
            self.develop_tab = draft.return_tab;
        }
        self.tool = LoupeTool::None;
        self.touchup_selected = None;
    }

    /// List `dir`'s subfolders into `subdirs` if not cached. Async on wasm32.
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

    /// Start loading `playlist`'s saved ratings, edits, and rotations in the
    /// background. `poll_catalog_load` copies them in when they land, so first
    /// paint never waits on the sidecars.
    fn seed_mirrors(&mut self, playlist: &Playlist) {
        // Finish pending edits before replacing the catalog they write to.
        self.cancel_auto_tone();
        self.cancel_scoring();
        self.cancel_delete();
        self.save_edit();
        self.adopt_signal_cache(playlist);
        self.request_catalog_load(playlist.dir());
    }

    /// Show `dir`'s images in the grid with nothing selected.
    #[cfg(not(target_arch = "wasm32"))]
    fn load_folder(&mut self, dir: PathBuf) {
        self.load_playlist(Playlist::from_dir(&dir), dir);
    }

    /// `load_folder` for an already-built playlist. The web build builds its
    /// playlist from directory handles, since it can't call `read_dir`.
    fn load_playlist(&mut self, playlist: Playlist, dir: PathBuf) {
        #[cfg(target_arch = "wasm32")]
        crate::analytics::folder_opened(playlist.entries().len());
        self.teardown_loupe_state();
        self.burst_scan = None;
        self.seed_mirrors(&playlist);
        self.playlist = Some(playlist);
        self.reset_eyes_filter();
        self.recompute_visible();
        self.sel = None;
        self.selected.clear();
        self.anchor = None;
        self.folder_sel = Some(dir);
        // `redraw` syncs textures against `grid_range` before layout updates
        // it. A range left from the old folder's scroll depth would drop the
        // textures actually on screen for a frame, so start at the top.
        self.grid_range = (0, 0);
        self.grid_scroll_reset = true;
        self.request_working_thumbs();
        self.request_redraw();
    }

    /// Paint one frame: run egui for the chrome, then submit the loupe image
    /// and egui paint jobs to the renderer in one wgpu submission.
    pub(crate) fn redraw(&mut self) {
        // Ahead of every path that can reach `present()`, including the
        // early return below: `presented` reads this frame's flag, and a
        // frame that never cleared it would report the last frame's photo.
        #[cfg(target_arch = "wasm32")]
        crate::analytics::begin_frame();
        // Upload thumbnails before egui references them.
        self.sync_thumb_textures();
        self.sync_compare_tiles();

        if self.hist.is_dirty() && self.develop_open {
            self.recompute_histogram();
        }

        // Both readers dedupe in-flight requests, so asking every frame is cheap.
        if let Some(path) = self.metadata_to_read() {
            #[cfg(not(target_arch = "wasm32"))]
            if let Some(loader) = &mut self.loader {
                loader.request_exif(path);
            }
            #[cfg(target_arch = "wasm32")]
            self.request_web_exif(path);
        }

        let (Some(window), Some(mut state)) = (self.window.clone(), self.egui_state.take()) else {
            if let Some(r) = &mut self.renderer {
                r.render(None, None, None);
            }
            return;
        };

        let mut raw_input = state.take_egui_input(&*window);
        // Drop Tab before egui sees it. Otherwise egui moves its own widget
        // focus and draws a focus ring, while Tab is already how the keyboard
        // moves between the controls of the focused region. The preset name
        // prompt is the only text field, it has one field and nothing to Tab
        // between, and it takes focus itself on the frame it opens.
        raw_input.events.retain(|e| {
            !matches!(
                e,
                egui::Event::Key {
                    key: egui::Key::Tab,
                    ..
                }
            )
        });
        // The web build has no system clipboard in egui_winit, so its paste
        // carries only text copied inside the app. The browser's clipboard
        // arrives through `request_web_paste` instead.
        #[cfg(target_arch = "wasm32")]
        {
            raw_input
                .events
                .retain(|e| !matches!(e, egui::Event::Paste(_)));
            raw_input.events.extend(
                std::iter::from_fn(|| self.web_paste_rx.try_recv().ok()).map(egui::Event::Paste),
            );
        }

        let mut out = ui::FrameOutput::default();
        let full_output = self.egui_ctx.clone().run_ui(raw_input, |ui| {
            out = ui::draw(ui, self);
        });

        self.repaint_at = full_output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .and_then(|v| Instant::now().checked_add(v.repaint_delay));

        #[cfg(target_arch = "wasm32")]
        web::write_web_clipboard(&full_output.platform_output.commands);
        state.handle_platform_output(&*window, full_output.platform_output);
        self.egui_state = Some(state);

        self.apply_ui_actions(out.actions);
        self.save_edit_unless_dragging();

        // Show catalog write failures, or the user loses the change silently.
        if let Some(cause) = self.catalog.take_error() {
            self.set_status(
                StatusKind::Error,
                (crate::i18n::t().catalog_save_failed)(&cause),
            );
        }
        if let Some(message) = self.presets.take_error() {
            self.set_status(StatusKind::Error, message);
        }

        let pixels_per_point = self.egui_ctx.pixels_per_point();
        let paint_jobs = self
            .egui_ctx
            .tessellate(full_output.shapes, pixels_per_point);

        // egui sets scissor rects from this size, so it has to be the
        // surface's own size: a scissor one pixel past it is a WebGPU
        // validation error that drops the whole frame.
        let size = match &self.renderer {
            Some(r) => r.surface_size(),
            None => {
                let s = window.inner_size();
                [s.width, s.height]
            }
        };
        let screen_descriptor = egui_wgpu::ScreenDescriptor {
            size_in_pixels: [size[0].max(1), size[1].max(1)],
            pixels_per_point,
        };

        // In Loupe, draw the image into egui's central rect, in physical pixels.
        let image_viewport = match self.mode {
            ViewMode::Loupe => out.loupe_rect.map(|r| {
                let x = (r.min.x * pixels_per_point).round().max(0.0) as u32;
                let y = (r.min.y * pixels_per_point).round().max(0.0) as u32;
                let w = (r.width() * pixels_per_point).round().max(0.0) as u32;
                let h = (r.height() * pixels_per_point).round().max(0.0) as u32;
                (x, y, w, h)
            }),
            ViewMode::Grid => Some((0, 0, 0, 0)),
        };

        if self.mode == ViewMode::Loupe {
            if image_viewport != self.loupe_viewport {
                if self.fitted {
                    self.loupe_viewport = image_viewport;
                    if self.crop_edit.is_some() {
                        self.fit_for_crop();
                    } else {
                        self.fit_to_window();
                    }
                } else {
                    // Manually zoomed: a resize should reveal more or less of
                    // the image, not rescale it. Keep absolute `zoom()` fixed.
                    let keep = self.zoom();
                    self.loupe_viewport = image_viewport;
                    let fs = self.fit_scale();
                    if fs > 0.0 {
                        self.zoom_rel = keep / fs;
                    }
                    self.push_transform();
                }
            }
        }

        // Compare mode draws the image twice in equal halves. An odd pixel
        // becomes a divider so both halves share one transform size.
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

        // While the web build decodes, draw nothing so the previous photo
        // doesn't show through. This overrides `primary_vp`, not
        // `image_viewport`, so the viewport change check above doesn't re-fit.
        // A zero-size viewport skips the draw and leaves the texture untouched.
        if self.loupe_is_loading() {
            primary_vp = Some((0, 0, 0, 0));
            compare_vp = None;
        }

        let loupe_bg = ui::theme::colors(&self.egui_ctx).loupe_bg;
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        renderer.set_clear_color(loupe_bg);
        let egui_paint = EguiPaint {
            textures_delta: full_output.textures_delta,
            paint_jobs,
            screen_descriptor,
        };
        let presented = renderer.render(primary_vp, compare_vp, Some(egui_paint));
        // Retry an unpresentable surface so it draws once revealed. If winit
        // reports it occluded, wait for Occluded(false) instead of spinning.
        if !presented && !self.occluded {
            self.request_redraw();
        }
    }

    /// Upload a mipped texture for each member of the shown photo's
    /// group, the shown photo among them, requesting previews as needed. A
    /// photo in no group, or the Edit view, leaves `compare_tiles` empty, which keeps
    /// the Loupe whole.
    fn sync_compare_tiles(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        if self.group_view == GroupView::Edit {
            if let (Some(tiles), Some(r)) = (self.compare_tiles.take(), self.renderer.as_mut()) {
                tiles.free(r);
            }
            return;
        }
        // `LIGHTPHOTOS_COMPARE_PX` caps the tile textures below the Loupe's size.
        let px = std::env::var("LIGHTPHOTOS_COMPARE_PX")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| self.preview_px());
        let square = self.compare_square();
        let (Some(shown), Some(pl)) = (self.selected_path(), self.playlist.as_ref()) else {
            return;
        };
        let group = shown
            .file_name()
            .zip(self.catalog.groups())
            .and_then(|(name, gs)| gs.get(gs.group_of(name)?));
        let paths: Vec<PathBuf> = group
            .map(|g| g.members().iter().map(|m| pl.dir().join(m)).collect())
            .unwrap_or_default();
        let order = self.compare_order(paths.clone());
        let (Some(loader), Some(r)) = (self.loader.as_mut(), self.renderer.as_mut()) else {
            return;
        };
        if self
            .compare_tiles
            .as_ref()
            .is_some_and(|t| t.key != (paths.clone(), px))
        {
            self.compare_tiles.take().unwrap().free(r);
            self.picks = GroupPicks::default();
        }
        if paths.is_empty() {
            return;
        }
        let tiles = self.compare_tiles.get_or_insert_with(|| {
            let page = order.iter().position(|p| *p == shown).unwrap_or(0) / COMPARE_PAGE;
            CompareTiles {
                key: (paths.clone(), px),
                order: order.clone(),
                shown: shown.clone(),
                page,
                members: Vec::new(),
                loaded_page: usize::MAX,
                sizes: HashMap::new(),
                square,
                square_moved: Instant::now(),
                wake_at: None,
                opened: Instant::now(),
                bytes: 0,
                upload_ms: 0.0,
                reported: false,
            }
        });
        tiles.shown = shown;
        if tiles.order != order {
            // A score landed or the flag filter changed: move the page's
            // tiles to their new slots rather than upload them again. A
            // shorter order can leave the page past its end.
            tiles.order = order;
            tiles.page = tiles.page.min(tiles.pages().saturating_sub(1));
            if tiles.loaded_page == tiles.page {
                let mut old: Vec<Tile> = tiles.members.drain(..).flatten().collect();
                let slots = tiles
                    .page_paths()
                    .iter()
                    .map(|p| {
                        let at = old.iter().position(|t| t.path == *p)?;
                        Some(old.swap_remove(at))
                    })
                    .collect();
                tiles.members = slots;
                for t in old {
                    t.textures().for_each(|id| r.free_thumb(id));
                }
            }
        }
        if tiles.loaded_page != tiles.page {
            for t in tiles.members.drain(..).flatten() {
                t.textures().for_each(|id| r.free_thumb(id));
            }
            tiles.members = (0..tiles.page_paths().len()).map(|_| None).collect();
            tiles.loaded_page = tiles.page;
        }
        let page_paths = tiles.page_paths().to_vec();
        for (slot, path) in tiles.members.iter_mut().zip(&page_paths) {
            if slot.is_some() {
                continue;
            }
            let Some(img) = loader.get_preview(path, px) else {
                // On wasm32 `request_web_preview` requests these.
                #[cfg(not(target_arch = "wasm32"))]
                loader.request_preview(path.clone(), px);
                continue;
            };
            let t = Instant::now();
            if let Some(id) = r.upload_egui_image(&img, true) {
                tiles.upload_ms += t.elapsed().as_secs_f32() * 1000.0;
                // Level 0 plus a full mip chain is 4/3 of it.
                tiles.bytes += img.width as u64 * img.height as u64 * 4 * 4 / 3;
                tiles.sizes.insert(path.clone(), (img.width, img.height));
                *slot = Some(Tile::new(path.clone(), id, img.width, img.height));
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        match self.tile_fidelity {
            TileFidelity::Full => group_compare::sync_full_crops(tiles, square, loader, r),
            TileFidelity::Speed => group_compare::drop_full_crops(tiles, r),
        }
        if !tiles.reported && tiles.members.iter().all(Option::is_some) {
            tiles.reported = true;
            eprintln!(
                "compare: {} tiles at preview {px}px drawn {:.0} ms after open, upload cpu {:.1} ms, gpu {:.0} MB",
                tiles.members.len(),
                tiles.opened.elapsed().as_secs_f32() * 1000.0,
                tiles.upload_ms,
                tiles.bytes as f64 / 1e6
            );
        }
    }

    /// When a Full re-crop is waiting on the zoom square to settle, the
    /// moment it may start. No decode is in flight to wake the frame loop
    /// then, so `pump` polls until it passes and redraws.
    pub(crate) fn compare_wake_at(&self) -> Option<Instant> {
        self.compare_tiles.as_ref()?.wake_at
    }

    fn apply_ui_actions(&mut self, actions: Vec<ui::UiAction>) {
        for action in actions {
            match action {
                ui::UiAction::GroupAllBursts => self.group_all_bursts(),
                ui::UiAction::ScoreAll => self.score_all(),
                ui::UiAction::ComparePage(page) => {
                    if let Some(t) = self.compare_tiles.as_mut() {
                        t.page = page.min(t.pages().saturating_sub(1));
                    }
                }
                ui::UiAction::SetTileFidelity(f) => self.set_tile_fidelity(f),
                ui::UiAction::PickGroupTile { path, how } => self.pick_group_tile(path, how),
                ui::UiAction::RateGroupMember { path, stars } => {
                    self.rate_group_member(path, stars)
                }
                ui::UiAction::FlagGroupMember { path, flag } => self.flag_group_member(path, flag),
                ui::UiAction::SetCompareCenter(center) => {
                    self.compare_center = center;
                    self.request_redraw();
                }
                ui::UiAction::SetMemberAsRep(path) => self.set_member_as_rep(path),
                ui::UiAction::DeleteMember(path) => self.request_delete_member(path),
                ui::UiAction::RequestDeletePicks => self.request_delete_picks(),
                ui::UiAction::Select(pos) => {
                    if pos < self.visible.len() {
                        self.select_single(pos);
                        self.show_sel();
                    }
                }
                ui::UiAction::SelectToggle(pos) => {
                    // The Loupe always shows a photo, so it keeps the last one.
                    let last = self.mode == ViewMode::Loupe && self.selected_cells() == [pos];
                    if pos < self.visible.len() && !last {
                        self.select_toggle(pos);
                        self.show_sel();
                    }
                }
                ui::UiAction::SelectRange(pos) => {
                    if pos < self.visible.len() {
                        self.select_range(pos);
                        self.show_sel();
                    }
                }
                ui::UiAction::CopySettings => self.copy_settings(),
                ui::UiAction::SavePresetPrompt => self.prompt_save_preset(),
                ui::UiAction::RenamePresetPrompt(id) => self.prompt_rename_preset(id),
                ui::UiAction::SetPresetNameText(text) => self.set_preset_name_text(text),
                ui::UiAction::CommitPresetName => self.commit_preset_name(),
                ui::UiAction::CancelPresetName => self.cancel_preset_name(),
                ui::UiAction::ApplyPreset(id) => self.apply_preset(id),
                ui::UiAction::RequestDeletePreset(id) => {
                    self.pending_confirm = Some(PendingConfirm::DeletePreset(id));
                    self.request_redraw();
                }
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::ImportLrPresets => self.import_lr_presets(),
                ui::UiAction::SetLeftTab(tab) => self.set_left_tab(tab),
                ui::UiAction::ToggleHelp => {
                    self.show_help = !self.show_help;
                    self.request_redraw();
                }
                ui::UiAction::ToggleExportForm => self.toggle_export_form(),
                ui::UiAction::SetExportSettings(settings) => self.set_export_settings(settings),
                ui::UiAction::RunExport => self.run_export_form(),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::ChooseExportFolder => self.choose_export_folder(),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::SetImmichUrl(url) => self.set_immich_fields(Some(url), None),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::SetImmichKey(key) => self.set_immich_fields(None, Some(key)),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::ConnectImmich => self.connect_immich(),
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::DisconnectImmich => self.disconnect_immich(),
                ui::UiAction::RequestBulk(kind) => self.request_bulk(kind),
                ui::UiAction::ScoreSelection => self.score_selection(),
                ui::UiAction::CancelScoring => self.cancel_scoring(),
                ui::UiAction::SetSort(sort) => self.set_sort(sort),
                ui::UiAction::SetZoom(zoom) => self.set_zoom(zoom),
                ui::UiAction::ConfirmPending => self.confirm_pending(),
                ui::UiAction::CancelPending => self.cancel_pending(),
                ui::UiAction::RemoveGroups => self.remove_selected_groups(),
                ui::UiAction::TrashGroups => self.trash_selected_groups(),
                ui::UiAction::OpenLoupe(pos) => {
                    if pos < self.visible.len() {
                        self.select_single(pos);
                        self.enter_loupe();
                    }
                }
                ui::UiAction::SetFilter(f) => self.set_filter(f),
                ui::UiAction::SetFilterCmp(cmp) => self.set_filter_cmp(cmp),
                ui::UiAction::SetRating(stars) => self.set_rating(stars),
                ui::UiAction::SetFlag(flag) => self.set_flag(flag),
                ui::UiAction::SetFlagFilter(f) => self.set_flag_filter(f),
                ui::UiAction::SetCompareFlagFilter(f) => self.set_compare_flag_filter(f),
                ui::UiAction::ScrollFilmstrip(delta) => self.scroll_filmstrip(delta),
                ui::UiAction::ToggleEyesClosed => self.toggle_eyes_filter(),
                ui::UiAction::ToggleSelection => self.toggle_selection(),
                ui::UiAction::ToggleSelectionInvert => self.toggle_selection_invert(),
                ui::UiAction::OpenFolder(p) => {
                    // A click does what Enter does: focus, load, toggle expansion.
                    self.set_focus(Region::Folders, FocusLevel::Entered);
                    self.open_folder(p);
                }
                ui::UiAction::PickFolder => self.open_folder_picker(),
                ui::UiAction::ReopenSession => self.reopen_session(),
                ui::UiAction::EnterGrid => self.enter_grid(),
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
                ui::UiAction::SetLanguage(lang) => {
                    crate::i18n::choose(lang);
                    self.update_window_title();
                    self.request_redraw();
                }
                ui::UiAction::SetTheme(theme) => {
                    ui::theme::set(&self.egui_ctx, theme);
                    self.request_redraw();
                }
                ui::UiAction::SetAutoToneCentering(centering) => {
                    self.autotone.set_centering(centering);
                    centering.save();
                    self.request_redraw();
                }
                ui::UiAction::ToggleSettings => {
                    self.show_settings = !self.show_settings;
                    self.request_redraw();
                }
                ui::UiAction::CloseSettings => {
                    self.show_settings = false;
                    self.request_redraw();
                }
                ui::UiAction::CropGrab(edge) => self.crop_grab(edge),
                ui::UiAction::CropGrabMove(u, v) => self.crop_grab_move(u, v),
                ui::UiAction::CropDragTo(u, v) => self.crop_drag_to(u, v),
                ui::UiAction::CropRelease => self.crop_release(),
                ui::UiAction::ToggleStraightenTool => self.toggle_straighten_tool(),
                ui::UiAction::StraightenLineFrom(u, v) => self.straighten_line_from(u, v),
                ui::UiAction::StraightenLineTo(u, v) => self.straighten_line_to(u, v),
                ui::UiAction::ResetStraighten => self.reset_straighten(),
                ui::UiAction::ResetCrop => self.reset_crop(),
                ui::UiAction::SetCropAspect(aspect) => self.set_crop_aspect(aspect),
                ui::UiAction::SetCropOverlay(overlay) => self.set_crop_overlay(overlay),
                ui::UiAction::SetCropOrientation(o) => self.set_crop_orientation(o),
                ui::UiAction::Rotate(cw) => self.rotate(cw),
                ui::UiAction::ToggleWbPicker => self.toggle_wb_picker(),
                ui::UiAction::PickWhiteBalance(u, v) => self.pick_white_balance(u, v),
                ui::UiAction::ToggleTouchUp => self.toggle_touchup(),
                ui::UiAction::ClickRail(item) => self.click_rail(item),
                ui::UiAction::SetTouchUpRadius(r) => {
                    self.set_touchup_radius(r);
                    self.request_redraw();
                }
                ui::UiAction::SetTouchUpFeather(f) => {
                    self.set_touchup_feather(f);
                    self.request_redraw();
                }
                ui::UiAction::SetTouchUpOpacity(o) => {
                    self.set_touchup_opacity(o);
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
                ui::UiAction::SetAdjustments(adj) => self.apply_adjustments(adj),
                ui::UiAction::AutoTone => self.auto_tone_shown(),
                ui::UiAction::ResetAdjustments => {
                    let Some(path) = self.shown.path().map(Path::to_path_buf) else {
                        continue;
                    };
                    #[cfg(target_arch = "wasm32")]
                    if !self.current_adjustments().is_identity()
                        || !self.current_touchups().is_empty()
                    {
                        crate::analytics::property("develop_edit_applied", "edit_kind", "reset");
                    }
                    self.edits.remove(&path);
                    self.touchups.remove(&path);
                    self.catalog.set_adjustments(&path, &Adjustments::default());
                    self.catalog.set_touchups(&path, &[]);
                    self.touchup_selected = None;
                    self.push_adjustments();
                    self.hist.invalidate();
                    self.request_redraw();
                }
            }
        }
    }
}

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

fn file_label(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

/// Inclusive positions between `anchor` and `pos`, in either order.
fn range_set(anchor: usize, pos: usize) -> BTreeSet<usize> {
    let (lo, hi) = if anchor <= pos {
        (anchor, pos)
    } else {
        (pos, anchor)
    };
    (lo..=hi).collect()
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
}

/// `LIGHTPHOTOS_COMPARE_PANE=paint` draws the pane with a bare painter
/// as the first version did; anything else allocates it inside a ScrollArea.
pub(crate) fn compare_claims_pane() -> bool {
    std::env::var("LIGHTPHOTOS_COMPARE_PANE").as_deref() != Ok("paint")
}

pub(crate) struct CompareTiles {
    /// Every member of the group, and the preview size the textures were made at.
    key: (Vec<PathBuf>, u32),
    /// The members as the pane lays them out, highest score first
    /// (`App::by_score`). Pages cut this, not `key`, so a score landing
    /// reorders the tiles without reloading them.
    order: Vec<PathBuf>,
    /// The member the Loupe shows, which the marker and the tile outline follow.
    pub(crate) shown: PathBuf,
    /// Which `COMPARE_PAGE`-sized run of the members the pane shows. It starts
    /// on the shown photo's run, but paging away leaves the representative
    /// with no tile.
    pub(crate) page: usize,
    /// One slot per member of `page_paths`, filled once its texture uploads.
    pub(crate) members: Vec<Option<Tile>>,
    /// The page `members` holds, so a page turn frees the old textures.
    loaded_page: usize,
    /// Pixel size of every member uploaded so far, which keeps the marker on
    /// the shown photo when its tile is on another page.
    pub(crate) sizes: HashMap<PathBuf, (u32, u32)>,
    /// The zoom square as of the last sync, and when it last changed, which
    /// Full waits on before cutting new crops.
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    square: Square,
    #[cfg_attr(target_arch = "wasm32", allow(dead_code))]
    square_moved: Instant,
    wake_at: Option<Instant>,
    opened: Instant,
    bytes: u64,
    upload_ms: f32,
    reported: bool,
}

/// Tiles per page. The web build keeps fewer textures and decodes.
#[cfg(target_arch = "wasm32")]
pub(crate) const COMPARE_PAGE: usize = 4;
#[cfg(not(target_arch = "wasm32"))]
pub(crate) const COMPARE_PAGE: usize = 9;

impl CompareTiles {
    fn free(self, r: &mut Renderer) {
        for t in self.members.into_iter().flatten() {
            t.textures().for_each(|id| r.free_thumb(id));
        }
    }

    /// The members the pane lays out, after its flag filter.
    pub(crate) fn group_len(&self) -> usize {
        self.order.len()
    }

    pub(crate) fn pages(&self) -> usize {
        self.group_len().div_ceil(COMPARE_PAGE)
    }

    /// The members on `page`, the last page possibly short.
    pub(crate) fn page_paths(&self) -> &[PathBuf] {
        let start = (self.page * COMPARE_PAGE).min(self.group_len());
        &self.order[start..(start + COMPARE_PAGE).min(self.group_len())]
    }
}
