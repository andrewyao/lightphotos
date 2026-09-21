// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless driver for the paths a culling session actually waits on. The
//! three that every session pays are listing a folder, filling the grid with
//! thumbnails, and opening one photo into the Loupe. Auto Tone and the Vision
//! signals run on demand and are driven here too, because both are expensive
//! enough to decide whether a feature can run unasked. Compiled only under
//! the `hotpath` feature.
//!
//! It exists because a report is only worth acting on if the next person can
//! reproduce it. Driving the window by hand gives a different scroll depth and
//! a different cache state every run, so the numbers cannot be compared
//! before and after a change. This runs the same `navigation`, `catalog`,
//! `Loader` and `thumbnail` code the window drives, minus egui and the GPU,
//! over a folder named on the command line, and then returns so the hotpath
//! guard drops and prints its report.
//!
//! ```sh
//! cargo run --release --features hotpath -- --profile ~/Pictures/Trip
//! LIGHTPHOTOS_PROFILE_COLD=1 cargo run --release --features hotpath-alloc -- --profile ~/Pictures/Trip
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::loader::Loader;
use crate::navigation::Playlist;
use crate::thumbnail::THUMB_PX;

/// One scripted run. The counts are the shape of a real session rather than
/// the whole folder: the grid paints a screenful before the user scrolls, and
/// the wait that matters in the Loupe is the first few photos, not the 500th.
struct Run {
    dir: PathBuf,
    /// Photos whose thumbnail the grid phase fills. Two screenfuls at the
    /// default window size.
    thumbs: usize,
    /// Photos the Loupe phase opens, stepping like next / next / next.
    opens: usize,
    /// Longest side the Loupe asks for: a 1100pt window on a 2x display.
    preview_px: u32,
    /// Photos the full-resolution phase decodes. Small by default, because
    /// each one is the whole frame in RGBA and the allocation report is the
    /// reason to run this phase at all.
    fulls: usize,
    /// Photos the Vision phase runs feature prints, faces and segmentation
    /// over. Small by default because every Vision call decodes the file at
    /// full resolution itself.
    vision: usize,
    /// Delete this folder's cached thumbnails first, so the grid phase
    /// measures a first visit rather than a revisit.
    cold: bool,
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
            opens: count("LIGHTPHOTOS_PROFILE_OPENS", 10),
            preview_px: count("LIGHTPHOTOS_PROFILE_PREVIEW_PX", 2200) as u32,
            fulls: count("LIGHTPHOTOS_PROFILE_FULLS", 5),
            vision: count("LIGHTPHOTOS_PROFILE_VISION", 8),
            cold: std::env::var("LIGHTPHOTOS_PROFILE_COLD").as_deref() == Ok("1"),
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

    /// What the grouping features cost per photo. Every call here makes Vision
    /// decode the file itself at full resolution, which is the reason the
    /// duplicate and burst tools are gated behind `SHOW_GROUPING_TOOLS`. The
    /// pairing mirrors `refine_by_feature_print`, which compares each member
    /// against one group anchor.
    #[cfg(target_os = "macos")]
    fn vision_signals(&self, photos: &[PathBuf]) {
        let wanted: Vec<&PathBuf> = photos.iter().take(self.vision).collect();
        let Some((anchor, members)) = wanted.split_first() else {
            return;
        };

        let t0 = Instant::now();
        let anchor_print = crate::featureprint::compute(anchor);
        let mut compared = 0usize;
        for member in members {
            let (Ok(a), Ok(m)) = (&anchor_print, crate::featureprint::compute(member)) else {
                continue;
            };
            if crate::featureprint::feature_distance(a, &m).is_ok() {
                compared += 1;
            }
        }
        eprintln!(
            "[profile] {compared} feature-print comparisons in {:?}",
            t0.elapsed()
        );

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
        let mut loader = Loader::new(16384);
        loader.set_thumb_working_set_size(wanted.len());
        for path in &wanted {
            loader.request_thumb(path.clone(), THUMB_PX);
        }
        while wanted
            .iter()
            .any(|p| loader.get_thumb(p, THUMB_PX).is_none() && !loader.thumb_failed(p, THUMB_PX))
        {
            loader.poll_all();
            std::thread::yield_now();
        }

        let t0 = Instant::now();
        let mut analysed = 0usize;
        for path in &wanted {
            let Some(img) = loader.get_thumb(path, THUMB_PX) else {
                continue;
            };
            let (grid, _, _) = crate::image_ops::downsample_linear(&img, 256);
            if !grid.is_empty() {
                crate::autotone::analyze(&grid, img.pixel_format);
                analysed += 1;
            }
        }
        eprintln!(
            "[profile] auto tone analysed {analysed} in {:?}",
            t0.elapsed()
        );
    }

    /// What `App::open` does on a folder before the first frame: list the
    /// images, list the sidebar's subfolders, read the sidecars, load the
    /// derived-signal cache, and sweep the thumbnail cache. The sidecar read
    /// and the sweep run on their own thread in the app; here they are inline,
    /// so the report attributes them. The signal cache load is on the UI
    /// thread in the app too, which is why its cost belongs in this phase.
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
        let mut loader = Loader::new(16384);
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

    /// The filmstrip filling itself while the user steps through the Loupe.
    /// It asks for the same `THUMB_PX` thumbnails through the same `Loader`
    /// as the grid, so what this phase measures is not a different decode but
    /// a different working-set shape. `App::working_positions` hands the
    /// Loupe a window of `strip_range` plus or minus eight, and that window
    /// slides by one photo per step, so all but its two new edges are already
    /// in the cache. One `Loader` spans the whole walk to keep that true, and
    /// the count on the report line is the number of photos the walk actually
    /// asked the decode pool for.
    fn thumbnail_strip(&self, photos: &[PathBuf]) {
        const MARGIN: usize = 8;
        let len = photos.len();
        let steps = self.opens.min(len);
        let mut loader = Loader::new(16384);
        let mut fetched: HashSet<PathBuf> = HashSet::new();

        let t0 = Instant::now();
        for i in 0..steps {
            let window = &photos[i.saturating_sub(MARGIN)..(i + MARGIN + 1).min(len)];
            loader.set_thumb_working_set_size(window.len());
            for path in window {
                fetched.insert(path.clone());
                loader.request_thumb(path.clone(), THUMB_PX);
            }
            while window.iter().any(|p| {
                loader.get_thumb(p, THUMB_PX).is_none() && !loader.thumb_failed(p, THUMB_PX)
            }) {
                loader.poll_all();
                std::thread::yield_now();
            }
        }
        eprintln!(
            "[profile] filmstrip stepped {steps} times over {} thumbnails in {:?}",
            fetched.len(),
            t0.elapsed()
        );
    }

    /// The tier above the preview, which the app reaches only when the user
    /// zooms past the preview's own pixels (`App::ensure_full_for_zoom`).
    /// `Loader::new(16384)` sets `full_target` to the renderer's `max_dim`,
    /// so nothing here downscales and the decode is the whole frame. That
    /// makes it the phase `hotpath-alloc` says the most about, and the reason
    /// `LIGHTPHOTOS_PROFILE_FULLS` defaults to five rather than a screenful.
    fn decode_full(&self, photos: &[PathBuf]) {
        let mut loader = Loader::new(16384);
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
        let mut loader = Loader::new(16384);
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
