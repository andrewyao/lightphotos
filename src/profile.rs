// SPDX-License-Identifier: GPL-3.0-or-later

//! Headless driver for the three paths a culling session actually waits on:
//! listing a folder, filling the grid with thumbnails, and opening one photo
//! into the Loupe. Compiled only under the `hotpath` feature.
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
            cold: std::env::var("LIGHTPHOTOS_PROFILE_COLD").as_deref() == Ok("1"),
        }
    }
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
            eprintln!("[profile] cold start: removed {removed} cached thumbnails");
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

        hotpath::measure_block!("path/thumbnail_grid", self.thumbnail_grid(&photos));
        hotpath::measure_block!("path/open_photo", self.open_photos(&photos));
        hotpath::measure_block!("path/auto_tone", self.auto_tone(&photos));
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
    /// images, list the sidebar's subfolders, read the sidecars, and sweep the
    /// thumbnail cache. The sidecar read and the sweep run on their own thread
    /// in the app; here they are inline, so the report attributes them.
    fn folder_load(&self) -> Playlist {
        let playlist = Playlist::from_dir(&self.dir);
        crate::navigation::list_subdirs(&self.dir);
        crate::catalog::load_sidecars(&self.dir);
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

    /// Opening a photo, then stepping to the next. Two numbers per step,
    /// because the Loupe shows two things. `first` is the `Speed` pass, often
    /// the file's embedded preview, which is what the user sees appear.
    /// `sharp` is when the forced decode-at-size has replaced it and the photo
    /// stops looking soft. Each step waits for both, so the next step's
    /// numbers are not the previous escalation still running.
    fn open_photos(&self, photos: &[PathBuf]) {
        let mut loader = Loader::new(16384);
        for path in photos.iter().take(self.opens) {
            let t0 = Instant::now();
            loader.request_preview(path.clone(), self.preview_px);

            let mut first = None;
            while loader.has_pending_image() {
                loader.poll_all();
                if first.is_none() && loader.get_preview(path, self.preview_px).is_some() {
                    first = Some(t0.elapsed());
                }
                std::thread::yield_now();
            }
            eprintln!(
                "[profile] open {}: first {:?}, sharp {:?}",
                path.file_name().unwrap_or_default().to_string_lossy(),
                first.unwrap_or_default(),
                t0.elapsed(),
            );
        }
    }
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
