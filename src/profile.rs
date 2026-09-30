// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless driver for the paths a culling session actually waits on.
//! Listing a folder gates the rest and always runs. Ten phases sit behind
//! it. The grid and the filmstrip both fill thumbnails and differ only in
//! the shape of their working set, and the scroll phase is the grid's working
//! set on the move, which is where a stale backlog shows. Opening a photo splits into the first
//! pixels on screen and the preview escalation that sharpens them. The
//! full-resolution decode is what zooming past the preview costs. Auto Tone,
//! a batch export and the Vision signals are the jobs a user starts and then
//! waits out. Subject selection is the wait behind the Loupe's "Show
//! selection" button. Compiled only under the `hotpath` feature.
//!
//! It exists because a report is only worth acting on if the next person can
//! reproduce it. Driving the window by hand gives a different scroll depth and
//! a different cache state every run, so the numbers cannot be compared
//! before and after a change. This runs the same `navigation`, `catalog`,
//! `Loader`, `thumbnail` and `export` code the window drives, minus egui and
//! the GPU, over a folder named on the command line, and then returns so the
//! hotpath guard drops and prints its report.
//!
//! `LIGHTPHOTOS_PROFILE_PHASES` narrows a run to a comma-separated list of
//! phase keys, and unset means every phase. `LIGHTPHOTOS_PROFILE_THUMBS`,
//! `_OPENS`, `_PREVIEW_PX`, `_FULLS`, `_EXPORTS` and `_VISION` size the
//! phases. `_SCROLL_ROWS`, `_SCROLL_COLS` and `_STEP_MS` shape the scroll
//! and the filmstrip walk. `LIGHTPHOTOS_PROFILE_COLD=1` clears the folder's cached
//! thumbnails first, so the grid phase measures a first visit.
//!
//! ```sh
//! cargo run --release --features hotpath -- --profile ~/Pictures/Trip
//! LIGHTPHOTOS_PROFILE_PHASES=full,export cargo run --release --features hotpath-alloc -- --profile ~/Pictures/Trip
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::app::{grid_working_range, load_order, strip_working_range};
use crate::loader::Loader;
use crate::navigation::Playlist;
use crate::thumbnail::THUMB_PX;

/// One scripted run. The counts are the shape of a real session rather than
/// the whole folder: the grid paints a screenful before the user scrolls, and
/// the wait that matters in the Loupe is the first few photos, not the 500th.
struct Run {
    dir: PathBuf,
    /// Photos whose thumbnail the grid phase fills. Two screenfuls at the
    /// default window size. Also how far the scroll phase scrolls.
    thumbs: usize,
    /// The scroll phase's simulated viewport.
    scroll_rows: usize,
    scroll_cols: usize,
    /// Time between two scroll rows or two filmstrip steps. 16 ms is a fast
    /// wheel flick, or an arrow key held down.
    step: Duration,
    /// Photos the Loupe phase opens, stepping like next / next / next.
    opens: usize,
    /// Longest side the Loupe asks for: a 1100pt window on a 2x display.
    preview_px: u32,
    /// Photos the full-resolution phase decodes. Small by default, because
    /// each one is the whole frame in RGBA and the allocation report is the
    /// reason to run this phase at all.
    fulls: usize,
    /// Photos the export phase writes out. Each one is a full-resolution
    /// decode, a bake and a JPEG encode, so a handful is already a minute's
    /// work on a folder of RAWs.
    exports: usize,
    /// Photos the Vision phase runs faces and segmentation over. Small by
    /// default because every Vision call decodes the file at full resolution
    /// itself.
    vision: usize,
    /// Delete this folder's cached thumbnails first, so the grid phase
    /// measures a first visit rather than a revisit.
    cold: bool,
    /// The loader's cache sizes, `LIGHTPHOTOS_CACHE_*` overrides included, so
    /// a tuning run profiles the limits it names.
    limits: crate::cache_limits::CacheLimits,
}

