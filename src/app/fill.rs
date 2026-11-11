// SPDX-License-Identifier: MIT OR Apache-2.0

//! The fill patches in memory. A patch is a PNG of up to 16 MiB of pixels,
//! so it is read and decoded off the frame, and the photo draws without it
//! until it lands.
//!
//! Natively the store holds the shown photo's patches for the Loupe, read
//! by a worker. Bakes don't come here: their own workers read the files
//! (`FillInput::OnDisk`). A read-only folder takes no file, so a fill made
//! there keeps its record and its patches in the store's memory for the
//! session, and bakes take them from there.
//!
//! In the browser no worker can reach the folder, so the app reads each
//! file the store wants through File System Access, as it does coverage,
//! and both the Loupe and thumbnail bakes take patches from the store. An
//! export reads its own. The browser draws fills and can't make them.

use std::path::Path;

use crate::develop::fill::{Fill, FillInput, FillSet};
#[cfg(target_arch = "wasm32")]
use crate::develop::fill::{Patch, PatchError};
use crate::develop::TouchUp;

use super::App;

/// One step of Touch Up's own undo: what the photo held before a spot or a
/// fill changed, newest last, so Cmd+Z walks both in the order they were
/// made.
pub(crate) enum TouchStep {
    Spots(Vec<TouchUp>),
    #[cfg(not(target_arch = "wasm32"))]
    Fills(Vec<Fill>),
}

/// What the Touch Up panel and the Loupe draw for the Generative brush.
#[cfg(not(target_arch = "wasm32"))]
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
    /// Where each of the shown photo's fills is pinned, in source UV: the
    /// middle of its painted area.
    pub pins: Vec<[f32; 2]>,
    pub picked: Option<PickedFill>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) enum JobView<'a> {
    Idle,
    /// The pointer is down on a stroke.
    Painting,
    /// `at` is the middle of the area being filled, in source UV, when it
    /// is on the shown photo.
    Generating {
        done: usize,
        of: usize,
        at: Option<[f32; 2]>,
    },
    /// A stroke whose job failed (`why`) or was cancelled, kept so Generate
    /// can send it again.
    Kept {
        why: Option<&'a str>,
    },
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy)]
pub(crate) struct PickedFill {
    pub index: usize,
    /// The variant shown, from 0.
    pub variant: usize,
    pub variants: usize,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) use store::PatchStore;
#[cfg(target_arch = "wasm32")]
pub(crate) use web_store::PatchStore;

/// The browser's store: patches of any photo in the folder, for the Loupe
/// and for thumbnail bakes, up to a budget. It reads nothing itself. It
/// lists the files it wants, the app reads each as a task and sends the
/// result back, and `drain` lands it. Plain data, so it is tested natively.
#[cfg(any(target_arch = "wasm32", test))]
mod web_store {
    use std::collections::{HashMap, VecDeque};
    use std::path::{Path, PathBuf};
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::sync::Arc;

    use crate::develop::fill::{Fill, FillSet, Patch, PatchError};
    use crate::develop::RasterId;

    /// The patch pixels kept in memory: a dozen patches of the largest
    /// size, in a heap every thread shares.
    const READY_BUDGET: usize = 192 << 20;

    type Key = (PathBuf, RasterId);

    enum Slot {
        Loading,
        Ready(Arc<Patch>),
        /// Settled: the fill draws nothing until the folder is opened again.
        Failed,
    }

    /// One patch file the app read for the store.
    pub(crate) struct Landing {
        pub photo: PathBuf,
        pub id: RasterId,
        pub result: Result<Patch, PatchError>,
    }

    pub(crate) struct PatchStore {
        slots: HashMap<Key, Slot>,
        /// Ready patches, oldest first.
        ready_order: VecDeque<Key>,
        ready_bytes: usize,
        /// Files the app must read, each once.
        wanted: Vec<Key>,
        landings: (Sender<Landing>, Receiver<Landing>),
        /// Reads asked for and not yet drained, so the frame loop keeps
        /// polling.
        in_flight: usize,
        budget: usize,
    }

    impl PatchStore {
        pub(crate) fn new() -> Self {
            Self::with_budget(READY_BUDGET)
        }

