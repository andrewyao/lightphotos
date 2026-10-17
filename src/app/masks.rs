//! Local adjustment masks on the shown photo: the one writer of the masks
//! mirror, the draw tool, and the selected mask. Every change goes through
//! the main undo history, so a drag is one step like a slider drag.

use super::*;

use super::coverage;
use crate::develop::{DetectTarget, Mask, MaskInput, MaskSet, RasterId, MAX_MASKS};

/// The shape the armed mask tool draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MaskKind {
    Linear,
    Radial,
}

impl App {
    pub(crate) fn current_masks(&self) -> &[Mask] {
        self.shown
            .path()
            .and_then(|p| self.masks.get(p))
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// The shown photo's masks with their coverage, for the Loupe and the
    /// histogram. Asks the store, so a missing coverage starts loading or
    /// detecting; a pending target renders as nothing until it lands.
    pub(crate) fn current_mask_set(&mut self) -> MaskSet {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return MaskSet::default();
        };
        let masks = self.masks.get(&path).map_or(&[][..], Vec::as_slice);
        let recorded = self.detections.get(&path);
        MaskSet::new(masks, |t| {
            match self.coverage.ask(&path, t, recorded.and_then(|d| d.get(t))) {
                coverage::Lookup::Ready(c) => Some(c),
                coverage::Lookup::Pending | coverage::Lookup::Failed => None,
            }
        })
    }

    /// [`current_mask_set`](Self::current_mask_set) placed on the shown
    /// photo, brush rasters and all: what the Loupe and the histogram draw.
    pub(crate) fn current_photo_masks(&mut self) -> crate::develop::PhotoMasks {
        let (w, h) = self.image_size();
        self.current_mask_set().on_photo(w / h)
    }

    /// The panel's mark on mask `i` of the shown photo: `None` for a
    /// parametric mask, or a detected one whose coverage is in hand.
    pub(crate) fn mask_status(&self, i: usize) -> Option<coverage::MaskStatus> {
        let path = self.shown.path()?;
        let target = self.current_masks().get(i)?.target()?;
        let recorded = self.detections.get(path).and_then(|d| d.get(target));
        self.coverage.status(path, target, recorded)
    }

