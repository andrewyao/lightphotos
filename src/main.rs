//! A fast macOS image viewer / Lightroom-lite photo browser.
//!
//! - Open a folder from Finder / "Open With" → thumbnail Grid; open a file → Loupe.
//! - `G` Grid, `E`/Enter Loupe (open selected), `Esc` backs out (Loupe→Grid, Grid→quit).
//! - Arrows move the grid selection / step the loupe; `1`–`5` rate, `0` clears.
//! - `Shift`+`1`–`5` set a "≥ N" star filter; `\` toggles the filter bar.
//! - `+`/`-` adjust thumbnail size (Grid).
//! - Loupe keeps the GPU pan/zoom path: scroll to zoom, Space+drag pan,
//!   Cmd+[ / Cmd+] rotate, grow-only fit. Alt+0 resets to 100%.
//!
//! Speed: images decode on background threads (Apple ImageIO) and live as a GPU
//! texture; zoom/pan only update a tiny transform uniform, never re-decode.
//! egui draws all chrome (grid, filmstrip, filter bar, rating overlays); the
//! hand-rolled wgpu renderer draws the loupe image, confined to a viewport rect.

mod catalog;
mod image_decode;
mod loader;
mod macos_delegate;
mod navigation;
mod renderer;
mod thumbnail;
mod ui;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use catalog::Catalog;
use loader::Loader;
use macos_delegate::UserEvent;
use navigation::{visible_indices, Cmp, Playlist};
use renderer::{EguiPaint, Renderer};

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

/// Thumbnail size bounds (longest-side pixels) for the grid/filmstrip.
const THUMB_MIN: u32 = 96;
const THUMB_MAX: u32 = 512;
const THUMB_DEFAULT: u32 = 192;
const THUMB_STEP: u32 = 32;

/// Two top-level views: a thumbnail Grid and a single-image Loupe.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Grid,
    Loupe,
}

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    loader: Option<Loader>,
    playlist: Option<Playlist>,

    /// Path we want shown in the loupe (may still be decoding).
    want: Option<PathBuf>,
    /// Path currently uploaded to the GPU.
    shown: Option<PathBuf>,
    /// Whether `shown` is the full-resolution image (vs. a thumbnail placeholder
    /// shown instantly while the full decode is still in flight).
    shown_is_full: bool,
    /// A file/dir requested before the window/renderer existed.
    pending_initial: Option<PathBuf>,

    // ---- Browser state ----
    /// Grid vs. Loupe.
    mode: ViewMode,
    /// Ratings catalog (persistent) + an in-memory mirror for fast lookups.
    catalog: Catalog,
    ratings: HashMap<PathBuf, u8>,
    /// Active star filter (`None` = show all).
    filter: Option<(Cmp, u8)>,
    /// Whether the filter bar is shown.
    filter_bar: bool,
    /// Indices into `playlist.entries()` that pass the current filter.
    visible: Vec<usize>,
    /// Position *within `visible`* of the current selection.
    sel: usize,
    /// Whether `sel` is an active selection. The Grid opens with no selection
    /// (browse-first) until the user clicks or arrows; the Loupe always has one.
    sel_active: bool,
    /// Last `sel` the filmstrip auto-scrolled to (so we only scroll on change,
    /// not every frame — which would fight clicks). `usize::MAX` = never.
    last_strip_sel: usize,
    /// Thumbnail longest-side pixels for the grid + filmstrip.
    thumb_px: u32,
    /// Columns the grid actually laid out last frame (for Up/Down row moves).
    grid_cols: usize,
    /// Visible cell range `[start, end)` the grid scrolled into view last frame.
    /// Drives thumbnail virtualization so huge folders don't load every image.
    grid_range: (usize, usize),
    /// egui textures for thumbnails, keyed by (path, thumb_px). Rebuilt as
    /// thumbnails arrive; pruned to the current working set each frame.
    thumb_tex: HashMap<(PathBuf, u32), egui::TextureHandle>,

    // ---- Loupe view state ----
    zoom: f32,
    pan: (f32, f32), // screen-space pixel coords of the image's top-left corner
    win_size: (f32, f32),
    /// True while the view is auto-fit to the window (so a resize re-fits).
    fitted: bool,
    /// Per-image rotation, in 90° clockwise steps (0..=3).
    rotations: HashMap<PathBuf, u8>,
    /// The image viewport rect (physical px) the loupe drew into last frame, if any.
    loupe_viewport: Option<(u32, u32, u32, u32)>,

    /// True while winit reports the window as occluded (hidden/minimized/behind
    /// another window). We pause redraw retries while occluded.
    occluded: bool,

    // ---- Folder-tree sidebar state ----
    /// Top of the tree (the opened folder, or an opened file's parent).
    folder_root: Option<PathBuf>,
    /// The folder whose images are shown in the grid (highlighted in the tree).
    folder_sel: Option<PathBuf>,
    /// Folders currently expanded in the tree.
    expanded: HashSet<PathBuf>,
    /// Lazily-cached immediate subdirectories, one `list_subdirs` per folder.
    subdirs: HashMap<PathBuf, Vec<PathBuf>>,

    // ---- Input state ----
    cursor: (f64, f64),
    modifiers: ModifiersState,
    space_down: bool,
    dragging: bool,
    last_drag: (f64, f64),

    // ---- egui chrome ----
    egui_ctx: egui::Context,
    egui_state: Option<egui_winit::State>,
}