impl Run {
    fn from_env(dir: PathBuf) -> Run {
        let count = |key: &str, fallback: usize| {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(fallback)
        };
        Run {
            dir,
            thumbs: count("LIGHTPHOTOS_PROFILE_THUMBS", 120),
            scroll_rows: count("LIGHTPHOTOS_PROFILE_SCROLL_ROWS", 5),
            scroll_cols: count("LIGHTPHOTOS_PROFILE_SCROLL_COLS", 6),
            step: Duration::from_millis(count("LIGHTPHOTOS_PROFILE_STEP_MS", 16) as u64),
            opens: count("LIGHTPHOTOS_PROFILE_OPENS", 10),
            preview_px: count("LIGHTPHOTOS_PROFILE_PREVIEW_PX", 2200) as u32,
            fulls: count("LIGHTPHOTOS_PROFILE_FULLS", 5),
            exports: count("LIGHTPHOTOS_PROFILE_EXPORTS", 5),
            vision: count("LIGHTPHOTOS_PROFILE_VISION", 8),
            cold: std::env::var("LIGHTPHOTOS_PROFILE_COLD").as_deref() == Ok("1"),
            limits: crate::cache_limits::CacheLimits::from_env(),
        }
    }
}

/// One scripted phase. `label` is the hotpath report label. `key` is the
/// short name `LIGHTPHOTOS_PROFILE_PHASES` selects the phase by, kept apart
/// from the label so a run can be narrowed by typing `grid` rather than
/// `thumbnail_grid`.
struct Phase {
    label: &'static str,
    key: &'static str,
    run: fn(&Run, &[PathBuf]),
}

/// Every phase `folder_load` feeds, in the order a session meets them. A
/// table rather than a run of calls in `drive`, so adding a phase is one row
/// and the selector has something to filter.
const PHASES: &[Phase] = &[
    Phase {
        label: "path/thumbnail_grid",
        key: "grid",
        run: Run::thumbnail_grid,
    },
    Phase {
        label: "path/thumbnail_strip",
        key: "strip",
        run: Run::thumbnail_strip,
    },
    Phase {
        label: "path/scroll_grid",
        key: "scroll",
        run: Run::scroll_grid,
    },
    Phase {
        label: "path/ui_frame",
        key: "frame",
        run: Run::ui_frame,
    },
    Phase {
        label: "path/open_photo",
        key: "open",
        run: Run::open_photos,
    },
    Phase {
        label: "path/decode_full",
        key: "full",
        run: Run::decode_full,
    },
    Phase {
        label: "path/auto_tone",
        key: "auto_tone",
        run: Run::auto_tone,
    },
    Phase {
        label: "path/export",
        key: "export",
        run: Run::export,
    },
    Phase {
        label: "path/select_subject",
        key: "select_subject",
        run: Run::select_subject,
    },
    Phase {
        label: "path/vision_signals",
        key: "vision",
        run: Run::vision_signals,
    },
];

/// The phases `LIGHTPHOTOS_PROFILE_PHASES` names, in table order. Unset means
/// every phase. An unknown key is a typo that would otherwise profile nothing
/// without saying why, so it is named on stderr and dropped rather than
/// failing the run.
fn selected_phases() -> Vec<&'static Phase> {
    let Ok(list) = std::env::var("LIGHTPHOTOS_PROFILE_PHASES") else {
        return PHASES.iter().collect();
    };
    let mut wanted: Vec<&str> = Vec::new();
    for key in list.split(',').map(str::trim).filter(|k| !k.is_empty()) {
        if PHASES.iter().any(|p| p.key == key) {
            wanted.push(key);
        } else {
            let valid: Vec<&str> = PHASES.iter().map(|p| p.key).collect();
            eprintln!(
                "[profile] unknown phase {key:?}, skipped. Valid keys: {}",
                valid.join(", ")
            );
        }
    }
    PHASES.iter().filter(|p| wanted.contains(&p.key)).collect()
}

/// Runs the scripted paths when `--profile <dir>` was passed, and reports
/// whether it did, so `main` can skip opening a window.
pub fn run_from_args() -> bool {
    let mut args = std::env::args().skip(1);
    let Some(dir) = args
        .find(|a| a == "--profile")
        .and_then(|_| args.next())
        .map(PathBuf::from)
    else {
        return false;
    };
    if !dir.is_dir() {
        eprintln!("[profile] not a folder: {}", dir.display());
        return true;
    }
    Run::from_env(dir).drive();
    true
}