    /// The mask row's Retry: clears the failure so the next frame asks
    /// again. A recorded file that went missing is forgotten too, so a
    /// detector that exists runs afresh.
    pub(super) fn retry_detection(&mut self, i: usize) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(target) = self.current_masks().get(i).and_then(Mask::target) else {
            return;
        };
        if matches!(
            self.mask_status(i),
            Some(coverage::MaskStatus::NothingFound)
                | Some(coverage::MaskStatus::Failed(coverage::Failure::Missing))
        ) {
            self.record_detection(&path, target, None);
        }
        self.coverage.retry(&path, target);
        self.push_adjustments();
        self.hist.invalidate();
    }

    /// Reads, as tasks, the coverage files the store asked for. File System
    /// Access is main-thread only, so no worker can do it; each result goes
    /// back through the store's channel and lands in `drain`.
    #[cfg(target_arch = "wasm32")]
    fn start_web_loads(&mut self) {
        use crate::develop::CoverageError;
        for (photo, target, id) in self.coverage.take_wanted_loads() {
            let tx = self.coverage.web_landing_sender();
            let handle = photo.parent().and_then(|dir| self.web.dir_handle(dir));
            let name = photo.file_name().map(|n| n.to_os_string());
            let (Some(handle), Some(name)) = (handle, name) else {
                let _ = tx.send(coverage::WebLanding::Loaded {
                    photo,
                    target,
                    id,
                    result: Err(CoverageError::Io("the folder is no longer open".into())),
                });
                continue;
            };
            wasm_bindgen_futures::spawn_local(async move {
                let result = read_coverage(&handle, &name, id).await;
                let _ = tx.send(coverage::WebLanding::Loaded {
                    photo,
                    target,
                    id,
                    result,
                });
            });
        }
    }

    /// Runs the detections the store wants for the shown photo, on the
    /// frame the Loupe already decoded (full, preview or thumbnail tier,
    /// whichever is in the cache), fit to 512 px so the heuristic takes a
    /// few milliseconds on the main thread. The coverage file is written
    /// through File System Access before the landing is sent, so the
    /// sidecar can only ever name a file that exists. A pending mask on a
    /// photo that isn't shown waits until it is.
    #[cfg(target_arch = "wasm32")]
    fn start_web_detects(&mut self) {
        use crate::scoring::detect::{self, DetectInput, Rgba8};
        const DETECT_SIDE: u32 = 512;
        let Some(shown) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let preview_px = self.preview_px();
        let Some(img) = self.loader.as_ref().and_then(|l| {
            l.get_full(&shown)
                .or_else(|| l.get_preview(&shown, preview_px))
                .or_else(|| l.get_thumb(&shown, crate::jobs::thumbnail::THUMB_PX))
        }) else {
            return;
        };
        let jobs = self.coverage.take_wanted_detects(|p| p == shown.as_path());
        if jobs.is_empty() {
            return;
        }
        let input = DetectInput::pixels(Rgba8::fit_from(&img, DETECT_SIDE));
        let tx = self.coverage.web_landing_sender();
        let handle = shown.parent().and_then(|dir| self.web.dir_handle(dir));
        let name = shown.file_name().map(|n| n.to_os_string());
        for (photo, target) in jobs {
            let detected = detect::detect(&input, target)
                .map_err(|e| e.to_string())
                .and_then(|c| {
                    let bytes = c.encode().map_err(|e| e.to_string())?;
                    Ok((c, bytes))
                });
            let fail = |why: String| coverage::WebLanding::Detected {
                photo: photo.clone(),
                target,
                result: Err(why),
            };
            let (coverage, bytes) = match detected {
                Ok(ok) => ok,
                Err(e) => {
                    let _ = tx.send(fail(e));
                    continue;
                }
            };
            let (Some(handle), Some(name)) = (handle.clone(), name.clone()) else {
                let _ = tx.send(fail("the folder is no longer open".into()));
                continue;
            };
            let tx = tx.clone();
            wasm_bindgen_futures::spawn_local(async move {
                let written = crate::web::web_folder_store_fs::write_coverage(
                    &handle,
                    &name,
                    coverage.id(),
                    &bytes,
                )
                .await;
                let _ = tx.send(coverage::WebLanding::Detected {
                    photo,
                    target,
                    result: written.map(|()| coverage),
                });
            });
        }
    }

    /// Writes what `target` resolves to for `path`: the sidecar and the
    /// mirror, outside the undo history. A detection is a fact about the
    /// photo, not an edit the user made.
    fn record_detection(&mut self, path: &Path, target: DetectTarget, id: Option<RasterId>) {
        self.folder_store.set_detection(path, target, id);
        let entry = self.detections.entry(path.to_path_buf()).or_default();
        entry.set(target, id);
        if entry.is_empty() {
            self.detections.remove(path);
        }
    }

    /// Each frame: finished detections and loads. A landing records its id
    /// and redraws the photo if it is shown; undo before it lands removes
    /// the mask, but the detection still records, so Redo brings the mask
    /// back with no second detection.
    pub(crate) fn poll_coverage(&mut self) {
        let selection = self.action_paths();
        self.coverage
            .kick(&selection, &self.masks, &self.detections);
        #[cfg(target_arch = "wasm32")]
        {
            self.start_web_loads();
            self.start_web_detects();
        }
        let landed = self.coverage.drain();
        self.sync_mask_overlay();
        if landed.is_empty() {
            return;
        }
        let t = crate::i18n::t();
        for l in landed {
            if let coverage::Record::Set(id) = l.record {
                self.record_detection(&l.photo, l.target, id);
            }
            if self.shown.path() == Some(l.photo.as_path()) {
                self.push_adjustments();
                self.hist.invalidate();
            }
            match l.notice {
                Some(coverage::Notice::NothingFound) => self.set_status(
                    StatusKind::Info,
                    (t.detect_nothing_found)(t.target(l.target)),
                ),
                Some(coverage::Notice::Failed(why)) => self.set_status(
                    StatusKind::Error,
                    (t.detect_failed)(t.target(l.target), &why),
                ),
                None => {}
            }
        }
        self.request_redraw();
    }

    /// What a background bake of `path` carries, or `None` while its
    /// detected masks' work is still out.
    pub(crate) fn mask_input(&mut self, path: &Path) -> Option<MaskInput> {
        mask_input_of(&self.masks, &self.detections, &mut self.coverage, path)
    }
}

