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

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use winit::event_loop::ActiveEventLoop;
use winit::keyboard::{KeyCode, ModifiersState};
use winit::window::Window;

use crate::catalog::Catalog;
use crate::develop::{self, Adjustments, GpuAdjust};
use crate::loader::Loader;
use crate::navigation::{self, flatten_visible_tree, visible_indices, Cmp, Playlist};
use crate::renderer::{EguiPaint, Renderer};
use crate::{image_decode, ui};

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
    /// Whether the filter bar is shown.
    filter_bar: bool,
    /// Indices into `playlist.entries()` that pass the current filter.
    visible: Vec<usize>,
    /// Position *within `visible`* of the current selection, or `None` in the
    /// Grid's browse-first state (no selection until the user clicks or arrows).
    /// The Loupe always has a selection. `None` makes the "no selection" state
    /// unrepresentable as a stray index — there is no separate active flag.
    sel: Option<usize>,
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
    /// egui textures for thumbnails, keyed by (path, thumb_px). Rebuilt as
    /// thumbnails arrive; pruned to the current working set each frame.
    thumb_tex: HashMap<(PathBuf, u32), egui::TextureHandle>,

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
            filter_bar: false,
            visible: Vec::new(),
            sel: None,
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
        }
        self.playlist = Some(playlist);
        self.recompute_visible();
        self.sel = None;
        self.folder_sel = Some(dir);
        self.request_working_thumbs();
        self.request_redraw();
    }

    /// Recompute `visible` from the current filter + ratings, clamping `sel`.
    fn recompute_visible(&mut self) {
        let Some(pl) = &self.playlist else {
            self.visible.clear();
            self.sel = None;
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
    }

    /// The playlist index of the current selection, if any. `None` when the
    /// grid has no active selection (browse-first state).
    fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel?).copied()
    }

    /// The path of the current selection, if any.
    fn selected_path(&self) -> Option<PathBuf> {
        let pl = self.playlist.as_ref()?;
        let idx = self.selected_index()?;
        pl.entry(idx).map(|p| p.to_path_buf())
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
        self.ensure_subdirs(&cursor);
        if !self.subdirs(&cursor).is_empty() {
            if self.expanded.contains(&cursor) {
                self.expanded.remove(&cursor);
            } else {
                self.expanded.insert(cursor.clone());
            }
        }
        self.load_folder(cursor);
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
        }
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

    /// Fit to the loupe area, centered, *grow-only*.
    fn fit_to_window(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        self.zoom = (ww / iw).min(wh / ih).max(1.0).min(MAX_ZOOM);
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
        self.rotations.insert(path, step);
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

    /// Recompute the shader transform from the current view state.
    pub(crate) fn push_transform(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.loupe_area();
        let denom_x = self.zoom * iw;
        let denom_y = self.zoom * ih;
        let scale = [ww / denom_x, wh / denom_y];
        let offset = [-self.pan.0 / denom_x, -self.pan.1 / denom_y];
        let rot = match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        };
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
        }
        self.request_redraw();
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
    fn working_thumb_keys(&self) -> Vec<(PathBuf, u32)> {
        let px = self.thumb_px;
        let Some(pl) = &self.playlist else { return Vec::new() };
        self.working_positions()
            .filter_map(|pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i).map(|p| (p.to_path_buf(), px)))
            .collect()
    }

    /// Request thumbnails for the working set. Returns true if any requested
    /// thumb is still missing (so the caller can keep redrawing until they
    /// arrive).
    pub(crate) fn request_working_thumbs(&mut self) -> bool {
        let px = self.thumb_px;
        let paths: Vec<PathBuf> = self.working_thumb_keys().into_iter().map(|(p, _)| p).collect();

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

        // Upload any wanted thumbnail that's decoded but not yet a texture.
        if let Some(loader) = &self.loader {
            for key in &wanted {
                if self.thumb_tex.contains_key(key) {
                    continue;
                }
                if let Some(img) = loader.get_thumb(&key.0, key.1) {
                    let color = egui::ColorImage::from_rgba_premultiplied(
                        [img.width as usize, img.height as usize],
                        &img.rgba,
                    );
                    let name = format!("thumb:{}:{}", key.0.display(), key.1);
                    let handle = self.egui_ctx.load_texture(
                        name,
                        color,
                        egui::TextureOptions::LINEAR,
                    );
                    self.thumb_tex.insert(key.clone(), handle);
                }
            }
        }

        // Drop handles outside the working set (frees GPU memory; egui-managed).
        let keep: std::collections::HashSet<(PathBuf, u32)> = wanted.into_iter().collect();
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
                r.render(None, None);
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
                    self.fit_to_window();
                } else {
                    self.push_transform();
                }
            }
        }

        let Some(renderer) = self.renderer.as_mut() else { return };
        let egui_paint = EguiPaint {
            textures_delta: full_output.textures_delta,
            paint_jobs,
            screen_descriptor,
        };
        let presented = renderer.render(image_viewport, Some(egui_paint));
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
                        self.sel = Some(pos);
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
                ui::UiAction::OpenLoupe(pos) => {
                    if pos < self.visible.len() {
                        self.sel = Some(pos);
                        self.enter_loupe();
                    }
                }
                ui::UiAction::SetThumbPx(px) => {
                    self.thumb_px = px.clamp(THUMB_MIN, THUMB_MAX);
                    self.request_redraw();
                }
                ui::UiAction::SetFilter(f) => self.set_filter(f),
                ui::UiAction::SetRating(stars) => self.set_rating(stars),
                ui::UiAction::SelectFolder(p) => {
                    // Show this folder's images in the grid (browse-first). Switch
                    // to the grid so picking a folder from the loupe sidebar lands
                    // on its contents rather than a stale loupe image.
                    self.ensure_subdirs(&p);
                    self.folder_cursor = Some(p.clone());
                    self.load_folder(p);
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.normalize_focus();
                }
                ui::UiAction::Focus(region) => {
                    self.focus = region;
                    self.normalize_focus();
                    self.on_focus_changed();
                    self.request_redraw();
                }
                ui::UiAction::ToggleFolder(p) => {
                    if self.expanded.contains(&p) {
                        self.expanded.remove(&p);
                    } else {
                        self.expanded.insert(p.clone());
                        self.ensure_subdirs(&p);
                    }
                    // Place the keyboard cursor on the toggled row so it follows
                    // the click (the paired Focus(Folders) seeds only if unset).
                    self.folder_cursor = Some(p);
                    self.request_redraw();
                }
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

    pub(crate) fn filter_bar_open(&self) -> bool {
        self.filter_bar
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
        let key = (path.to_path_buf(), self.thumb_px);
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

        // Shift+1..5 → filter ≥ N (works in both modes), checked before plain digits.
        if shift {
            if let Some(n) = digit_of(code) {
                if (1..=5).contains(&n) {
                    self.set_filter(Some((Cmp::Gte, n)));
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
            // Enter is focus-dependent (open image / expand folder / …).
            KeyCode::Enter | KeyCode::NumpadEnter => self.nav_enter(),
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

            KeyCode::ArrowLeft => self.nav_left(),
            KeyCode::ArrowRight => self.nav_right(),
            KeyCode::ArrowUp => self.nav_up(),
            KeyCode::ArrowDown => self.nav_down(),

            // `\` toggles the filter bar.
            KeyCode::Backslash => {
                self.filter_bar = !self.filter_bar;
                self.request_redraw();
            }

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