impl Run {
    fn drive(&self) {
        eprintln!("[profile] {:?}", self.limits);
        if self.cold {
            let removed = drop_thumb_cache(&self.dir);
            let signals = drop_signal_cache(&self.dir);
            eprintln!(
                "[profile] cold start: removed {removed} cached thumbnails, \
                 signal cache present: {signals}"
            );
        }

        let playlist = hotpath::measure_block!("path/folder_load", self.folder_load());
        let photos = playlist.entries().to_vec();
        eprintln!(
            "[profile] {} photos in {}",
            photos.len(),
            self.dir.display()
        );
        if photos.is_empty() {
            return;
        }

        for phase in selected_phases() {
            hotpath::measure_block!(phase.label, (phase.run)(self, &photos));
        }
    }

    /// What the Loupe's "Show selection" button costs, and the only path that
    /// builds a `segmentation::Mask`. `App::request_selection_mask` runs this
    /// same call on a worker thread, one photo per click.
    ///
    /// The masks are held until the phase ends rather than dropped one by one,
    /// because the Loupe holds one for as long as the photo is on screen and a
    /// live-object census of a mask discarded immediately would read zero.
    fn select_subject(&self, photos: &[PathBuf]) {
        let wanted = photos.iter().take(self.opens);
        let masks: Vec<_> = wanted
            .filter_map(|path| crate::segmentation::segment(path).ok())
            .collect();
        let pixels: usize = masks.iter().map(|m| m.alpha.len()).sum();
        eprintln!(
            "[profile] {} of {} photos have a subject, {pixels} mask pixels held",
            masks.len(),
            self.opens.min(photos.len()),
        );
    }

    #[cfg(target_os = "macos")]
    fn vision_signals(&self, photos: &[PathBuf]) {
        let wanted: Vec<&PathBuf> = photos.iter().take(self.vision).collect();
        let Some(&anchor) = wanted.first() else {
            return;
        };

        // Through the signal cache, the way `App::request_face_quality` reads
        // it: a photo whose analysis was seeded from disk is never submitted.
        // A cold run analyses every photo, a warm one none, which is the whole
        // claim `signalcache` makes.
        let mut cache = crate::signalcache::SignalCache::load(&self.dir);
        let t0 = Instant::now();
        let (mut hits, mut analysed) = (0usize, 0usize);
        for p in &wanted {
            if cache.get(p).and_then(|s| s.faces).is_some() {
                hits += 1;
                continue;
            }
            if let Ok(q) = crate::facequality::analyze(p) {
                cache.record(p, crate::signalcache::Signal::Faces(q));
                analysed += 1;
            }
        }
        cache.flush_blocking(std::time::Duration::from_secs(10));
        eprintln!(
            "[profile] {analysed} face analyses, {hits} served from the signal \
             cache, in {:?}",
            t0.elapsed()
        );

        // Segmentation runs for one photo on demand, not over a set, so this
        // phase reports the single wait the Loupe's overlay makes a user sit
        // through rather than a throughput number.
        let t0 = Instant::now();
        let mask = crate::segmentation::segment(anchor);
        eprintln!(
            "[profile] segment {}: {:?} ({})",
            anchor.file_name().unwrap_or_default().to_string_lossy(),
            t0.elapsed(),
            match &mask {
                Ok(m) => format!("{:?} {}x{}", m.source, m.width, m.height),
                Err(e) => e.clone(),
            }
        );
    }

    #[cfg(not(target_os = "macos"))]
    fn vision_signals(&self, _photos: &[PathBuf]) {
        eprintln!("[profile] Vision signals are macOS only; phase skipped");
    }

    /// What an Auto Tone batch costs per photo once its thumbnail is in
    /// memory. This half runs on the UI thread and cannot be parallelised, so
    /// it decides whether batching the decodes is worth anything. Mirrors
    /// `App::analyze_thumb`.
    fn auto_tone(&self, photos: &[PathBuf]) {
        let wanted: Vec<PathBuf> = photos.iter().take(self.thumbs).cloned().collect();
        let mut loader = Loader::new(16384, self.limits);
        loader.set_thumb_working_set_size(wanted.len());
        for path in &wanted {
            loader.request_thumb(path.clone(), THUMB_PX);
        }
        wait_for_thumbs(&mut loader, &wanted);

        let t0 = Instant::now();
        let mut analysed = 0usize;
        for path in &wanted {
            let Some(img) = loader.get_thumb(path, THUMB_PX) else {
                continue;
            };
            let (grid, _, _) = crate::image_ops::downsample_linear(&img, 256);
            if !grid.is_empty() {
                crate::autotone::analyze(&grid, img.pixel_format, Default::default());
                analysed += 1;
            }
        }
        eprintln!(
            "[profile] auto tone analysed {analysed} in {:?}",
            t0.elapsed()
        );
    }

