// SPDX-License-Identifier: MIT OR Apache-2.0

//! The shown photo's fill patches, for the Loupe. A patch is a PNG of up
//! to 16 MiB of pixels, so a worker reads and decodes it and the frame
//! draws the photo without it until it lands. Bakes don't come here: their
//! own workers read the files (`FillInput::OnDisk`).
//!
//! Native only. No worker in the browser can reach the folder, and nothing
//! reads patches through File System Access yet, so there a fill draws
//! nothing, in the Loupe or in a bake.
//!
//! A read-only folder takes no file, so a fill made there keeps its record
//! and its patches in the store's memory for the session, and bakes take
//! them from there.

use std::path::Path;

use crate::develop::fill::{Fill, FillInput, FillSet};
use crate::develop::TouchUp;

use super::App;

/// One step of Touch Up's own undo: what the photo held before a spot or a
/// fill changed, newest last, so Cmd+Z walks both in the order they were
/// made.
pub(crate) enum TouchStep {
    Spots(Vec<TouchUp>),
    Fills(Vec<Fill>),
}

/// What the Touch Up panel and the Loupe draw for the Generative brush.
pub(crate) struct GenerativeView<'a> {
    /// The panel's switch is on.
    pub on: bool,
    /// A drag on the photo paints: the switch is on, Touch Up is open and
    /// ComfyUI can run the fill.
    pub armed: bool,
    /// Why the controls are disabled, for their hover text.
    pub blocked: Option<String>,
    pub prompt: &'a str,
    pub job: JobView<'a>,
    /// The shown photo's fills, as source UV rects.
    pub fills: Vec<[f32; 4]>,
    pub picked: Option<PickedFill>,
}

pub(crate) enum JobView<'a> {
    Idle,
    /// The pointer is down on a stroke.
    Painting,
    /// `bbox` is the rect being filled, when it is on the shown photo.
    Generating {
        done: usize,
        of: usize,
        bbox: Option<[f32; 4]>,
    },
    /// A stroke whose job failed (`why`) or was cancelled, kept so Generate
    /// can send it again.
    Kept {
        why: Option<&'a str>,
    },
}

#[derive(Clone, Copy)]
pub(crate) struct PickedFill {
    pub index: usize,
    /// The variant shown, from 0.
    pub variant: usize,
    pub variants: usize,
}

pub(crate) use store::PatchStore;

/// The browser's store: nothing there is read-only or made in the session.
#[cfg(target_arch = "wasm32")]
mod store {
    use std::path::Path;

    use crate::develop::fill::{Fill, FillSet};

    #[derive(Default)]
    pub(crate) struct PatchStore;

    impl PatchStore {
        pub(crate) fn new() -> Self {
            PatchStore
        }

        pub(crate) fn unsaved_fills(&self, _photo: &Path) -> Option<&[Fill]> {
            None
        }

        pub(crate) fn unsaved_set(&self, _photo: &Path) -> Option<FillSet> {
            None
        }
    }
}

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

    /// A read-only photo's fills made this session, with their patches.
    #[derive(Default)]
    struct Unsaved {
        fills: Option<Vec<Fill>>,
        patches: HashMap<RasterId, Arc<Patch>>,
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
        /// What read-only folders could not be given as files.
        unsaved: HashMap<PathBuf, Unsaved>,
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
                unsaved: HashMap::new(),
            }
        }

        /// The fills of a read-only `photo` as this session left them;
        /// `None` when it made none there, so the sidecar's stand.
        pub(crate) fn unsaved_fills(&self, photo: &Path) -> Option<&[Fill]> {
            self.unsaved.get(photo)?.fills.as_deref()
        }

        /// Keeps `fills` as a read-only `photo`'s, in place of a sidecar
        /// write.
        pub(crate) fn keep_fills(&mut self, photo: &Path, fills: Vec<Fill>) {
            self.unsaved.entry(photo.to_path_buf()).or_default().fills = Some(fills);
        }

        /// Patches a generation just made for `photo`, so the Loupe draws
        /// them without reading them back. `keep` holds them, and the shown
        /// photo's other loaded patches, for the session: a read-only
        /// folder's patches have no file.
        pub(crate) fn hand_in(
            &mut self,
            photo: &Path,
            patches: impl Iterator<Item = Arc<Patch>>,
            keep: bool,
        ) {
            let current = self.photo.as_deref() == Some(photo);
            for patch in patches {
                if keep {
                    let kept = self.unsaved.entry(photo.to_path_buf()).or_default();
                    kept.patches.insert(patch.id(), Arc::clone(&patch));
                }
                if current {
                    self.slots.insert(patch.id(), Slot::Ready(patch));
                }
            }
            if keep && current {
                let kept = self.unsaved.entry(photo.to_path_buf()).or_default();
                for (id, slot) in &self.slots {
                    if let Slot::Ready(patch) = slot {
                        kept.patches.entry(*id).or_insert_with(|| Arc::clone(patch));
                    }
                }
            }
        }

        /// Patch `id` of `photo`, if it is in memory.
        pub(crate) fn ready(&self, photo: &Path, id: RasterId) -> Option<Arc<Patch>> {
            let kept = || self.unsaved.get(photo)?.patches.get(&id).cloned();
            match self.slots.get(&id) {
                Some(Slot::Ready(patch)) if self.photo.as_deref() == Some(photo) => {
                    Some(Arc::clone(patch))
                }
                _ => kept(),
            }
        }

        /// A read-only `photo`'s fills with the patches held for it, for a
        /// bake; `None` when the session made none there.
        pub(crate) fn unsaved_set(&self, photo: &Path) -> Option<FillSet> {
            let kept = self.unsaved.get(photo)?;
            let fills = kept.fills.as_deref()?;
            Some(FillSet::new(fills, |id| kept.patches.get(&id).cloned()))
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
                    let kept = self.unsaved.get(photo).and_then(|u| u.patches.get(&id));
                    let slot = match kept {
                        Some(patch) => Slot::Ready(Arc::clone(patch)),
                        None => self.start_load(photo, id),
                    };
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
            let fills = self.fills_of(path).to_vec();
            return self.patches.ask(path, &fills);
        }
        FillSet::default()
    }

    /// The browser build can't reach a ComfyUI server, so it never shows
    /// the Generative brush.
    #[cfg(target_arch = "wasm32")]
    pub(crate) fn generative_view(&self) -> Option<GenerativeView<'_>> {
        None
    }

    /// A photo's fills: the sidecar's, or for a read-only folder the ones
    /// made this session and kept in memory.
    pub(crate) fn fills_of(&self, path: &Path) -> &[Fill] {
        fills_in(&self.folder_store, &self.patches, path)
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
        fill_input_of(&self.folder_store, &self.patches, path)
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

/// [`App::fills_of`] as a free function, so a caller holding the loader
/// can still ask.
pub(crate) fn fills_in<'a>(
    folder_store: &'a crate::persist::folder_store::FolderStore,
    patches: &'a PatchStore,
    path: &Path,
) -> &'a [Fill] {
    patches
        .unsaved_fills(path)
        .unwrap_or_else(|| folder_store.fills(path))
}

/// [`App::fill_input`] as a free function, as [`fills_in`] is.
pub(crate) fn fill_input_of(
    folder_store: &crate::persist::folder_store::FolderStore,
    patches: &PatchStore,
    path: &Path,
) -> FillInput {
    if let Some(set) = patches.unsaved_set(path) {
        return FillInput::Ready(set);
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        FillInput::OnDisk(folder_store.fills(path).to_vec())
    }
    #[cfg(target_arch = "wasm32")]
    {
        let _ = folder_store;
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
