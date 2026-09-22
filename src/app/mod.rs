// SPDX-License-Identifier: GPL-3.0-or-later

//! The `App`: all viewer state, plus the logic that ties together the GPU
//! renderer, the background loader, the catalog, the egui chrome, and the
//! keyboard bindings. `main.rs` runs the winit event loop and calls into the
//! `pub(crate)` methods and fields here.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender};
use std::time::SystemTime;
// std::time::Instant panics on wasm32, which has no OS clock.
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

/// Side of a grid cell, in egui points. Smaller than
/// [`crate::thumbnail::THUMB_PX`] so HiDPI displays get real pixels to draw.
pub(crate) const GRID_CELL_PT: f32 = 192.0;

/// Whether the Grid toolbar shows the Bursts / Duplicates / Eyes-closed
/// buttons. The `B` and `D` keys work either way. Both
/// `ui::toolbar::grid_toolbar` and `App::TOOLBAR_CONTROLS` read this flag, so
/// the drawn controls and the F6 focus cycle stay in agreement.
pub(crate) const SHOW_GROUPING_TOOLS: bool = false;

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
/// Fraction of the patch radius used to blend the correction into its edges.
const TOUCHUP_FEATHER: f32 = 1.0;

/// One font face's vertical metrics, in ems, already multiplied by the
/// `FontTweak::scale` epaint draws that face at.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
struct FaceMetrics {
    ascent: f32,
    /// Ascent + descent + line gap.
    row_height: f32,
}

#[cfg(not(target_arch = "wasm32"))]
fn face_metrics(bytes: &[u8], index: u32, scale: f32) -> Option<FaceMetrics> {
    use skrifa::MetadataProvider as _;
    let font = skrifa::FontRef::from_index(bytes, index).ok()?;
    let metrics = font.metrics(
        skrifa::instance::Size::unscaled(),
        skrifa::instance::LocationRef::default(),
    );
    let upem = metrics.units_per_em as f32;
    if upem <= 0.0 {
        return None;
    }
    let ascent = metrics.ascent / upem * scale;
    // Descent is negative (it points below the baseline), hence the subtraction.
    let descent = metrics.descent / upem * scale;
    let leading = metrics.leading / upem * scale;
    Some(FaceMetrics {
        ascent,
        row_height: ascent - descent + leading,
    })
}

/// The `FontTweak::y_offset_factor` that lands `face`'s baseline on `primary`'s.
///
/// egui takes a row's metrics from the family's first face, then places each
/// glyph at `face.ascent + (primary.row_height - face.row_height) / 2`. A
/// fallback face with a different ascent-to-line-height ratio sits off the
/// baseline. Hiragino's large line gap puts CJK text about a quarter em too
/// high. epaint multiplies the factor by the face's `scale`, so we divide it out.
#[cfg(not(target_arch = "wasm32"))]
fn baseline_correction(primary: FaceMetrics, face: FaceMetrics, scale: f32) -> f32 {
    let drawn_baseline = face.ascent + 0.5 * (primary.row_height - face.row_height);
    (primary.ascent - drawn_baseline) / scale
}

