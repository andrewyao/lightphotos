// SPDX-License-Identifier: MIT OR Apache-2.0

//! The shown photo's fill patches, for the Loupe. A patch is a PNG of up
//! to 16 MiB of pixels, so a worker reads and decodes it and the frame
//! draws the photo without it until it lands. Bakes don't come here: their
//! own workers read the files (`FillInput::OnDisk`).
//!
//! Native only. No worker in the browser can reach the folder, and nothing
//! reads patches through File System Access yet, so there a fill draws
//! nothing, in the Loupe or in a bake.

use std::path::Path;

use crate::develop::fill::{FillInput, FillSet};

use super::App;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use store::PatchStore;

#[cfg(not(target_arch = "wasm32"))]
mod store {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use crate::develop::fill::{Fill, FillSet, Patch, PatchError};
    use crate::develop::RasterId;
    use crate::worker_pool::WorkerPool;

    enum Slot {
        Loading,
        Ready(Arc<Patch>),
        /// Settled: the fill draws nothing this session.
        Failed,
    }

    struct LoadJob {
        photo: PathBuf,
        id: RasterId,
    }

    pub(crate) struct PatchStore {
        /// The photo the slots belong to. Showing another drops them, so
        /// at most one photo's patches are held.
        photo: Option<PathBuf>,
        slots: HashMap<RasterId, Slot>,
        load: WorkerPool<LoadJob, Result<Patch, PatchError>>,
        /// Loads submitted and not yet drained, so the frame loop keeps
        /// polling.
        in_flight: usize,
    }

    impl PatchStore {
        pub(crate) fn new() -> Self {
            Self {
                photo: None,
                slots: HashMap::new(),
                load: WorkerPool::spawn(
                    "fill-load",
                    1,
                    || {},
                    |job: &LoadJob| Ok(crate::develop::fill::read(&job.photo, job.id)),
                    |_| "reading a fill's image file panicked".into(),
                ),
                in_flight: 0,
            }
        }

        /// `photo`'s fills with the patches in hand, starting a load for each
        /// chosen patch that isn't.
        pub(crate) fn ask(&mut self, photo: &Path, fills: &[Fill]) -> FillSet {
            if self.photo.as_deref() != Some(photo) {
                self.photo = Some(photo.to_path_buf());
                self.slots.clear();
            }
            FillSet::new(fills, |id| {
                if !self.slots.contains_key(&id) {
                    let slot = self.start_load(photo, id);
                    self.slots.insert(id, slot);
                }
                match self.slots.get(&id) {
                    Some(Slot::Ready(patch)) => Some(Arc::clone(patch)),
                    _ => None,
                }
            })
        }

        fn start_load(&mut self, photo: &Path, id: RasterId) -> Slot {
            let job = LoadJob {
                photo: photo.to_path_buf(),
                id,
            };
            if self.load.submit(job) {
                self.in_flight += 1;
                Slot::Loading
            } else {
                Slot::Failed
            }
        }

        pub(crate) fn busy(&self) -> bool {
            self.in_flight > 0
        }

        /// Lands finished loads; true when a patch of the shown photo arrived,
        /// so the Loupe should upload again. A load for a photo no longer shown
        /// is dropped. A corrupt file is removed, unless `read_only` says its
        /// folder is: content-addressed, so a later write of the same patch
        /// would skip over the bad file.
        pub(crate) fn drain(&mut self, read_only: impl Fn(&Path) -> bool) -> bool {
            let mut landed = false;
            for (job, result) in self.load.poll() {
                self.in_flight = self.in_flight.saturating_sub(1);
                let result = result.unwrap_or_else(|why| Err(PatchError::Io(why)));
                if let Err(e) = &result {
                    eprintln!("[fill] {}: {e}", job.photo.display());
                    if matches!(e, PatchError::Corrupt(_)) && !read_only(&job.photo) {
                        crate::develop::fill::remove(&job.photo, job.id);
                    }
                }
                let current = self.photo.as_deref() == Some(job.photo.as_path());
                if !current || !matches!(self.slots.get(&job.id), Some(Slot::Loading)) {
                    continue;
                }
                let slot = match result {
                    Ok(patch) => {
                        landed = true;
                        Slot::Ready(Arc::new(patch))
                    }
                    Err(_) => Slot::Failed,
                };
                self.slots.insert(job.id, slot);
            }
            landed
        }
    }
}