    /// What a grid frame pays on the UI thread once thumbnails have landed,
    /// which the decode pool cannot absorb. `App::sync_thumb_textures` bakes
    /// an edited photo's thumbnail inline before uploading it, so a viewport
    /// of edited photos arriving together is one frame's bake. The signal
    /// cache's periodic write lists the folder and serializes every entry on
    /// the same thread, after `on_capture_times` has stat'ed each photo.
    fn ui_frame(&self, photos: &[PathBuf]) {
        let wanted: Vec<PathBuf> = photos.iter().take(self.thumbs).cloned().collect();
        let mut loader = Loader::new(16384, self.limits);
        loader.set_thumb_working_set_size(wanted.len());
        for path in &wanted {
            loader.request_thumb(path.clone(), THUMB_PX);
        }
        wait_for_thumbs(&mut loader, &wanted);

        let adj = crate::develop::Adjustments {
            exposure: 0.5,
            contrast: 20.0,
            ..Default::default()
        };
        let mut bakes: Vec<Duration> = Vec::new();
        for path in &wanted {
            let Some(img) = loader.get_thumb(path, THUMB_PX) else {
                continue;
            };
            let t0 = Instant::now();
            let baked = crate::image_ops::bake_edited(&img, &adj, &[], 0);
            bakes.push(t0.elapsed());
            std::hint::black_box(baked);
        }
        let viewport = self.scroll_rows * self.scroll_cols;
        let mut sorted = bakes.clone();
        sorted.sort();
        if let Some(max) = sorted.last() {
            let worst_viewport = bakes
                .windows(viewport.min(bakes.len()).max(1))
                .map(|w| w.iter().sum::<Duration>())
                .max()
                .unwrap_or_default();
            eprintln!(
                "[profile] bake_edited per {THUMB_PX}px thumb: p50 {:?}, max {max:?}; \
                 worst {viewport}-cell viewport landing in one frame: {worst_viewport:?}",
                sorted[sorted.len() / 2],
            );
        }

        // The same viewport the way `sync_thumb_textures` hands it over now:
        // the frame only queues each bake and later takes the results, and
        // the decode workers do the baking.
        let cells: Vec<&PathBuf> = wanted.iter().take(viewport).collect();
        let mut frame = Duration::ZERO;
        let t0 = Instant::now();
        for (sig, path) in cells.iter().enumerate() {
            let Some(img) = loader.get_thumb(path, THUMB_PX) else {
                continue;
            };
            let t = Instant::now();
            loader.request_bake(path, THUMB_PX, sig as u64 + 1, img, adj, &[], 0);
            frame += t.elapsed();
        }
        let mut landed = 0usize;
        while loader.bakes_pending() {
            loader.poll_all();
            let t = Instant::now();
            landed += loader.take_baked().len();
            frame += t.elapsed();
            std::thread::yield_now();
        }
        eprintln!(
            "[profile] worker bakes for a {}-cell viewport: UI thread {frame:?} in total, \
             all {landed} landed after {:?}",
            cells.len(),
            t0.elapsed(),
        );

        // A scratch folder of empty files with the same names, so the sweep
        // lists as many entries as the real folder and the made-up capture
        // times never reach the real folder's signal cache.
        let scratch = std::env::temp_dir().join(format!(
            "lightphotos-profile-signals-{}",
            std::process::id()
        ));
        if let Err(e) = std::fs::create_dir_all(&scratch) {
            eprintln!(
                "[profile] no signal scratch dir at {}: {e}",
                scratch.display()
            );
            return;
        }
        let stand_ins: Vec<PathBuf> = photos
            .iter()
            .filter_map(|p| p.file_name())
            .map(|name| scratch.join(name))
            .filter(|p| std::fs::write(p, []).is_ok())
            .collect();
        {
            let mut cache = crate::signalcache::SignalCache::load(&scratch);
            let now = std::time::SystemTime::now();
            let t0 = Instant::now();
            for p in &stand_ins {
                cache.record(p, crate::signalcache::Signal::Capture(Some(now)));
            }
            let recorded = t0.elapsed();
            let t0 = Instant::now();
            cache.flush_blocking(Duration::from_secs(30));
            eprintln!(
                "[profile] signal cache over {} photos: record {recorded:?} (a stat each), \
                 flush {:?}; queue_write in the report is the UI-thread share",
                stand_ins.len(),
                t0.elapsed(),
            );
        }
        if let Err(e) = std::fs::remove_dir_all(&scratch) {
            eprintln!("[profile] left {} behind: {e}", scratch.display());
        }
    }

