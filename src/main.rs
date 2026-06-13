//! A fast macOS image viewer.
//!
//! - Open from Finder / "Open With" (via a custom NSApplicationDelegate)
//! - Scroll to zoom (anchored at the cursor), trackpad pinch to zoom
//! - Hold Space + drag to pan
//! - Alt+0 resets to 100% (1:1 pixels)
//! - Cmd+[ / Cmd+] rotate 90° left / right (remembered per image)
//! - Arrow keys go to the previous / next image in the folder
//!
//! Speed: images decode on a background thread (Apple ImageIO) and live as a
//! GPU texture; zoom/pan only update a tiny transform uniform, never re-decode.

mod catalog;
mod image_decode;
mod loader;
mod macos_delegate;
mod navigation;
mod renderer;
mod thumbnail;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, ModifiersState, PhysicalKey};
use winit::window::{Window, WindowId};

use loader::Loader;
use macos_delegate::UserEvent;
use navigation::Playlist;
use renderer::Renderer;

const MIN_ZOOM: f32 = 0.02;
const MAX_ZOOM: f32 = 64.0;

struct App {
    window: Option<Arc<Window>>,
    renderer: Option<Renderer>,
    loader: Option<Loader>,
    playlist: Option<Playlist>,

    /// Path we want shown (may still be decoding).
    want: Option<PathBuf>,
    /// Path currently uploaded to the GPU.
    shown: Option<PathBuf>,
    /// A file requested before the window/renderer existed.
    pending_initial: Option<PathBuf>,

    // View state.
    zoom: f32,
    pan: (f32, f32), // screen-space pixel coords of the image's top-left corner
    win_size: (f32, f32),
    /// True while the view is auto-fit to the window (so a resize re-fits).
    fitted: bool,
    /// Per-image rotation, in 90° clockwise steps (0..=3). Lightroom-style:
    /// each image remembers its own rotation for the session.
    rotations: HashMap<PathBuf, u8>,

    // Input state.
    cursor: (f64, f64),
    modifiers: ModifiersState,
    space_down: bool,
    dragging: bool,
    last_drag: (f64, f64),
}

impl App {
    fn new(initial: Option<PathBuf>) -> Self {
        Self {
            window: None,
            renderer: None,
            loader: None,
            playlist: None,
            want: None,
            shown: None,
            pending_initial: initial,
            zoom: 1.0,
            pan: (0.0, 0.0),
            win_size: (1.0, 1.0),
            fitted: false,
            rotations: HashMap::new(),
            cursor: (0.0, 0.0),
            modifiers: ModifiersState::empty(),
            space_down: false,
            dragging: false,
            last_drag: (0.0, 0.0),
        }
    }

    /// Open a file: build the folder playlist, request decode of it + neighbors.
    fn open(&mut self, path: PathBuf) {
        eprintln!("[image-viewer] open: {}", path.display());
        let playlist = Playlist::from_file(&path);
        let current = playlist.current().to_path_buf();
        let neighbors = playlist.neighbors();
        if let Some(loader) = &mut self.loader {
            loader.request(current.clone());
            for n in neighbors {
                loader.request(n);
            }
        }
        self.playlist = Some(playlist);
        self.want = Some(current);
        self.try_show();
    }

