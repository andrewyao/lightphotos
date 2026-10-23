//! Local adjustment masks on the shown photo: the one writer of the masks
//! mirror, the draw tool, the brush, and the selected mask. Every change
//! goes through the main undo history, so a drag is one step like a slider
//! drag, and so is a brush stroke.

use super::*;

use super::coverage;
use crate::develop::brush::{self, BrushRaster};
use crate::develop::mask::{
    BRUSH_RADIUS_RANGE, MAX_BRUSH_POINTS, MAX_STROKES_PER_MASK, STROKE_POINTS,
};
use crate::develop::{
    DetectTarget, Mask, MaskInput, MaskSet, MaskShape, RasterId, Stroke, MAX_MASKS,
};

/// The shape the armed mask tool draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum MaskKind {
    Linear,
    Radial,
    /// Paints: each press-to-release is one stroke.
    Brush,
}

/// The largest brush the panel and keys allow, as a fraction of the
/// shorter side. A sidecar or command may hold up to the full range.
pub(crate) const BRUSH_MAX_RADIUS: f32 = 0.5;

/// What the next stroke paints with. Kept apart from Touch Up's brush, as
/// Lightroom keeps them.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct BrushSettings {
    /// A fraction of the photo's shorter side.
    pub radius: f32,
    pub feather: f32,
    pub flow: f32,
    /// Erases without Alt held.
    pub erase: bool,
}

impl Default for BrushSettings {
    fn default() -> Self {
        BrushSettings {
            radius: 0.05,
            feather: 0.5,
            flow: 1.0,
            erase: false,
        }
    }
}

impl BrushSettings {
    fn clamped(self) -> Self {
        let finite = |v: f32, or: f32| if v.is_finite() { v } else { or };
        let d = BrushSettings::default();
        BrushSettings {
            radius: finite(self.radius, d.radius)
                .clamp(*BRUSH_RADIUS_RANGE.start(), BRUSH_MAX_RADIUS),
            feather: finite(self.feather, d.feather).clamp(0.0, 1.0),
            flow: finite(self.flow, d.flow).clamp(0.01, 1.0),
            erase: self.erase,
        }
    }
}

/// A stroke from press to release. The photo's masks as they will be
/// saved, this stroke included, show in the Loupe and the panel while it is
/// painted; the release saves them as one undo step.
pub(crate) struct Painting {
    path: PathBuf,
    /// The photo's masks with this stroke in mask `index`.
    masks: Vec<Mask>,
    index: usize,
    /// Mask `index`'s strokes before this one.
    before: Arc<[Stroke]>,
    stroke: Stroke,
    /// Mask `index`'s raster, stamped a dab at a time as points arrive.
    raster: BrushRaster,
    aspect: f32,
    /// Points this stroke may still take toward `MAX_BRUSH_POINTS`.
    points_left: usize,
    /// The point limit was reported, so a drag past it says so once.
    capped: bool,
}

/// The last stroke's raster, still in floats, with the strokes it holds.
/// The next stroke on that mask starts from it instead of re-stamping every
/// stroke before it.
pub(crate) struct KeptRaster {
    strokes: Arc<[Stroke]>,
    aspect: f32,
    raster: BrushRaster,
}

impl Painting {
    /// Stamps the points from `from` on and puts the masks and their raster
    /// where the next render finds them.
    fn stamp_from(&mut self, from: usize) {
        self.raster.stamp(&self.stroke, from);
        let strokes: Arc<[Stroke]> = self
            .before
            .iter()
            .chain(std::iter::once(&self.stroke))
            .cloned()
            .collect();
        let Some(m) = self.masks.get_mut(self.index) else {
            return;
        };
        // The raster one point ago is never drawn again; the committed one
        // is, after an undo.
        if let Some(old) = m.strokes().filter(|old| !Arc::ptr_eq(old, &self.before)) {
            brush::forget(old, self.aspect);
        }
        brush::remember(&strokes, self.aspect, Arc::new(self.raster.coverage()));
        m.shape = MaskShape::Brush { strokes };
    }
}