        pub(crate) fn with_budget(budget: usize) -> Self {
            PatchStore {
                slots: HashMap::new(),
                ready_order: VecDeque::new(),
                ready_bytes: 0,
                wanted: Vec::new(),
                landings: channel(),
                in_flight: 0,
                budget,
            }
        }

        /// Forgets every patch, as opening another folder does.
        pub(crate) fn clear(&mut self) {
            self.slots.clear();
            self.ready_order.clear();
            self.ready_bytes = 0;
            self.wanted.clear();
        }

        /// `photo`'s fills with the patches in hand, wanting each chosen
        /// patch that isn't: what the Loupe draws.
        pub(crate) fn ask(&mut self, photo: &Path, fills: &[Fill]) -> FillSet {
            FillSet::new(fills, |id| {
                let key = (photo.to_path_buf(), id);
                match self.slots.get(&key) {
                    Some(Slot::Ready(patch)) => Some(Arc::clone(patch)),
                    Some(Slot::Loading | Slot::Failed) => None,
                    None => {
                        self.slots.insert(key.clone(), Slot::Loading);
                        self.wanted.push(key);
                        self.in_flight += 1;
                        None
                    }
                }
            })
        }

        /// [`ask`](Self::ask) for a bake, which draws once and so waits:
        /// `None` while a chosen patch is still being read. A patch that
        /// couldn't be read holds nothing up; its fill bakes as nothing.
        pub(crate) fn ready_set(&mut self, photo: &Path, fills: &[Fill]) -> Option<FillSet> {
            let set = self.ask(photo, fills);
            let loading = |f: &Fill| {
                let key = (photo.to_path_buf(), f.chosen());
                matches!(self.slots.get(&key), Some(Slot::Loading))
            };
            (!fills.iter().any(loading)).then_some(set)
        }

        /// The files the app must read for the store, each once.
        pub(crate) fn take_wanted(&mut self) -> Vec<Key> {
            std::mem::take(&mut self.wanted)
        }

        /// Where the app sends what it read.
        pub(crate) fn landing_sender(&self) -> Sender<Landing> {
            self.landings.0.clone()
        }

        pub(crate) fn busy(&self) -> bool {
            self.in_flight > 0
        }

        /// Lands finished reads; true when any settled, read or failed, so
        /// the Loupe uploads again and a waiting bake can go. A read for a
        /// folder since closed is dropped.
        pub(crate) fn drain(&mut self) -> bool {
            let mut landed = false;
            while let Ok(Landing { photo, id, result }) = self.landings.1.try_recv() {
                self.in_flight = self.in_flight.saturating_sub(1);
                let key = (photo, id);
                if !matches!(self.slots.get(&key), Some(Slot::Loading)) {
                    continue;
                }
                landed = true;
                match result {
                    Ok(patch) => {
                        self.ready_bytes = self.ready_bytes.saturating_add(patch.rgba().len());
                        self.ready_order.push_back(key.clone());
                        self.slots.insert(key.clone(), Slot::Ready(Arc::new(patch)));
                        self.evict(&key.0);
                    }
                    Err(_) => {
                        self.slots.insert(key, Slot::Failed);
                    }
                }
            }
            landed
        }

        /// Drops the oldest patches until the budget holds, sparing
        /// `keep`'s: the photo that just landed is about to draw. A patch
        /// dropped here is read again when its photo next asks.
        fn evict(&mut self, keep: &Path) {
            let mut i = 0;
            while self.ready_bytes > self.budget {
                let Some(key) = self.ready_order.get(i) else {
                    return;
                };
                if key.0 == keep {
                    i += 1;
                    continue;
                }
                let Some(key) = self.ready_order.remove(i) else {
                    return;
                };
                if let Some(Slot::Ready(patch)) = self.slots.remove(&key) {
                    self.ready_bytes = self.ready_bytes.saturating_sub(patch.rgba().len());
                }
            }
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
        let Some(path) = self.shown.path() else {
            return FillSet::default();
        };
        let fills = fills_in(&self.folder_store, &self.patches, path).to_vec();
        self.patches.ask(path, &fills)
    }

    /// A photo's fills: the sidecar's, or for a read-only folder the ones
    /// made this session and kept in memory.
    pub(crate) fn fills_of(&self, path: &Path) -> &[Fill] {
        fills_in(&self.folder_store, &self.patches, path)
    }