    /// Navigate within the current playlist.
    fn navigate(&mut self, forward: bool) {
        let next = match &mut self.playlist {
            Some(p) => {
                if forward { p.next().to_path_buf() } else { p.prev().to_path_buf() }
            }
            None => return,
        };
        let neighbors = self.playlist.as_ref().map(|p| p.neighbors()).unwrap_or_default();
        if let Some(loader) = &mut self.loader {
            loader.request(next.clone());
            for n in neighbors {
                loader.request(n);
            }
        }
        self.want = Some(next);
        self.try_show();
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
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn update_window_title(&self) {
        if let (Some(w), Some(p), Some(pl)) = (&self.window, &self.shown, &self.playlist) {
            let name = p.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
            w.set_title(&format!("{}  ({}/{})", name, pl.position() + 1, pl.len()));
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

    /// Fit to window, centered, *grow-only*: enlarge an image small enough to fit
    /// the window so it fills it; leave an image already larger than the window in
    /// either dimension at 100% (1:1 pixels) rather than shrinking it.
    fn fit_to_window(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.win_size;
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
        let (ww, wh) = self.win_size;
        self.pan = ((ww - iw * self.zoom) / 2.0, (wh - ih * self.zoom) / 2.0);
    }

    /// Zoom by `factor`, keeping the image point under (cx, cy) fixed.
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

    /// Recompute the shader transform from the current view state.
    fn push_transform(&mut self) {
        let (iw, ih) = self.display_size();
        let (ww, wh) = self.win_size;
        let denom_x = self.zoom * iw;
        let denom_y = self.zoom * ih;
        let scale = [ww / denom_x, wh / denom_y];
        let offset = [-self.pan.0 / denom_x, -self.pan.1 / denom_y];
        // Row-major 2x2 mapping display-UV -> texture-UV for the current rotation.
        let rot = match self.current_rotation() {
            1 => [0.0, 1.0, -1.0, 0.0],
            2 => [-1.0, 0.0, 0.0, -1.0],
            3 => [0.0, -1.0, 1.0, 0.0],
            _ => [1.0, 0.0, 0.0, 1.0],
        };
        if let Some(r) = &mut self.renderer {
            r.set_transform(scale, offset, rot);
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
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

        let size = window.inner_size();
        self.win_size = (size.width.max(1) as f32, size.height.max(1) as f32);
        self.window = Some(window);
        self.renderer = Some(renderer);
        self.loader = Some(loader);

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
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                self.win_size = (size.width.max(1) as f32, size.height.max(1) as f32);
                if let Some(r) = &mut self.renderer {
                    r.resize(size.width, size.height);
                }
                // Keep a fitted image fitted (preserve aspect ratio); leave a
                // manually zoomed/panned view exactly as the user set it.
                if self.fitted {
                    self.fit_to_window();
                } else {
                    self.push_transform();
                }
            }

            WindowEvent::RedrawRequested => {
                if let Some(r) = &mut self.renderer {
                    r.render();
                }
            }

            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging {
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
                ElementState::Pressed if self.space_down => {
                    self.dragging = true;
                    self.last_drag = self.cursor;
                }
                ElementState::Released => self.dragging = false,
                _ => {}
            },

            WindowEvent::MouseWheel { delta, .. } => {
                let s = match delta {
                    MouseScrollDelta::PixelDelta(p) => p.y as f32,
                    MouseScrollDelta::LineDelta(_, y) => y * 20.0,
                };
                if s != 0.0 {
                    let factor = (s * 0.0025).exp();
                    let (cx, cy) = (self.cursor.0 as f32, self.cursor.1 as f32);
                    self.zoom_at(factor, cx, cy);
                }
            }

            WindowEvent::PinchGesture { delta, .. } => {
                if delta != 0.0 {
                    let factor = 1.0 + delta as f32;
                    let (cx, cy) = (self.cursor.0 as f32, self.cursor.1 as f32);
                    self.zoom_at(factor, cx, cy);
                }
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::Space => self.space_down = pressed,
                        KeyCode::Digit0 if pressed && self.modifiers.alt_key() => self.reset_100(),
                        KeyCode::BracketLeft if pressed && self.modifiers.super_key() => self.rotate(false),
                        KeyCode::BracketRight if pressed && self.modifiers.super_key() => self.rotate(true),
                        KeyCode::ArrowLeft | KeyCode::ArrowUp if pressed => self.navigate(false),
                        KeyCode::ArrowRight | KeyCode::ArrowDown if pressed => self.navigate(true),
                        KeyCode::Escape if pressed => event_loop.exit(),
                        _ => {}
                    }
                }
            }

            _ => {}
        }
    }

    fn about_to_wait(&mut self, _event_loop: &ActiveEventLoop) {
        if let Some(loader) = &mut self.loader {
            let arrived = loader.poll();
            if !arrived.is_empty() {
                self.try_show();
            }
        }
    }
}

fn main() {
    // A file path may be passed on the command line (e.g. `open -a` or CLI use).
    let initial = std::env::args().nth(1).map(PathBuf::from).filter(|p| p.exists());

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    // Make the proxy available to the Finder open-file handler, then inject
    // `application:openURLs:` into winit's delegate class. winit creates and
    // registers that class during `EventLoop::build` above, and we do this
    // before `run_app` (i.e. before `[NSApplication run]` processes any Apple
    // events), so even the launch-time "open document" event is delivered.
    macos_delegate::set_proxy(event_loop.create_proxy());
    if !macos_delegate::install_open_handler() {
        eprintln!("[image-viewer] warning: could not install Finder open handler");
    }

    let mut app = App::new(initial);
    event_loop.run_app(&mut app).expect("run app");
}