    /// The batch a user starts and walks away from, and the only phase that
    /// bakes edits into a full-resolution decode and encodes a JPEG. It goes
    /// through `crate::export::Exporter` the way `App` does, so the worker
    /// pool is part of what gets measured rather than a private function
    /// called on this thread.
    ///
    /// Every `dest` lands in a scratch directory named after this process,
    /// and the directory goes away when the phase ends. Profiling a folder
    /// must not leave JPEGs in it.
    fn export(&self, photos: &[PathBuf]) {
        let wanted: Vec<&PathBuf> = photos.iter().take(self.exports).collect();
        let dir =
            std::env::temp_dir().join(format!("lightphotos-profile-export-{}", std::process::id()));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            eprintln!("[profile] no export scratch dir at {}: {e}", dir.display());
            return;
        }

        let exporter = crate::export::Exporter::new();
        let t0 = Instant::now();
        for (i, src) in wanted.iter().enumerate() {
            exporter.submit(crate::export::ExportJob {
                src: (*src).clone(),
                dest: crate::export::ExportDest::Folder(dir.join(format!("{i:04}.jpg"))),
                adj: crate::develop::Adjustments::default(),
                touchups: Vec::new(),
                rot: 0,
                max_px: u32::MAX,
            });
        }

        let (mut written, mut failed) = (0usize, 0usize);
        while written + failed < wanted.len() {
            for outcome in exporter.poll() {
                match outcome.result {
                    Ok(_) => written += 1,
                    Err(e) => {
                        failed += 1;
                        eprintln!("[profile] export {}: {e}", outcome.src.display());
                    }
                }
            }
            std::thread::yield_now();
        }
        eprintln!(
            "[profile] exported {written}, failed {failed}, in {:?}",
            t0.elapsed()
        );

