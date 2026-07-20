// SPDX-License-Identifier: MIT OR Apache-2.0

//! LightPhotos — a fast macOS Lightroom-lite photo culling & develop tool.
//!
//! - Open a folder from Finder / "Open With" → thumbnail Grid; open a file → Loupe.
//! - `G` Grid, `E`/Enter Loupe (open selected), `Esc` backs out (Loupe→Grid, Grid→quit).
//! - Arrows move the grid selection / step the loupe; `1`–`5` rate, `0` clears.
//! - The always-visible top toolbar hosts the rating filter (All + 5 stars);
//!   `Shift`+`1`–`5` set a "≥ N" star filter and `Shift`+`0` clears it.
//! - `+`/`-` adjust thumbnail size (Grid).
//! - Loupe keeps the GPU pan/zoom path: scroll to zoom, Space+drag pan,
//!   Cmd+[ / Cmd+] rotate, grow-only fit. Alt+0 resets to 100%.
//! - `C` enters crop mode (Loupe): drag the 4 edges (hold `Shift` to keep the
//!   ratio), `C`/Enter commits, `Esc` cancels. `X` exports the selected image as
//!   a `.jpg` in the same folder (edits baked in), never overwriting.
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
mod burst;
mod catalog;
mod coregraphics;
mod develop;
mod export;
mod hash;
mod image_decode;
mod image_encode;
mod image_ops;
mod loader;
mod macos_delegate;
mod navigation;
mod paths;
mod renderer;
mod sharpness;
mod thumbnail;
mod trash;
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
            .with_title("LightPhotos")
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
        self.exporter = Some(export::Exporter::new());
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
        // egui widget) normally skip the app's own handling.
        let consumed = if let (Some(window), Some(state)) =
            (self.window.clone(), self.egui_state.as_mut())
        {
            let response = state.on_window_event(&*window, &event);
            if response.repaint {
                window.request_redraw();
            }
            response.consumed
        } else {
            false
        };
        if consumed {
            // Exception: arrow keys keep driving navigation even when a develop
            // slider still holds egui's keyboard focus (egui reports the key as
            // consumed for as long as the slider stays focused). Let them fall
            // through unless a slider is actively being dragged or we're cropping.
            let arrow_nav = matches!(
                event,
                WindowEvent::KeyboardInput {
                    event: winit::event::KeyEvent {
                        state: ElementState::Pressed,
                        physical_key: PhysicalKey::Code(
                            KeyCode::ArrowLeft
                                | KeyCode::ArrowRight
                                | KeyCode::ArrowUp
                                | KeyCode::ArrowDown
                        ),
                        ..
                    },
                    ..
                }
            ) && self.arrow_should_fall_through();
            if !arrow_nav {
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

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // The user confirmed quit in the Esc modal — exit the event loop (lets
        // Drop run for the loader/exporter, unlike a hard process::exit).
        if self.quit_requested {
            event_loop.exit();
            return;
        }

        // Drain all loader tiers once per frame.
        if let Some(loader) = &mut self.loader {
            let (full, thumbs, metas, exifs) = loader.poll_all();
            let any =
                !full.is_empty() || !thumbs.is_empty() || !metas.is_empty() || !exifs.is_empty();
            // Note: `loader`'s borrow ends at `poll_all` above (NLL), so these
            // `&mut self` calls are allowed even though `loader` is still in scope.
            if !metas.is_empty() {
                self.on_capture_times(metas);
            }
            if !exifs.is_empty() {
                self.on_exif_info(exifs);
            }
            if !thumbs.is_empty() {
                self.score_arrived_thumbs(&thumbs);
            }
            if any {
                self.try_show();
                self.request_redraw();
            }
        }
        // Drain finished background exports and fold them into the progress toast.
        let outcomes = self.exporter.as_ref().map(|e| e.poll()).unwrap_or_default();
        if !outcomes.is_empty() {
            self.on_export_outcomes(outcomes);
            self.request_redraw();
        }
        // While an export is in flight, poll the results channel a few times a
        // second instead of forcing a full egui re-tessellation + GPU submit
        // every vsync. WaitUntil wakes `about_to_wait` on a timer without a
        // redraw; the actual redraw only happens above when outcomes arrive.
        if self.export_progress.is_some() {
            event_loop.set_control_flow(ControlFlow::WaitUntil(
                std::time::Instant::now() + std::time::Duration::from_millis(100),
            ));
        } else {
            event_loop.set_control_flow(ControlFlow::Wait);
        }

        // Keep redrawing while working-set thumbnails are still loading.
        if self.request_working_thumbs() {
            self.request_redraw();
        }

        // Keep the loop alive while burst background work (capture-time reads,
        // off-screen member scoring) is outstanding. Worker-thread completions
        // don't wake the loop, so without this a settled grid would freeze burst
        // badges mid-computation until an unrelated event arrives.
        if self.request_burst_thumbs() {
            self.request_redraw();
        }
    }
}

/// True when running from inside a `.app` bundle (i.e. launched by Finder,
/// double-click, or "Open With"). Those launches never carry a CLI arg — a
/// file path instead arrives later via an AppleEvent (see
/// [`macos_delegate`]) — so the dev-CLI's "require an argument" rule doesn't
/// apply to them.
fn is_app_bundle() -> bool {
    std::env::current_exe()
        .ok()
        .is_some_and(|exe| exe.components().any(|c| c.as_os_str().to_string_lossy().ends_with(".app")))
}

fn print_usage_and_exit() -> ! {
    eprintln!("Usage: lightphotos <photo-or-folder>");
    eprintln!();
    eprintln!("  <photo-or-folder>  Path to a folder of photos (opens in Grid)");
    eprintln!("                     or a single photo (opens in Loupe).");
    std::process::exit(1);
}

fn main() {
    // A file/dir path may be passed on the command line. The dev CLI binary
    // requires one; the packaged .app doesn't (Finder "Open With" delivers
    // the path via an AppleEvent after launch, with no argv).
    let arg = std::env::args().nth(1).map(PathBuf::from);
    let initial = match arg {
        Some(p) if p.exists() => Some(p),
        Some(_) if is_app_bundle() => None,
        Some(_) => print_usage_and_exit(),
        None if is_app_bundle() => None,
        None => print_usage_and_exit(),
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    macos_delegate::set_proxy(event_loop.create_proxy());
    if !macos_delegate::install_open_handler() {
        eprintln!("[lightphotos] warning: could not install Finder open handler");
    }

    let mut app = App::new(initial);
    event_loop.run_app(&mut app).expect("run app");
}
