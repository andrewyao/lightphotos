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
//!
//! This file is the crate root: it owns the winit event loop (translating raw
//! window events into `App` calls) and `main()`. All viewer state and behavior
//! lives in [`app`]; the other modules are the supporting layers it coordinates.

mod app;
mod catalog;
mod develop;
mod image_decode;
mod loader;
mod macos_delegate;
mod navigation;
mod paths;
mod renderer;
mod thumbnail;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{Window, WindowId};

use app::{App, ViewMode};
use loader::Loader;
use macos_delegate::UserEvent;
use renderer::Renderer;

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