impl App {
    /// The shown photo's fills with the patches loaded so far: what the
    /// Loupe draws. Asks the store, so a missing patch starts loading.
    pub(crate) fn current_fill_set(&mut self) -> FillSet {
        #[cfg(not(target_arch = "wasm32"))]
        if let Some(path) = self.shown.path() {
            return self.patches.ask(path, self.folder_store.fills(path));
        }
        FillSet::default()
    }

    /// Whether a patch is still loading, so the frame loop keeps polling.
    pub(crate) fn fills_loading(&self) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        {
            self.patches.busy()
        }
        #[cfg(target_arch = "wasm32")]
        {
            false
        }
    }

    /// What a background bake of `path` carries. A native worker reads the
    /// patch files itself, in a read-only folder too, since reading writes
    /// nothing.
    pub(crate) fn fill_input(&self, path: &Path) -> FillInput {
        fill_input_of(&self.folder_store, path)
    }

    /// Each frame: patches that finished loading go up to the Loupe.
    pub(crate) fn poll_fills(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let coverage = &self.coverage;
            if self.patches.drain(|photo| coverage.read_only_photo(photo)) {
                self.push_adjustments();
            }
        }
    }
}

/// [`App::fill_input`] as a free function, so a caller holding the loader
/// can still build one.
pub(crate) fn fill_input_of(
    folder_store: &crate::persist::folder_store::FolderStore,
    path: &Path,
) -> FillInput {
    #[cfg(not(target_arch = "wasm32"))]
    {
        FillInput::OnDisk(folder_store.fills(path).to_vec())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (folder_store, path);
        FillInput::none()
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::develop::fill::{self, Fill, Patch};
    use std::path::PathBuf;

    fn folder(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("lightphotos-patches-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn patch(v: u8) -> Patch {
        Patch::new(1, 1, vec![v, v, v, 255]).unwrap()
    }

    fn fill_of(p: &Patch) -> Fill {
        Fill::new([0.0, 0.0, 1.0, 1.0], vec![p.id()], String::new()).unwrap()
    }

    fn file_of(photo: &Path, p: &Patch) -> PathBuf {
        let name = format!("{}.{}.png", photo.file_name().unwrap().display(), p.id());
        photo.with_file_name(".lightphotos/fills").join(name)
    }

    /// Drains until the store's loads are in, returning whether any landed.
    fn settle(store: &mut PatchStore, read_only: bool) -> bool {
        let start = std::time::Instant::now();
        let mut landed = false;
        while store.busy() {
            landed |= store.drain(|_| read_only);
            assert!(start.elapsed().as_secs() < 10, "the load never finished");
            std::thread::yield_now();
        }
        landed
    }

    #[test]
    fn a_patch_loads_off_the_frame_and_then_draws() {
        let dir = folder("load");
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        let p = patch(9);
        fill::write(&a, &p).unwrap();
        let fills = [fill_of(&p)];
        let mut store = PatchStore::new();

        assert!(store.ask(&a, &fills).is_empty(), "not read on the frame");
        assert!(settle(&mut store, false));
        assert_eq!(store.ask(&a, &fills).len(), 1);
        assert!(!store.busy(), "a loaded patch is not asked for again");

        // Another photo names the same id and has no file of its own.
        assert!(store.ask(&b, &fills).is_empty());
        assert!(!settle(&mut store, false), "a missing file lands nothing");
        assert!(store.ask(&b, &fills).is_empty());
        assert!(!store.busy(), "and is not retried every frame");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_load_for_a_photo_no_longer_shown_is_dropped() {
        let dir = folder("moved-on");
        let (a, b) = (dir.join("a.jpg"), dir.join("b.jpg"));
        let p = patch(9);
        fill::write(&a, &p).unwrap();
        let mut store = PatchStore::new();
        store.ask(&a, &[fill_of(&p)]);
        store.ask(&b, &[]);
        assert!(!settle(&mut store, false));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_corrupt_patch_file_is_removed_except_from_a_read_only_folder() {
        let dir = folder("corrupt");
        let photo = dir.join("a.jpg");
        let p = patch(9);
        let file = file_of(&photo, &p);
        for (read_only, kept) in [(true, true), (false, false)] {
            fill::write(&photo, &p).unwrap();
            let bytes = std::fs::read(&file).unwrap();
            std::fs::write(&file, &bytes[..bytes.len() / 2]).unwrap();
            let mut store = PatchStore::new();
            store.ask(&photo, &[fill_of(&p)]);
            assert!(!settle(&mut store, read_only));
            assert!(store.ask(&photo, &[fill_of(&p)]).is_empty());
            assert_eq!(file.is_file(), kept, "read-only: {read_only}");
            let _ = std::fs::remove_file(&file);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