    /// Whether a patch is still loading, so the frame loop keeps polling.
    pub(crate) fn fills_loading(&self) -> bool {
        self.patches.busy()
    }

    /// What a background bake of `path` carries; see [`fill_input_of`].
    pub(crate) fn fill_input(&mut self, path: &Path) -> Option<FillInput> {
        fill_input_of(&self.folder_store, &mut self.patches, path)
    }

    /// Each frame: patches that finished loading go up to the Loupe.
    pub(crate) fn poll_fills(&mut self) {
        #[cfg(not(target_arch = "wasm32"))]
        let landed = {
            let coverage = &self.coverage;
            self.patches.drain(|photo| coverage.read_only_photo(photo))
        };
        #[cfg(target_arch = "wasm32")]
        let landed = {
            self.start_web_fill_loads();
            self.patches.drain()
        };
        if landed {
            self.push_adjustments();
            // In the browser a thumbnail bake may be waiting on it too.
            self.request_redraw();
        }
    }

    /// Reads, as tasks, the patch files the store asked for. File System
    /// Access is main-thread only, so no worker can do it; each result goes
    /// back through the store's channel and lands in `drain`.
    #[cfg(target_arch = "wasm32")]
    fn start_web_fill_loads(&mut self) {
        for (photo, id) in self.patches.take_wanted() {
            let tx = self.patches.landing_sender();
            let handle = photo.parent().and_then(|dir| self.web.dir_handle(dir));
            wasm_bindgen_futures::spawn_local(async move {
                let result = read_patch(handle.as_ref(), &photo, id).await;
                let _ = tx.send(web_store::Landing { photo, id, result });
            });
        }
    }
}

/// Reads and decodes one patch file through File System Access. A failure
/// is logged to the console: the fill then draws nothing, with no notice.
#[cfg(target_arch = "wasm32")]
async fn read_patch(
    handle: Option<&web_sys::FileSystemDirectoryHandle>,
    photo: &Path,
    id: crate::develop::RasterId,
) -> Result<Patch, PatchError> {
    let read = async {
        let (Some(handle), Some(name)) = (handle, photo.file_name()) else {
            return Err(PatchError::Io("the folder is no longer open".into()));
        };
        match crate::web::web_folder_store_fs::read_fill(handle, name, id).await {
            Ok(Some(bytes)) => Patch::decode(&bytes, id),
            Ok(None) => Err(PatchError::Missing),
            Err(e) => Err(PatchError::Io(e)),
        }
    };
    let result = read.await;
    if let Err(e) = &result {
        web_sys::console::warn_1(&format!("[fill] {}: {e}", photo.display()).into());
    }
    result
}

/// `fills` with the patch each one shows, read from the folder `handle`
/// points at, for a web export about to run. It reads rather than asks the
/// store, so a batch holds only the photos in flight. A patch that can't
/// be read draws nothing, as [`FillSet::read`] has it natively.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn read_fill_set(
    handle: Option<&web_sys::FileSystemDirectoryHandle>,
    photo: &Path,
    fills: &[Fill],
) -> FillSet {
    let mut read = Vec::new();
    for fill in fills {
        if let Ok(patch) = read_patch(handle, photo, fill.chosen()).await {
            read.push(std::sync::Arc::new(patch));
        }
    }
    FillSet::new(fills, |id| {
        read.iter()
            .find(|p| p.id() == id)
            .map(std::sync::Arc::clone)
    })
}

/// [`App::fills_of`] as a free function, so a caller holding the loader
/// can still ask.
pub(crate) fn fills_in<'a>(
    folder_store: &'a crate::persist::folder_store::FolderStore,
    patches: &'a PatchStore,
    path: &Path,
) -> &'a [Fill] {
    #[cfg(not(target_arch = "wasm32"))]
    if let Some(unsaved) = patches.unsaved_fills(path) {
        return unsaved;
    }
    #[cfg(target_arch = "wasm32")]
    let _ = patches;
    folder_store.fills(path)
}