/// Load macOS system fonts at runtime so the binary doesn't embed a large
/// Unicode font. egui's built-in fonts stay as fallbacks. Every face gets a
/// baseline correction against the first, so mixed-script text sits on one line.
#[cfg(not(target_arch = "wasm32"))]
fn add_system_fonts(definitions: &mut egui::FontDefinitions) {
    let candidates = [
        ("macos-ui", "/System/Library/Fonts/SFNS.ttf", 0u32),
        // CJK coverage SFNS lacks. Index 0 of these TTCs is the regular face.
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
        let Some(metrics) = face_metrics(&bytes, index, 1.0) else {
            continue;
        };
        loaded.push((name.to_owned(), bytes, index, metrics));
    }

    // The first loaded face is the primary that every other face aligns to.
    let Some(primary) = loaded.first().map(|entry| entry.3) else {
        return;
    };

    let mut names = Vec::new();
    for (name, bytes, index, metrics) in loaded {
        definitions.font_data.insert(
            name.clone(),
            Arc::new(egui::FontData {
                font: std::borrow::Cow::Owned(bytes),
                index,
                tweak: egui::FontTweak {
                    y_offset_factor: baseline_correction(primary, metrics, 1.0),
                    ..Default::default()
                },
            }),
        );
        names.push(name);
    }

    // egui's built-in faces need the same correction against the new primary.
    // This runs before the system fonts join the family list, so it clones only
    // egui's static font data, not the megabytes just read from disk.
    let builtins = definitions
        .families
        .get(&egui::FontFamily::Proportional)
        .cloned()
        .unwrap_or_default();
    for name in builtins {
        let Some(data) = definitions.font_data.get(&name) else {
            continue;
        };
        let scale = data.tweak.scale;
        let Some(metrics) = face_metrics(data.font.as_ref(), data.index, scale) else {
            continue;
        };
        let mut data = (**data).clone();
        data.tweak.y_offset_factor = baseline_correction(primary, metrics, scale);
        definitions.font_data.insert(name, Arc::new(data));
    }

    if let Some(fonts) = definitions
        .families
        .get_mut(&egui::FontFamily::Proportional)
    {
        for name in names.iter().rev() {
            fonts.insert(0, name.clone());
        }
    }
    // Monospace keeps Hack first and uses these only for glyphs Hack lacks.
    // Prepending them would make monospace text proportional.
    if let Some(fonts) = definitions.families.get_mut(&egui::FontFamily::Monospace) {
        fonts.extend(names.iter().cloned());
    }
}