/// What a background bake of `path` carries, from the app's mirrors, or
/// `None` while a detected mask's work is still out: the bake defers and
/// asks again next frame, so it never runs without a mask its signature
/// claims. Native workers read the coverage beside the photo themselves;
/// the browser's can't reach the folder, so they take what the store has in
/// memory. A free function, so a caller holding the loader can still build
/// one.
pub(crate) fn mask_input_of(
    masks: &HashMap<PathBuf, Vec<Mask>>,
    detections: &HashMap<PathBuf, crate::develop::Detections>,
    coverage: &mut coverage::CoverageStore,
    path: &Path,
) -> Option<MaskInput> {
    let masks = masks.get(path).cloned().unwrap_or_default();
    let detections = detections.get(path).cloned().unwrap_or_default();
    if coverage.readiness(path, &masks, &detections) == coverage::Readiness::Pending {
        return None;
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Some(MaskInput::OnDisk { masks, detections })
    }
    #[cfg(target_arch = "wasm32")]
    {
        Some(MaskInput::Ready(MaskSet::new(&masks, |t| {
            coverage.ready(path, t)
        })))
    }
}

/// Reads and decodes one recorded coverage file through File System Access.
#[cfg(target_arch = "wasm32")]
async fn read_coverage(
    handle: &web_sys::FileSystemDirectoryHandle,
    photo: &std::ffi::OsStr,
    id: RasterId,
) -> Result<crate::develop::Coverage, crate::develop::CoverageError> {
    use crate::develop::{Coverage, CoverageError};
    match crate::web::web_folder_store_fs::read_coverage(handle, photo, id).await {
        Ok(Some(bytes)) => Coverage::decode(&bytes, id),
        Ok(None) => Err(CoverageError::Missing),
        Err(e) => Err(CoverageError::Io(e)),
    }
}

/// `masks` with the coverage each recorded target names, read from the
/// folder `handle` points at, for a web export about to run. It reads
/// rather than asks the store, so a batch holds only the photos in flight.
/// A target with nothing recorded (failed or unsupported) renders as
/// nothing, as it does everywhere.
#[cfg(target_arch = "wasm32")]
pub(crate) async fn read_mask_set(
    handle: Option<&web_sys::FileSystemDirectoryHandle>,
    photo: &Path,
    masks: &[Mask],
    recorded: &crate::develop::Detections,
) -> Result<MaskSet, String> {
    let mut read = Vec::new();
    for t in DetectTarget::ALL {
        let Some(id) = recorded.get(t) else {
            continue;
        };
        if !masks.iter().any(|m| m.target() == Some(t)) {
            continue;
        }
        let (Some(handle), Some(name)) = (handle, photo.file_name()) else {
            return Err("the folder is no longer open".into());
        };
        let coverage = read_coverage(handle, name, id)
            .await
            .map_err(|e| e.to_string())?;
        read.push((t, Arc::new(coverage)));
    }
    Ok(MaskSet::new(masks, |t| {
        read.iter()
            .find(|(r, _)| *r == t)
            .map(|(_, c)| Arc::clone(c))
    }))
}

impl App {
    /// The selected mask's index, while it still names one.
    pub(crate) fn selected_mask(&self) -> Option<usize> {
        self.mask_selected
            .filter(|&i| i < self.current_masks().len())
    }

    /// The shape the next drag on the photo draws, if the tool is armed.
    pub(crate) fn mask_tool(&self) -> Option<MaskKind> {
        match self.tool {
            LoupeTool::Mask(kind) => Some(kind),
            _ => None,
        }
    }

    /// Whether the Loupe shows the mask overlay: the Masks page is open and
    /// Touch Up and compare aren't using the photo.
    pub(crate) fn masks_shown(&self) -> bool {
        self.mode == ViewMode::Loupe
            && self.develop_open
            && self.develop_tab == DevelopTab::Masks
            && !self.touchup_active()
            && !self.compare
    }

    /// Arms the tool for `kind`, or puts it down when it is already armed.
    /// Arming opens the Masks page, where the new mask's sliders are.
    pub(super) fn toggle_mask_tool(&mut self, kind: MaskKind) {
        if self.mask_tool() == Some(kind) {
            self.tool = LoupeTool::None;
        } else {
            if self.mode != ViewMode::Loupe {
                self.enter_loupe();
            }
            if self.selected_path().is_none() {
                return;
            }
            self.reveal_develop();
            if self.develop_tab != DevelopTab::Masks {
                self.set_develop_tab(DevelopTab::Masks);
            }
            self.tool = LoupeTool::Mask(kind);
            self.touchup_selected = None;
        }
        self.request_redraw();
    }

    pub(super) fn select_mask(&mut self, i: Option<usize>) {
        self.mask_selected = i.filter(|&i| i < self.current_masks().len());
        self.request_redraw();
    }

