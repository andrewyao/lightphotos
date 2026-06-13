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

use std::collections::HashMap;
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
    /// Thumbnail longest-side pixels for the grid + filmstrip.
    thumb_px: u32,
    /// Columns the grid actually laid out last frame (for Up/Down row moves).
    grid_cols: usize,
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
            pending_initial: initial,
            mode: ViewMode::Grid,
            catalog,
            ratings: HashMap::new(),
            filter: None,
            filter_bar: false,
            visible: Vec::new(),
            sel: 0,
            thumb_px: THUMB_DEFAULT,
            grid_cols: 1,
            thumb_tex: HashMap::new(),
            zoom: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            rotations: HashMap::new(),
            loupe_viewport: None,
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

        let playlist = if is_dir {
            Playlist::from_dir(&path)
        } else {
            Playlist::from_file(&path)
        };

        // Seed the in-memory ratings mirror for everything in the folder.
        for p in playlist.entries() {
            if let Some(stars) = self.catalog.get(p) {
                self.ratings.insert(p.clone(), stars);
            }
        }

        let start_index = playlist.position();
        self.playlist = Some(playlist);
        self.recompute_visible();

        // Place the selection on the opened file (Loupe) or index 0 (Grid).
        self.sel = self.visible.iter().position(|&i| i == start_index).unwrap_or(0);

        self.mode = if is_dir { ViewMode::Grid } else { ViewMode::Loupe };
        if self.mode == ViewMode::Loupe {
            self.load_selected();
        }
        self.request_neighbors();
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

    /// The playlist index of the current selection, if any.
    fn selected_index(&self) -> Option<usize> {
        self.visible.get(self.sel).copied()
    }

    /// The path of the current selection, if any.
    fn selected_path(&self) -> Option<PathBuf> {
        let pl = self.playlist.as_ref()?;
        let idx = self.selected_index()?;
        pl.entry(idx).map(|p| p.to_path_buf())
    }

    /// In Loupe mode, make the selection the wanted image and request decode.
    fn load_selected(&mut self) {
        let Some(path) = self.selected_path() else { return };
        if let Some(loader) = &mut self.loader {
            loader.request(path.clone());
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

    /// Move the grid selection by (dx, dy) cells (clamped, row-aware).
    fn move_grid(&mut self, dx: isize, dy: isize) {
        if self.visible.is_empty() {
            return;
        }
        self.sel = navigation::grid_move(self.sel, self.visible.len(), self.grid_cols, dx, dy);
        self.request_redraw();
    }

    /// Enter Loupe on the current selection.
    fn enter_loupe(&mut self) {
        if self.selected_path().is_none() {
            return;
        }
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

    /// If the wanted image is decoded, upload it and reset the view to "fit".
    fn try_show(&mut self) {
        let Some(want) = self.want.clone() else { return };
        if self.shown.as_ref() == Some(&want) {
            return;
        }
        let img = match self.loader.as_ref().and_then(|l| l.get(&want)) {
            Some(img) => img,
            None => return,
        };
        if let Some(renderer) = self.renderer.as_mut() {
            renderer.set_image(&img);
        } else {
            return;
        }
        self.shown = Some(want);
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
        let ppp = self.egui_ctx.pixels_per_point().max(0.01);
        let (px, py) = (self.cursor.0 as f32 * ppp, self.cursor.1 as f32 * ppp);
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

    /// Request thumbnails for the working set (visible grid range + filmstrip
    /// neighbors of the selection). Returns true if any requested thumb is still
    /// missing (so the caller can keep redrawing until they arrive).
    fn request_working_thumbs(&mut self) -> bool {
        let Some(pl) = &self.playlist else { return false };
        let px = self.thumb_px;
        // Working set: in Grid request all visible (bounded by playlist size);
        // in Loupe just the filmstrip neighbors around the selection.
        let want_positions: Vec<usize> = match self.mode {
            ViewMode::Grid => (0..self.visible.len()).collect(),
            ViewMode::Loupe => {
                let lo = self.sel.saturating_sub(12);
                let hi = (self.sel + 12).min(self.visible.len().saturating_sub(1));
                if self.visible.is_empty() { Vec::new() } else { (lo..=hi).collect() }
            }
        };
        let paths: Vec<PathBuf> = want_positions
            .iter()
            .filter_map(|&pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i).map(|p| p.to_path_buf()))
            .collect();

        let mut any_missing = false;
        if let Some(loader) = &mut self.loader {
            for p in &paths {
                if loader.get_thumb(p, px).is_none() {
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
        let Some(pl) = &self.playlist else { return };
        let px = self.thumb_px;

        // Build the set of keys we want this frame.
        let positions: Vec<usize> = match self.mode {
            ViewMode::Grid => (0..self.visible.len()).collect(),
            ViewMode::Loupe => {
                if self.visible.is_empty() {
                    Vec::new()
                } else {
                    let lo = self.sel.saturating_sub(16);
                    let hi = (self.sel + 16).min(self.visible.len() - 1);
                    (lo..=hi).collect()
                }
            }
        };
        let wanted: Vec<(PathBuf, u32)> = positions
            .iter()
            .filter_map(|&pos| self.visible.get(pos).copied())
            .filter_map(|i| pl.entry(i).map(|p| (p.to_path_buf(), px)))
            .collect();

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
        renderer.render(image_viewport, Some(egui_paint));
    }

    /// Apply the actions egui emitted this frame.
    fn apply_ui_actions(&mut self, actions: Vec<ui::UiAction>) {
        for action in actions {
            match action {
                ui::UiAction::Select(pos) => {
                    if pos < self.visible.len() {
                        self.sel = pos;
                        self.request_redraw();
                    }
                }
                ui::UiAction::OpenLoupe(pos) => {
                    if pos < self.visible.len() {
                        self.sel = pos;
                        self.enter_loupe();
                    }
                }
                ui::UiAction::SetThumbPx(px) => {
                    self.thumb_px = px.clamp(THUMB_MIN, THUMB_MAX);
                    self.request_redraw();
                }
                ui::UiAction::SetFilter(f) => self.set_filter(f),
                ui::UiAction::SetRating(stars) => self.set_rating(stars),
            }
        }
    }
}

/// Accessors used by the egui UI module (`ui.rs`).
impl App {
    pub(crate) fn mode(&self) -> ViewMode {
        self.mode
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

    pub(crate) fn visible_len(&self) -> usize {
        self.visible.len()
    }

    pub(crate) fn set_grid_cols(&mut self, cols: usize) {
        self.grid_cols = cols.max(1);
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

            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging && self.mode == ViewMode::Loupe {
                    let dx = position.x - self.last_drag.0;
                    let dy = position.y - self.last_drag.1;
                    let ppp = self.egui_ctx.pixels_per_point() as f64;
                    self.pan.0 += (dx * ppp) as f32;
                    self.pan.1 += (dy * ppp) as f32;
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
            if !full.is_empty() {
                self.try_show();
            }
            if !thumbs.is_empty() {
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
