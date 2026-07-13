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
use std::sync::Arc;
use std::time::Instant;

use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

use crate::catalog::Catalog;
use crate::develop::{self, Adjustments, Crop, GpuAdjust};
use crate::loader::Loader;
use crate::navigation::{self, flatten_visible_tree, visible_indices, Cmp, Playlist};
use crate::renderer::{EguiPaint, Renderer};
use crate::{image_decode, image_encode, paths, trash, ui};

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

/// Thumbnail size bounds (longest-side pixels) for the grid/filmstrip.
const THUMB_MIN: u32 = 96;
const THUMB_MAX: u32 = 512;
const THUMB_DEFAULT: u32 = 192;
const THUMB_STEP: u32 = 32;

/// Number of Develop sliders the keyboard cycles through (panel order: temp,
/// tint, exposure, contrast, highlights, shadows, whites, blacks).
const DEVELOP_SLIDERS: usize = 8;

/// Two top-level views: a thumbnail Grid and a single-image Loupe.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    Loupe,
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
const FULL_CROP: Crop = Crop { left: 0.0, top: 0.0, right: 1.0, bottom: 1.0 };
/// Smallest crop edge separation, in normalized units, so the rect never collapses.
const MIN_CROP: f32 = 0.02;

/// The UI region that currently receives arrow/Enter keys. Following
/// Lightroom, this is *not* moved by Tab (Tab hides/shows panels); it's driven
/// by the mouse (clicking into a panel) and by the module (Grid vs Loupe). The
/// content region of each mode — `Grid` / `Filmstrip` — is always available and
/// is the fallback when focus lands on a region that isn't currently shown.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Region {
    Folders,
    Grid,
    Filmstrip,
    Develop,
}

/// What the loupe currently has uploaded to the GPU. The decode *target* is
/// tracked separately in `App::want`; `try_show` reconciles `want` into `shown`.
/// Folding the old `shown: Option<PathBuf>` + `shown_is_full: bool` pair into one
/// enum makes the "thumbnail vs full" tier part of the path's identity, so a
/// stray `shown_is_full` can't disagree with which path is shown.
enum Shown {
    /// Nothing uploaded yet.
    Nothing,
    /// A thumbnail placeholder, shown instantly while the full image decodes.
    Thumb(PathBuf),
    /// The full-resolution image.
    Full(PathBuf),
}

impl Shown {
    /// The path shown in any tier, or `None` when nothing is shown.
    fn path(&self) -> Option<&Path> {
        match self {
            Shown::Nothing => None,
            Shown::Thumb(p) | Shown::Full(p) => Some(p),
        }
    }

    /// True when `path` is shown at full resolution.
    fn is_full_of(&self, path: &Path) -> bool {
        matches!(self, Shown::Full(p) if p == path)
    }
}

pub(crate) struct App {
    pub(crate) window: Option<Arc<Window>>,
    pub(crate) renderer: Option<Renderer>,
    pub(crate) loader: Option<Loader>,
    playlist: Option<Playlist>,

    /// Path we want shown in the loupe (may still be decoding).
    want: Option<PathBuf>,
    /// What's currently uploaded to the GPU (nothing / thumbnail / full).
    shown: Shown,
    /// A file/dir requested before the window/renderer existed.
    pub(crate) pending_initial: Option<PathBuf>,

    // ---- Browser state ----
    /// Grid vs. Loupe.
    pub(crate) mode: ViewMode,
    /// Ratings catalog (persistent) + an in-memory mirror for fast lookups.
    catalog: Catalog,
    ratings: HashMap<PathBuf, u8>,
    /// Per-image non-destructive develop edits (persistent, mirrored in-memory).
    /// Only non-identity edits are stored to keep the map small.
    edits: HashMap<PathBuf, Adjustments>,
    /// Whether the loupe's right-hand Develop panel is open.
    develop_open: bool,
    /// A small downsampled LINEAR-light RGB sample of the shown image, used to
    /// recompute the live histogram cheaply when adjustments change. Rebuilt
    /// whenever a new full image is uploaded.
    hist_sample: Vec<[f32; 3]>,
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
    /// Last `sel` the filmstrip auto-scrolled to (so we only scroll on change,
    /// not every frame — which would fight clicks). `None` = never.
    last_strip_sel: Option<usize>,
    /// Thumbnail longest-side pixels for the grid + filmstrip.
    thumb_px: u32,
    /// Columns the grid actually laid out last frame (for Up/Down row moves).
    grid_cols: usize,
    /// Visible cell range `[start, end)` the grid scrolled into view last frame.
    /// Drives thumbnail virtualization so huge folders don't load every image.
    grid_range: (usize, usize),
    /// Visible cell range `[start, end)` the loupe filmstrip scrolled into view
    /// last frame. The horizontal equivalent of `grid_range`.
    strip_range: (usize, usize),
    /// egui textures for thumbnails, keyed by (path, thumb_px, edit_signature).
    /// The edit signature makes an edit change (crop/tone/rotation) mint a new key,
    /// so `sync_thumb_textures` drops the stale texture and re-bakes. Rebuilt as
    /// thumbnails arrive; pruned to the current working set each frame.
    thumb_tex: HashMap<(PathBuf, u32, u64), egui::TextureHandle>,

    // ---- Loupe view state ----
    zoom: f32,
    pub(crate) pan: (f32, f32), // screen-space pixel coords of the image's top-left corner
    pub(crate) win_size: (f32, f32),
    /// True while the view is auto-fit to the window (so a resize re-fits).
    pub(crate) fitted: bool,
    /// Per-image rotation, in 90° clockwise steps (0..=3).
    rotations: HashMap<PathBuf, u8>,
    /// The image viewport rect (physical px) the loupe drew into last frame, if any.
    loupe_viewport: Option<(u32, u32, u32, u32)>,
    /// Transient crop-mode state; `Some` while the user is editing a crop.
    crop_edit: Option<CropDraft>,
    /// Before/after compare mode (Loupe only): the image is drawn twice, the
    /// left half with identity tone (but crop + rotation), the right with the
    /// full develop edits.
    compare: bool,

    /// A bulk action awaiting confirmation. `Some` while the confirm modal is up.
    pending_bulk: Option<ui::BulkKind>,

    /// Copied develop settings (tone only, no crop) plus the source file's path,
    /// for pasting onto other selected photos. `None` until the user copies.
    copied_settings: Option<(PathBuf, Adjustments)>,

    /// Whether the keyboard-shortcut help overlay is showing (toggled by `?`).
    show_help: bool,

    /// A short-lived status message (e.g. an export result), with the time it was
    /// set; shown as a toast for a few seconds, then ignored.
    status: Option<(String, Instant)>,

    /// True while winit reports the window as occluded (hidden/minimized/behind
    /// another window). We pause redraw retries while occluded.
    pub(crate) occluded: bool,

    // ---- Folder-tree sidebar state ----
    /// Top of the tree (the opened folder, or an opened file's parent).
    folder_root: Option<PathBuf>,
    /// The folder whose images are shown in the grid (highlighted in the tree).
    folder_sel: Option<PathBuf>,
    /// Folders currently expanded in the tree.
    expanded: HashSet<PathBuf>,
    /// Lazily-cached immediate subdirectories, one `list_subdirs` per folder.
    subdirs: HashMap<PathBuf, Vec<PathBuf>>,

    // ---- Keyboard focus state ----
    /// Which UI region currently receives arrow/Enter keys. Set by clicks and
    /// module changes (Lightroom-style), never by Tab.
    focus: Region,
    /// Tab hides the side panels (folders left, develop right).
    side_panels_hidden: bool,
    /// Shift+Tab hides all panels (side panels + filmstrip).
    all_panels_hidden: bool,
    /// Keyboard cursor in the folder tree (the highlighted-for-navigation row),
    /// distinct from `folder_sel` (the folder whose images are loaded). A
    /// `PathBuf` so it survives the tree re-flattening on expand/collapse.
    folder_cursor: Option<PathBuf>,
    /// Index of the keyboard-focused Develop slider (0..=7, panel order).
    develop_focus: usize,

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

impl App {
    pub(crate) fn new(initial: Option<PathBuf>) -> Self {
        let catalog = Catalog::load();
        Self {
            window: None,
            renderer: None,
            loader: None,
            playlist: None,
            want: None,
            shown: Shown::Nothing,
            pending_initial: initial,
            mode: ViewMode::Grid,
            catalog,
            ratings: HashMap::new(),
            edits: HashMap::new(),
            develop_open: true,
            hist_sample: Vec::new(),
            histogram: None,
            hist_dirty: false,
            filter: None,
            filter_cmp: Cmp::Gte,
            visible: Vec::new(),
            sel: None,
            selected: BTreeSet::new(),
            anchor: None,
            last_strip_sel: None,
            thumb_px: THUMB_DEFAULT,
            grid_cols: 1,
            grid_range: (0, 0),
            strip_range: (0, 0),
            thumb_tex: HashMap::new(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            rotations: HashMap::new(),
            loupe_viewport: None,
            crop_edit: None,
            compare: false,
            pending_bulk: None,
            copied_settings: None,
            show_help: false,
            status: None,
            occluded: false,
            folder_root: None,
            folder_sel: None,
            expanded: HashSet::new(),
            subdirs: HashMap::new(),
            focus: Region::Grid,
            side_panels_hidden: false,
            all_panels_hidden: false,
            folder_cursor: None,
            develop_focus: 0,
            cursor: (0.0, 0.0),
            modifiers: ModifiersState::empty(),
            space_down: false,
            dragging: false,
            last_drag: (0.0, 0.0),
            egui_ctx: egui::Context::default(),
            egui_state: None,
        }
    }

    /// Open a path: a directory → Grid (selection at 0); a file → Loupe (start
    /// on that file). Builds the playlist, seeds ratings, computes the visible
    /// view, and kicks off thumbnail/full requests.
    pub(crate) fn open(&mut self, path: PathBuf) {
        let is_dir = std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false);
        eprintln!(
            "[image-viewer] open {}: {}",
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
            // Seed the in-memory ratings + edits mirrors for everything in the folder.
            for p in playlist.entries() {
                if let Some(stars) = self.catalog.get(p) {
                    self.ratings.insert(p.clone(), stars);
                }
                let adj = self.catalog.adjustments(p);
                if !adj.is_identity() {
                    self.edits.insert(p.clone(), adj);
                }
            }
            let start_index = playlist.position();
            self.playlist = Some(playlist);
            self.recompute_visible();
            self.sel = Some(self.visible.iter().position(|&i| i == start_index).unwrap_or(0));
            self.collapse_selection();
            self.mode = ViewMode::Loupe;
            self.load_selected();
            self.request_neighbors();
            self.request_redraw();
        }
    }

    /// Populate `subdirs[dir]` (the folder's immediate children) if not cached.
    fn ensure_subdirs(&mut self, dir: &Path) {
        if !self.subdirs.contains_key(dir) {
            self.subdirs.insert(dir.to_path_buf(), navigation::list_subdirs(dir));
        }
    }