impl App {
    /// The shown photo's masks, with a stroke being painted on it.
    pub(crate) fn current_masks(&self) -> &[Mask] {
        let Some(path) = self.shown.path() else {
            return &[];
        };
        if let Some(p) = self.painting.as_ref().filter(|p| p.path == path) {
            return &p.masks;
        }
        self.masks.get(path).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The shown photo's masks with their coverage, for the Loupe and the
    /// histogram. Asks the store, so a missing coverage starts loading or
    /// detecting; a pending target renders as nothing until it lands.
    pub(crate) fn current_mask_set(&mut self) -> MaskSet {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return MaskSet::default();
        };
        let painting = self.painting.as_ref().filter(|p| p.path == path);
        let masks = match painting {
            Some(p) => &p.masks[..],
            None => self.masks.get(&path).map_or(&[][..], Vec::as_slice),
        };
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
    ///
    /// Not in a read-only folder: the coverage stays in the store's memory,
    /// and its sidecar isn't the app's to write.
    fn record_detection(&mut self, path: &Path, target: DetectTarget, id: Option<RasterId>) {
        if self.coverage.read_only_photo(path) {
            return;
        }
        // A job result like any other: through the cache when the folder
        // takes edits, else parked for the photo's sidecar (save_patch).
        let at = crate::sidecar::unix_now();
        self.save_patch(
            path,
            crate::persist::folder_store::Patch::Detection { target, id, at },
        );
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
        // A stroke whose brush was put down mid-drag (Esc, another tool or
        // page) ends where it got to, so it is saved and never left open.
        let off = self
            .painting
            .as_ref()
            .is_some_and(|p| Some(p.path.as_path()) != self.shown.path());
        if self.painting.is_some() && (off || !(self.brush_armed() && self.masks_shown())) {
            self.end_stroke(None);
        }
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
    // Natively a worker reads the coverage files itself, except in a
    // read-only folder, whose detections write none; there, as in the
    // browser, the bake takes what the store has in memory.
    #[cfg(not(target_arch = "wasm32"))]
    if !coverage.in_memory_for(path) {
        return Some(MaskInput::OnDisk { masks, detections });
    }
    Some(MaskInput::Ready(MaskSet::new(&masks, |t| {
        coverage.ready(path, t)
    })))
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

    /// Whether the armed tool is the Brush.
    pub(crate) fn brush_armed(&self) -> bool {
        self.mask_tool() == Some(MaskKind::Brush)
    }

    /// Whether a stroke is being painted.
    pub(crate) fn painting(&self) -> bool {
        self.painting.is_some()
    }

    pub(crate) fn brush(&self) -> BrushSettings {
        self.brush
    }

    pub(super) fn set_brush(&mut self, brush: BrushSettings) {
        self.brush = brush.clamped();
        self.request_redraw();
    }

    /// Multiplicative, so each press is the same visible step at any size,
    /// as Touch Up's brackets are.
    pub(super) fn step_brush_radius(&mut self, grow: bool) {
        let factor = if grow { 1.15 } else { 1.0 / 1.15 };
        self.set_brush(BrushSettings {
            radius: self.brush.radius * factor,
            ..self.brush
        });
    }

    pub(super) fn step_brush_feather(&mut self, delta: f32) {
        self.set_brush(BrushSettings {
            feather: self.brush.feather + delta,
            ..self.brush
        });
    }

    /// The brush's radius in source pixels, for the panel's Size readout.
    pub(crate) fn brush_radius_px(&self) -> f32 {
        let (w, h) = self.image_size();
        self.brush.radius * w.min(h)
    }

    /// A press with the Brush armed starts a stroke at source `uv`. It adds
    /// to the picked brush mask, or makes a new one when none is picked;
    /// `erase` is Alt, which erases as the panel's Erase does.
    pub(super) fn begin_stroke(&mut self, uv: [f32; 2], erase: bool) {
        if !self.brush_armed() || self.painting.is_some() {
            return;
        }
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let t = crate::i18n::t();
        let mut masks = self.current_masks().to_vec();
        let used: usize = masks.iter().map(Mask::brush_points).sum();
        let points_left = MAX_BRUSH_POINTS
            .saturating_sub(used)
            .saturating_sub(STROKE_POINTS);
        if points_left == 0 {
            self.set_status(StatusKind::Error, t.brush_point_limit.into());
            return;
        }
        let picked = self
            .selected_mask()
            .filter(|&i| masks.get(i).is_some_and(|m| m.strokes().is_some()));
        let full = picked
            .and_then(|i| masks.get(i))
            .and_then(Mask::strokes)
            .is_some_and(|s| s.len() >= MAX_STROKES_PER_MASK);
        if full {
            self.set_status(StatusKind::Error, t.brush_stroke_limit.into());
            return;
        }
        let index = match picked {
            Some(i) => i,
            None if masks.len() >= MAX_MASKS => {
                self.set_status(StatusKind::Error, t.mask_limit.into());
                return;
            }
            None => {
                masks.push(Mask::brush());
                masks.len() - 1
            }
        };
        let before = masks
            .get(index)
            .and_then(Mask::strokes)
            .cloned()
            .unwrap_or_else(|| Arc::from([]));
        let (w, h) = self.image_size();
        let aspect = w / h;
        let b = self.brush;
        let mut stroke = Stroke {
            radius: b.radius,
            feather: b.feather,
            flow: b.flow,
            erase: b.erase || erase,
            points: Vec::new(),
        };
        brush::push_point(&mut stroke, uv, aspect);
        let raster = match self.kept_raster.take() {
            Some(k) if Arc::ptr_eq(&k.strokes, &before) && k.aspect == aspect => k.raster,
            _ => BrushRaster::of(&before, aspect),
        };
        let mut painting = Painting {
            path,
            masks,
            index,
            raster,
            before,
            stroke,
            aspect,
            points_left,
            capped: false,
        };
        painting.stamp_from(0);
        self.painting = Some(painting);
        self.mask_selected = Some(index);
        self.painted();
    }

    /// The pointer moved to `uv` while painting: the stroke gains it, unless
    /// it lies within a quarter radius of the last point or the photo is out
    /// of points.
    pub(super) fn extend_stroke(&mut self, uv: [f32; 2]) {
        let shown = self.shown.path();
        let Some(p) = self
            .painting
            .as_mut()
            .filter(|p| Some(p.path.as_path()) == shown)
        else {
            return;
        };
        if p.stroke.points.len() >= p.points_left {
            if !std::mem::replace(&mut p.capped, true) {
                let t = crate::i18n::t();
                self.set_status(StatusKind::Error, t.brush_point_limit.into());
            }
            return;
        }
        if brush::push_point(&mut p.stroke, uv, p.aspect) {
            let last = p.stroke.points.len() - 1;
            p.stamp_from(last);
            self.painted();
        }
    }

    /// The pointer let go, at `uv` when it is known: the stroke ends there
    /// and the masks are saved, one undo step.
    pub(super) fn end_stroke(&mut self, uv: Option<[f32; 2]>) {
        let Some(mut p) = self.painting.take() else {
            return;
        };
        let room = p.stroke.points.len() < p.points_left;
        if let Some(uv) = uv.filter(|_| room) {
            if brush::finish_at(&mut p.stroke, uv) {
                let last = p.stroke.points.len() - 1;
                p.stamp_from(last);
            }
        }
        let index = p.index;
        if let Some(strokes) = p.masks.get(index).and_then(Mask::strokes) {
            self.kept_raster = Some(KeptRaster {
                strokes: Arc::clone(strokes),
                aspect: p.aspect,
                raster: p.raster,
            });
        }
        self.put_masks(&p.path, p.masks);
        if self.shown.path() == Some(p.path.as_path()) {
            self.mask_selected = Some(index);
        }
    }

    /// A stroke changed what the Loupe shows: upload it and its tint.
    fn painted(&mut self) {
        self.push_adjustments();
        self.sync_mask_overlay();
        self.request_redraw();
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
        // An edit while a stroke is held (Delete, a command, a paste) was
        // made on the masks with the stroke in them, so save the stroke
        // first and it lands on top.
        if self.painting.as_ref().is_some_and(|p| p.path == path) {
            self.end_stroke(None);
        }
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
        // Undo or redo mid-stroke replaces the masks under the stroke, so the
        // stroke goes rather than write them back on release.
        if self.painting.as_ref().is_some_and(|p| p.path == path) {
            self.painting = None;
        }
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
        assert_eq!(
            app.current_photo_masks().coverage_key(),
            0,
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
        assert_ne!(
            app.current_photo_masks().coverage_key(),
            0,
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

    /// An app showing photo 0 of `n` in the Loupe, with K pressed: the Brush
    /// armed on the Masks page.
    fn brushing(tag: &str, n: usize) -> (App, PathBuf, Vec<PathBuf>) {
        let (mut app, dir, paths) = folder_app(tag, n);
        app.select_single(0);
        app.shown = Shown::Preview(paths[0].clone(), 1500, 1000);
        app.mode = ViewMode::Loupe;
        app.handle_key(KeyCode::KeyK);
        (app, dir, paths)
    }

    fn paint(app: &mut App, points: &[[f32; 2]]) {
        let (first, rest) = points.split_first().unwrap();
        app.begin_stroke(*first, false);
        for p in rest {
            app.extend_stroke(*p);
        }
        app.end_stroke(points.last().copied());
    }

    /// The defect: strokes past `MAX_STROKES_PER_MASK` were painted and
    /// saved, then cut on the next load, changing the mask.
    #[test]
    fn a_full_brush_mask_refuses_another_stroke() {
        let (mut app, dir, _) = brushing("brush-stroke-cap", 1);
        paint(&mut app, &[[0.5, 0.5]]);
        let one = app.current_masks()[0].strokes().unwrap()[0].clone();
        let mut full = app.current_masks()[0].clone();
        full.shape = MaskShape::Brush {
            strokes: vec![one; MAX_STROKES_PER_MASK].into(),
        };
        app.set_mask(0, full.clone());
        paint(&mut app, &[[0.2, 0.2], [0.3, 0.3]]);
        assert!(!app.painting());
        assert_eq!(app.current_masks(), &[full]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stroke_painted_on_the_last_one_matches_painting_them_all_at_once() {
        let (mut app, dir, _) = brushing("brush-kept", 1);
        paint(&mut app, &[[0.2, 0.5], [0.6, 0.5]]);
        paint(&mut app, &[[0.4, 0.2], [0.4, 0.8]]);
        app.begin_stroke([0.3, 0.3], true);
        app.end_stroke(Some([0.5, 0.6]));
        let strokes = app.current_masks()[0].strokes().unwrap().clone();
        assert_eq!(strokes.len(), 3);
        let (w, h) = app.image_size();
        let shown = brush::coverage(&strokes, w / h);
        let fresh = BrushRaster::of(&strokes, w / h).coverage();
        assert_eq!(shown.id(), fresh.id());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn k_arms_the_brush_on_the_masks_page_and_k_again_puts_it_down() {
        let (mut app, dir, _) = brushing("brush-k", 1);
        assert!(app.brush_armed());
        assert!(app.develop_open && app.develop_tab == DevelopTab::Masks);
        assert!(app.masks_shown());
        app.handle_key(KeyCode::KeyK);
        assert!(!app.brush_armed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn one_stroke_is_one_undo_step_and_the_next_adds_to_the_picked_mask() {
        let (mut app, dir, paths) = brushing("brush-undo", 1);
        paint(&mut app, &[[0.2, 0.5], [0.4, 0.5], [0.6, 0.55]]);
        app.save_edit_unless_dragging();
        assert_eq!(app.current_masks().len(), 1, "the first stroke made a mask");
        assert_eq!(app.selected_mask(), Some(0), "and picked it");
        let strokes = app.current_masks()[0].strokes().unwrap().clone();
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].points.first(), Some(&[0.2, 0.5]));
        assert_eq!(strokes[0].points.last(), Some(&[0.6, 0.55]));
        assert!(!app.painting());

        app.begin_stroke([0.5, 0.2], true);
        assert!(app.painting());
        assert_eq!(
            app.current_masks()[0].strokes().unwrap().len(),
            2,
            "the Loupe shows the stroke while it is painted"
        );
        app.extend_stroke([0.5, 0.8]);
        app.end_stroke(None);
        app.save_edit_unless_dragging();
        assert_eq!(app.current_masks().len(), 1, "added to the picked mask");
        let strokes = app.current_masks()[0].strokes().unwrap().clone();
        assert_eq!(strokes.len(), 2);
        assert!(strokes[1].erase, "Alt erases");

        app.modifiers = winit::keyboard::ModifiersState::SUPER;
        app.handle_key(KeyCode::KeyZ);
        assert_eq!(
            app.current_masks()[0].strokes().unwrap().len(),
            1,
            "Cmd+Z took back one stroke"
        );
        app.handle_key(KeyCode::KeyZ);
        assert!(app.current_masks().is_empty(), "then the first");
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        assert!(app.folder_store.masks(&paths[0]).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The defect: the release saved the masks as they were at the press,
    /// so a Delete or an undo made while the button was held came undone,
    /// and a stroke held across a change of photo painted the old one.
    #[test]
    fn edits_and_photo_changes_mid_stroke_are_not_lost() {
        let (mut app, dir, paths) = brushing("brush-mid", 2);
        app.add_mask(radial());
        app.select_mask(None);
        app.begin_stroke([0.2, 0.5], false);
        app.delete_mask(0);
        assert!(!app.painting(), "the delete saved the stroke first");
        let masks = app.current_masks();
        assert_eq!(masks.len(), 1, "{masks:?}");
        assert!(masks[0].strokes().is_some(), "the brush mask stays");

        app.select_mask(Some(0));
        app.begin_stroke([0.3, 0.5], false);
        app.extend_stroke([0.6, 0.5]);
        app.modifiers = winit::keyboard::ModifiersState::SUPER;
        app.handle_key(KeyCode::KeyZ);
        assert!(!app.painting(), "undo drops the held stroke");
        let undone = app.current_masks().to_vec();
        app.end_stroke(Some([0.7, 0.5]));
        assert_eq!(
            app.current_masks(),
            &undone[..],
            "and the release doesn't write over the undo"
        );
        app.modifiers = winit::keyboard::ModifiersState::empty();
        app.store_masks(&paths[0], vec![Mask::brush()]);
        app.select_mask(Some(0));

        app.begin_stroke([0.3, 0.5], false);
        app.shown = Shown::Preview(paths[1].clone(), 1500, 1000);
        app.extend_stroke([0.9, 0.9]);
        app.poll_coverage();
        assert!(!app.painting(), "a new photo ends the stroke");
        let strokes = app.masks[&paths[0]][0].strokes().unwrap().clone();
        assert_eq!(strokes.len(), 1);
        assert_eq!(strokes[0].points, vec![[0.3, 0.5]], "nothing from photo 1");
        assert!(!app.masks.contains_key(&paths[1]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stroke_reaches_the_renderer_as_coverage() {
        let (mut app, dir, _) = brushing("brush-render", 1);
        let mut m = Mask::brush();
        m.amounts.exposure = 1.0;
        app.add_mask(m);
        paint(&mut app, &[[0.3, 0.5], [0.7, 0.5]]);
        let placed = app.current_photo_masks();
        assert_ne!(placed.coverage_key(), 0, "a raster");
        let base = crate::develop::LocalAmounts::default();
        assert_eq!(placed.local_tone(base, [0.5, 0.5]).exposure, 1.0);
        assert_eq!(placed.local_tone(base, [0.5, 0.95]).exposure, 0.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_brush_respects_max_masks_and_the_point_cap() {
        let (mut app, dir, _) = brushing("brush-caps", 1);
        for _ in 0..MAX_MASKS {
            assert!(app.add_mask(radial()).is_some());
        }
        app.select_mask(None);
        app.begin_stroke([0.5, 0.5], false);
        assert!(!app.painting(), "no room for a new mask");
        assert_eq!(app.current_masks().len(), MAX_MASKS);

        app.delete_mask(0);
        let mut nearly = Mask::brush();
        nearly.shape = MaskShape::Brush {
            strokes: vec![
                Stroke {
                    radius: 0.01,
                    feather: 0.5,
                    flow: 1.0,
                    erase: false,
                    points: vec![[0.5, 0.5]; MAX_BRUSH_POINTS - 2 * STROKE_POINTS - 2],
                };
                1
            ]
            .into(),
        };
        app.add_mask(nearly);
        paint(&mut app, &[[0.1, 0.1], [0.2, 0.1], [0.3, 0.1], [0.4, 0.1]]);
        let total: usize = app.current_masks().iter().map(Mask::brush_points).sum();
        assert_eq!(total, MAX_BRUSH_POINTS, "the stroke stopped at the cap");
        app.begin_stroke([0.5, 0.5], false);
        assert!(!app.painting(), "and a full photo takes no new stroke");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn paste_carries_brush_strokes_to_another_photo() {
        use crate::develop::copy::{CopyItem, CopyItems};
        let (mut app, dir, paths) = brushing("brush-paste", 2);
        paint(&mut app, &[[0.2, 0.5], [0.6, 0.5]]);
        let painted = app.current_masks().to_vec();
        app.copied_settings = Some(super::super::folder_store::CopiedSettings {
            path: paths[0].clone(),
            adj: Adjustments::default(),
            masks: painted.clone(),
            touchups: Vec::new(),
            items: CopyItems::NONE.with(CopyItem::Masks, true),
        });
        app.sel = Some(1);
        app.selected = [1].into_iter().collect();
        app.apply_settings_to_selection();
        app.save_edit_unless_dragging();
        assert_eq!(app.masks[&paths[1]], painted);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(10));
        assert_eq!(
            app.folder_store.masks(&paths[1]),
            painted,
            "and the sidecar"
        );
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

    /// Review 3.7 round 4: a detection result while the folder isn't
    /// editable yet (its lock held, its sidecars still loading) is a field
    /// change that lands on the sidecar, not one dropped.
    #[test]
    fn a_detection_before_the_load_lands_is_saved() {
        let dir = temp_folder("detect-before-load");
        let photo = dir.join("a.jpg");
        std::fs::write(&photo, b"").unwrap();
        let mut app = App::new(None);
        app.load_playlist(crate::navigation::Playlist::from_dir(&dir), dir.clone());
        let opening = app.folder_store.opening();
        let id: RasterId = "00000000000000ab".parse().unwrap();
        app.record_detection(&photo, DetectTarget::Sky, Some(id));
        wait_for_folder_store(&mut app);
        app.folder_store
            .flush_blocking(std::time::Duration::from_secs(5));
        let on_disk = crate::sidecar::read_record(&photo).unwrap().detections.sky;
        let _ = std::fs::remove_dir_all(&dir);
        assert!(opening);
        assert_eq!(on_disk, Some(id));
    }
}