    /// Adds `mask` to the shown photo and selects it. Returns its index, or
    /// `None` when the photo already holds [`MAX_MASKS`].
    pub(super) fn add_mask(&mut self, mask: Mask) -> Option<usize> {
        let path = self.shown.path().map(Path::to_path_buf)?;
        let mut all = self.current_masks().to_vec();
        if all.len() >= MAX_MASKS {
            self.set_status(StatusKind::Error, crate::i18n::t().mask_limit.into());
            return None;
        }
        all.push(mask);
        let i = all.len() - 1;
        self.put_masks(&path, all);
        self.mask_selected = Some(i);
        Some(i)
    }

    /// Replaces mask `i` of the shown photo.
    pub(super) fn set_mask(&mut self, i: usize, mask: Mask) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let mut all = self.current_masks().to_vec();
        let Some(slot) = all.get_mut(i) else {
            return;
        };
        if *slot == mask {
            return;
        }
        *slot = mask;
        self.put_masks(&path, all);
    }

    pub(super) fn delete_mask(&mut self, i: usize) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let mut all = self.current_masks().to_vec();
        if i >= all.len() {
            return;
        }
        all.remove(i);
        self.put_masks(&path, all);
        self.mask_selected = None;
    }

    /// The one writer of a photo's masks for an edit: records the masks it
    /// replaces for Undo, then stores the new ones.
    pub(super) fn put_masks(&mut self, path: &Path, masks: Vec<Mask>) {
        let before = self.masks.get(path).cloned().unwrap_or_default();
        if before == masks {
            return;
        }
        let adj = self.edits.get(path).copied().unwrap_or_default();
        self.history.note_masks(path, adj, &before);
        self.store_masks(path, masks);
        #[cfg(target_arch = "wasm32")]
        crate::web::analytics::property("develop_edit_applied", "edit_kind", "mask");
    }

    /// Stores `masks` as `path`'s in the mirror and the folder store, and
    /// redraws the photo if it is the shown one. No undo record.
    pub(super) fn store_masks(&mut self, path: &Path, masks: Vec<Mask>) {
        self.folder_store.set_masks(path, &masks);
        // Add, paste, a command, undo and redo all converge here: a detected
        // mask on a photo without that detection gets one queued.
        self.coverage.ensure_detected(
            path,
            &masks,
            self.detections
                .get(path)
                .unwrap_or(&crate::develop::Detections::default()),
        );
        if masks.is_empty() {
            self.masks.remove(path);
        } else {
            self.masks.insert(path.to_path_buf(), masks);
        }
        if self.shown.path() == Some(path) {
            self.push_adjustments();
            self.hist.invalidate();
        }
        self.request_redraw();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::*;
    use crate::develop::MaskShape;

    fn radial() -> Mask {
        let mut m = Mask::radial([0.5, 0.5], [0.2, 0.1]);
        m.amounts.exposure = 1.0;
        m
    }

    #[test]
    fn adding_then_dragging_a_mask_undoes_in_two_steps() {
        let (mut app, dir, photo) = one_photo("mask-undo");
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;

        assert_eq!(app.add_mask(radial()), Some(0));
        app.save_edit_unless_dragging();
        press(&app, true);
        for step in 1..=5 {
            let mut m = radial();
            if let MaskShape::Radial { center, .. } = &mut m.shape {
                center[0] = 0.5 + step as f32 * 0.02;
            }
            app.set_mask(0, m);
            app.save_edit_unless_dragging();
        }
        press(&app, false);
        app.save_edit_unless_dragging();
        let MaskShape::Radial { center, .. } = app.current_masks()[0].shape else {
            panic!("a radial mask");
        };
        assert!((center[0] - 0.6).abs() < 1e-6);
        assert_eq!(app.pushed_masks, 1, "the Loupe got the mask count");

        app.modifiers = winit::keyboard::ModifiersState::SUPER;
        app.handle_key(KeyCode::KeyZ);
        assert_eq!(app.current_masks(), &[radial()], "Cmd+Z undid the drag");
        app.handle_key(KeyCode::KeyZ);
        assert!(app.current_masks().is_empty(), "then the add");
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(app.folder_store.masks(&photo).is_empty(), "and the sidecar");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Polls the app until `done`, as frames would, failing after ten seconds.
    #[cfg(not(target_arch = "wasm32"))]
    fn poll_until(app: &mut App, mut done: impl FnMut(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done(app) {
            assert!(std::time::Instant::now() < deadline, "detection stalled");
            app.poll_coverage();
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The defect: a landing that went through the undo history would make
    /// Cmd+Z after a detection undo the detection instead of the add, and a
    /// re-added mask would detect its subject a second time.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_detection_lands_outside_undo_and_a_re_added_mask_reuses_it() {
        use super::super::coverage::{test_support, CoverageStore, MaskStatus};
        use crate::develop::DetectTarget;
        let (mut app, dir, photo) = one_photo("mask-detect");
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;
        app.coverage = CoverageStore::with_detector(test_support::half_left);
        let runs = test_support::runs_on(&photo);

        assert_eq!(app.add_mask(Mask::detected(DetectTarget::Subject)), Some(0));
        app.save_edit_unless_dragging();
        assert_eq!(app.mask_status(0), Some(MaskStatus::Detecting));
        assert!(
            app.current_mask_set().coverage(0).is_none(),
            "renders as nothing while pending"
        );
        let before = app.edit_sig_for(&photo);

        poll_until(&mut app, |a| a.mask_status(0).is_none());
        assert_eq!(test_support::runs_on(&photo), runs + 1);
        let id = app.detections[&photo]
            .subject
            .expect("recorded in the mirror");
        assert_eq!(
            app.folder_store.detections(&photo).subject,
            Some(id),
            "and the sidecar"
        );
        assert_ne!(app.edit_sig_for(&photo), before, "the landing re-bakes");
        assert!(
            app.current_mask_set().coverage(0).is_some(),
            "and the Loupe gets the coverage"
        );

        app.modifiers = winit::keyboard::ModifiersState::SUPER;
        app.handle_key(KeyCode::KeyZ);
        assert!(app.current_masks().is_empty(), "one step undid the add");
        assert_eq!(
            app.detections[&photo].subject,
            Some(id),
            "the detection stays"
        );
        app.handle_key(KeyCode::KeyZ);
        assert!(
            app.current_masks().is_empty(),
            "there was nothing else to undo"
        );

        app.add_mask(Mask::detected(DetectTarget::Subject));
        app.save_edit_unless_dragging();
        assert_eq!(app.mask_status(0), None, "ready at once");
        assert!(app.coverage.drain().is_empty());
        assert_eq!(
            test_support::runs_on(&photo),
            runs + 1,
            "no second detection"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn a_failed_detection_shows_retry_and_retry_runs_it_again() {
        use super::super::coverage::{test_support, CoverageStore, Failure, MaskStatus};
        use crate::develop::DetectTarget;
        let (mut app, dir, photo) = one_photo("mask-retry");
        app.coverage = CoverageStore::with_detector(test_support::always_fails);
        let runs = test_support::runs_on(&photo);
        app.add_mask(Mask::detected(DetectTarget::Subject));
        poll_until(&mut app, |a| {
            a.mask_status(0) != Some(MaskStatus::Detecting)
        });
        assert_eq!(
            app.mask_status(0),
            Some(MaskStatus::Failed(Failure::Detect("no model today".into())))
        );
        assert!(!app.detections.contains_key(&photo), "nothing recorded");
        for _ in 0..3 {
            app.push_adjustments();
            app.poll_coverage();
        }
        assert_eq!(
            test_support::runs_on(&photo),
            runs + 1,
            "a failure doesn't loop"
        );
        app.retry_detection(0);
        assert_eq!(app.mask_status(0), Some(MaskStatus::Detecting));
        poll_until(&mut app, |a| {
            a.mask_status(0) != Some(MaskStatus::Detecting)
        });
        assert_eq!(test_support::runs_on(&photo), runs + 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The Masks page offers Subject and Sky beside the gradients, names
    /// each mask by its kind, and marks one whose detection failed with a
    /// Retry.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_masks_page_offers_subject_and_sky_and_names_masks_by_kind() {
        use super::super::coverage::{test_support, CoverageStore};
        use crate::develop::DetectTarget;
        use crate::ui::UiAction;
        let (mut app, dir, _photo) = one_photo("mask-page");
        app.playlist = Some(crate::navigation::Playlist::from_dir(&dir));
        app.mode = ViewMode::Loupe;
        app.develop_open = true;
        app.set_develop_tab(DevelopTab::Masks);
        app.coverage = CoverageStore::with_detector(test_support::always_fails);
        let t = crate::i18n::t();

        let painted = settled(&mut app);
        let subject = format!("+ {}", t.subject_mask);
        assert!(painted.has(&subject), "{:?}", painted.texts());
        assert!(painted.has(&format!("+ {}", t.sky_mask)));
        let (actions, _) = click(&mut app, painted.pos_of(&subject));
        let added = actions
            .iter()
            .any(|a| matches!(a, UiAction::AddDetectedMask(DetectTarget::Subject)));
        assert_eq!(
            added,
            cfg!(target_os = "macos"),
            "enabled only where a detector exists: {actions:?}"
        );

        app.add_mask(Mask::radial([0.5, 0.5], [0.1, 0.1]));
        app.add_mask(Mask::detected(DetectTarget::Subject));
        poll_until(&mut app, |a| !a.coverage.busy());
        app.poll_coverage();
        let painted = settled(&mut app);
        assert!(
            painted.has(&format!("{} 1", t.radial_gradient)),
            "{:?}",
            painted.texts()
        );
        let failed = format!("{} 2 !", t.subject_mask);
        assert!(painted.has(&failed), "{:?}", painted.texts());
        assert!(
            painted.has(t.mask_retry),
            "the picked failed mask offers Retry"
        );
        let (actions, _) = click(&mut app, painted.pos_of(t.mask_retry));
        assert!(actions
            .iter()
            .any(|a| matches!(a, UiAction::RetryDetection(1))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Paste lays the same detected mask on every target photo, and each
    /// photo detects its own subject: Lightroom's behaviour, with no paste
    /// special case.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn pasting_a_detected_mask_onto_two_photos_detects_each_photo_once() {
        use super::super::coverage::{test_support, CoverageStore};
        use crate::develop::copy::{CopyItem, CopyItems};
        use crate::develop::DetectTarget;
        let (mut app, dir, paths) = folder_app("mask-paste", 3);
        app.coverage = CoverageStore::with_detector(test_support::half_left);
        app.copied_settings = Some(super::super::folder_store::CopiedSettings {
            path: paths[0].clone(),
            adj: Adjustments::default(),
            masks: vec![Mask::detected(DetectTarget::Subject)],
            touchups: Vec::new(),
            items: CopyItems::NONE.with(CopyItem::Masks, true),
        });
        app.sel = Some(1);
        app.selected = [1, 2].into_iter().collect();
        app.apply_settings_to_selection();
        app.save_edit_unless_dragging();
        assert!(app.coverage.busy());
        poll_until(&mut app, |a| !a.coverage.busy());
        app.poll_coverage();
        for p in &paths[1..] {
            assert_eq!(app.masks[p], vec![Mask::detected(DetectTarget::Subject)]);
            assert_eq!(test_support::runs_on(p), 1, "{}", p.display());
            assert!(app.detections[p].subject.is_some(), "{}", p.display());
            assert!(app.folder_store.detections(p).subject.is_some());
        }
        assert_eq!(
            test_support::runs_on(&paths[0]),
            0,
            "the source photo wasn't pasted onto"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `mask.add` takes a detected shape like any other, and the app queues
    /// its detection as the button would.
    #[cfg(not(target_arch = "wasm32"))]
    #[test]
    fn the_mask_add_command_accepts_a_detected_shape() {
        use crate::command::Command;
        use crate::develop::{DetectTarget, MaskShape};
        let (mut app, dir, paths) = folder_app("mask-add-detected", 1);
        let photo = paths[0].clone();
        app.select_single(0);
        app.shown = Shown::Preview(photo.clone(), 1024, 1024);
        app.mode = ViewMode::Loupe;
        let cmd = Command::parse(
            r#"{"id":"mask.add","params":{"shape":{"kind":"detected","target":"sky"},"amounts":{"exposure":-1}}}"#,
        )
        .unwrap();
        app.run(cmd).unwrap();
        assert_eq!(
            app.current_masks()[0].shape,
            MaskShape::Detected {
                target: DetectTarget::Sky
            }
        );
        assert_eq!(app.selected_mask(), Some(0));
        assert!(
            app.mask_status(0).is_some(),
            "a sky mask is pending or unsupported, never silently ready"
        );
        assert_eq!(app.folder_store.masks(&photo).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_photo_holds_at_most_max_masks() {
        let (mut app, dir, _) = one_photo("mask-limit");
        for _ in 0..MAX_MASKS {
            assert!(app.add_mask(radial()).is_some());
        }
        assert_eq!(app.add_mask(radial()), None);
        assert_eq!(app.current_masks().len(), MAX_MASKS);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