    /// Load `dir`'s images into the grid (browse-first): rebuild the playlist,
    /// seed ratings, recompute the visible view, reset selection to nothing
    /// selected, mark `dir` as the selected folder, and request thumbnails.
    fn load_folder(&mut self, dir: PathBuf) {
        let playlist = Playlist::from_dir(&dir);
        // Seed the in-memory ratings + edits mirrors for everything in the folder.
        for p in playlist.entries() {
            if let Some(stars) = self.catalog.get(p) {
                self.ratings.insert(p.clone(), stars);
            }
            let adj = self.catalog.adjustments(p);
            if !adj.is_identity() {
                self.edits.insert(p.clone(), adj);
            }
            let rot = self.catalog.rotation(p);
            if rot != 0 {
                self.rotations.insert(p.clone(), rot);
            }
        }
        self.playlist = Some(playlist);
        self.recompute_visible();
        self.sel = None;
        self.selected.clear();
        self.anchor = None;
        self.folder_sel = Some(dir);
        self.request_working_thumbs();
        self.request_redraw();
    }

    /// Recompute `visible` from the current filter + ratings, clamping `sel` and
    /// remapping the multi-selection so it survives re-filtering.
    fn recompute_visible(&mut self) {
        // Snapshot the multi-selection + anchor as *playlist* indices before the
        // rebuild: positions within `visible` shift when the filter changes, but
        // playlist indices are stable, so we can restore the same photos after.
        let sel_pl: Vec<usize> = self
            .selected
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .collect();
        let anchor_pl = self.anchor.and_then(|p| self.visible.get(p).copied());

        let Some(pl) = &self.playlist else {
            self.visible.clear();
            self.sel = None;
            self.selected.clear();
            self.anchor = None;
            return;
        };
        let ratings = &self.ratings;
        self.visible = visible_indices(pl.entries(), self.filter, |p| {
            ratings.get(p).copied().unwrap_or(0)
        });
        // Clamp the cursor to the new bounds; clear it if nothing is visible.
        if self.visible.is_empty() {
            self.sel = None;
        } else if let Some(s) = self.sel {
            if s >= self.visible.len() {
                self.sel = Some(self.visible.len() - 1);
            }
        }
        // Remap the multi-selection + anchor from playlist indices to their new
        // positions, dropping any photo the filter removed.
        self.selected = remap_positions(&sel_pl, &self.visible);
        self.anchor = anchor_pl.and_then(|i| self.visible.iter().position(|&v| v == i));
    }

