// SPDX-License-Identifier: GPL-3.0-or-later

//! LightPhotos, a fast Lightroom-lite photo culling and develop tool. This
//! crate root owns `main()` and the winit event loop, which turns window events
//! into `App` calls. Viewer state and behavior live in [`app`].

mod app;
mod autotone;
mod burst;
mod catalog;
#[cfg(target_os = "macos")]
mod coregraphics;
mod develop;
#[cfg(not(target_arch = "wasm32"))]
mod dialog;
mod duplicates;
mod export;
mod facequality;
mod featureprint;
mod hash;
mod i18n;
mod image_decode;
mod image_encode;
mod image_ops;
mod loader;
mod macos_delegate;
mod navigation;
mod paths;
mod phash;
#[cfg(feature = "hotpath")]
mod profile;
mod renderer;
mod segmentation;
mod sharpness;
mod thumbnail;
mod trash;
mod ui;
#[cfg(target_os = "macos")]
mod vision;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_canvas.rs"]
mod web_canvas;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_catalog_fs.rs"]
mod web_catalog_fs;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_export_fs.rs"]
mod web_export_fs;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_fs.rs"]
mod web_fs;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_thumb_cache.rs"]
mod web_thumb_cache;
#[cfg(target_arch = "wasm32")]
#[path = "web/web_worker_pool.rs"]
mod web_worker_pool;

#[cfg(not(target_arch = "wasm32"))]
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