/// System fonts where the platform has them, then the Chinese UI glyphs as a
/// last resort. The browser can't read system fonts, and Linux and Windows
/// have no known font paths here, so without the bundled subset those builds
/// would draw Chinese as boxes.
fn configure_fonts(ctx: &egui::Context) {
    let mut definitions = egui::FontDefinitions::default();
    #[cfg(not(target_arch = "wasm32"))]
    add_system_fonts(&mut definitions);

    // Built by `scripts/subset-cjk-font.sh` from the CJK text in `i18n.rs`.
    const CJK: &str = "noto-sans-sc-ui-subset";
    definitions.font_data.insert(
        CJK.to_owned(),
        Arc::new(egui::FontData::from_static(include_bytes!(
            "../../assets/fonts/NotoSansSC-ui-subset.otf"
        ))),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        if let Some(fonts) = definitions.families.get_mut(&family) {
            fonts.push(CJK.to_owned());
        }
    }
    ctx.set_fonts(definitions);
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    Loupe,
    /// Side-by-side review of one duplicate group (`survey_members`), opened
    /// from a Grid badge. Uses `Region::Grid` for keyboard focus, since it is
    /// a modal screen over the Grid, not part of the F6 cycle.
    Survey,
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
/// normalized texture space and is committed to `Adjustments.crop` on exit.
/// While cropping, the GPU draws the full frame and egui draws the mask.
pub struct CropDraft {
    rect: Crop,
    /// `None` when no drag is in progress.
    grab: Option<CropGrab>,
    /// Pixel aspect ratio (w/h) captured at grab time, for Shift-lock.
    aspect: f32,
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
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Region {
    Toolbar,
    Folders,
    Grid,
    Detail,
    Filmstrip,
    Develop,
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
}

/// A finished background sidecar load: its directory, the navigation token it
/// was requested under, its place in the sidecar write order, and the records.
pub(crate) type CatalogLoadResult = (
    PathBuf,
    u64,
    crate::catalog::LoadMark,
    crate::catalog::SidecarLoad,
);

pub(crate) struct App {
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) renderer: Option<Renderer>,
    /// Last adjustments handed to the GPU. Tests run without a renderer, so
    /// this is the only way to assert what the loupe would actually show.
    #[cfg(test)]
    pub(crate) pushed_adj: Option<Adjustments>,
    pub(crate) loader: Option<Loader>,
    /// `None` until the window is created.
    pub(crate) exporter: Option<Exporter>,
    pub(crate) feature_pool: Option<crate::featureprint::DistancePool>,
    pub(crate) face_pool: Option<crate::facequality::FacePool>,
    pub(crate) export_progress: Option<ExportProgress>,
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

    /// True while `showDirectoryPicker` and its listing are in flight. Disables
    /// the landing page's "Choose Folder" button so a second picker can't open.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_folder_pending: bool,
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
    web_thumb_recovery_pending: Vec<crate::web_worker_pool::PoolResult>,
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
    /// deadline backs off per attempt (`retry_backoff`). Cleared on success or
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

    /// A hand-rolled `web_sys::Worker` pool for parallel decode.
    /// `wasm-bindgen-rayon` needs a JS-driven init that doesn't fit a binary
    /// crate, and nightly Rust. File bytes are still read on the main thread,
    /// since `FileSystemFileHandle` reads are async there, then sent to a worker.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_worker_pool: crate::web_worker_pool::WorkerPool,
    /// `Preview` and `Speed` results that `poll_web_thumbs` pulled off the
    /// pool's single shared channel. `poll_web_preview` consumes them in the
    /// same frame.
    #[cfg(target_arch = "wasm32")]
    web_preview_pending: Vec<crate::web_worker_pool::PoolResult>,
    /// `Full` results set aside the same way, for `poll_web_full`.
    #[cfg(target_arch = "wasm32")]
    web_full_pending: Vec<crate::web_worker_pool::PoolResult>,

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
    touchup_selected: Option<usize>,
    develop_open: bool,
    /// Small row-major grid (`hist_dw` x `hist_dh`) of the shown image in
    /// linear-light RGB, so the histogram recomputes cheaply as edits change.
    /// A real 2D grid so denoise can read neighbors and crop can drop cells.
    hist_sample: Vec<[f32; 3]>,
    /// Format of the image behind `hist_sample`. RAW linear samples need the
    /// RAW shader's sRGB transfer and preview boost.
    hist_pixel_format: image_decode::PixelFormat,
    hist_dw: usize,
    hist_dh: usize,
    /// Per-channel display-space histogram. Float bins: each sample splits
    /// across neighboring bins, so a tone stretch doesn't leave a comb of gaps.
    histogram: Option<[[f32; 256]; 3]>,
    hist_dirty: bool,
    /// Photo whose edit is not yet written to its sidecar. See
    /// `save_edit_unless_dragging`.
    unsaved_edit: Option<PathBuf>,
    #[cfg(target_arch = "wasm32")]
    unsaved_edit_kind: &'static str,
    /// Active star filter. `None` shows all.
    filter: Option<(Cmp, u8)>,
    /// Comparator used when a star level is clicked. Stays set across "All".
    filter_cmp: Cmp,
    /// Indices into `playlist.entries()` that pass the current filter.
    visible: Vec<usize>,
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
    /// The filmstrip's equivalent of `grid_range`.
    strip_range: (usize, usize),
    /// Thumbnail textures keyed by (path, THUMB_PX, edit signature). An edit
    /// changes the signature, so the stale texture drops and re-bakes. Pruned
    /// to the working set each frame.
    ///
    /// The renderer owns the GPU side, so dropping an entry here is not enough;
    /// `sync_thumb_textures` hands every pruned id back to `Renderer::free_thumb`.
    thumb_tex: HashMap<(PathBuf, u32, u64), ThumbTexture>,

    /// Burst badges and dimming. Mutually exclusive with the star filter.
    bursts_on: bool,
    /// Capture time per path, from EXIF or mtime. `Some(None)` means the read
    /// found no time, so it isn't requested again.
    capture_times: HashMap<PathBuf, Option<SystemTime>>,
    sharpness: HashMap<PathBuf, f64>,
    /// Indexed by playlist entry, not visible position. Empty when bursts are off.
    burst_marks: Vec<Option<BurstMark>>,

    /// Content-duplicate badges. Independent of bursts: a photo can be in both.
    dupes_on: bool,
    /// dHash per path, computed for the whole folder as thumbnails arrive.
    phashes: HashMap<PathBuf, u64>,
    /// dHash groups before feature-print refinement, by playlist entry.
    /// `request_feature_prints` reads each group's anchor and candidates here.
    dup_index: crate::duplicates::HashGroups,
    /// Groups after the feature-print split, by playlist entry. Survey Mode
    /// uses this to find a clicked badge's members, which `dup_marks` loses.
    dup_refined: Vec<u32>,
    /// Vision feature-print distance per (group anchor, member). Anchor-relative,
    /// not all pairs.
    feature_distances: HashMap<(PathBuf, PathBuf), f32>,
    /// Comparisons that failed for good, so they aren't resubmitted every frame.
    feature_failed: HashSet<(PathBuf, PathBuf)>,
    feature_pending: HashSet<(PathBuf, PathBuf)>,
    /// Indexed by playlist entry, not visible position. Empty when dupes are off.
    dup_marks: Vec<Option<DuplicateMark>>,

    /// Photos in the running Auto Tone batch still waiting on a thumbnail.
    /// Emptied by `cancel_auto_tone` on a folder change.
    ///
    /// The batch is paced across three stages so its memory never tracks the
    /// selection: this set is the whole outstanding batch, for progress and
    /// deduplication, and every photo in it sits in exactly one of
    /// `autotone_queue` or `autotone_window`.
    autotone_pending: HashSet<PathBuf>,
    /// Batch photos whose thumbnail has not been asked for yet, in the order
    /// they will be. Unbounded, but a `PathBuf` each, not a decoded thumbnail.
    autotone_queue: VecDeque<PathBuf>,
    /// Batch photos whose thumbnail has been requested, oldest first. Capped at
    /// `AUTOTONE_WINDOW`, and this is the only part of a batch the thumbnail
    /// cache has to hold at once.
    autotone_window: VecDeque<PathBuf>,
    /// Each pending photo's edits when it was queued. If they changed by the
    /// time its thumbnail lands, the user edited by hand, and `tone_one` must
    /// not overwrite that.
    autotone_base: HashMap<PathBuf, crate::develop::Adjustments>,
    /// Targets waiting for the sidecar scan, so Auto Tone snapshots their
    /// saved edits and not an empty default.
    autotone_deferred: Option<Vec<PathBuf>>,
    /// Photos toned so far in the running batch. The total is this plus
    /// `autotone_pending.len()`.
    autotone_done: usize,

    /// Face count and worst eye openness per path. Computed only for photos
    /// already in a burst or duplicate group, because Vision decodes the file
    /// at full resolution to find faces.
    face_quality: HashMap<PathBuf, crate::facequality::FaceQuality>,
    face_pending: HashSet<PathBuf>,
    /// Analyses that failed for good (corrupt or unsupported files).
    face_failed: HashSet<PathBuf>,
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

    /// Show only photos with a detected blink. Stacks with the star filter. A
    /// photo the face pass hasn't reached stays hidden.
    eyes_filter: bool,

    /// The duplicate group under review. Empty outside `ViewMode::Survey`.
    survey_members: Vec<PathBuf>,
    /// The group's best member from `dup_marks` when Survey opened, so
    /// `keep_best_reject_rest` agrees with the grid badge.
    survey_best: Option<PathBuf>,
    /// Index into `survey_members` that ratings and arrows apply to.
    survey_focus: usize,

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

    /// A bulk action awaiting confirmation in the modal.
    pending_bulk: Option<ui::BulkKind>,

    /// Copied tone settings (no crop) and the path they came from.
    copied_settings: Option<(PathBuf, Adjustments)>,

    /// The named-look library, global to the app rather than per folder.
    presets: crate::presets::PresetStore,
    /// A preset awaiting delete confirmation in its own modal.
    pending_preset_delete: Option<u64>,
    /// Open name prompt: the edit buffer, and which preset it renames (`None`
    /// is a new one).
    preset_name_edit: Option<(String, Option<u64>)>,

    show_help: bool,

    pending_quit: bool,
    /// `main.rs` exits the event loop when this is set.
    pub(crate) quit_requested: bool,

    /// Toast message and when it was set.
    status: Option<(String, Instant)>,

    /// Redraw retries pause while the window is hidden or minimized.
    pub(crate) occluded: bool,

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
mod catalog;
mod crop;
mod export;
mod histogram;
mod keys;
mod loupe;
mod nav;
mod presets;
mod thumbs;
#[cfg(target_arch = "wasm32")]
mod web;

impl App {
    pub(crate) fn new(initial: Option<PathBuf>) -> Self {
        let catalog = Catalog::new();
        let egui_ctx = egui::Context::default();
        configure_fonts(&egui_ctx);
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
            #[cfg(test)]
            pushed_adj: None,
            loader: None,
            exporter: None,
            feature_pool: None,
            face_pool: None,
            export_progress: None,
            bulk_delete: None,
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
            tool: LoupeTool::None,
            // Clamped up to TOUCHUP_MIN_PIXELS once an image is loaded.
            touchup_radius: 0.001,
            touchup_selected: None,
            develop_open: true,
            hist_sample: Vec::new(),
            hist_pixel_format: image_decode::PixelFormat::Srgb8,
            hist_dw: 0,
            hist_dh: 0,
            histogram: None,
            hist_dirty: false,
            unsaved_edit: None,
            #[cfg(target_arch = "wasm32")]
            unsaved_edit_kind: "adjustment",
            filter: None,
            filter_cmp: Cmp::Gte,
            visible: Vec::new(),
            sel: None,
            selected: BTreeSet::new(),
            anchor: None,
            filmstrip_scroll_accum: 0.0,
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
            dup_index: Default::default(),
            dup_refined: Vec::new(),
            feature_distances: HashMap::new(),
            feature_failed: HashSet::new(),
            feature_pending: HashSet::new(),
            dup_marks: Vec::new(),
            autotone_pending: HashSet::new(),
            autotone_queue: VecDeque::new(),
            autotone_window: VecDeque::new(),
            autotone_base: HashMap::new(),
            autotone_deferred: None,
            autotone_done: 0,
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
            zoom_rel: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            rotations: HashMap::new(),
            loupe_viewport: None,
            crop_edit: None,
            compare: false,
            exif_cache: HashMap::new(),
            source_size: None,
            pending_bulk: None,
            copied_settings: None,
            // A test must never write the developer's own preset library.
            #[cfg(test)]
            presets: crate::presets::PresetStore::in_memory(),
            #[cfg(not(test))]
            presets: crate::presets::PresetStore::load(),
            pending_preset_delete: None,
            preset_name_edit: None,
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

    /// Open a folder picker and load the choice. On the web the picker is
    /// async and `poll_folder_pick` receives the result. Native uses a modal dialog.
    pub(crate) fn open_folder_picker(&mut self) {
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
        self.crop_edit = None;
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
        self.cancel_delete();
        self.save_edit();
        self.request_catalog_load(playlist.dir());
    }

    /// Show `dir`'s images in the grid with nothing selected.
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

    /// `load_folder` for an already-built playlist. The web build builds its
    /// playlist from directory handles, since it can't call `read_dir`.
    fn load_playlist(&mut self, playlist: Playlist, dir: PathBuf) {
        #[cfg(target_arch = "wasm32")]
        crate::analytics::folder_opened(playlist.entries().len());
        self.teardown_loupe_state();
        self.seed_mirrors(&playlist);
        self.playlist = Some(playlist);
        self.reset_burst_state();
        self.reset_dup_state();
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
        // Upload thumbnails before egui references them.
        self.sync_thumb_textures();

        if self.hist_dirty && self.develop_open {
            self.recompute_histogram();
        }

        // The loader dedupes in-flight requests, so asking every frame is cheap.
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

        #[cfg(target_arch = "wasm32")]
        crate::analytics::begin_frame();
        let mut out = ui::FrameOutput::default();
        let full_output = self.egui_ctx.clone().run_ui(raw_input, |ui| {
            out = ui::draw(ui, self);
        });

        state.handle_platform_output(&*window, full_output.platform_output);
        self.egui_state = Some(state);

        self.apply_ui_actions(out.actions);
        self.save_edit_unless_dragging();

        // Show catalog write failures, or the user loses the change silently.
        if let Some(cause) = self.catalog.take_error() {
            self.set_status((crate::i18n::t().catalog_save_failed)(&cause));
        }
        if let Some(message) = self.presets.take_error() {
            self.set_status(message);
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

        // In Loupe, draw the image into egui's central rect, in physical pixels.
        // Grid and Survey are egui only.
        let image_viewport = match self.mode {
            ViewMode::Loupe => out.loupe_rect.map(|r| {
                let x = (r.min.x * pixels_per_point).round().max(0.0) as u32;
                let y = (r.min.y * pixels_per_point).round().max(0.0) as u32;
                let w = (r.width() * pixels_per_point).round().max(0.0) as u32;
                let h = (r.height() * pixels_per_point).round().max(0.0) as u32;
                (x, y, w, h)
            }),
            ViewMode::Grid | ViewMode::Survey => Some((0, 0, 0, 0)),
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

        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
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

    fn apply_ui_actions(&mut self, actions: Vec<ui::UiAction>) {
        for action in actions {
            match action {
                ui::UiAction::Select(pos) => {
                    if pos < self.visible.len() {
                        self.select_single(pos);
                        // In the loupe, the selection is what's shown. In the
                        // grid, Enter or double-click opens it.
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
                ui::UiAction::SavePresetPrompt => self.prompt_save_preset(),
                ui::UiAction::RenamePresetPrompt(id) => self.prompt_rename_preset(id),
                ui::UiAction::SetPresetNameText(text) => self.set_preset_name_text(text),
                ui::UiAction::CommitPresetName => self.commit_preset_name(),
                ui::UiAction::CancelPresetName => self.cancel_preset_name(),
                ui::UiAction::ApplyPreset(id) => self.apply_preset(id),
                ui::UiAction::RequestDeletePreset(id) => {
                    self.pending_preset_delete = Some(id);
                    self.request_redraw();
                }
                ui::UiAction::ConfirmDeletePreset => {
                    if let Some(id) = self.pending_preset_delete.take() {
                        self.delete_preset(id);
                    }
                    self.request_redraw();
                }
                ui::UiAction::CancelDeletePreset => {
                    self.pending_preset_delete = None;
                    self.request_redraw();
                }
                #[cfg(not(target_arch = "wasm32"))]
                ui::UiAction::ImportLrPresets => self.import_lr_presets(),
                ui::UiAction::ToggleHelp => {
                    self.show_help = !self.show_help;
                    self.request_redraw();
                }
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
                    // A click does what Enter does: focus, load, toggle expansion.
                    self.set_focus(Region::Folders, FocusLevel::Entered);
                    self.open_folder(p);
                }
                ui::UiAction::PickFolder => self.open_folder_picker(),
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
                ui::UiAction::CropGrab(edge) => self.crop_grab(edge),
                ui::UiAction::CropGrabMove(u, v) => self.crop_grab_move(u, v),
                ui::UiAction::CropDragTo(u, v) => self.crop_drag_to(u, v),
                ui::UiAction::CropRelease => self.crop_release(),
                ui::UiAction::ToggleWbPicker => self.toggle_wb_picker(),
                ui::UiAction::PickWhiteBalance(u, v) => self.pick_white_balance(u, v),
                ui::UiAction::ToggleTouchUp => {
                    self.tool = if self.tool == LoupeTool::TouchUp {
                        LoupeTool::None
                    } else {
                        LoupeTool::TouchUp
                    };
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
                    self.hist_dirty = true;
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

/// Map a selection given as playlist indices to positions in `new_visible`,
/// dropping photos the filter removed.
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

    /// The primary face defines the baseline, so it is never shifted.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn the_primary_face_needs_no_baseline_correction() {
        let sf = FaceMetrics {
            ascent: 0.9668,
            row_height: 1.1777,
        };
        assert_eq!(baseline_correction(sf, sf, 1.0), 0.0);
    }

    /// Hiragino's half-em line gap draws its baseline a quarter em above San
    /// Francisco's. The correction must push it down, so it is positive.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_cjk_face_with_a_large_line_gap_is_pushed_back_down() {
        let sf = FaceMetrics {
            ascent: 0.9668,
            row_height: 1.1777,
        };
        let hiragino = FaceMetrics {
            ascent: 0.88,
            row_height: 1.5,
        };
        let factor = baseline_correction(sf, hiragino, 1.0);
        assert!(
            (factor - 0.2479).abs() < 1e-3,
            "expected ~0.248 em down, got {factor}"
        );
    }

    /// epaint multiplies `y_offset_factor` by the face's `scale`, so a shrunk
    /// face (egui's emoji fonts) needs a larger factor for the same shift.
    #[test]
    #[cfg(not(target_arch = "wasm32"))]
    fn a_scaled_down_face_gets_its_scale_divided_out() {
        let primary = FaceMetrics {
            ascent: 1.0,
            row_height: 1.2,
        };
        let face = FaceMetrics {
            ascent: 0.8,
            row_height: 1.2,
        };
        assert!((baseline_correction(primary, face, 1.0) - 0.2).abs() < 1e-6);
        assert!((baseline_correction(primary, face, 0.5) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn range_set_is_inclusive_and_order_agnostic() {
        assert_eq!(range_set(2, 5), set(&[2, 3, 4, 5]));
        assert_eq!(range_set(5, 2), set(&[2, 3, 4, 5])); // same range, anchor after
        assert_eq!(range_set(3, 3), set(&[3])); // single cell
    }

    #[test]
    fn remap_keeps_surviving_photos_and_drops_filtered() {
        // Selected playlist indices 11 and 13 land at positions 0 and 2.
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