    /// The playlist index of the current selection, if any. `None` when the
    /// grid has no active selection (browse-first state).
    fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel?).copied()
    }

    /// The path of the current selection, if any.
    pub(crate) fn selected_path(&self) -> Option<PathBuf> {
        let pl = self.playlist.as_ref()?;
        let idx = self.selected_index()?;
        pl.entry(idx).map(|p| p.to_path_buf())
    }

    /// Paths of every photo in the multi-selection, in `visible` order. Falls
    /// back to the primary cell when the set is empty but a cell is active, so
    /// bulk operations always have at least the current photo to work on.
    pub(crate) fn selected_paths(&self) -> Vec<PathBuf> {
        let Some(pl) = self.playlist.as_ref() else {
            return Vec::new();
        };
        let positions: Vec<usize> = if self.selected.is_empty() {
            self.sel.into_iter().collect()
        } else {
            self.selected.iter().copied().collect()
        };
        positions
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .filter_map(|i| pl.entry(i).map(|p| p.to_path_buf()))
            .collect()
    }

    /// Number of photos a bulk action would affect (the multi-selection, or the
    /// single primary cell when the set is empty).
    pub(crate) fn selection_count(&self) -> usize {
        if self.selected.is_empty() {
            usize::from(self.sel.is_some())
        } else {
            self.selected.len()
        }
    }

    /// Collapse the multi-selection down to just the primary cell (or empty when
    /// nothing is active). Called after a plain arrow move / single click.
    fn collapse_selection(&mut self) {
        self.selected = self.sel.into_iter().collect();
        self.anchor = self.sel;
    }

    /// Plain select: primary = `pos`, selection = `{pos}`.
    fn select_single(&mut self, pos: usize) {
        self.sel = Some(pos);
        self.anchor = Some(pos);
        self.selected = BTreeSet::from([pos]);
    }

    /// Cmd-click: toggle `pos` in the multi-selection; the primary follows the
    /// clicked cell (or an adjacent survivor when the primary is deselected).
    fn select_toggle(&mut self, pos: usize) {
        if self.selected.remove(&pos) {
            // Deselected the clicked cell: move the primary to another member.
            self.sel = self.selected.iter().next_back().copied();
        } else {
            self.selected.insert(pos);
            self.sel = Some(pos);
        }
        self.anchor = self.sel;
    }

    /// Shift-click / Shift-arrow: select the inclusive range from the anchor
    /// (or the primary, seeded on first use) to `pos`. The anchor stays put so
    /// the range can be re-dragged from the same origin.
    fn select_range(&mut self, pos: usize) {
        if self.anchor.is_none() {
            self.anchor = self.sel.or(Some(pos));
        }
        let a = self.anchor.unwrap_or(pos);
        self.selected = range_set(a, pos);
        self.sel = Some(pos);
    }

    /// Cmd+A: select every visible cell.
    fn select_all(&mut self) {
        let n = self.visible.len();
        if n == 0 {
            return;
        }
        self.selected = (0..n).collect();
        if self.sel.is_none() {
            self.sel = Some(0);
        }
        self.anchor = self.sel;
    }

    /// Shift+arrow in the grid: extend the range selection to the cell the
    /// arrow lands on, keeping the anchor fixed.
    fn extend_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        let from = self.sel.unwrap_or(0);
        if self.anchor.is_none() {
            self.anchor = Some(from);
        }
        let pos = navigation::grid_move(from, self.visible.len(), self.grid_cols, dx, dy);
        self.select_range(pos);
        self.request_redraw();
    }

    /// True when the visible cell at `pos` is part of the multi-selection.
    pub(crate) fn is_selected(&self, pos: usize) -> bool {
        self.selected.contains(&pos)
    }

    /// In Loupe mode, make the selection the wanted image and request decode.
    /// Requests both the full image and (as an instant placeholder) the
    /// thumbnail, so the shown image updates immediately even before the full
    /// decode finishes.
    fn load_selected(&mut self) {
        let Some(path) = self.selected_path() else { return };
        let px = self.thumb_px;
        if let Some(loader) = &mut self.loader {
            loader.request(path.clone());
            loader.request_thumb(path.clone(), px);
        }
        self.want = Some(path);
        self.try_show();
    }

    /// Request full-image decodes of the loupe neighbors (prev/next in the
    /// visible list) so stepping feels instant.
    fn request_neighbors(&mut self) {
        if self.visible.len() <= 1 {
            return;
        }
        let Some(cur) = self.sel else { return };
        let prev = (cur + self.visible.len() - 1) % self.visible.len();
        let next = (cur + 1) % self.visible.len();
        let paths: Vec<PathBuf> = [prev, next]
            .iter()
            .filter_map(|&p| self.visible.get(p).copied())
            .filter_map(|i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .map(|p| p.to_path_buf())
            .collect();
        if let Some(loader) = &mut self.loader {
            for p in paths {
                loader.request(p);
            }
        }
    }

    /// Move the loupe selection by ±1 within the visible list (wraps).
    fn step_loupe(&mut self, forward: bool) {
        if self.visible.is_empty() {
            return;
        }
        let n = self.visible.len();
        let cur = self.sel.unwrap_or(0);
        self.sel = Some(if forward {
            (cur + 1) % n
        } else {
            (cur + n - 1) % n
        });
        self.collapse_selection();
        self.load_selected();
        self.request_neighbors();
        self.request_redraw();
    }

    /// Move the grid selection by (dx, dy) cells (clamped, row-aware). The first
    /// arrow press with no active selection lands on the first cell.
    fn move_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        // First arrow press with no selection lands on the first cell.
        self.sel = Some(match self.sel {
            None => 0,
            Some(s) => navigation::grid_move(s, self.visible.len(), self.grid_cols, dx, dy),
        });
        self.collapse_selection();
        self.request_redraw();
    }

    /// Enter Loupe on the current selection. A no-op in the grid when nothing is
    /// selected (the `selected_path` guard below).
    fn enter_loupe(&mut self) {
        // No-op when nothing is selected; the guard also guarantees sel is Some.
        if self.selected_path().is_none() {
            return;
        }
        self.mode = ViewMode::Loupe;
        self.compare = false;
        self.load_selected();
        self.request_neighbors();
        self.normalize_focus();
        self.request_redraw();
    }

    // ---- Keyboard focus & panel visibility ----

    /// Whether the left folder-tree panel is currently shown.
    pub(crate) fn folders_visible(&self) -> bool {
        !self.side_panels_hidden && !self.all_panels_hidden
    }

    /// Whether the right Develop panel is currently shown (Loupe only).
    pub(crate) fn develop_visible(&self) -> bool {
        self.develop_open && !self.side_panels_hidden && !self.all_panels_hidden
    }

    /// Whether the bottom filmstrip is currently shown (Loupe only).
    pub(crate) fn filmstrip_visible(&self) -> bool {
        !self.all_panels_hidden
    }

    /// Whether a region can receive keyboard focus right now. The content regions
    /// (`Grid` in the grid, `Filmstrip` in the loupe) are always available — their
    /// arrows act on the images even when the strip panel is hidden. Side regions
    /// are available only while their panel is shown.
    fn region_available(&self, r: Region) -> bool {
        match r {
            Region::Grid => self.mode == ViewMode::Grid,
            Region::Filmstrip => self.mode == ViewMode::Loupe,
            Region::Folders => self.folders_visible(),
            Region::Develop => self.mode == ViewMode::Loupe && self.develop_visible(),
        }
    }

    /// Snap focus to a valid region when the current one isn't available (after a
    /// mode switch, or when its panel was hidden). Defaults to the mode's content
    /// region — Grid or Filmstrip — so focus lands on the images, not a sidebar.
    fn normalize_focus(&mut self) {
        if self.region_available(self.focus) {
            return;
        }
        self.focus = match self.mode {
            ViewMode::Grid => Region::Grid,
            ViewMode::Loupe => Region::Filmstrip,
        };
        self.on_focus_changed();
    }

    /// Tab: hide/show the side panels (folders + develop), Lightroom-style.
    fn toggle_side_panels(&mut self) {
        self.side_panels_hidden = !self.side_panels_hidden;
        self.normalize_focus();
        self.request_redraw();
    }

    /// Shift+Tab: hide/show all panels (side panels + filmstrip).
    fn toggle_all_panels(&mut self) {
        self.all_panels_hidden = !self.all_panels_hidden;
        self.normalize_focus();
        self.request_redraw();
    }

    /// Hook run whenever focus changes region. Seeds the folder cursor when focus
    /// lands on the tree, and resets the Develop cursor to the first slider.
    fn on_focus_changed(&mut self) {
        match self.focus {
            Region::Folders => self.seed_folder_cursor(),
            Region::Develop => self.develop_focus = 0,
            _ => {}
        }
    }

    /// The folder tree flattened to its currently-visible rows (DFS over expanded
    /// folders), top to bottom — the order folder arrow-nav moves through.
    fn visible_tree(&self) -> Vec<PathBuf> {
        let Some(root) = self.folder_root.clone() else {
            return Vec::new();
        };
        let is_expanded = |p: &Path| self.expanded.contains(p);
        let children = |p: &Path| self.subdirs.get(p).cloned().unwrap_or_default();
        flatten_visible_tree(&root, &is_expanded, &children)
    }

    /// Ensure the folder cursor points at a currently-visible row, preferring the
    /// loaded folder and falling back to the tree root.
    fn seed_folder_cursor(&mut self) {
        let tree = self.visible_tree();
        let valid = self
            .folder_cursor
            .as_ref()
            .map(|c| tree.iter().any(|p| p == c))
            .unwrap_or(false);
        if valid {
            return;
        }
        self.folder_cursor = self
            .folder_sel
            .clone()
            .filter(|s| tree.iter().any(|p| p == s))
            .or_else(|| self.folder_root.clone());
    }

    /// Move the folder cursor by `delta` rows within the visible tree (clamped).
    fn folder_move(&mut self, delta: isize) {
        let tree = self.visible_tree();
        if tree.is_empty() {
            return;
        }
        let cur = self
            .folder_cursor
            .as_ref()
            .and_then(|c| tree.iter().position(|p| p == c))
            .unwrap_or(0);
        let next = (cur as isize + delta).clamp(0, tree.len() as isize - 1) as usize;
        self.folder_cursor = Some(tree[next].clone());
        self.request_redraw();
    }

    /// Right-arrow in the tree: expand the cursor folder, or descend into its
    /// first child if already expanded. A no-op on a childless folder.
    fn folder_expand(&mut self) {
        let Some(cursor) = self.folder_cursor.clone() else {
            return;
        };
        self.ensure_subdirs(&cursor);
        if self.subdirs(&cursor).is_empty() {
            return; // leaf
        }
        if self.expanded.contains(&cursor) {
            if let Some(first) = self.subdirs(&cursor).first().cloned() {
                self.folder_cursor = Some(first);
            }
        } else {
            self.expanded.insert(cursor);
        }
        self.request_redraw();
    }

    /// Left-arrow in the tree: collapse the cursor folder if open, else move the
    /// cursor up to its parent (stopping at the root).
    fn folder_collapse(&mut self) {
        let Some(cursor) = self.folder_cursor.clone() else {
            return;
        };
        if self.expanded.contains(&cursor) {
            self.expanded.remove(&cursor);
        } else if Some(cursor.as_path()) != self.folder_root.as_deref() {
            if let Some(parent) = cursor.parent() {
                self.folder_cursor = Some(parent.to_path_buf());
            }
        }
        self.request_redraw();
    }

    /// Enter in the tree: toggle the cursor folder's expansion (when it has
    /// children) and load its images into the grid.
    fn folder_enter(&mut self) {
        let Some(cursor) = self.folder_cursor.clone() else {
            return;
        };
        self.open_folder(cursor);
    }

    /// Open a folder as one unit: toggle its expansion (when it has children),
    /// load its images into the grid, and move the keyboard cursor onto it.
    /// Shared by the folder-row click and the Enter key so mouse and keyboard
    /// behave identically.
    fn open_folder(&mut self, path: PathBuf) {
        self.ensure_subdirs(&path);
        if !self.subdirs(&path).is_empty() {
            if self.expanded.contains(&path) {
                self.expanded.remove(&path);
            } else {
                self.expanded.insert(path.clone());
            }
        }
        self.folder_cursor = Some(path.clone());
        self.load_folder(path);
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
        self.request_redraw();
    }

    // ---- Focus-routed arrow / Enter dispatchers ----

    fn nav_left(&mut self) {
        match self.focus {
            Region::Folders => self.folder_collapse(),
            Region::Grid => self.move_grid(-1, 0),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_adjust(-1),
        }
    }

    fn nav_right(&mut self) {
        match self.focus {
            Region::Folders => self.folder_expand(),
            Region::Grid => self.move_grid(1, 0),
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_adjust(1),
        }
    }

    fn nav_up(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(-1),
            Region::Grid => self.move_grid(0, -1),
            Region::Filmstrip => self.step_loupe(false),
            Region::Develop => self.develop_move(-1),
        }
    }

    fn nav_down(&mut self) {
        match self.focus {
            Region::Folders => self.folder_move(1),
            Region::Grid => self.move_grid(0, 1),
            Region::Filmstrip => self.step_loupe(true),
            Region::Develop => self.develop_move(1),
        }
    }

    fn nav_enter(&mut self) {
        match self.focus {
            Region::Folders => self.folder_enter(),
            Region::Grid => self.enter_loupe(),
            // Filmstrip / Develop: Enter has no distinct action.
            _ => {}
        }
    }

    /// Route an arrow key. With Shift held in the grid it extends the range
    /// selection; otherwise it's the normal focus-routed move. `(dx, dy)` maps to
    /// left/right/up/down.
    fn nav_arrow(&mut self, dx: isize, dy: isize, shift: bool) {
        if shift && self.focus == Region::Grid {
            self.extend_grid(dx, dy);
            return;
        }
        match (dx, dy) {
            (-1, 0) => self.nav_left(),
            (1, 0) => self.nav_right(),
            (0, -1) => self.nav_up(),
            _ => self.nav_down(),
        }
    }

    /// Move the Develop slider cursor by `delta` (clamped to 0..=7).
    fn develop_move(&mut self, delta: isize) {
        let max = DEVELOP_SLIDERS as isize - 1;
        self.develop_focus = (self.develop_focus as isize + delta).clamp(0, max) as usize;
        self.request_redraw();
    }

    /// Nudge the focused Develop slider's value (`dir` = -1/+1) by one step and
    /// apply it. Tone fields step ±1 (200-unit span, integer display); exposure
    /// steps ±0.05 (10-stop span, two-decimal display).
    fn develop_adjust(&mut self, dir: isize) {
        let mut adj = self.current_adjustments();
        let sign = dir as f32;
        let (field, range, step): (&mut f32, std::ops::RangeInclusive<f32>, f32) =
            match self.develop_focus {
                0 => (&mut adj.temp, develop::TONE_RANGE, 1.0),
                1 => (&mut adj.tint, develop::TONE_RANGE, 1.0),
                2 => (&mut adj.exposure, develop::EXPOSURE_RANGE, 0.05),
                3 => (&mut adj.contrast, develop::TONE_RANGE, 1.0),
                4 => (&mut adj.highlights, develop::TONE_RANGE, 1.0),
                5 => (&mut adj.shadows, develop::TONE_RANGE, 1.0),
                6 => (&mut adj.whites, develop::TONE_RANGE, 1.0),
                _ => (&mut adj.blacks, develop::TONE_RANGE, 1.0),
            };
        *field = (*field + sign * step).clamp(*range.start(), *range.end());
        self.apply_adjustments(adj);
    }

    /// Set the rating of the selected/shown image; recompute the view if the
    /// active filter drops it.
    fn set_rating(&mut self, stars: u8) {
        let Some(path) = self.selected_path() else { return };
        if stars == 0 {
            self.ratings.remove(&path);
        } else {
            self.ratings.insert(path.clone(), stars);
        }
        self.catalog.set(&path, stars);
        // A rating change can move the item in/out of a filtered view.
        if self.filter.is_some() {
            let want_idx = self.selected_index();
            self.recompute_visible();
            // Keep selection on the same playlist entry if still visible.
            if let Some(idx) = want_idx {
                if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
                    self.sel = Some(pos);
                }
            }
            // If the rated photo dropped out of the filtered view, the cursor
            // has moved to a neighbor — resync the loupe's main image to it.
            self.resync_loupe_selection();
        }
        self.request_redraw();
    }

    /// After a filtered-view recompute, keep the loupe's shown image in sync with
    /// the selection: if the previously shown photo was filtered out, the cursor
    /// moved to a neighbor and the main image must follow it.
    fn resync_loupe_selection(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        if self.want != self.selected_path() {
            self.load_selected();
            self.request_neighbors();
        }
    }

    /// Human-readable prompt for the pending bulk action, or `None` when no
    /// confirmation is open. Drives the confirm modal.
    pub(crate) fn pending_bulk_prompt(&self) -> Option<String> {
        let kind = self.pending_bulk?;
        let n = self.selection_count();
        Some(match kind {
            ui::BulkKind::Rate(0) => format!("Clear the rating on {n} photo(s)?"),
            ui::BulkKind::Rate(s) => {
                format!("Apply {} to {n} photo(s)?", "\u{2605}".repeat(s as usize))
            }
            ui::BulkKind::Export => format!("Export {n} photo(s) as JPG?"),
            ui::BulkKind::ApplySettings => {
                format!("Apply the copied settings to {n} photo(s)?")
            }
            ui::BulkKind::Delete => format!("Move {n} photo(s) to the Trash?"),
        })
    }

    /// Open the confirm modal for `kind` (no-op when nothing is selected). Shared
    /// by the toolbar buttons and the Delete/Backspace key.
    fn request_bulk(&mut self, kind: ui::BulkKind) {
        if self.selection_count() > 0 {
            self.pending_bulk = Some(kind);
            self.request_redraw();
        }
    }

    /// Run a confirmed bulk action against the current selection.
    fn run_bulk(&mut self, kind: ui::BulkKind) {
        match kind {
            ui::BulkKind::Rate(stars) => self.apply_rating_to_selection(stars),
            ui::BulkKind::ApplySettings => self.apply_settings_to_selection(),
            ui::BulkKind::Export => self.export_selection(),
            ui::BulkKind::Delete => self.delete_selection(),
        }
    }

    /// Move every selected photo to the Trash, then drop it from the playlist,
    /// the in-memory maps, and the catalog, repairing the cursor + loupe.
    fn delete_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        let total = paths.len();
        let mut trashed: Vec<PathBuf> = Vec::new();
        let mut last_err: Option<String> = None;
        for path in &paths {
            match trash::move_to_trash(path) {
                Ok(()) => trashed.push(path.clone()),
                Err(e) => {
                    eprintln!("[image-viewer] trash failed for {}: {e}", path.display());
                    last_err = Some(e);
                }
            }
        }
        if !trashed.is_empty() {
            let gone: HashSet<PathBuf> = trashed.iter().cloned().collect();
            if let Some(pl) = self.playlist.as_mut() {
                pl.remove_matching(|p| gone.contains(p));
            }
            for p in &trashed {
                self.ratings.remove(p);
                self.edits.remove(p);
                self.rotations.remove(p);
                self.catalog.remove(p);
            }
            // Every index is now invalidated; rebuild the view. The cursor keeps
            // its position (clamped), landing on a neighbor of the deleted photos.
            self.selected.clear();
            self.anchor = None;
            self.recompute_visible();
            self.collapse_selection();
            if self.mode == ViewMode::Loupe {
                if self.visible.is_empty() {
                    // Nothing left to show — fall back to the grid.
                    self.mode = ViewMode::Grid;
                    self.normalize_focus();
                    self.update_window_title();
                } else {
                    self.load_selected();
                    self.request_neighbors();
                }
            }
        }
        let n = trashed.len();
        self.set_status(match last_err {
            None => format!("Moved {n} photo(s) to Trash"),
            Some(e) => format!("Trashed {n}/{total} \u{2014} last error: {e}"),
        });
        self.request_redraw();
    }

    /// Copy the primary photo's develop settings (tone only, no crop) to the
    /// in-app clipboard for pasting onto other photos. Copy is from a single
    /// photo, so it's a no-op unless exactly one is selected.
    fn copy_settings(&mut self) {
        if self.selection_count() != 1 {
            return;
        }
        let Some(path) = self.selected_path() else {
            return;
        };
        let tone = self.edits.get(&path).copied().unwrap_or_default().tone_only();
        let name = file_label(&path);
        self.copied_settings = Some((path, tone));
        self.set_status(format!("Copied settings from {name}"));
        self.request_redraw();
    }

    /// Apply the copied tone settings to every selected photo, preserving each
    /// photo's own crop (and rotation). Thumbnails re-bake automatically because
    /// their cache key includes the edit signature.
    fn apply_settings_to_selection(&mut self) {
        let Some((_, tone)) = self.copied_settings.clone() else {
            return;
        };
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        for path in &paths {
            // Overwrite the tone fields; keep this photo's existing crop.
            let existing = self.edits.get(path).copied().unwrap_or_default();
            let merged = Adjustments {
                crop: existing.crop,
                ..tone
            };
            if merged.is_identity() {
                self.edits.remove(path);
            } else {
                self.edits.insert(path.clone(), merged);
            }
            self.catalog.set_adjustments(path, &merged);
        }
        // If the shown image was among them, push its new look to the GPU live.
        if let Some(shown) = self.shown.path().map(Path::to_path_buf) {
            if paths.contains(&shown) {
                self.push_adjustments();
                self.hist_dirty = true;
            }
        }
        self.set_status(format!("Applied settings to {} photo(s)", paths.len()));
        self.request_redraw();
    }

    /// Name of the file the copied settings came from, if any (for the toolbar).
    pub(crate) fn copied_settings_name(&self) -> Option<String> {
        self.copied_settings.as_ref().map(|(p, _)| file_label(p))
    }

    /// Whether develop settings are on the clipboard (enables bulk Apply Settings).
    pub(crate) fn has_copied_settings(&self) -> bool {
        self.copied_settings.is_some()
    }

    /// Apply `stars` (0 clears) to every photo in the multi-selection.
    fn apply_rating_to_selection(&mut self, stars: u8) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            return;
        }
        for path in &paths {
            if stars == 0 {
                self.ratings.remove(path);
            } else {
                self.ratings.insert(path.clone(), stars);
            }
            self.catalog.set(path, stars);
        }
        // Rated photos may move in/out of a filtered view; recompute + resync.
        if self.filter.is_some() {
            self.recompute_visible();
            self.resync_loupe_selection();
        }
        let n = paths.len();
        self.set_status(if stars == 0 {
            format!("Cleared rating on {n} photo(s)")
        } else {
            format!("Rated {n} photo(s) \u{2605}{stars}")
        });
        self.request_redraw();
    }

    /// Apply a new filter (or clear it) and recompute the visible view.
    fn set_filter(&mut self, filter: Option<(Cmp, u8)>) {
        // Keep the selected entry across the recompute when possible.
        let want_idx = self.selected_index();
        self.filter = filter;
        self.recompute_visible();
        if let Some(idx) = want_idx {
            if let Some(pos) = self.visible.iter().position(|&i| i == idx) {
                self.sel = Some(pos);
            }
        }
        // In Loupe, the shown image may have been filtered out; snap to selection.
        if self.mode == ViewMode::Loupe {
            self.load_selected();
        }
        self.request_redraw();
    }

    /// Change the toolbar comparator (≥ / = / ≤). If a star-level filter is
    /// already active, re-apply it with the new comparator so the view updates
    /// immediately.
    fn set_filter_cmp(&mut self, cmp: Cmp) {
        self.filter_cmp = cmp;
        if let Some((_, n)) = self.filter {
            if (1..=5).contains(&n) {
                self.set_filter(Some((cmp, n)));
            }
        }
        self.request_redraw();
    }

    fn adjust_thumb_px(&mut self, grow: bool) {
        let next = if grow {
            self.thumb_px + THUMB_STEP
        } else {
            self.thumb_px.saturating_sub(THUMB_STEP)
        };
        self.thumb_px = next.clamp(THUMB_MIN, THUMB_MAX);
        self.request_redraw();
    }

    /// Rating of a given path (0 when unset).
    fn rating_of(&self, path: &Path) -> u8 {
        self.ratings.get(path).copied().unwrap_or(0)
    }

    pub(crate) fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show the wanted image: prefer the full-resolution decode, but fall back
    /// to the cached thumbnail as an instant placeholder while the full image is
    /// still decoding. Swaps thumbnail → full once the full image arrives.
    pub(crate) fn try_show(&mut self) {
        let Some(want) = self.want.clone() else { return };

        // Full image ready → show it (unless it's already the shown full image).
        if let Some(img) = self.loader.as_ref().and_then(|l| l.get(&want)) {
            if !self.shown.is_full_of(&want) {
                self.upload_shown(&want, &img, true);
            }
            return;
        }

        // Full not ready: show the thumbnail placeholder if we aren't already
        // showing this image in some form.
        if self.shown.path() != Some(want.as_path()) {
            if let Some(thumb) = self.loader.as_ref().and_then(|l| l.get_thumb(&want, self.thumb_px))
            {
                self.upload_shown(&want, &thumb, false);
            }
        }
    }

    /// Upload an image to the renderer as the currently-shown image and re-fit.
    fn upload_shown(&mut self, path: &Path, img: &image_decode::DecodedImage, is_full: bool) {
        let Some(renderer) = self.renderer.as_mut() else { return };
        renderer.set_image(img);
        self.shown = if is_full {
            Shown::Full(path.to_path_buf())
        } else {
            Shown::Thumb(path.to_path_buf())
        };
        // Rebuild the histogram sample from the newly-shown image, then mark the
        // histogram dirty so it's recomputed before the next draw.
        self.build_hist_sample(img);
        // Load this image's stored edits into the shader (or identity if none).
        self.push_adjustments();
        self.fit_to_window();
        self.update_window_title();
        self.request_redraw();
    }

    fn update_window_title(&self) {
        let Some(w) = &self.window else { return };
        match self.mode {
            ViewMode::Loupe => {
                if let (Some(p), Some(_pl)) = (self.shown.path(), &self.playlist) {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let pos = self.sel.unwrap_or(0) + 1;
                    w.set_title(&format!("{}  ({}/{})", name, pos, self.visible.len()));
                }
            }
            ViewMode::Grid => {
                w.set_title(&format!("Grid  ({} photos)", self.visible.len()));
            }
        }
    }

    fn image_size(&self) -> (f32, f32) {
        self.renderer
            .as_ref()
            .map(|r| (r.image_size.0 as f32, r.image_size.1 as f32))
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .unwrap_or((1.0, 1.0))
    }

    /// Rotation (in 90° CW steps) of the image currently shown.
    fn current_rotation(&self) -> u8 {
        self.shown.path().and_then(|p| self.rotations.get(p)).copied().unwrap_or(0)
    }

    /// Develop adjustments of the image currently shown (identity if unset).
    pub(crate) fn current_adjustments(&self) -> Adjustments {
        self.shown
            .path()
            .and_then(|p| self.edits.get(p))
            .copied()
            .unwrap_or_default()
    }

    /// Persist and apply `adj` to the currently-shown image: update the in-memory
    /// edits map (dropping identity edits), write the catalog, push to the GPU
    /// uniform, and mark the histogram dirty. Shared by the Develop sliders
    /// (`SetAdjustments`) and the keyboard slider nudges (`develop_adjust`).
    fn apply_adjustments(&mut self, adj: Adjustments) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if adj.is_identity() {
            self.edits.remove(&path);
        } else {
            self.edits.insert(path.clone(), adj);
        }
        self.catalog.set_adjustments(&path, &adj);
        self.push_adjustments();
        self.hist_dirty = true;
        self.request_redraw();
    }

    /// Push the current image's adjustments into the renderer uniform. Mirrors
    /// `push_transform`; call it whenever the shown image or its edits change.
    fn push_adjustments(&mut self) {
        let gpu = GpuAdjust::from(&self.current_adjustments());
        if let Some(r) = &mut self.renderer {
            r.set_adjustments(gpu);
        }
        self.request_redraw();
    }

    // ---- Crop mode ----

    /// The crop rectangle currently being edited, if crop mode is active.
    pub(crate) fn crop_rect(&self) -> Option<Crop> {
        self.crop_edit.as_ref().map(|d| d.rect)
    }

    /// Enter crop mode on the current image. Crop is a Loupe sub-mode: from the
    /// Grid this first opens the loupe. Seeds the draft from any existing crop,
    /// drops the mask on the GPU so the whole frame is visible, and fits it.
    fn enter_crop(&mut self) {
        if self.mode != ViewMode::Loupe {
            self.enter_loupe();
            if self.mode != ViewMode::Loupe {
                return; // nothing was selected
            }
        }
        if self.shown.path().is_none() {
            return;
        }
        let rect = self.current_adjustments().crop.unwrap_or(FULL_CROP);
        self.crop_edit = Some(CropDraft { rect, grab: None, aspect: 1.0 });
        // Show the full frame (identity crop) while framing; the overlay masks.
        self.push_crop_preview();
        self.fit_for_crop();
        self.request_redraw();
    }

    /// Push the current image's tone edits with the crop forced to full-frame,
    /// so the whole image is visible while the crop overlay is being edited.
    fn push_crop_preview(&mut self) {
        let mut adj = self.current_adjustments();
        adj.crop = None;
        let gpu = GpuAdjust::from(&adj);
        if let Some(r) = &mut self.renderer {
            r.set_adjustments(gpu);
        }
        self.request_redraw();
    }

    /// Commit the crop draft into the image's persisted adjustments (full-frame
    /// crops store as `None`), then leave crop mode.
    fn commit_crop(&mut self) {
        let Some(draft) = self.crop_edit.take() else { return };
        let r = draft.rect;
        let is_full = r.left <= MIN_CROP
            && r.top <= MIN_CROP
            && r.right >= 1.0 - MIN_CROP
            && r.bottom >= 1.0 - MIN_CROP;
        let mut adj = self.current_adjustments();
        adj.crop = if is_full { None } else { Some(r) };
        self.apply_adjustments(adj); // persists to catalog + pushes real crop to GPU
        self.request_redraw();
    }

    /// Leave crop mode without committing, restoring the previously-committed crop.
    fn cancel_crop(&mut self) {
        if self.crop_edit.take().is_some() {
            self.push_adjustments(); // restore the committed crop on the GPU
            self.request_redraw();
        }
    }

    /// Begin resizing by `edge`: record it and capture the current pixel aspect
    /// ratio (for Shift-lock while dragging).
    fn crop_grab(&mut self, edge: CropEdge) {
        let (w, h) = self.image_size();
        if let Some(d) = self.crop_edit.as_mut() {
            let cw = (d.rect.right - d.rect.left) * w;
            let ch = (d.rect.bottom - d.rect.top) * h;
            d.aspect = if ch > 0.0 { cw / ch } else { 1.0 };
            d.grab = Some(CropGrab::Edge(edge));
        }
    }

    /// Begin moving the whole crop rectangle: anchor the drag at texture
    /// coordinate `(u, v)` and remember the rectangle as it is now.
    fn crop_grab_move(&mut self, u: f32, v: f32) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = Some(CropGrab::Move { anchor: (u, v), rect0: d.rect });
        }
    }

    /// Apply the active crop drag at texture coordinate `(u, v)`:
    /// - `Move`: translate the whole rectangle (size fixed), clamped to the frame.
    /// - `Edge`: move that edge; with Shift, the perpendicular edges co-move about
    ///   their center to preserve the pixel aspect ratio captured at grab.
    fn crop_drag_to(&mut self, u: f32, v: f32) {
        let shift = self.modifiers.shift_key();
        let (w, h) = self.image_size();
        let Some(d) = self.crop_edit.as_mut() else { return };
        let Some(grab) = d.grab else { return };
        let mut r = d.rect;
        match grab {
            CropGrab::Move { anchor, rect0 } => {
                // Keep the size; translate by the pointer delta, clamped so the
                // rectangle stays inside the frame.
                let cw = rect0.right - rect0.left;
                let ch = rect0.bottom - rect0.top;
                let nl = (rect0.left + (u - anchor.0)).clamp(0.0, 1.0 - cw);
                let nt = (rect0.top + (v - anchor.1)).clamp(0.0, 1.0 - ch);
                r.left = nl;
                r.right = nl + cw;
                r.top = nt;
                r.bottom = nt + ch;
            }
            CropGrab::Edge(edge) => {
                match edge {
                    CropEdge::Left => r.left = u.clamp(0.0, r.right - MIN_CROP),
                    CropEdge::Right => r.right = u.clamp(r.left + MIN_CROP, 1.0),
                    CropEdge::Top => r.top = v.clamp(0.0, r.bottom - MIN_CROP),
                    CropEdge::Bottom => r.bottom = v.clamp(r.top + MIN_CROP, 1.0),
                }
                if shift && d.aspect > 0.0 && w > 0.0 && h > 0.0 {
                    match edge {
                        CropEdge::Left | CropEdge::Right => {
                            // Width just changed; set height from the locked ratio,
                            // centered on the current vertical center.
                            let ch_norm = (((r.right - r.left) * w) / d.aspect / h).clamp(MIN_CROP, 1.0);
                            let cy = (r.top + r.bottom) / 2.0;
                            r.top = (cy - ch_norm / 2.0).clamp(0.0, 1.0 - MIN_CROP);
                            r.bottom = (r.top + ch_norm).min(1.0);
                        }
                        CropEdge::Top | CropEdge::Bottom => {
                            let cw_norm = (((r.bottom - r.top) * h) * d.aspect / w).clamp(MIN_CROP, 1.0);
                            let cx = (r.left + r.right) / 2.0;
                            r.left = (cx - cw_norm / 2.0).clamp(0.0, 1.0 - MIN_CROP);
                            r.right = (r.left + cw_norm).min(1.0);
                        }
                    }
                }
            }
        }
        d.rect = r;
        self.request_redraw();
    }

    /// Clear the active crop drag (released).
    fn crop_release(&mut self) {
        if let Some(d) = self.crop_edit.as_mut() {
            d.grab = None;
        }
    }

    // ---- Export ----

    /// Export the selected image to a JPG in the same folder, with the central
    /// crop/rotation/develop edits baked in. Never overwrites an existing file.
    fn export_selected(&mut self) {
        let Some(path) = self.selected_path() else {
            self.set_status("Export: no image selected".into());
            self.request_redraw();
            return;
        };
        match self.export_image(&path) {
            Ok(out) => {
                let name = out.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                eprintln!("[image-viewer] exported {}", out.display());
                self.set_status(format!("Exported {name}"));
            }
            Err(e) => {
                eprintln!("[image-viewer] export failed: {e}");
                self.set_status(format!("Export failed: {e}"));
            }
        }
        self.request_redraw();
    }

    /// Export every selected photo to a baked JPG in its own folder. Runs
    /// synchronously (may briefly block on large selections); reports a count.
    fn export_selection(&mut self) {
        let paths = self.selected_paths();
        if paths.is_empty() {
            self.set_status("Export: nothing selected".into());
            self.request_redraw();
            return;
        }
        let total = paths.len();
        let mut ok = 0usize;
        let mut last_err: Option<String> = None;
        for path in &paths {
            match self.export_image(path) {
                Ok(out) => {
                    eprintln!("[image-viewer] exported {}", out.display());
                    ok += 1;
                }
                Err(e) => {
                    eprintln!("[image-viewer] export failed for {}: {e}", path.display());
                    last_err = Some(e);
                }
            }
        }
        self.set_status(match last_err {
            None => format!("Exported {ok} photo(s)"),
            Some(e) => format!("Exported {ok}/{total} \u{2014} last error: {e}"),
        });
        self.request_redraw();
    }

    /// Decode `path` at full resolution, bake in its edits, and write the JPG.
    /// Returns the path written. Runs synchronously (one image; brief).
    fn export_image(&self, path: &Path) -> Result<PathBuf, String> {
        // Full resolution: u32::MAX means `fit_within` never downscales.
        let img = image_decode::decode(path, u32::MAX)?;
        let adj = self.catalog.adjustments(path);
        let rot = self.rotations.get(path).copied().unwrap_or(0);
        let (w, h, rgba) = bake_edited(&img, &adj, rot);
        let out = paths::jpg_export_target(path);
        image_encode::encode_jpeg(&out, w, h, &rgba)?;
        Ok(out)
    }

    // ---- Status toast ----

    fn set_status(&mut self, msg: String) {
        self.status = Some((msg, Instant::now()));
    }

    /// The current status message, if one was set within the last few seconds.
    pub(crate) fn status_text(&self) -> Option<&str> {
        self.status.as_ref().and_then(|(s, t)| {
            (t.elapsed().as_secs_f32() < 3.0).then_some(s.as_str())
        })
    }

    /// Build the histogram sample from a freshly-shown image: a strided
    /// downsample (~256 px on the longest side) of LINEAR-light RGB, stored so
    /// `recompute_histogram` can re-bin it cheaply as adjustments change.
    ///
    /// The decode is premultiplied sRGB RGBA8; we un-premultiply (guarding a==0)
    /// and convert sRGB → linear with the 2.2 gamma `apply_linear` assumes, so
    /// the histogram domain matches the develop pipeline's input.
    fn build_hist_sample(&mut self, img: &image_decode::DecodedImage) {
        let (w, h) = (img.width as usize, img.height as usize);
        if w == 0 || h == 0 || img.rgba.len() < w * h * 4 {
            self.hist_sample.clear();
            self.hist_dirty = true;
            return;
        }
        // Stride so the longest side maps to ~256 samples.
        const TARGET: usize = 256;
        let step = (w.max(h) / TARGET).max(1);
        let mut sample = Vec::with_capacity((w / step + 1) * (h / step + 1));
        let srgb_to_linear = |c: f32| (c / 255.0).powf(2.2);
        let mut y = 0;
        while y < h {
            let mut x = 0;
            while x < w {
                let i = (y * w + x) * 4;
                let (r, g, b, a) = (img.rgba[i], img.rgba[i + 1], img.rgba[i + 2], img.rgba[i + 3]);
                // Un-premultiply (the decode is premultiplied alpha).
                let (r, g, b) = if a == 0 {
                    (0.0, 0.0, 0.0)
                } else if a == 255 {
                    (r as f32, g as f32, b as f32)
                } else {
                    let inv = 255.0 / a as f32;
                    ((r as f32 * inv).min(255.0), (g as f32 * inv).min(255.0), (b as f32 * inv).min(255.0))
                };
                sample.push([srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)]);
                x += step;
            }
            y += step;
        }
        self.hist_sample = sample;
        self.hist_dirty = true;
    }

    /// Recompute the cached histogram from `hist_sample` under the current
    /// image's adjustments: run `apply_linear` per sample, gamma-encode the
    /// linear output to display space (matching what the shader puts on screen),
    /// and bin each channel into 256 buckets.
    fn recompute_histogram(&mut self) {
        if self.hist_sample.is_empty() {
            self.histogram = None;
            self.hist_dirty = false;
            return;
        }
        let adj = self.current_adjustments();
        let mut bins = [[0f32; 256]; 3];
        for &px in &self.hist_sample {
            let out = develop::apply_linear(&adj, px);
            for ch in 0..3 {
                // Linear → display gamma (the same encoding the shader output gets).
                let v = out[ch].max(0.0).powf(1.0 / 2.2).clamp(0.0, 1.0);
                // Fractional ("float") binning: splat the sample across its two
                // neighbouring buckets by sub-bin position instead of rounding to
                // one. Spreading the energy continuously is what keeps the curve
                // smooth after a tone stretch, rather than re-quantizing to a comb.
                let pos = v * 255.0;
                let lo = pos.floor();
                let frac = pos - lo;
                let lo = lo as usize;
                bins[ch][lo] += 1.0 - frac;
                if lo < 255 {
                    bins[ch][lo + 1] += frac;
                }
            }
        }
        self.histogram = Some(bins);
        self.hist_dirty = false;
    }

    /// The cached histogram bins for the panel (`None` when no image is shown).
    pub(crate) fn histogram(&self) -> Option<&[[f32; 256]; 3]> {
        self.histogram.as_ref()
    }

    /// On-screen footprint after rotation (w/h swapped for 90°/270°).
    fn display_size(&self) -> (f32, f32) {
        let (w, h) = self.image_size();
        if self.current_rotation() % 2 == 1 { (h, w) } else { (w, h) }
    }

    /// The loupe image area in physical pixels: the whole surface unless a
    /// viewport was carved out by egui panels last frame.
    fn loupe_area(&self) -> (f32, f32) {
        match self.loupe_viewport {
            Some((_, _, w, h)) => (w.max(1) as f32, h.max(1) as f32),
            None => self.win_size,
        }
    }

    /// Fit to the loupe area, centered: scales the image up or down so the whole
    /// image is as large as possible while staying fully on-screen ("contain").
    fn fit_to_window(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        self.zoom = (ww / iw).min(wh / ih).clamp(MIN_ZOOM, MAX_ZOOM);
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Fit the *whole* image into the loupe area for crop mode: unlike
    /// `fit_to_window` this shrinks images larger than the viewport (no grow-only
    /// floor) and leaves a small margin, so the entire image — and thus all four
    /// crop edges and their handles — stay on-screen and grabbable.
    fn fit_for_crop(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        // ~5% border each side so edge handles aren't flush against the viewport.
        const MARGIN: f32 = 0.9;
        self.zoom = ((ww / iw).min(wh / ih) * MARGIN).clamp(MIN_ZOOM, MAX_ZOOM);
        self.fitted = true;
        self.center();
        self.push_transform();
    }

    /// Reset to 100% (1 image pixel == 1 screen pixel), centered.
    fn reset_100(&mut self) {
        self.zoom = 1.0;
        self.fitted = false;
        self.center();
        self.push_transform();
    }

    /// Rotate the current image 90° (clockwise if `cw`), remembering it per-image.
    fn rotate(&mut self, cw: bool) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else { return };
        let step = (self.current_rotation() + if cw { 1 } else { 3 }) % 4;
        if step == 0 {
            self.rotations.remove(&path);
        } else {
            self.rotations.insert(path.clone(), step);
        }
        self.catalog.set_rotation(&path, step);
        if self.fitted {
            self.fit_to_window();
        } else {
            self.center();
            self.push_transform();
        }
    }

    fn center(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        self.pan = ((ww - iw * self.zoom) / 2.0, (wh - ih * self.zoom) / 2.0);
    }

    /// Zoom by `factor`, keeping the image point under (cx, cy) fixed. `cx/cy`
    /// are in loupe-area-local pixels (origin at the viewport's top-left).
    pub(crate) fn zoom_at(&mut self, factor: f32, cx: f32, cy: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ipx = (cx - self.pan.0) / self.zoom;
        let ipy = (cy - self.pan.1) / self.zoom;
        self.pan.0 = cx - ipx * new_zoom;
        self.pan.1 = cy - ipy * new_zoom;
        self.zoom = new_zoom;

        // Once an axis fully fits in the viewport, keep the image centered on that
        // axis so the surrounding gap stays even (matches `center()`).
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        if iw * new_zoom <= ww {
            self.pan.0 = (ww - iw * new_zoom) / 2.0;
        }
        if ih * new_zoom <= wh {
            self.pan.1 = (wh - ih * new_zoom) / 2.0;
        }

        self.fitted = false;
        self.push_transform();
    }

    /// Cursor position relative to the loupe viewport's top-left, in physical px.
    pub(crate) fn cursor_in_loupe(&self) -> (f32, f32) {
        // `self.cursor` is already physical pixels (winit `CursorMoved` reports a
        // `PhysicalPosition`), and `loupe_viewport` is physical too — so we just
        // subtract the viewport origin; no scale-factor conversion.
        let (px, py) = (self.cursor.0 as f32, self.cursor.1 as f32);
        match self.loupe_viewport {
            Some((x, y, _, _)) => (px - x as f32, py - y as f32),
            None => (px, py),
        }
    }

    /// The `(scale, offset, rot)` the shader transform is currently built from —
    /// the values `push_transform` uploads. Shared so the crop overlay can map
    /// between screen points and texture UVs using the exact same geometry.
    /// `rot` is the row-major 2×2 `[m00, m01, m10, m11]` used by the shader.
    fn loupe_transform(&self) -> ([f32; 2], [f32; 2], [f32; 4]) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let denom_x = self.zoom * iw;
        let denom_y = self.zoom * ih;
        let scale = [ww / denom_x, wh / denom_y];
        let offset = [-self.pan.0 / denom_x, -self.pan.1 / denom_y];
        (scale, offset, self.rot_matrix())
    }

    /// The display-UV → texture-UV rotation matrix for the current 90° step.
    fn rot_matrix(&self) -> [f32; 4] {
        match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        }
    }

    /// A centered "contain" fit transform for an area `(aw, ah)` physical px,
    /// independent of the current zoom/pan. Used for the before/after halves.
    fn fit_transform_for(&self, aw: f32, ah: f32) -> ([f32; 2], [f32; 2], [f32; 4]) {
        let (iw, ih) = self.display_size();
        let zoom = (aw / iw).min(ah / ih).clamp(MIN_ZOOM, MAX_ZOOM);
        let denom_x = zoom * iw;
        let denom_y = zoom * ih;
        let pan_x = (aw - iw * zoom) / 2.0;
        let pan_y = (ah - ih * zoom) / 2.0;
        let scale = [aw / denom_x, ah / denom_y];
        let offset = [-pan_x / denom_x, -pan_y / denom_y];
        (scale, offset, self.rot_matrix())
    }

    /// Configure the renderer for the before/after compare view: a shared
    /// half-size fit transform, the primary adjustments = "before" (identity
    /// tone but the same crop), the secondary = "after" (the full edits).
    /// `(half_w, half_h)` is each side's size in physical px.
    fn push_compare(&mut self, half_w: f32, half_h: f32) {
        let after = self.current_adjustments();
        let before = Adjustments { crop: after.crop, ..Adjustments::default() };
        let (scale, offset, rot) = self.fit_transform_for(half_w, half_h);
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
            r.set_adjustments(GpuAdjust::from(&before));
            r.set_adjustments_b(GpuAdjust::from(&after));
        }
    }

    /// Toggle the before/after compare view (Loupe only). Turning it off
    /// restores the normal single-image transform + adjustments.
    fn toggle_compare(&mut self) {
        if self.mode != ViewMode::Loupe {
            return;
        }
        self.compare = !self.compare;
        if !self.compare {
            self.push_transform();
            self.push_adjustments();
        }
        self.request_redraw();
    }

    /// Whether the before/after compare view is active.
    pub(crate) fn compare(&self) -> bool {
        self.compare
    }

    /// Recompute the shader transform from the current view state.
    pub(crate) fn push_transform(&mut self) {
        let (scale, offset, rot) = self.loupe_transform();
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
        }
        self.request_redraw();
    }

    /// Map a normalized texture UV (crop space, 0..1) to a screen point inside
    /// the loupe rect `central` (egui logical px). Inverse of
    /// `loupe_screen_to_tex`; used to draw the crop rectangle/handles/mask.
    pub(crate) fn loupe_tex_to_screen(&self, central: egui::Rect, u: f32, v: f32) -> egui::Pos2 {
        let (scale, offset, rot) = self.loupe_transform();
        // Invert uv = R·(d − 0.5) + 0.5. R is a rotation, so R⁻¹ = Rᵀ.
        let (du, dv) = (u - 0.5, v - 0.5);
        let dx = rot[0] * du + rot[2] * dv + 0.5;
        let dy = rot[1] * du + rot[3] * dv + 0.5;
        // Invert d = base_uv · scale + offset.
        let bx = (dx - offset[0]) / scale[0];
        let by = (dy - offset[1]) / scale[1];
        egui::pos2(
            central.min.x + bx * central.width(),
            central.min.y + by * central.height(),
        )
    }

    /// Map a screen point inside the loupe rect `central` to a normalized texture
    /// UV (crop space, 0..1). Inverse of `loupe_tex_to_screen`; used to turn a
    /// crop-edge drag into a crop coordinate.
    pub(crate) fn loupe_screen_to_tex(&self, central: egui::Rect, p: egui::Pos2) -> (f32, f32) {
        let (scale, offset, rot) = self.loupe_transform();
        let bx = if central.width() > 0.0 { (p.x - central.min.x) / central.width() } else { 0.0 };
        let by = if central.height() > 0.0 { (p.y - central.min.y) / central.height() } else { 0.0 };
        let dx = bx * scale[0] + offset[0];
        let dy = by * scale[1] + offset[1];
        // uv = R·(d − 0.5) + 0.5, R row-major [m00, m01, m10, m11].
        let u = rot[0] * (dx - 0.5) + rot[1] * (dy - 0.5) + 0.5;
        let v = rot[2] * (dx - 0.5) + rot[3] * (dy - 0.5) + 0.5;
        (u, v)
    }

    /// The range of visible positions whose thumbnails we keep loaded — the
    /// *working set*. Bounded so huge folders never load every image:
    /// - Grid: only the cells scrolled into view (`grid_range`) plus a few rows
    ///   of prefetch margin.
    /// - Loupe: the filmstrip neighbors around the selection.
    fn working_positions(&self) -> std::ops::Range<usize> {
        let len = self.visible.len();
        if len == 0 {
            return 0..0;
        }
        match self.mode {
            ViewMode::Grid => {
                let margin = self.grid_cols.saturating_mul(3).max(1);
                let start = self.grid_range.0.saturating_sub(margin);
                let end = (self.grid_range.1 + margin).min(len);
                start..end.max(start)
            }
            ViewMode::Loupe => {
                // The filmstrip reports its scrolled-into-view range; load that
                // window plus a prefetch margin (the horizontal analogue of the
                // grid). Union with the selection so the current image's own
                // thumbnail always loads even before the strip reports a range.
                let margin = 8;
                let mut start = self.strip_range.0.saturating_sub(margin);
                let mut end = (self.strip_range.1 + margin).min(len);
                if let Some(s) = self.sel {
                    start = start.min(s);
                    end = end.max((s + 1).min(len));
                }
                start..end.max(start)
            }
        }
    }

    /// Paths (with thumb size) for the current working set.
    /// Signature of the persisted edits (tone + crop + manual rotation) for
    /// `path`. Folded into thumbnail-texture keys so an edit change re-bakes the
    /// thumbnail. `edits`/`rotations` are in-memory maps seeded at folder load.
    fn edit_sig_for(&self, path: &Path) -> u64 {
        let adj = self.edits.get(path).copied().unwrap_or_default();
        let rot = self.rotations.get(path).copied().unwrap_or(0);
        develop::edit_signature(&adj, rot)
    }

    fn working_thumb_keys(&self) -> Vec<(PathBuf, u32, u64)> {
        let px = self.thumb_px;
        let Some(pl) = &self.playlist else { return Vec::new() };
        self.working_positions()
            .filter_map(|pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i))
            .map(|p| {
                let sig = self.edit_sig_for(p);
                (p.to_path_buf(), px, sig)
            })
            .collect()
    }

    /// Request thumbnails for the working set. Returns true if any requested
    /// thumb is still missing (so the caller can keep redrawing until they
    /// arrive).
    pub(crate) fn request_working_thumbs(&mut self) -> bool {
        let px = self.thumb_px;
        let paths: Vec<PathBuf> = self.working_thumb_keys().into_iter().map(|(p, _, _)| p).collect();

        let mut any_missing = false;
        if let Some(loader) = &mut self.loader {
            for p in &paths {
                // Skip thumbs whose decode permanently failed (deleted/corrupt) —
                // otherwise we'd re-request every frame and spin the redraw loop.
                if loader.get_thumb(p, px).is_none() && !loader.thumb_failed(p, px) {
                    loader.request_thumb(p.clone(), px);
                    any_missing = true;
                }
            }
        }
        any_missing
    }

    /// Sync `thumb_tex` with the loader's available thumbnails for the working
    /// set, uploading new ones as egui textures and dropping stale handles.
    fn sync_thumb_textures(&mut self) {
        if self.playlist.is_none() {
            return;
        }
        // Only the working set — bounds GPU textures to what's on screen so a
        // folder with thousands of images can't exhaust memory.
        let wanted = self.working_thumb_keys();

        // Upload any wanted thumbnail that's decoded but not yet a texture, baking
        // the image's edits (crop + tone + rotation) into it first so the grid
        // matches the loupe. The loader/disk thumbnail stays RAW (so the loupe's
        // placeholder isn't double-edited); the bake happens only here.
        for key in &wanted {
            if self.thumb_tex.contains_key(key) {
                continue;
            }
            let Some(img) = self.loader.as_ref().and_then(|l| l.get_thumb(&key.0, key.1)) else {
                continue;
            };
            let adj = self.edits.get(&key.0).copied().unwrap_or_default();
            let rot = self.rotations.get(&key.0).copied().unwrap_or(0);
            let color = if adj.is_identity() && rot % 4 == 0 {
                // Fast path: no edits, so upload the raw thumbnail verbatim (also
                // avoids a needless sRGB round-trip through the tone pipeline).
                egui::ColorImage::from_rgba_premultiplied(
                    [img.width as usize, img.height as usize],
                    &img.rgba,
                )
            } else {
                let (w, h, rgba) = bake_edited(&img, &adj, rot);
                // bake_edited yields opaque (alpha=255) pixels, so premultiplied
                // == straight; from_rgba_premultiplied is correct.
                egui::ColorImage::from_rgba_premultiplied([w as usize, h as usize], &rgba)
            };
            let name = format!("thumb:{}:{}:{:016x}", key.0.display(), key.1, key.2);
            let handle = self.egui_ctx.load_texture(name, color, egui::TextureOptions::LINEAR);
            self.thumb_tex.insert(key.clone(), handle);
        }

        // Drop handles outside the working set (frees GPU memory; egui-managed).
        // A stale edit signature isn't in `wanted`, so this also evicts the old
        // texture after an edit change.
        let keep: std::collections::HashSet<(PathBuf, u32, u64)> = wanted.into_iter().collect();
        self.thumb_tex.retain(|k, _| keep.contains(k));
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

        let (Some(window), Some(mut state)) =
            (self.window.clone(), self.egui_state.take())
        else {
            if let Some(r) = &mut self.renderer {
                r.render(None, None, None);
            }
            return;
        };

        let raw_input = state.take_egui_input(&*window);

        // Run the UI, collecting the central image rect (Loupe) and any actions.
        let mut out = ui::FrameOutput::default();
        let full_output = self.egui_ctx.clone().run_ui(raw_input, |ui| {
            out = ui::draw(ui, self);
        });

        state.handle_platform_output(&*window, full_output.platform_output);
        self.egui_state = Some(state);

        // Apply actions the UI produced (clicks, double-clicks, slider, filter).
        self.apply_ui_actions(out.actions);

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
            ViewMode::Grid => Some((0, 0, 0, 0)), // degenerate → renderer draws nothing
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

        // Before/after compare (Loupe): split the central rect into two halves,
        // set up the two-uniform draw, and render the image twice.
        let mut primary_vp = image_viewport;
        let mut compare_vp = None;
        if self.compare && self.mode == ViewMode::Loupe {
            if let Some((x, y, w, h)) = image_viewport {
                if w >= 2 && h > 0 {
                    let half = w / 2;
                    self.push_compare(half as f32, h as f32);
                    primary_vp = Some((x, y, half, h));
                    compare_vp = Some((x + half, y, w - half, h));
                }
            }
        }

        let Some(renderer) = self.renderer.as_mut() else { return };
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
                ui::UiAction::OpenFolder(p) => {
                    // The folder row is one unit: clicking it focuses the tree,
                    // loads the folder, and toggles its expansion — same as Enter.
                    self.focus = Region::Folders;
                    self.open_folder(p);
                }
                ui::UiAction::Focus(region) => {
                    self.focus = region;
                    self.normalize_focus();
                    self.on_focus_changed();
                    self.request_redraw();
                }
                ui::UiAction::CropGrab(edge) => self.crop_grab(edge),
                ui::UiAction::CropGrabMove(u, v) => self.crop_grab_move(u, v),
                ui::UiAction::CropDragTo(u, v) => self.crop_drag_to(u, v),
                ui::UiAction::CropRelease => self.crop_release(),
                ui::UiAction::SetAdjustments(adj) => self.apply_adjustments(adj),
                ui::UiAction::ResetAdjustments => {
                    let Some(path) = self.shown.path().map(Path::to_path_buf) else { continue };
                    self.edits.remove(&path);
                    self.catalog.set_adjustments(&path, &Adjustments::default());
                    self.push_adjustments();
                    self.hist_dirty = true;
                    self.request_redraw();
                }
            }
        }
    }
}