impl App {
    fn new(initial: Option<PathBuf>) -> Self {
        let catalog = Catalog::load();
        Self {
            window: None,
            renderer: None,
            loader: None,
            playlist: None,
            want: None,
            shown: None,
            shown_is_full: false,
            pending_initial: initial,
            mode: ViewMode::Grid,
            catalog,
            ratings: HashMap::new(),
            filter: None,
            filter_bar: false,
            visible: Vec::new(),
            sel: 0,
            sel_active: false,
            last_strip_sel: usize::MAX,
            thumb_px: THUMB_DEFAULT,
            grid_cols: 1,
            grid_range: (0, 0),
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
    fn open(&mut self, path: PathBuf) {
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
            // Seed the in-memory ratings mirror for everything in the folder.
            for p in playlist.entries() {
                if let Some(stars) = self.catalog.get(p) {
                    self.ratings.insert(p.clone(), stars);
                }
            }
            let start_index = playlist.position();
            self.playlist = Some(playlist);
            self.recompute_visible();
            self.sel = self.visible.iter().position(|&i| i == start_index).unwrap_or(0);
            self.mode = ViewMode::Loupe;
            self.sel_active = true;
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
        // Seed the in-memory ratings mirror for everything in the folder.
        for p in playlist.entries() {
            if let Some(stars) = self.catalog.get(p) {
                self.ratings.insert(p.clone(), stars);
            }
        }
        self.playlist = Some(playlist);
        self.recompute_visible();
        self.sel = 0;
        self.sel_active = false;
        self.folder_sel = Some(dir);
        self.request_working_thumbs();
        self.request_redraw();
    }

    /// Recompute `visible` from the current filter + ratings, clamping `sel`.
    fn recompute_visible(&mut self) {
        let Some(pl) = &self.playlist else {
            self.visible.clear();
            self.sel = 0;
            return;
        };
        let ratings = &self.ratings;
        self.visible = visible_indices(pl.entries(), self.filter, |p| {
            ratings.get(p).copied().unwrap_or(0)
        });
        if self.visible.is_empty() {
            self.sel = 0;
        } else if self.sel >= self.visible.len() {
            self.sel = self.visible.len() - 1;
        }
    }

    /// The playlist index of the current selection, if any. `None` when the
    /// grid has no active selection (browse-first state).
    fn selected_index(&self) -> Option<usize> {
        if !self.sel_active {
            return None;
        }
        self.visible.get(self.sel).copied()
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
        let prev = (self.sel + self.visible.len() - 1) % self.visible.len();
        let next = (self.sel + 1) % self.visible.len();
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
        self.sel = if forward {
            (self.sel + 1) % n
        } else {
            (self.sel + n - 1) % n
        };
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
        if !self.sel_active {
            self.sel = 0;
            self.sel_active = true;
        } else {
            self.sel = navigation::grid_move(self.sel, self.visible.len(), self.grid_cols, dx, dy);
        }
        self.request_redraw();
    }

    /// Enter Loupe on the current selection. A no-op in the grid when nothing is
    /// selected (the `selected_path` guard below).
    fn enter_loupe(&mut self) {
        if self.selected_path().is_none() {
            return;
        }
        self.sel_active = true;
        self.mode = ViewMode::Loupe;
        self.load_selected();
        self.request_neighbors();
        self.request_redraw();
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
                    self.sel = pos;
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
                self.sel = pos;
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

    fn request_redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    /// Show the wanted image: prefer the full-resolution decode, but fall back
    /// to the cached thumbnail as an instant placeholder while the full image is
    /// still decoding. Swaps thumbnail → full once the full image arrives.
    fn try_show(&mut self) {
        let Some(want) = self.want.clone() else { return };

        // Full image ready → show it (unless it's already the shown full image).
        if let Some(img) = self.loader.as_ref().and_then(|l| l.get(&want)) {
            if self.shown.as_ref() != Some(&want) || !self.shown_is_full {
                self.upload_shown(&want, &img, true);
            }
            return;
        }

        // Full not ready: show the thumbnail placeholder if we aren't already
        // showing this image in some form.
        if self.shown.as_ref() != Some(&want) {
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
        self.shown = Some(path.to_path_buf());
        self.shown_is_full = is_full;
        self.fit_to_window();
        self.update_window_title();
        self.request_redraw();
    }

    fn update_window_title(&self) {
        let Some(w) = &self.window else { return };
        match self.mode {
            ViewMode::Loupe => {
                if let (Some(p), Some(_pl)) = (&self.shown, &self.playlist) {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let pos = self.sel + 1;
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
        self.shown.as_ref().and_then(|p| self.rotations.get(p)).copied().unwrap_or(0)
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
        let Some(path) = self.shown.clone() else { return };
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
    fn zoom_at(&mut self, factor: f32, cx: f32, cy: f32) {
        let new_zoom = (self.zoom * factor).clamp(MIN_ZOOM, MAX_ZOOM);
        let ipx = (cx - self.pan.0) / self.zoom;
        let ipy = (cy - self.pan.1) / self.zoom;
        self.pan.0 = cx - ipx * new_zoom;
        self.pan.1 = cy - ipy * new_zoom;
        self.zoom = new_zoom;
        self.fitted = false;
        self.push_transform();
    }

    /// Cursor position relative to the loupe viewport's top-left, in physical px.
    fn cursor_in_loupe(&self) -> (f32, f32) {
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
    fn push_transform(&mut self) {
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
                let lo = self.sel.saturating_sub(16);
                let hi = (self.sel + 17).min(len);
                lo..hi
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
    fn request_working_thumbs(&mut self) -> bool {
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
    fn redraw(&mut self) {
        // Make sure thumbnails for the working set are uploaded before egui
        // references them.
        self.sync_thumb_textures();

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
                        self.sel = pos;
                        self.sel_active = true;
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
                        self.sel = pos;
                        self.sel_active = true;
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
                    // Show this folder's images in the grid (browse-first).
                    self.ensure_subdirs(&p);
                    self.load_folder(p);
                }
                ui::UiAction::ToggleFolder(p) => {
                    if self.expanded.contains(&p) {
                        self.expanded.remove(&p);
                    } else {
                        self.expanded.insert(p.clone());
                        self.ensure_subdirs(&p);
                    }
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

    pub(crate) fn sel(&self) -> usize {
        self.sel
    }

    /// Whether there is an active selection to highlight (false in the grid's
    /// browse-first state before any click/arrow).
    pub(crate) fn sel_active(&self) -> bool {
        self.sel_active
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

    /// Whether the filmstrip should scroll the selection into view this frame.
    /// Returns true only when the selection changed since the last call, so the
    /// strip doesn't re-center every frame (which fights clicks).
    pub(crate) fn take_filmstrip_follow(&mut self) -> bool {
        let changed = self.sel != self.last_strip_sel;
        self.last_strip_sel = self.sel;
        changed
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

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Image Viewer")
            .with_inner_size(LogicalSize::new(1100.0, 800.0));
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));

        let renderer = Renderer::new(window.clone());
        let loader = Loader::new(renderer.max_dim);

        let egui_state = egui_winit::State::new(
            self.egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        let size = window.inner_size();
        self.win_size = (size.width.max(1) as f32, size.height.max(1) as f32);
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.loader = Some(loader);
        self.egui_state = Some(egui_state);

        if let Some(path) = self.pending_initial.take() {
            self.open(path);
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::OpenFile(path) => {
                if self.renderer.is_some() {
                    self.open(path);
                } else {
                    self.pending_initial = Some(path);
                }
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Give egui first crack at the event. Consumed events (clicks/keys in an
        // egui widget) skip the app's own handling.
        if let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) {
            let response = state.on_window_event(&*window, &event);
            if response.repaint {
                window.request_redraw();
            }
            if response.consumed {
                return;
            }
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                self.win_size = (size.width.max(1) as f32, size.height.max(1) as f32);
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                // The loupe viewport is recomputed from egui panels next frame.
                self.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                // Becoming visible again: redraw (we skip frames while occluded,
                // so the surface needs a fresh draw to stop showing blank).
                if !occluded {
                    self.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging && self.mode == ViewMode::Loupe {
                    // `position` is physical pixels, the same units as `pan` — add
                    // the delta directly (no scale-factor multiply).
                    let dx = position.x - self.last_drag.0;
                    let dy = position.y - self.last_drag.1;
                    self.pan.0 += dx as f32;
                    self.pan.1 += dy as f32;
                    self.fitted = false;
                    self.push_transform();
                }
                self.last_drag = (position.x, position.y);
                self.cursor = (position.x, position.y);
            }

            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => match state {
                ElementState::Pressed if self.space_down && self.mode == ViewMode::Loupe => {
                    self.dragging = true;
                    self.last_drag = self.cursor;
                }
                ElementState::Released => self.dragging = false,
                _ => {}
            },

            WindowEvent::MouseWheel { delta, .. } => {
                if self.mode == ViewMode::Loupe {
                    let s = match delta {
                        MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        MouseScrollDelta::LineDelta(_, y) => y * 20.0,
                    };
                    if s != 0.0 {
                        let factor = (s * 0.0025).exp();
                        let (cx, cy) = self.cursor_in_loupe();
                        self.zoom_at(factor, cx, cy);
                    }
                }
            }

            WindowEvent::PinchGesture { delta, .. } => {
                if self.mode == ViewMode::Loupe && delta != 0.0 {
                    let factor = 1.0 + delta as f32;
                    let (cx, cy) = self.cursor_in_loupe();
                    self.zoom_at(factor, cx, cy);
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed {
                    if let PhysicalKey::Code(code) = event.physical_key {
                        self.handle_key(code, event_loop);
                    }
                }
                // Track Space release for pan.
                if let PhysicalKey::Code(KeyCode::Space) = event.physical_key {
                    self.space_down = event.state == ElementState::Pressed;
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        // Drain both loader tiers once per frame.
        if let Some(loader) = &mut self.loader {
            let (full, thumbs) = loader.poll_all();
            // Any arrival may be the wanted image (full) or its placeholder
            // (thumb), so try to (re)show on either; redraw to paint new thumbs.
            let any = !full.is_empty() || !thumbs.is_empty();
            if any {
                self.try_show();
                self.request_redraw();
            }
        }
        // Keep redrawing while working-set thumbnails are still loading.
        if self.request_working_thumbs() {
            self.request_redraw();
        }
    }
}

impl App {
    /// Handle a key press per the Lightroom key-binding table.
    fn handle_key(&mut self, code: KeyCode, event_loop: &ActiveEventLoop) {
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

            KeyCode::KeyG => {
                if self.mode != ViewMode::Grid {
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.request_redraw();
                }
            }
            KeyCode::KeyE | KeyCode::Enter | KeyCode::NumpadEnter => {
                if self.mode == ViewMode::Grid {
                    self.enter_loupe();
                }
            }
            KeyCode::Escape => match self.mode {
                ViewMode::Loupe => {
                    self.mode = ViewMode::Grid;
                    self.update_window_title();
                    self.request_redraw();
                }
                ViewMode::Grid => event_loop.exit(),
            },

            KeyCode::ArrowLeft => match self.mode {
                ViewMode::Grid => self.move_grid(-1, 0),
                ViewMode::Loupe => self.step_loupe(false),
            },
            KeyCode::ArrowRight => match self.mode {
                ViewMode::Grid => self.move_grid(1, 0),
                ViewMode::Loupe => self.step_loupe(true),
            },
            KeyCode::ArrowUp => match self.mode {
                ViewMode::Grid => self.move_grid(0, -1),
                ViewMode::Loupe => self.step_loupe(false),
            },
            KeyCode::ArrowDown => match self.mode {
                ViewMode::Grid => self.move_grid(0, 1),
                ViewMode::Loupe => self.step_loupe(true),
            },

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

fn main() {
    // A file/dir path may be passed on the command line.
    let initial = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.exists());

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    macos_delegate::set_proxy(event_loop.create_proxy());
    if !macos_delegate::install_open_handler() {
        eprintln!("[image-viewer] warning: could not install Finder open handler");
    }

    let mut app = App::new(initial);
    event_loop.run_app(&mut app).expect("run app");
}