/// Finish window setup once a `Renderer` exists. Native calls it from
/// `resumed`; wasm calls it from `about_to_wait` when the async renderer init
/// lands. `size` is passed in because winit's wasm `inner_size()` is stale here.
fn finish_window_setup(
    app: &mut App,
    window: Arc<Window>,
    renderer: Renderer,
    size: winit::dpi::PhysicalSize<u32>,
) {
    let loader = Loader::new(renderer.max_dim);

    let egui_state = egui_winit::State::new(
        app.egui_ctx.clone(),
        egui::ViewportId::ROOT,
        &*window,
        Some(window.scale_factor() as f32),
        None,
        None,
    );

    app.win_size = (size.width.max(1) as f32, size.height.max(1) as f32);
    app.window = Some(window);
    app.renderer = Some(renderer);
    app.loader = Some(loader);
    app.exporter = Some(export::Exporter::new());
    app.feature_pool = Some(featureprint::DistancePool::new());
    app.face_pool = Some(facequality::FacePool::new());
    app.egui_state = Some(egui_state);

    if let Some(path) = app.pending_initial.take() {
        loader::mark("opening initial path");
        app.open(path);
        loader::mark("initial open() returned");
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("LightPhotos")
            .with_inner_size(LogicalSize::new(1100.0, 800.0));
        // Create the window hidden and show it once the renderer is ready. wgpu
        // setup blocks this thread, and under a software rasterizer (llvmpipe)
        // it takes seconds. A visible X11/Wayland window that stops pumping
        // events that long gets a "not responding" dialog.
        #[cfg(not(target_arch = "wasm32"))]
        let attrs = attrs.with_visible(false);
        loader::mark("resumed: creating window");
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        loader::mark("window created; initializing wgpu");

        // `Renderer::new` is async because WebGPU device setup is a browser
        // Promise, and the browser main thread can't block on it. Native blocks
        // with `pollster`.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let size = window.inner_size();
            let renderer = pollster::block_on(Renderer::new(window.clone(), size));
            loader::mark("wgpu ready");
            finish_window_setup(self, window.clone(), renderer, size);
            window.set_visible(true);
            window.request_redraw();
        }
        #[cfg(target_arch = "wasm32")]
        {
            // winit creates a <canvas> on wasm but doesn't add it to the page.
            // `attach` adds it and returns the real size.
            let size = web_canvas::attach(&window);
            self.window = Some(window.clone());
            let tx = self.renderer_init_tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let renderer = Renderer::new(window, size).await;
                let _ = tx.send((renderer, size));
            });
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
        // egui sees each event first. Events it consumes skip the app.
        let consumed =
            if let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) {
                let response = state.on_window_event(&*window, &event);
                // egui_winit asks for a repaint on `RedrawRequested` itself.
                // Honoring that would redraw forever and `ControlFlow::Wait`
                // would never wait.
                if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
                    window.request_redraw();
                }
                response.consumed
            } else {
                false
            };
        if consumed {
            // Navigation keys may still reach the app. See
            // `nav_key_should_fall_through`.
            let nav_key = matches!(
                event,
                WindowEvent::KeyboardInput {
                    event: winit::event::KeyEvent {
                        state: ElementState::Pressed,
                        physical_key: PhysicalKey::Code(
                            KeyCode::ArrowLeft
                                | KeyCode::ArrowRight
                                | KeyCode::ArrowUp
                                | KeyCode::ArrowDown
                                | KeyCode::PageUp
                                | KeyCode::PageDown
                                | KeyCode::Tab
                        ),
                        ..
                    },
                    ..
                }
            ) && self.nav_key_should_fall_through();
            if !nav_key {
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
                // The preview decode is sized from the window, so a bigger
                // window may need a sharper one.
                self.try_show();
                self.request_redraw();
            }

            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                // Frames are skipped while occluded, so redraw when visible again.
                if !occluded {
                    self.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
            }

            WindowEvent::CursorMoved { position, .. } => {
                if self.dragging && self.mode == ViewMode::Loupe {
                    // `position` and `pan` are both physical pixels.
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

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
                ElementState::Pressed if self.space_down && self.mode == ViewMode::Loupe => {
                    self.dragging = true;
                    self.space_panned = true;
                    self.last_drag = self.cursor;
                }
                ElementState::Released => self.dragging = false,
                _ => {}
            },

            WindowEvent::MouseWheel { delta, .. } => {
                if self.mode == ViewMode::Loupe {
                    let (dx, dy) = match delta {
                        MouseScrollDelta::PixelDelta(p) => (p.x as f32, p.y as f32),
                        MouseScrollDelta::LineDelta(x, y) => (x * 20.0, y * 20.0),
                    };
                    self.on_scroll(dx, dy);
                }
            }

            WindowEvent::PinchGesture { delta, .. } => {
                if self.mode == ViewMode::Loupe && delta != 0.0 {
                    let factor = 1.0 + delta as f32;
                    let (cx, cy) = self.cursor_in_loupe();
                    self.zoom_at(factor, cx, cy);
                }
            }

            WindowEvent::KeyboardInput { event, .. } => match event.physical_key {
                // Holding Space and dragging pans the loupe, so Space acts
                // only on a release that didn't pan.
                PhysicalKey::Code(KeyCode::Space) => match event.state {
                    ElementState::Pressed if !self.space_down => {
                        self.space_down = true;
                        self.space_panned = false;
                    }
                    ElementState::Released if self.space_down => {
                        self.space_down = false;
                        if !self.space_panned {
                            self.handle_key(KeyCode::Space);
                        }
                    }
                    _ => {}
                },
                PhysicalKey::Code(code) if event.state == ElementState::Pressed => {
                    self.handle_key(code)
                }
                _ => {}
            },

            _ => {}
        }
    }

    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        self.save_edit();
        // AppKit delivers this from `applicationWillTerminate:` and then calls
        // `exit()` itself, so `main`'s return is never reached on Cmd+Q and
        // this is the last chance to get queued sidecars onto the disk. The
        // bound is what a stuck filesystem can hold up the quit for; 20 000
        // queued writes drain in about 2.5 s on APFS, and the usual backlog
        // is one record.
        #[cfg(not(target_arch = "wasm32"))]
        self.catalog
            .flush_blocking(std::time::Duration::from_secs(10));
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Exit the loop rather than `process::exit`, so loader and exporter
        // `Drop`s run.
        if self.quit_requested {
            event_loop.exit();
            return;
        }

        // Finish wasm window setup once the async renderer init lands.
        #[cfg(target_arch = "wasm32")]
        if let Ok((renderer, size)) = self.renderer_init_rx.try_recv() {
            loader::mark("wgpu ready (async)");
            if let Some(window) = self.window.clone() {
                finish_window_setup(self, window, renderer, size);
            }
            self.request_redraw();
        }

        // Outside the loader block below, because a queued sidecar write has
        // no loader to wait on and can outlive the folder it came from.
        self.catalog.pump();
        self.poll_delete();

        // True while web folder picking or listing is in flight. Feeds the poll
        // interval below.
        #[cfg(target_arch = "wasm32")]
        let web_folder_pending = {
            let pick_pending = self.poll_folder_pick();
            let listing_pending = self.poll_dir_listing();
            pick_pending || listing_pending
        };

        if let Some(loader) = &mut self.loader {
            let (full, thumbs, metas, exifs) = loader.poll_all();
            let any =
                !full.is_empty() || !thumbs.is_empty() || !metas.is_empty() || !exifs.is_empty();
            if !metas.is_empty() {
                self.on_capture_times(metas);
            }
            if !exifs.is_empty() {
                self.on_exif_info(exifs);
            }
            if !thumbs.is_empty() {
                self.score_arrived_thumbs(&thumbs);
                self.score_arrived_dup_thumbs(&thumbs);
            }
            // Runs even with no arrivals, so Auto Tone notices failed thumbnails.
            self.poll_auto_tone();
            if any {
                self.try_show();
                // Prefetch waits until the current photo is on screen, which
                // may now be true.
                self.request_neighbors();
                self.request_redraw();
            }
        }
        let catalog_load_pending = self.poll_catalog_load();

        let outcomes = self.exporter.as_ref().map(|e| e.poll()).unwrap_or_default();
        if !outcomes.is_empty() {
            self.on_export_outcomes(outcomes);
            self.request_redraw();
        }
        // Worker threads don't wake winit, so poll on a timer while work is
        // pending. `WaitUntil` re-polls without redrawing every vsync. A loupe
        // decode polls every 16 ms because the user is waiting on it; an export
        // only updates a toast, so 100 ms is enough.
        let image_pending = self.loader.as_ref().is_some_and(|l| l.has_pending_image())
            || self.selection_pending()
            || catalog_load_pending;
        #[cfg(target_arch = "wasm32")]
        let image_pending = image_pending || web_folder_pending || self.web_decode_pending();
        // A write backlog needs the tight interval on wasm, where `pump` is
        // the scheduler; on native the worker drains on its own and only the
        // toast needs refreshing.
        let poll_delay = if image_pending || self.bulk_delete_running() {
            Some(16)
        } else if self.export_progress.is_some() || self.catalog.backlog() > 0 {
            Some(if cfg!(target_arch = "wasm32") {
                16
            } else {
                100
            })
        } else {
            None
        };
        match poll_delay {
            Some(ms) => event_loop.set_control_flow(ControlFlow::WaitUntil(
                web_time::Instant::now() + std::time::Duration::from_millis(ms),
            )),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }

        if self.request_working_thumbs() {
            self.request_redraw();
        }
        // The loader has no workers on wasm, so web decodes are polled here.
        #[cfg(target_arch = "wasm32")]
        {
            self.prepare_web_thumb_cache();
            let web_thumbs = self.poll_web_thumbs();
            if !web_thumbs.is_empty() {
                self.score_arrived_thumbs(&web_thumbs);
                self.score_arrived_dup_thumbs(&web_thumbs);
                // Web thumbnails skip `loader.poll_all()`, so feed them to Auto
                // Tone here too.
                self.poll_auto_tone();
            }
            if self.request_web_thumbs() {
                self.request_redraw();
            }
            self.poll_web_preview();
            if self.request_web_preview() {
                self.request_redraw();
            }
            self.poll_web_full();
            if self.request_web_full() {
                self.request_redraw();
            }

            // Write each JPEG a Web Worker finished baking, then report the
            // completed writes.
            for r in self.web_worker_pool.poll_exports() {
                let crate::web_worker_pool::ExportPoolResult {
                    path,
                    folder,
                    dest_dir,
                    filename,
                    result,
                } = r;
                let tx = self.web_export_tx.clone();
                match result {
                    Ok(jpeg) => {
                        let file_handles = self.web_file_handles.clone();
                        let dest = dest_dir.join(&filename);
                        wasm_bindgen_futures::spawn_local(async move {
                            use crate::export::ExportFs;
                            let result = crate::web_export_fs::WebFs::new(folder, file_handles)
                                .write_atomic(&dest, &jpeg)
                                .await
                                .map(|()| dest.clone());
                            let _ = tx.send(crate::export::ExportOutcome { src: path, result });
                        });
                    }
                    Err(e) => {
                        let _ = tx.send(crate::export::ExportOutcome {
                            src: path,
                            result: Err(e),
                        });
                    }
                }
            }
            let export_outcomes: Vec<_> =
                std::iter::from_fn(|| self.web_export_rx.try_recv().ok()).collect();
            if !export_outcomes.is_empty() {
                self.on_export_outcomes(export_outcomes);
                self.request_redraw();
            }
        }

        // Each `request_*` below returns true while background work is still
        // outstanding. Worker completions don't wake the loop, so keep redrawing.
        if self.request_burst_thumbs() {
            self.request_redraw();
        }

        if self.request_dup_thumbs() {
            self.request_redraw();
        }

        self.poll_feature_prints();
        if self.request_feature_prints() {
            self.request_redraw();
        }

        self.poll_face_quality();
        if self.request_face_quality() {
            self.request_redraw();
        }

        // Segmentation starts from user actions, so it has no `request_*` call.
        self.poll_selection_mask();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn print_usage_and_exit(code: i32) -> ! {
    eprintln!("Usage: lightphotos [photo-or-folder]");
    eprintln!();
    eprintln!("  photo-or-folder  Optional path to a folder of photos (opens in");
    eprintln!("                   Grid) or a single photo (opens in Loupe). With");
    eprintln!("                   no path, the app opens on a landing page with a");
    eprintln!("                   \"Choose Folder\" button.");
    std::process::exit(code);
}