/// Accessors used by the egui UI module (`ui.rs`).
impl App {
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
    }

    /// The keyboard-focused region (lit panel, arrow-key target).
    pub(crate) fn focus(&self) -> Region {
        self.focus
    }

    /// The folder-tree keyboard cursor (the row navigation highlights), distinct
    /// from `folder_sel` (the loaded folder).
    pub(crate) fn folder_cursor(&self) -> Option<PathBuf> {
        self.folder_cursor.clone()
    }

    /// The index of the keyboard-focused Develop slider (0..=7).
    pub(crate) fn develop_focus(&self) -> usize {
        self.develop_focus
    }

    /// The root of the folder tree (the opened folder, or a file's parent).
    pub(crate) fn folder_root(&self) -> Option<PathBuf> {
        self.folder_root.clone()
    }

    /// The folder whose images are currently in the grid (highlighted in tree).
    pub(crate) fn folder_sel(&self) -> Option<PathBuf> {
        self.folder_sel.clone()
    }

    /// Whether a tree folder is expanded.
    pub(crate) fn is_expanded(&self, dir: &Path) -> bool {
        self.expanded.contains(dir)
    }

    /// The cached immediate subdirectories of `dir` (empty slice if uncached).
    pub(crate) fn subdirs(&self, dir: &Path) -> &[PathBuf] {
        self.subdirs.get(dir).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub(crate) fn filter(&self) -> Option<(Cmp, u8)> {
        self.filter
    }

    /// The comparator the toolbar will apply to the next star-level click.
    pub(crate) fn filter_cmp(&self) -> Cmp {
        self.filter_cmp
    }

    /// Count of photos in the current folder at each rating 0..=5 (index =
    /// stars). Computed over the whole playlist, ignoring the active filter, so
    /// the toolbar histogram shows the folder's true distribution.
    pub(crate) fn rating_counts(&self) -> [usize; 6] {
        let mut counts = [0usize; 6];
        if let Some(pl) = &self.playlist {
            for p in pl.entries() {
                counts[self.rating_of(p).min(5) as usize] += 1;
            }
        }
        counts
    }

    /// Whether the shortcut-help overlay is showing.
    pub(crate) fn show_help(&self) -> bool {
        self.show_help
    }

    pub(crate) fn thumb_px(&self) -> u32 {
        self.thumb_px
    }


    /// Position of the current selection within `visible`, or `None` in the
    /// grid's browse-first state (before any click/arrow).
    pub(crate) fn sel(&self) -> Option<usize> {
        self.sel
    }

    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn set_grid_cols(&mut self, cols: usize) {
        self.grid_cols = cols.max(1);
    }

    /// The grid reports which cell range `[start, end)` is scrolled into view so
    /// thumbnail loading can be virtualized to just those cells.
    pub(crate) fn set_visible_grid_range(&mut self, start: usize, end: usize) {
        self.grid_range = (start, end);
    }

    /// The filmstrip reports which cell range `[start, end)` is scrolled into view
    /// so thumbnail loading is virtualized to just those cells (horizontal
    /// equivalent of `set_visible_grid_range`).
    pub(crate) fn set_visible_strip_range(&mut self, start: usize, end: usize) {
        self.strip_range = (start, end);
    }

    /// Whether the filmstrip should scroll the selection into view this frame.
    /// Returns true only when the selection changed since the last call, so the
    /// strip doesn't re-center every frame (which fights clicks).
    pub(crate) fn take_filmstrip_follow(&mut self) -> bool {
        let changed = self.sel != self.last_strip_sel;
        self.last_strip_sel = self.sel;
        changed && self.sel.is_some()
    }

    /// Rating of the visible cell at `pos` (0 when unset/out of range).
    pub(crate) fn rating_at(&self, pos: usize) -> u8 {
        self.visible
            .get(pos)
            .and_then(|&i| self.playlist.as_ref().and_then(|pl| pl.entry(i)))
            .map(|p| self.rating_of(p))
            .unwrap_or(0)
    }

    /// Rating of the current selection (0 when unset).
    pub(crate) fn selected_rating(&self) -> u8 {
        self.selected_path().map(|p| self.rating_of(&p)).unwrap_or(0)
    }

    /// The egui texture + source dimensions for the visible cell at `pos`, if
    /// its thumbnail has been uploaded this frame.
    pub(crate) fn thumb_texture_for(
        &self,
        pos: usize,
    ) -> Option<(&egui::TextureHandle, u32, u32)> {
        let idx = *self.visible.get(pos)?;
        let path = self.playlist.as_ref()?.entry(idx)?;
        let key = (path.to_path_buf(), self.thumb_px, self.edit_sig_for(path));
        let handle = self.thumb_tex.get(&key)?;
        let [w, h] = handle.size();
        Some((handle, w as u32, h as u32))
    }
}

