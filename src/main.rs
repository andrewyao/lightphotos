// SPDX-License-Identifier: GPL-3.0-or-later

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
// All four live under src/web/ (physically separated from native-only
// code), but keep their existing flat module names via #[path] — every
// `crate::web_fs::`/etc. call site elsewhere in the codebase resolves by
// module path, not file location, so this move needed no other file's
// `use` statements touched.
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
mod image_decode;
mod image_encode;
mod image_ops;
mod loader;
mod macos_delegate;
mod navigation;
mod paths;
mod phash;
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

/// The rest of window setup, once a `Renderer` exists — shared by native's
/// `resumed()` (called directly, straight after the blocking
/// `pollster::block_on`) and wasm32's `about_to_wait` poll (called once the
/// async renderer-init task's result arrives over `renderer_init_rx`; see
/// `resumed()`'s doc comment). Everything here is itself synchronous and
/// platform-independent — only *how the Renderer got here* differs.
///
/// `size` is passed in rather than read via `window.inner_size()` here too —
/// same reason as `Renderer::new`'s identical parameter (see its doc
/// comment): winit's wasm32 `inner_size()` is a stale cache at this point,
/// not a live query, and `app.win_size` below drives egui's own layout
/// sizing, so getting this wrong doesn't just affect wgpu.
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
        // Native: create the window hidden and only map it once the renderer is
        // up and the first frame's state is ready (below). wgpu device/pipeline
        // bring-up runs synchronously on this thread via `pollster::block_on`,
        // and under a software rasterizer (llvmpipe in a VM) it takes several
        // seconds — long enough that a *mapped* X11/Wayland window that can't
        // pump events in the meantime trips the desktop's "application is not
        // responding — Wait / Force Quit" dialog. An unmapped window is never
        // pinged, so the user just sees the window appear a beat later, already
        // drawn, instead of a frozen frame behind a system prompt.
        #[cfg(not(target_arch = "wasm32"))]
        let attrs = attrs.with_visible(false);
        loader::mark("resumed: creating window");
        let window = Arc::new(event_loop.create_window(attrs).expect("create window"));
        loader::mark("window created; initializing wgpu");

        // `Renderer::new` is async (see its doc comment) — wgpu's
        // adapter/device acquisition is a real browser Promise under WebGPU,
        // and the browser main thread can never block waiting on one.
        // `pollster::block_on` (native only — it has no wasm32 support at
        // all) is the one native/wasm fork in this function; everything
        // finish_window_setup does afterward is shared, unchanged code.
        #[cfg(not(target_arch = "wasm32"))]
        {
            let size = window.inner_size();
            let renderer = pollster::block_on(Renderer::new(window.clone(), size));
            loader::mark("wgpu ready");
            finish_window_setup(self, window.clone(), renderer, size);
            // Renderer and initial open() are done — map the window and draw.
            window.set_visible(true);
            window.request_redraw();
        }
        #[cfg(target_arch = "wasm32")]
        {
            // winit creates its own <canvas> on wasm32 but doesn't insert it
            // into the page for you — do that now, before anything tries to
            // draw. Also hands back the real viewport size: window.inner_size()
            // can't be used here (see Renderer::new's doc comment).
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
        // Give egui first crack at the event. Consumed events (clicks/keys in an
        // egui widget) normally skip the app's own handling.
        let consumed =
            if let (Some(window), Some(state)) = (self.window.clone(), self.egui_state.as_mut()) {
                let response = state.on_window_event(&*window, &event);
                // egui_winit reports `repaint: true` for `RedrawRequested` itself
                // (it's in its "things that may require repaint" bucket alongside
                // Resized/Moved/etc.) — forwarding that into another
                // `request_redraw()` would re-arm the very redraw we're about to
                // perform in the `RedrawRequested` arm below, forever, regardless
                // of whether anything actually changed. Every other event in that
                // bucket legitimately means "something happened, please repaint";
                // this one alone must be excluded or `ControlFlow::Wait` never
                // actually gets to wait.
                if response.repaint && !matches!(event, WindowEvent::RedrawRequested) {
                    window.request_redraw();
                }
                response.consumed
            } else {
                false
            };
        if consumed {
            // Exception: arrow keys keep driving navigation even when a develop
            // slider still holds egui's keyboard focus (egui reports the key as
            // consumed for as long as the slider stays focused), and Tab is
            // always reported as consumed by egui_winit regardless of focus (its
            // own hardcoded widget-focus traversal — see the Tab-stripping
            // comment in `App::redraw`). Let both fall through unless a slider is
            // actively being dragged or we're cropping.
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
                // The preview decode is sized from the window, so a resize can
                // mean the loupe now wants a sharper one than it is showing.
                self.try_show();
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

            WindowEvent::MouseInput {
                state,
                button: MouseButton::Left,
                ..
            } => match state {
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

        // Pick up the async-initialized Renderer once it lands — see
        // resumed()'s doc comment. `window` is already set (resumed() does
        // that synchronously before spawning); everything else (loader,
        // exporter, worker pools, egui_state, the initial open()) is only
        // constructed now, once there's a real device to hand them.
        #[cfg(target_arch = "wasm32")]
        if let Ok((renderer, size)) = self.renderer_init_rx.try_recv() {
            loader::mark("wgpu ready (async)");
            if let Some(window) = self.window.clone() {
                finish_window_setup(self, window, renderer, size);
            }
            self.request_redraw();
        }

        // Landing page's folder picker (see ui::draw's landing-page branch
        // and app/web.rs) — its own bool return feeds the poll-cadence
        // calculation below, same convention as request_working_thumbs.
        #[cfg(target_arch = "wasm32")]
        let web_folder_pending = {
            // Fire-and-forget sidecar writes/deletes (catalog.rs's wasm32
            // `write_sidecar`/`delete_sidecar`) report failures
            // asynchronously — drain those into `last_error` every frame,
            // same convention as every other wasm32 poll here.
            self.poll_catalog_persist_errors();
            self.poll_web_deletes();
            let pick_pending = self.poll_folder_pick();
            let listing_pending = self.poll_dir_listing();
            pick_pending || listing_pending
        };

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
                self.score_arrived_dup_thumbs(&thumbs);
            }
            if any {
                self.try_show();
                // Now that something landed, the current photo may be on screen
                // — which is the condition `request_neighbors` waits for before
                // it will spend workers on prefetch.
                self.request_neighbors();
                self.request_redraw();
            }
        }
        // Drain finished background catalog (sidecar) loads. Redraws itself
        // when a load actually reconciles into the ratings/edits/touchups/
        // rotations mirrors; its return value only feeds `image_pending`
        // below so the loop keeps polling at the tight cadence until it lands.
        let catalog_load_pending = self.poll_catalog_load();

        // Drain finished background exports and fold them into the progress toast.
        let outcomes = self.exporter.as_ref().map(|e| e.poll()).unwrap_or_default();
        if !outcomes.is_empty() {
            self.on_export_outcomes(outcomes);
            self.request_redraw();
        }
        // Worker threads finishing a job do not wake winit, so anything whose
        // result arrives over a channel needs the loop kept alive or it sits
        // undrained until some unrelated event happens to arrive — which for a
        // loupe decode means the blurry placeholder stays on screen long after
        // the sharp image is ready. WaitUntil wakes `about_to_wait` on a timer
        // to re-poll *without* forcing a full egui re-tessellation + GPU submit
        // every vsync; the actual redraw only happens above, when results land.
        //
        // A loupe decode is what the user is staring at, so it gets a tight
        // cadence; an export only feeds a progress toast, so it gets a lazy one.
        let image_pending = self.loader.as_ref().is_some_and(|l| l.has_pending_image())
            || self.selection_pending()
            || catalog_load_pending;
        #[cfg(target_arch = "wasm32")]
        let image_pending = image_pending
            || web_folder_pending
            || self.web_decode_pending()
            || self.web_delete_pending.is_some();
        let poll_delay = if image_pending {
            Some(16)
        } else if self.export_progress.is_some() {
            Some(100)
        } else {
            None
        };
        match poll_delay {
            Some(ms) => event_loop.set_control_flow(ControlFlow::WaitUntil(
                web_time::Instant::now() + std::time::Duration::from_millis(ms),
            )),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }

        // Keep redrawing while working-set thumbnails are still loading.
        if self.request_working_thumbs() {
            self.request_redraw();
        }
        // wasm32's own thumbnail path — loader.rs's worker queue above has
        // no workers to service it yet (see app/web.rs). Drain first (a
        // decode that finished this frame should count toward "did
        // anything arrive" the same way loader results do), then keep
        // requesting/redrawing while any are still outstanding.
        #[cfg(target_arch = "wasm32")]
        {
            let web_thumbs = self.poll_web_thumbs();
            if !web_thumbs.is_empty() {
                self.score_arrived_thumbs(&web_thumbs);
                self.score_arrived_dup_thumbs(&web_thumbs);
            }
            if self.request_web_thumbs() {
                self.request_redraw();
            }
            // Loupe tier — poll_web_preview calls try_show() itself once
            // something lands, same as the native loader-arrival branch
            // above does for its own tier.
            self.poll_web_preview();
            if self.request_web_preview() {
                self.request_redraw();
            }
            // Zoom-triggered full-resolution tier — see
            // `ensure_full_for_zoom`'s wasm32 branch for what enqueues this.
            self.poll_web_full();
            if self.request_web_full() {
                self.request_redraw();
            }

            // Pipeline 3 (export): the Web Worker has finished baking JPEG
            // bytes; hand each to `WebFs::write_atomic` (async), then drain
            // the completed writes into the shared `on_export_outcomes`.
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

        // Keep the loop alive while burst background work (capture-time reads,
        // off-screen member scoring) is outstanding. Worker-thread completions
        // don't wake the loop, so without this a settled grid would freeze burst
        // badges mid-computation until an unrelated event arrives.
        if self.request_burst_thumbs() {
            self.request_redraw();
        }

        // Same rationale as above, for the content-duplicate dHash pass.
        if self.request_dup_thumbs() {
            self.request_redraw();
        }

        // Drain finished feature-print comparisons (the second, Vision-backed
        // refinement tier), then keep polling while any are still in flight.
        self.poll_feature_prints();
        if self.request_feature_prints() {
            self.request_redraw();
        }

        // Same again for the face/eyes-closed pass, which runs over whatever
        // the two grouping passes above have already identified.
        self.poll_face_quality();
        if self.request_face_quality() {
            self.request_redraw();
        }

        // Pick up a finished subject-segmentation run. No matching "request"
        // call here: unlike the passes above, that one is driven by the user
        // switching the overlay on or stepping to another photo.
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
fn main() {
    loader::start_clock();
    // A file/dir path may be passed on the command line, but it's optional:
    // with none (or an unreadable one) the app opens on the landing page and
    // the user picks a folder there. A packaged .app also gets its path this
    // way *or* later via an AppleEvent (Finder "Open With", no argv).
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

    loader::mark("event loop built; constructing App");
    let mut app = App::new(initial);
    loader::mark("App constructed; entering event loop");
    event_loop.run_app(&mut app).expect("run app");
}

/// wasm32 entry point. No CLI args (no argv on the web) — nothing to open
/// yet; picking a folder is File System Access's `showDirectoryPicker`,
/// wired up in a later milestone (see the wasm port plan's M1), not
/// something available at process-start the way a CLI arg is.
///
/// `EventLoopExtWebSys::spawn_app`, not `run_app`: winit's web backend can't
/// use the blocking native entry point at all — the browser main thread must
/// return control to the browser's own event loop rather than looping
/// forever inside Rust, so winit instead schedules the app's callbacks via
/// the browser's normal animation-frame/event machinery under the hood.
#[cfg(target_arch = "wasm32")]
fn main() {
    console_error_panic_hook::set_once();
    loader::start_clock();

    let event_loop = EventLoop::<UserEvent>::with_user_event()
        .build()
        .expect("build event loop");
    event_loop.set_control_flow(ControlFlow::Wait);

    loader::mark("event loop built; constructing App");
    let app = App::new(None);
    loader::mark("App constructed; entering event loop");

    use winit::platform::web::EventLoopExtWebSys;
    event_loop.spawn_app(app);
}