        if let Err(e) = std::fs::remove_dir_all(&dir) {
            eprintln!("[profile] left {} behind: {e}", dir.display());
        }
    }

    /// What `App::open` does on a folder before the first frame: list the
    /// images, list the sidebar's subfolders, read the sidecars, load the
    /// derived-signal cache, and sweep the thumbnail cache. The sidecar read
    /// and the sweep run on their own thread in the app; here they are inline,
    /// so the report attributes them. So does the signal cache load, which
    /// the app also runs on its own thread.
    fn folder_load(&self) -> Playlist {
        let playlist = Playlist::from_dir(&self.dir);
        crate::navigation::list_subdirs(&self.dir);
        crate::catalog::load_sidecars(&self.dir);
        crate::signalcache::SignalCache::load(&self.dir);
        crate::thumbnail::sweep_orphans(&self.dir);
        playlist
    }

    /// The grid asking for every thumbnail in its working set and waiting for
    /// the decode pool to answer.
    fn thumbnail_grid(&self, photos: &[PathBuf]) {
        let wanted: Vec<PathBuf> = photos.iter().take(self.thumbs).cloned().collect();
        let mut loader = Loader::new(16384, self.limits);
        loader.set_thumb_working_set_size(wanted.len());

        let t0 = Instant::now();
        for path in &wanted {
            loader.request_thumb(path.clone(), THUMB_PX);
        }
        let mut landed = 0usize;
        while landed < wanted.len() {
            loader.poll_all();
            landed = wanted
                .iter()
                .filter(|p| {
                    loader.get_thumb(p, THUMB_PX).is_some() || loader.thumb_failed(p, THUMB_PX)
                })
                .count();
            std::thread::yield_now();
        }
        eprintln!(
            "[profile] {} thumbnails in {:?}",
            wanted.len(),
            t0.elapsed()
        );
    }

    /// The filmstrip filling itself while the user holds the arrow key in the
    /// Loupe. It asks for the same `THUMB_PX` thumbnails through the same
    /// `Loader` as the grid, so what this phase measures is not a different
    /// decode but a different working-set shape: nine thumbnails around the
    /// selection on screen, and `App::working_positions`' margin of eight
    /// either side, asked for the way `App::request_working_thumbs` asks. The
    /// strip advances one photo per `step` without waiting, so the number
    /// that matters is how long the strip around the landing photo takes to
    /// fill once the key is released.
    fn thumbnail_strip(&self, photos: &[PathBuf]) {
        const HALF_VISIBLE: usize = 4;
        let len = photos.len();
        let steps = self.opens.min(len);
        if steps == 0 {
            return;
        }
        let mut loader = Loader::new(16384, self.limits);
        let mut fetched: HashSet<PathBuf> = HashSet::new();

        let t0 = Instant::now();
        let mut visible = 0..0;
        for i in 0..steps {
            visible = i.saturating_sub(HALF_VISIBLE)..(i + HALF_VISIBLE + 1).min(len);
            let strip = (visible.start, visible.end);
            let working = strip_working_range(strip, Some(i), len);
            let paths: Vec<PathBuf> = load_order(working, strip, Some(i))
                .into_iter()
                .map(|pos| photos[pos].clone())
                .collect();
            fetched.extend(paths.iter().cloned());
            loader.set_thumb_working_set_size(paths.len());
            loader.set_viewport_thumbs(&paths, THUMB_PX);
            if i + 1 < steps {
                self.poll_until_next_step(&mut loader);
            }
        }
        let released = Instant::now();
        wait_for_thumbs(&mut loader, &photos[visible]);
        eprintln!(
            "[profile] filmstrip: {steps} steps every {:?}, {} thumbnails asked for, \
             landing strip filled {:?} after the last step, {:?} in all",
            self.step,
            fetched.len(),
            released.elapsed(),
            t0.elapsed()
        );
    }

    /// A wheel flick down a big grid. The viewport moves one row per `step`
    /// over the first `thumbs` photos and asks for its working set every tick
    /// the way `App::request_working_thumbs` does, without waiting. The
    /// number that matters is how long the last viewport takes to fill once
    /// the scroll stops, and (from the report's `get_or_make` count) how many
    /// thumbnails the pool decoded to get there.
    fn scroll_grid(&self, photos: &[PathBuf]) {
        let (rows, cols) = (self.scroll_rows.max(1), self.scroll_cols.max(1));
        let len = self.thumbs.min(photos.len());
        if len == 0 {
            return;
        }
        let mut loader = Loader::new(16384, self.limits);
        let mut fetched: HashSet<PathBuf> = HashSet::new();

        let t0 = Instant::now();
        let mut ticks = 0usize;
        let mut top = 0usize;
        let visible = loop {
            let visible = top..(top + rows * cols).min(len);
            let grid = (visible.start, visible.end);
            let working = grid_working_range(grid, cols, len);
            let paths: Vec<PathBuf> = load_order(working, grid, None)
                .into_iter()
                .map(|pos| photos[pos].clone())
                .collect();
            fetched.extend(paths.iter().cloned());
            loader.set_thumb_working_set_size(paths.len());
            loader.set_viewport_thumbs(&paths, THUMB_PX);
            ticks += 1;
            if visible.end >= len {
                break visible;
            }
            self.poll_until_next_step(&mut loader);
            top += cols;
        };
        let stopped = Instant::now();
        wait_for_thumbs(&mut loader, &photos[visible]);
        eprintln!(
            "[profile] scroll: {rows}x{cols} viewport, {ticks} rows every {:?} over {len} \
             photos, {} thumbnails asked for, last viewport filled {:?} after the scroll \
             stopped, {:?} in all",
            self.step,
            fetched.len(),
            stopped.elapsed(),
            t0.elapsed()
        );
    }

    /// The frame loop between two input events: drain results until the next
    /// step is due.
    fn poll_until_next_step(&self, loader: &mut Loader) {
        let due = Instant::now() + self.step;
        while Instant::now() < due {
            loader.poll_all();
            std::thread::yield_now();
        }
    }

    /// The tier above the preview, which the app reaches only when the user
    /// zooms past the preview's own pixels (`App::ensure_full_for_zoom`).
    /// `Loader::new(16384, _)` sets `full_target` to the renderer's `max_dim`,
    /// so nothing here downscales and the decode is the whole frame. That
    /// makes it the phase `hotpath-alloc` says the most about, and the reason
    /// `LIGHTPHOTOS_PROFILE_FULLS` defaults to five rather than a screenful.
    fn decode_full(&self, photos: &[PathBuf]) {
        let mut loader = Loader::new(16384, self.limits);
        for path in photos.iter().take(self.fulls) {
            let t0 = Instant::now();
            loader.request_full(path.clone());
            while loader.get_full(path).is_none() && loader.has_pending_image() {
                loader.poll_all();
                std::thread::yield_now();
            }
            let decoded = match loader.get_full(path) {
                Some(img) => format!("{}x{}", img.width, img.height),
                None => "no decode".to_string(),
            };
            eprintln!(
                "[profile] full {}: {decoded} in {:?}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                t0.elapsed()
            );
        }
    }

    /// Opening a photo, then stepping to the next. The Loupe makes the user
    /// wait twice and the two waits have different causes, so each gets its
    /// own label. `path/first_pixels` ends when something is on screen,
    /// because `Loader::get_preview` hands back the `Speed` result, usually
    /// the file's embedded preview, until the `Preview` decode lands.
    /// `path/decode_preview` ends when that forced decode-at-size has
    /// replaced them and the photo stops looking soft. A JPEG whose speed
    /// pass already meets `target_px` never enqueues a `Preview` job, so its
    /// second block is near zero. That is the right answer, not a missed
    /// measurement. Both blocks are per photo rather than per loop, which is
    /// what gives the report percentiles over the steps. Neither reuses this
    /// phase's own `path/open_photo` label. `measure_block!` aggregates by
    /// label string and subtracts nothing for nesting, so sharing a label
    /// with the enclosing phase would count the same waits twice and wreck
    /// the average and the percentiles.
    fn open_photos(&self, photos: &[PathBuf]) {
        let mut loader = Loader::new(16384, self.limits);
        for path in photos.iter().take(self.opens) {
            let t0 = Instant::now();
            loader.request_preview(path.clone(), self.preview_px);

            hotpath::measure_block!("path/first_pixels", {
                while loader.get_preview(path, self.preview_px).is_none()
                    && loader.has_pending_image()
                {
                    loader.poll_all();
                    std::thread::yield_now();
                }
            });
            let first = t0.elapsed();

            hotpath::measure_block!("path/decode_preview", {
                while loader.has_pending_image() {
                    loader.poll_all();
                    std::thread::yield_now();
                }
            });

            eprintln!(
                "[profile] open {}: first {:?}, sharp {:?}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                first,
                t0.elapsed(),
            );
        }
    }
}

/// Polls until every one of `photos` has a thumbnail or has failed.
fn wait_for_thumbs(loader: &mut Loader, photos: &[PathBuf]) {
    while photos
        .iter()
        .any(|p| loader.get_thumb(p, THUMB_PX).is_none() && !loader.thumb_failed(p, THUMB_PX))
    {
        loader.poll_all();
        std::thread::yield_now();
    }
}

/// Removes `dir`'s derived-signal cache, so the Vision phase measures a first
/// visit. Reports whether there was one.
fn drop_signal_cache(dir: &Path) -> bool {
    let file = dir
        .join(crate::catalog::SIDECAR_DIR)
        .join(crate::signalcache::CACHE_FILE);
    std::fs::remove_file(file).is_ok()
}

/// Removes `dir`'s cached thumbnails and nothing else. Sidecars carry the
/// user's ratings and edits and share the folder, so the suffix match has to
/// be exact.
fn drop_thumb_cache(dir: &Path) -> usize {
    let cache = dir.join(crate::catalog::SIDECAR_DIR);
    let Ok(entries) = std::fs::read_dir(&cache) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.ends_with(".thumb.jpg"))
                && std::fs::remove_file(e.path()).is_ok()
        })
        .count()
}