impl App {
    /// Handle a key press per the Lightroom key-binding table.
    pub(crate) fn handle_key(&mut self, code: KeyCode, event_loop: &ActiveEventLoop) {
        let shift = self.modifiers.shift_key();
        let cmd = self.modifiers.super_key();
        let alt = self.modifiers.alt_key();

        // While cropping, the keyboard is limited to the crop sub-mode: `C`/Enter
        // commit, `Esc` cancels, `X` still exports. Everything else is inert so a
        // stray arrow/digit can't move the selection out from under the crop.
        if self.crop_edit.is_some() {
            match code {
                KeyCode::KeyC | KeyCode::Enter | KeyCode::NumpadEnter => self.commit_crop(),
                KeyCode::Escape => self.cancel_crop(),
                KeyCode::KeyX if !cmd && !alt => self.export_selected(),
                _ => {}
            }
            return;
        }

        // `?` (Shift+/) toggles the shortcut-help overlay; Esc closes it if open.
        if shift && code == KeyCode::Slash {
            self.show_help = !self.show_help;
            self.request_redraw();
            return;
        }
        if self.show_help && code == KeyCode::Escape {
            self.show_help = false;
            self.request_redraw();
            return;
        }

        // Shift+1..5 → filter ≥ N; Shift+0 → clear (both modes). Checked before
        // plain digits.
        if shift {
            if let Some(n) = digit_of(code) {
                if (1..=5).contains(&n) {
                    self.set_filter(Some((Cmp::Gte, n)));
                    return;
                }
                if n == 0 {
                    self.set_filter(None);
                    return;
                }
            }
        }

        // Plain digits rate (0 clears) in both modes — never zoom.
        if !shift && !cmd && !alt {
            if let Some(n) = digit_of(code) {
                self.set_rating(n);
                return;
            }
        }

        match code {
            // Loupe-only existing transforms.
            KeyCode::Digit0 if alt => self.reset_100(),
            KeyCode::BracketLeft if cmd && self.mode == ViewMode::Loupe => self.rotate(false),
            KeyCode::BracketRight if cmd && self.mode == ViewMode::Loupe => self.rotate(true),

            // Tab hides/shows the side panels; Shift+Tab hides/shows all panels
            // (Lightroom-style). Focus is never moved by Tab.
            KeyCode::Tab => {
                if shift {
                    self.toggle_all_panels();
                } else {
                    self.toggle_side_panels();
                }
            }

            KeyCode::KeyG => {
                if self.mode != ViewMode::Grid {
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.normalize_focus();
                    self.request_redraw();
                }
            }
            // `E` is the focus-independent "enter loupe" edit key (Lightroom).
            KeyCode::KeyE => {
                if self.mode == ViewMode::Grid {
                    self.enter_loupe();
                }
            }
            // `C` enters crop mode (opening the loupe first from the grid).
            KeyCode::KeyC if !cmd && !alt => self.enter_crop(),
            // `X` exports the selected image as a baked JPG, in either mode.
            KeyCode::KeyX if !cmd && !alt => self.export_selected(),
            // Enter is focus-dependent (open image / expand folder / …).
            KeyCode::Enter | KeyCode::NumpadEnter => self.nav_enter(),
            // `Y` toggles the before/after compare view (Loupe only).
            KeyCode::KeyY if self.mode == ViewMode::Loupe => self.toggle_compare(),
            // `D` toggles the Develop panel (Loupe only).
            KeyCode::KeyD if self.mode == ViewMode::Loupe => {
                self.develop_open = !self.develop_open;
                self.normalize_focus();
                self.request_redraw();
            }
            KeyCode::Escape => match self.mode {
                ViewMode::Loupe => {
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.normalize_focus();
                    self.request_redraw();
                }
                ViewMode::Grid => event_loop.exit(),
            },

            // Cmd+A selects every visible cell in the grid.
            KeyCode::KeyA if cmd && self.mode == ViewMode::Grid => self.select_all(),
            // Cmd+Shift+C copies the primary photo's develop settings.
            KeyCode::KeyC if cmd && shift => self.copy_settings(),
            // Delete / Backspace move the selection to the Trash (after confirm).
            KeyCode::Delete | KeyCode::Backspace => self.request_bulk(ui::BulkKind::Delete),

            KeyCode::ArrowLeft => self.nav_arrow(-1, 0, shift),
            KeyCode::ArrowRight => self.nav_arrow(1, 0, shift),
            KeyCode::ArrowUp => self.nav_arrow(0, -1, shift),
            KeyCode::ArrowDown => self.nav_arrow(0, 1, shift),

            // +/- thumbnail size (Grid). Equal/Plus share a physical key.
            KeyCode::Equal | KeyCode::NumpadAdd if self.mode == ViewMode::Grid => {
                self.adjust_thumb_px(true)
            }
            KeyCode::Minus | KeyCode::NumpadSubtract if self.mode == ViewMode::Grid => {
                self.adjust_thumb_px(false)
            }

            _ => {}
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
    let (lo, hi) = if anchor <= pos { (anchor, pos) } else { (pos, anchor) };
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

/// Bake `adj` (crop + tone) and `rot` (90° CW steps) into a fresh, straight
/// (opaque) sRGB8 RGBA buffer. Order: crop in texture space → apply the tone
/// pipeline per pixel → rotate. Returns `(w, h, rgba)`. Used both for JPEG export
/// (full-res) and to render edited grid/filmstrip thumbnails (on the cached raw
/// thumbnail RGBA), so the two always agree.
///
/// The decode is premultiplied sRGB8; the un-premultiply + sRGB→linear here
/// matches `build_hist_sample`, and `develop::apply_linear` is the same tone
/// pipeline the shader runs, so the result matches what's on screen.
fn bake_edited(
    img: &image_decode::DecodedImage,
    adj: &Adjustments,
    rot: u8,
) -> (u32, u32, Vec<u8>) {
    let (w, h) = (img.width, img.height);
    if w == 0 || h == 0 || img.rgba.len() < (w * h * 4) as usize {
        return (0, 0, Vec::new());
    }

    // Crop rectangle → integer pixel bounds in texture space.
    let (cl, ct, cr, cb) = match adj.crop {
        Some(c) => (c.left, c.top, c.right, c.bottom),
        None => (0.0, 0.0, 1.0, 1.0),
    };
    let x0 = ((cl * w as f32).round() as i64).clamp(0, w as i64 - 1) as u32;
    let y0 = ((ct * h as f32).round() as i64).clamp(0, h as i64 - 1) as u32;
    let x1 = ((cr * w as f32).round() as i64).clamp(x0 as i64 + 1, w as i64) as u32;
    let y1 = ((cb * h as f32).round() as i64).clamp(y0 as i64 + 1, h as i64) as u32;
    let (cw, ch) = (x1 - x0, y1 - y0);

    let srgb_to_linear = |c: f32| (c / 255.0).powf(2.2);
    let encode = |v: f32| (v.max(0.0).powf(1.0 / 2.2) * 255.0).round().clamp(0.0, 255.0) as u8;

    // Cropped + tone-applied buffer, still in texture orientation.
    let mut cropped = vec![0u8; (cw * ch * 4) as usize];
    for y in 0..ch {
        for x in 0..cw {
            let si = (((y0 + y) * w + (x0 + x)) * 4) as usize;
            let (r, g, b, a) = (
                img.rgba[si],
                img.rgba[si + 1],
                img.rgba[si + 2],
                img.rgba[si + 3],
            );
            // Un-premultiply (the decode is premultiplied alpha).
            let (r, g, b) = if a == 0 {
                (0.0, 0.0, 0.0)
            } else if a == 255 {
                (r as f32, g as f32, b as f32)
            } else {
                let inv = 255.0 / a as f32;
                ((r as f32 * inv).min(255.0), (g as f32 * inv).min(255.0), (b as f32 * inv).min(255.0))
            };
            let lin = [srgb_to_linear(r), srgb_to_linear(g), srgb_to_linear(b)];
            let out = develop::apply_linear(adj, lin);
            let di = ((y * cw + x) * 4) as usize;
            cropped[di] = encode(out[0]);
            cropped[di + 1] = encode(out[1]);
            cropped[di + 2] = encode(out[2]);
            cropped[di + 3] = 255;
        }
    }

    rotate_rgba(&cropped, cw, ch, rot)
}

/// Rotate a tightly-packed RGBA8 buffer by `steps` × 90° clockwise. Returns the
/// (possibly swapped) `(width, height, rgba)`.
fn rotate_rgba(src: &[u8], w: u32, h: u32, steps: u8) -> (u32, u32, Vec<u8>) {
    let steps = steps % 4;
    if steps == 0 {
        return (w, h, src.to_vec());
    }
    let (nw, nh) = if steps == 2 { (w, h) } else { (h, w) };
    let mut dst = vec![0u8; (nw * nh * 4) as usize];
    let px = |x: u32, y: u32| ((y * w + x) * 4) as usize;
    for yo in 0..nh {
        for xo in 0..nw {
            // Source pixel that lands at output (xo, yo).
            let (xs, ys) = match steps {
                1 => (yo, h - 1 - xo),         // 90° CW
                2 => (w - 1 - xo, h - 1 - yo), // 180°
                _ => (w - 1 - yo, xo),         // 270° CW (90° CCW)
            };
            let s = px(xs, ys);
            let d = ((yo * nw + xo) * 4) as usize;
            dst[d..d + 4].copy_from_slice(&src[s..s + 4]);
        }
    }
    (nw, nh, dst)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(v: u8) -> [u8; 4] {
        [v, v, v, 255]
    }

    #[test]
    fn rotate_90cw_swaps_dims_and_moves_pixels() {
        // Two horizontal pixels A,B (w=2,h=1). 90° CW → a 1×2 column A over B.
        let src = [px(10), px(20)].concat();
        let (w, h, out) = rotate_rgba(&src, 2, 1, 1);
        assert_eq!((w, h), (1, 2));
        assert_eq!(&out[0..4], &px(10)); // top
        assert_eq!(&out[4..8], &px(20)); // bottom
    }

    #[test]
    fn rotate_360_is_identity() {
        let src = [px(1), px(2), px(3), px(4)].concat(); // 2×2
        let (w, h, out) = rotate_rgba(&src, 2, 2, 4);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn identity_bake_preserves_opaque_pixels() {
        // No crop, no rotation, identity adjustments → pixels survive the
        // premultiply/sRGB↔linear round-trip unchanged (alpha becomes opaque).
        let src = [px(0), px(64), px(128), px(255)].concat(); // 2×2
        let img = image_decode::DecodedImage { width: 2, height: 2, rgba: src.clone() };
        let (w, h, out) = bake_edited(&img, &Adjustments::default(), 0);
        assert_eq!((w, h), (2, 2));
        assert_eq!(out, src);
    }

    #[test]
    fn bake_crop_slices_to_the_crop_rect() {
        // 4×1 image; crop the right half → 2×1 keeping the last two pixels.
        let src = [px(1), px(2), px(3), px(4)].concat();
        let img = image_decode::DecodedImage { width: 4, height: 1, rgba: src };
        let mut adj = Adjustments::default();
        adj.crop = Some(Crop { left: 0.5, top: 0.0, right: 1.0, bottom: 1.0 });
        let (w, h, out) = bake_edited(&img, &adj, 0);
        assert_eq!((w, h), (2, 1));
        assert_eq!(&out[0..4], &px(3));
        assert_eq!(&out[4..8], &px(4));
    }

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