/// What a background bake of `path` carries. A native worker reads the
/// patch files itself, in a read-only folder too, since reading writes
/// nothing, so there is always an answer. In the browser the bake takes
/// the patches the store holds: `None` while one is still being read, and
/// the caller asks again on a later frame.
pub(crate) fn fill_input_of(
    folder_store: &crate::persist::folder_store::FolderStore,
    patches: &mut PatchStore,
    path: &Path,
) -> Option<FillInput> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(fill_input_on_disk(folder_store, patches, path))
    }
    #[cfg(target_arch = "wasm32")]
    {
        patches
            .ready_set(path, folder_store.fills(path))
            .map(FillInput::Ready)
    }
}

/// [`fill_input_of`] natively, where there is always an answer.
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn fill_input_on_disk(
    folder_store: &crate::persist::folder_store::FolderStore,
    patches: &PatchStore,
    path: &Path,
) -> FillInput {
    match patches.unsaved_set(path) {
        Some(set) => FillInput::Ready(set),
        None => FillInput::OnDisk(folder_store.fills(path).to_vec()),
    }
}

#[cfg(test)]
#[cfg(not(target_arch = "wasm32"))]
mod tests {
    use super::*;
    use crate::develop::fill::{self, Fill, Patch, PatchError};
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

    #[test]
    fn the_browsers_store_wants_each_file_once_and_holds_a_bake_until_it_lands() {
        use super::web_store::{Landing, PatchStore};
        let photo = PathBuf::from("/photos/a.jpg");
        let (p, q) = (patch(1), patch(2));
        let fills = [fill_of(&p), fill_of(&q)];
        let mut store = PatchStore::new();

        assert!(store.ready_set(&photo, &fills).is_none(), "a bake waits");
        assert!(
            store.ask(&photo, &fills).is_empty(),
            "the Loupe draws without"
        );
        let wanted = store.take_wanted();
        assert_eq!(wanted, [(photo.clone(), p.id()), (photo.clone(), q.id())]);
        assert!(store.take_wanted().is_empty(), "asked for once");
        assert!(store.busy());
        assert!(!store.drain(), "nothing read yet");

        let tx = store.landing_sender();
        let land = |patch: &Patch, result| {
            let (photo, id) = (photo.clone(), patch.id());
            tx.send(Landing { photo, id, result }).unwrap();
        };
        land(&p, Ok(p.clone()));
        assert!(store.drain());
        assert_eq!(
            store.ask(&photo, &fills).len(),
            1,
            "the Loupe draws what landed"
        );
        assert!(
            store.ready_set(&photo, &fills).is_none(),
            "one is still out"
        );

        // A file that can't be read settles: the bake goes without it.
        land(&q, Err(PatchError::Missing));
        assert!(store.drain(), "a failure lets the waiting bake go");
        assert!(!store.busy());
        assert_eq!(store.ready_set(&photo, &fills).map(|s| s.len()), Some(1));
        assert!(store.take_wanted().is_empty(), "and is not asked for again");

        // Another folder: everything is forgotten, and a late read dropped.
        store.clear();
        land(&p, Ok(p.clone()));
        assert!(!store.drain());
        assert!(store.ask(&photo, &fills).is_empty());
    }

    #[test]
    fn the_browsers_store_drops_the_oldest_photos_patches_over_its_budget() {
        use super::web_store::{Landing, PatchStore};
        let (a, b) = (
            PathBuf::from("/photos/a.jpg"),
            PathBuf::from("/photos/b.jpg"),
        );
        let (p, q) = (patch(1), patch(2));
        // Room for one 1x1 patch.
        let mut store = PatchStore::with_budget(4);
        let tx = store.landing_sender();
        for (photo, patch) in [(&a, &p), (&b, &q)] {
            store.ask(photo, &[fill_of(patch)]);
            assert_eq!(store.take_wanted().len(), 1);
            let (photo, id) = (photo.clone(), patch.id());
            let result = Ok(patch.clone());
            tx.send(Landing { photo, id, result }).unwrap();
            assert!(store.drain());
        }
        assert_eq!(store.ask(&b, &[fill_of(&q)]).len(), 1, "the newest stays");
        assert!(store.take_wanted().is_empty());
        assert!(store.ask(&a, &[fill_of(&p)]).is_empty(), "the oldest went");
        assert_eq!(
            store.take_wanted(),
            [(a.clone(), p.id())],
            "and is read again"
        );
    }
}