#[cfg(not(target_arch = "wasm32"))]
#[hotpath::main(percentiles = [50, 95, 99])]
fn main() {
    loader::start_clock();

    // Returning rather than exiting: that drops the hotpath guard, which is
    // what prints the report.
    #[cfg(feature = "hotpath")]
    if profile::run_from_args() {
        return;
    }

    // The path argument is optional. Without one the app opens on the landing
    // page. A packaged .app may instead get its path from Finder later.
    let initial = match std::env::args().nth(1).as_deref() {
        Some("-h" | "--help") => print_usage_and_exit(0),
        Some(s) => {
            let p = PathBuf::from(s);
            if p.exists() {
                Some(p)
            } else {
                eprintln!("[lightphotos] no such path: {s} — opening the folder picker");
                None
            }
        }
        None => None,
    };

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    macos_delegate::set_proxy(event_loop.create_proxy());
    if !macos_delegate::install_open_handler() {
        eprintln!("[lightphotos] warning: could not install Finder open handler");
    }

    i18n::init();
    loader::mark("event loop built; constructing App");
    let mut app = App::new(initial);
    loader::mark("App constructed; entering event loop");
    event_loop.run_app(&mut app).expect("run app");
}

/// wasm entry point. Uses `spawn_app`, not the blocking `run_app`, because
/// the browser main thread must return to the browser's event loop.
#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    loader::start_clock();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    i18n::init();
    loader::mark("event loop built; constructing App");
    let app = App::new(None);
    loader::mark("App constructed; entering event loop");

    use winit::platform::web::EventLoopExtWebSys;
    event_loop.spawn_app(app);
}
