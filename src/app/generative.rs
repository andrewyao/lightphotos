// SPDX-License-Identifier: MIT OR Apache-2.0

//! Touch Up's Generative brush: paint an area, and a local ComfyUI server
//! fills it. A stroke is painted with the mask brush's raster, sent on
//! release, and comes back as up to three variants of one [`Fill`]. The
//! job runs on its own thread; this module owns the stroke, the job and
//! the picked fill, and is polled from the frame loop.
//!
//! Native only: the browser build draws finished fills and can't make them.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use super::comfy::{bare, Gate};
use super::fill::{GenerativeView, JobView, PickedFill, TouchStep};
use super::{App, LoupeTool, StatusKind};
use crate::comfy::client::{ComfyServer, LOST, STOPPED};
use crate::comfy::features::{self, GENERATIVE_FILL};
use crate::comfy::workflow;
use crate::decode::image_decode::DecodedImage;
use crate::develop::brush::{self, BrushRaster};
use crate::develop::fill::{self, Fill, Patch, MAX_FILLS, MAX_VARIANTS};
use crate::develop::fill_request::{Painted, Request};
use crate::develop::{CaScale, Coverage, Stroke};
use crate::ui::{ComfyAction, FillAction};

/// How long one generation may take, all variants together. The first run
/// after ComfyUI starts also loads gigabytes of weights.
const JOB_TIMEOUT: Duration = Duration::from_secs(20 * 60);

/// The area being painted, or painted and kept: while its job runs, and
/// after a job that failed or was cancelled so Generate can send it again.
struct Painting {
    path: PathBuf,
    aspect: f32,
    stroke: Stroke,
    raster: BrushRaster,
    /// The raster as the Loupe tints it.
    coverage: Arc<Coverage>,
    /// The pointer is still down.
    down: bool,
    /// Why the last job for it failed.
    failed: Option<String>,
}

/// What a job's variants become.
enum Target {
    /// A new fill from the painted stroke.
    New,
    /// New variants for this fill, as it stood when asked.
    More(Fill),
}

struct Job {
    path: PathBuf,
    target: Target,
    /// The server it runs on.
    url: String,
    bbox: [f32; 4],
    /// The middle of the painted area, where the spinner turns.
    pin: [f32; 2],
    prompt: String,
    /// False for a read-only folder: the patches stay in memory.
    written: bool,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
    result: Receiver<Result<Generated, String>>,
}

/// What a job brings back when at least one variant arrived.
#[derive(Debug, PartialEq)]
struct Generated {
    patches: Vec<Patch>,
    /// Why each variant that didn't arrive failed.
    failed: Vec<String>,
}

/// Sorts a job's variants, one result each, into what it brings back. The
/// variants that arrived are kept whatever became of the others; the job
/// fails only when it was stopped or none arrived, with the first reason.
fn settle(variants: Vec<Result<Patch, String>>) -> Result<Generated, String> {
    if variants
        .iter()
        .any(|v| v.as_ref().is_err_and(|e| e == STOPPED))
    {
        return Err(STOPPED.into());
    }
    let (mut patches, mut failed) = (Vec::<Patch>::new(), Vec::new());
    for variant in variants {
        match variant {
            // Two identical results are one variant.
            Ok(patch) if patches.iter().any(|p| p.id() == patch.id()) => {}
            Ok(patch) => patches.push(patch),
            Err(why) => failed.push(why),
        }
    }
    if patches.is_empty() {
        let why = failed.into_iter().next();
        return Err(why.unwrap_or_else(|| "ComfyUI returned no image".into()));
    }
    Ok(Generated { patches, failed })
}

/// A failure as its notice says it. A server that stopped answering is
/// named by its address, with `button` as the way to try again.
fn worded(why: String, url: &str, button: &str) -> String {
    if why != LOST {
        return why;
    }
    crate::i18n::t()
        .comfy_lost
        .replace("{where}", bare(url))
        .replace("{button}", button)
}

fn centre(rect: [f32; 4]) -> [f32; 2] {
    [(rect[0] + rect[2]) * 0.5, (rect[1] + rect[3]) * 0.5]
}

/// Everything a job's thread needs; it shares nothing else with the app.
struct JobInput {
    url: String,
    workflow: &'static str,
    photo: PathBuf,
    img: Arc<DecodedImage>,
    ca: CaScale,
    painted: Painted,
    bbox: [f32; 4],
    prompt: String,
    write: bool,
    stop: Arc<AtomicBool>,
    done: Arc<AtomicUsize>,
}

#[derive(Default)]
pub(crate) struct Generative {
    /// The panel's Generative switch.
    on: bool,
    prompt: String,
    painting: Option<Painting>,
    job: Option<Job>,
    /// The picked fill of the shown photo.
    picked: Option<usize>,
    /// Touch Up was open last frame, to see it open and close.
    was_open: bool,
    /// Settings was open last frame.
    settings_was_open: bool,
    /// Photos whose fills gained variants since Touch Up opened.
    unpruned: HashSet<PathBuf>,
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
}

/// A fresh seed for variant `i`: the clock through splitmix64, so
/// neighbouring variants share no bits.
fn seed(i: usize) -> u64 {
    let mut z = (nanos() as u64).wrapping_add((i as u64 + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15));
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// One generation, start to finish, on the job's thread: upload the crop,
/// queue a workflow per variant, and turn each result into a patch, written
/// beside the photo before the app hears of it. A variant that fails
/// doesn't stop the others.
fn run(input: &JobInput) -> Result<Generated, String> {
    let request = Request::new(&input.img, input.ca, &input.painted, input.bbox)?;
    let server = ComfyServer::new(&input.url)?;
    let nodes = workflow::parse(input.workflow)?;
    let run_id = format!("{:x}", nanos());
    let uploaded =
        server.upload_png(request.upload(), &format!("lightphotos_fill_{run_id}.png"))?;
    let client_id = format!("lightphotos-{run_id}");

    let queued: Vec<Result<(String, String), String>> = (0..MAX_VARIANTS)
        .map(|i| {
            let w = workflow::inject(&nodes, &uploaded.image_ref(), &input.prompt, seed(i))?;
            Ok((server.queue(&w.prompt, &client_id)?, w.output_node))
        })
        .collect();

    let deadline = Instant::now() + JOB_TIMEOUT;
    let mut variants: Vec<Result<Patch, String>> = Vec::new();
    for (n, sent) in queued.iter().enumerate() {
        let patch = sent
            .clone()
            .and_then(|(id, node)| server.wait(&id, &node, deadline, &input.stop))
            .and_then(|images| {
                let first = images.first().ok_or("ComfyUI finished without an image")?;
                server.fetch(first)
            })
            .and_then(|bytes| request.patch(&bytes))
            .and_then(|patch| {
                if input.write {
                    fill::write(&input.photo, &patch)
                        .map_err(|e| format!("could not save the fill beside the photo: {e}"))?;
                }
                Ok(patch)
            });
        let stopped = patch.as_ref().is_err_and(|e| e == STOPPED);
        variants.push(patch);
        if stopped {
            for (id, _) in queued.iter().skip(n).flatten() {
                let _ = server.cancel(id);
            }
            break;
        }
        input.done.store(n + 1, Ordering::Relaxed);
    }
    settle(variants)
}

impl App {
    pub(super) fn fill_action(&mut self, action: FillAction) {
        match action {
            FillAction::SetOn(on) => self.set_generative(on),
            FillAction::SetPrompt(prompt) => self.set_fill_prompt(prompt),
            FillAction::Press(uv) => self.begin_fill_stroke(uv),
            FillAction::Drag(uv) => self.extend_fill_stroke(uv),
            FillAction::Release(uv) => self.end_fill_stroke(uv),
            FillAction::Generate => self.generate(),
            FillAction::GenerateMore => self.generate_more(),
            FillAction::Cancel => self.cancel_generation(),
            FillAction::Discard => self.discard_fill_stroke(),
            FillAction::Select(i) => self.select_fill(i),
            FillAction::Cycle { forward } => self.cycle_fill_variant(forward),
            FillAction::Delete => self.delete_picked_fill(),
        }
    }

    pub(super) fn comfy_action(&mut self, action: ComfyAction) {
        match action {
            ComfyAction::SetEnabled(on) => self.comfy.set_enabled(on),
            ComfyAction::EditServer(text) => self.comfy.edit_server(text),
            ComfyAction::CommitServer => {
                if let Err(why) = self.comfy.commit_server() {
                    self.set_status(StatusKind::Error, why);
                }
            }
            ComfyAction::ChooseModelsFolder => {
                let start = self
                    .comfy
                    .models_dir()
                    .map(Path::to_path_buf)
                    .or_else(super::comfy::suggested_models_dir);
                if let Some(dir) = crate::shell::dialog::pick_models_folder(start) {
                    self.comfy.set_models_dir(dir);
                }
            }
            ComfyAction::SetFeature(id, on) => self.comfy.set_feature(id, on),
            ComfyAction::Download(file) => self.comfy.download(file),
            ComfyAction::CancelDownload(file) => self.comfy.cancel_download(file),
        }
        self.request_redraw();
    }

    fn fill_gate(&self) -> Gate {
        match features::feature(GENERATIVE_FILL) {
            Some(feature) => self.comfy.gate(feature),
            None => Gate::Hidden,
        }
    }

    /// Whether the Generative brush paints: Touch Up is open, the switch is
    /// on and ComfyUI can run it.
    pub(crate) fn generative_armed(&self) -> bool {
        self.touchup_active() && self.generative.on && matches!(self.fill_gate(), Gate::Ready(_))
    }

    /// What the Touch Up panel and the Loupe draw for the Generative brush;
    /// `None` while ComfyUI is off in Settings.
    pub(crate) fn generative_view(&self) -> Option<GenerativeView<'_>> {
        let blocked = match self.fill_gate() {
            Gate::Hidden => return None,
            Gate::Blocked(why) => Some(why),
            // The server is only asked while Touch Up is open.
            Gate::Checking if self.touchup_active() => {
                Some(crate::i18n::t().comfy_checking.to_string())
            }
            Gate::Checking | Gate::Ready(_) => None,
        };
        let g = &self.generative;
        let shown = self.shown.path();
        let job = match (&g.job, &g.painting) {
            (Some(job), _) => JobView::Generating {
                done: job.done.load(Ordering::Relaxed),
                of: MAX_VARIANTS,
                at: (shown == Some(job.path.as_path())).then_some(job.pin),
            },
            (None, Some(p)) if p.down => JobView::Painting,
            (None, Some(p)) if shown == Some(p.path.as_path()) => JobView::Kept {
                why: p.failed.as_deref(),
            },
            _ => JobView::Idle,
        };
        let fills = shown.map(|p| self.fills_of(p)).unwrap_or_default();
        let picked = g.picked.and_then(|index| {
            let fill = fills.get(index)?;
            Some(PickedFill {
                index,
                variant: fill.chosen_index(),
                variants: fill.variants().len(),
            })
        });
        Some(GenerativeView {
            on: g.on,
            armed: self.generative_armed(),
            blocked,
            prompt: &g.prompt,
            job,
            pins: fills.iter().map(|f| self.fill_pin(shown, f)).collect(),
            picked,
        })
    }

    /// Where a fill's pin sits: the middle of its painted area, or of its
    /// rect until its patch is in memory.
    fn fill_pin(&self, photo: Option<&Path>, fill: &Fill) -> [f32; 2] {
        photo
            .and_then(|photo| self.patches.ready(photo, fill.chosen()))
            .map_or_else(|| centre(fill.bbox()), |p| p.painted_centre(fill.bbox()))
    }

    /// The stroke painted and not yet filled, as the Loupe tints it.
    pub(crate) fn generative_coverage(&self) -> Option<Arc<Coverage>> {
        let p = self.generative.painting.as_ref()?;
        (self.touchup_active() && self.shown.path() == Some(p.path.as_path()))
            .then(|| Arc::clone(&p.coverage))
    }

    pub(super) fn set_generative(&mut self, on: bool) {
        self.generative.on = on;
        if !on {
            self.discard_fill_stroke();
        }
        self.generative.picked = None;
        self.touchup_selected = None;
        self.request_redraw();
    }

    pub(super) fn set_fill_prompt(&mut self, prompt: String) {
        self.generative.prompt = match prompt.char_indices().nth(fill::MAX_PROMPT_CHARS) {
            Some((cut, _)) => prompt.get(..cut).unwrap_or_default().to_string(),
            None => prompt,
        };
    }

    pub(super) fn select_fill(&mut self, i: usize) {
        self.generative.picked = Some(i);
        self.touchup_selected = None;
        self.request_redraw();
    }

    /// A press on the photo with the Generative brush: a stroke starts at
    /// source `uv`, with Touch Up's brush size and feather.
    pub(super) fn begin_fill_stroke(&mut self, uv: [f32; 2]) {
        if !self.generative_armed() || self.generative.job.is_some() {
            return;
        }
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        if self.fills_of(&path).len() >= MAX_FILLS {
            let t = crate::i18n::t();
            self.set_status(StatusKind::Error, t.fill_limit.into());
            return;
        }
        let (w, h) = self.image_size();
        let aspect = w / h;
        if !(aspect.is_finite() && aspect > 0.0) {
            return;
        }
        let mut stroke = Stroke {
            radius: self.touchup_radius(),
            feather: self.touchup_feather(),
            flow: 1.0,
            erase: false,
            points: Vec::new(),
        };
        brush::push_point(&mut stroke, uv, aspect);
        let mut raster = BrushRaster::new(aspect);
        raster.stamp(&stroke, 0);
        let coverage = Arc::new(raster.coverage());
        self.generative.painting = Some(Painting {
            path,
            aspect,
            stroke,
            raster,
            coverage,
            down: true,
            failed: None,
        });
        self.generative.picked = None;
        self.request_redraw();
    }

    pub(super) fn extend_fill_stroke(&mut self, uv: [f32; 2]) {
        let Some(p) = self.generative.painting.as_mut().filter(|p| p.down) else {
            return;
        };
        if p.stroke.points.len() >= crate::develop::mask::STROKE_POINTS {
            return;
        }
        if brush::push_point(&mut p.stroke, uv, p.aspect) {
            let last = p.stroke.points.len().saturating_sub(1);
            p.raster.stamp(&p.stroke, last);
            p.coverage = Arc::new(p.raster.coverage());
            self.request_redraw();
        }
    }

    /// The pointer let go: the stroke ends and is sent.
    pub(super) fn end_fill_stroke(&mut self, uv: Option<[f32; 2]>) {
        let Some(p) = self.generative.painting.as_mut().filter(|p| p.down) else {
            return;
        };
        p.down = false;
        if let Some(uv) = uv {
            if brush::finish_at(&mut p.stroke, uv) {
                let last = p.stroke.points.len().saturating_sub(1);
                p.raster.stamp(&p.stroke, last);
                p.coverage = Arc::new(p.raster.coverage());
            }
        }
        self.generate();
    }

    /// Sends the painted stroke: on release, and again from the panel's
    /// Generate after a job that failed or was cancelled.
    pub(super) fn generate(&mut self) {
        let Some(p) = self.generative.painting.as_ref().filter(|p| !p.down) else {
            return;
        };
        let (path, painted) = (p.path.clone(), Painted::whole(Arc::clone(&p.coverage)));
        let (Some(bbox), Some(bounds)) = (painted.padded_bbox(), painted.bounds()) else {
            self.generative.painting = None;
            return;
        };
        if let Err(why) = self.start_fill_job(path, Target::New, painted, bbox, centre(bounds)) {
            self.fill_failed(why);
        }
    }

    /// New variants for the picked fill, in place of the ones it has.
    pub(super) fn generate_more(&mut self) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(fill) = self
            .generative
            .picked
            .and_then(|i| self.fills_of(&path).get(i))
            .cloned()
        else {
            return;
        };
        let pin = self.fill_pin(Some(&path), &fill);
        let painted = self
            .patches
            .ready(&path, fill.chosen())
            .and_then(|patch| Painted::of_patch(&patch, fill.bbox()));
        let started = match painted {
            Some(painted) => {
                let bbox = fill.bbox();
                self.start_fill_job(path, Target::More(fill), painted, bbox, pin)
            }
            None => Err(crate::i18n::t().fill_not_loaded.into()),
        };
        if let Err(why) = started {
            self.set_status(StatusKind::Error, why);
        }
    }

    fn start_fill_job(
        &mut self,
        path: PathBuf,
        target: Target,
        painted: Painted,
        bbox: [f32; 4],
        pin: [f32; 2],
    ) -> Result<(), String> {
        if self.generative.job.is_some() {
            return Ok(());
        }
        let t = crate::i18n::t();
        let url = match self.fill_gate() {
            Gate::Ready(url) => url,
            Gate::Blocked(why) => return Err(why),
            Gate::Checking => return Err(t.comfy_checking.into()),
            Gate::Hidden => return Ok(()),
        };
        let feature =
            features::feature(GENERATIVE_FILL).ok_or("Generative Fill is not built in")?;
        // Samples by UV, so whichever loupe tier has landed will do.
        let img = self
            .loader
            .as_ref()
            .and_then(|l| l.get_best(&path, self.preview_px()))
            .ok_or(t.touch_up_needs_full)?;
        let ca = self
            .edits
            .get(&path)
            .and_then(|adj| adj.chromatic_aberration)
            .unwrap_or_default();
        let write = !self.coverage.read_only_photo(&path);
        let (stop, done) = (
            Arc::new(AtomicBool::new(false)),
            Arc::new(AtomicUsize::new(0)),
        );
        let input = JobInput {
            url: url.clone(),
            workflow: feature.workflow,
            photo: path.clone(),
            img,
            ca,
            painted,
            bbox,
            prompt: self.generative.prompt.trim().to_string(),
            write,
            stop: Arc::clone(&stop),
            done: Arc::clone(&done),
        };
        let prompt = input.prompt.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("fill-generate".into())
            .spawn(move || {
                let _ = tx.send(run(&input));
            })
            .map_err(|e| {
                eprintln!("[fill] could not start the job's thread: {e}");
                t.fill_not_started.to_string()
            })?;
        self.generative.job = Some(Job {
            path,
            target,
            url,
            bbox,
            pin,
            prompt,
            written: write,
            stop,
            done,
            result: rx,
        });
        self.request_redraw();
        Ok(())
    }

    /// A job failed: say why, keep the stroke for Generate, and ask the
    /// server again at once, so a dead one turns the controls off.
    fn fill_failed(&mut self, why: String) {
        if let Some(p) = &mut self.generative.painting {
            p.failed = Some(why.clone());
        }
        self.set_status(StatusKind::Error, why);
        self.comfy.check_now();
        self.request_redraw();
    }

    /// Whether Delete removes a fill rather than a spot.
    pub(crate) fn fill_picked(&self) -> bool {
        self.generative.picked.is_some()
    }

    /// Whether Esc has a generation to stop or a kept stroke to drop.
    pub(crate) fn generative_holds_esc(&self) -> bool {
        self.generative.job.is_some() || self.generative.painting.is_some()
    }

    /// Esc: stops the running job, keeping its stroke; with none running,
    /// drops the kept stroke.
    pub(super) fn cancel_generation(&mut self) {
        match &self.generative.job {
            Some(job) => job.stop.store(true, Ordering::Relaxed),
            None => self.discard_fill_stroke(),
        }
        self.request_redraw();
    }

    pub(super) fn discard_fill_stroke(&mut self) {
        if self.generative.job.is_none() {
            self.generative.painting = None;
        }
        self.request_redraw();
    }

    /// Shows the picked fill's next variant, or the one before; wraps.
    pub(super) fn cycle_fill_variant(&mut self, forward: bool) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(i) = self.generative.picked else {
            return;
        };
        let mut fills = self.fills_of(&path).to_vec();
        let Some(fill) = fills.get_mut(i) else {
            return;
        };
        let n = fill.variants().len();
        let step = if forward { 1 } else { n.saturating_sub(1) };
        fill.choose((fill.chosen_index() + step) % n.max(1));
        // Not an undo step: the arrows are their own way back.
        self.store_fills(&path, fills);
    }

    pub(super) fn delete_picked_fill(&mut self) {
        let Some(path) = self.shown.path().map(Path::to_path_buf) else {
            return;
        };
        let Some(i) = self.generative.picked.take() else {
            return;
        };
        let mut fills = self.fills_of(&path).to_vec();
        if i < fills.len() {
            fills.remove(i);
            self.edit_fills(&path, fills);
        }
    }

    /// Replaces `path`'s fills, remembering the ones replaced for Undo.
    fn edit_fills(&mut self, path: &Path, fills: Vec<Fill>) {
        let before = self.fills_of(path).to_vec();
        if before == fills {
            return;
        }
        self.store_fills(path, fills);
        self.touchup_undo
            .entry(path.to_path_buf())
            .or_default()
            .push(TouchStep::Fills(before));
    }

    /// Writes `path`'s fills to its sidecar, or keeps them in memory for a
    /// read-only folder, and has the Loupe draw them.
    pub(super) fn store_fills(&mut self, path: &Path, fills: Vec<Fill>) {
        if self.coverage.read_only_photo(path) || self.patches.unsaved_fills(path).is_some() {
            self.patches.keep_fills(path, fills);
        } else {
            self.folder_store.set_fills(path, &fills);
        }
        if self.shown.path() == Some(path) {
            let n = self.fills_of(path).len();
            self.generative.picked = self.generative.picked.filter(|&i| i < n);
            self.push_adjustments();
            self.hist.invalidate();
        }
        self.request_redraw();
    }

    /// Leaving Touch Up keeps only the variant each fill shows, so the
    /// orphan sweep frees the others' files. The steps Undo holds lose them
    /// too, so no undo brings back a variant without its file.
    fn prune_fill_variants(&mut self) {
        for path in std::mem::take(&mut self.generative.unpruned) {
            let mut fills = self.fills_of(&path).to_vec();
            fills.iter_mut().for_each(Fill::keep_chosen);
            if fills.as_slice() != self.fills_of(&path) {
                self.store_fills(&path, fills);
            }
            for step in self.touchup_undo.entry(path).or_default() {
                if let TouchStep::Fills(fills) = step {
                    fills.iter_mut().for_each(Fill::keep_chosen);
                }
            }
        }
    }

    /// A finished job: its variants become a fill, or replace the picked
    /// fill's. Variants that failed while others arrived get a notice and
    /// the fill is made of the ones that arrived.
    fn land_job(&mut self, job: Job, result: Result<Generated, String>) {
        let t = crate::i18n::t();
        let stopped = job.stop.load(Ordering::Relaxed);
        let Generated { patches, failed } = match result {
            Ok(generated) => generated,
            Err(why) if stopped || why == STOPPED => return,
            Err(why) => {
                match job.target {
                    Target::New => {
                        self.fill_failed(worded(why, &job.url, t.fill_generate));
                    }
                    Target::More(_) => {
                        let why = worded(why, &job.url, t.fill_generate_more);
                        self.set_status(StatusKind::Error, why);
                        self.comfy.check_now();
                    }
                }
                return;
            }
        };
        let arrived = patches.len();
        let ids: Vec<_> = patches.iter().map(Patch::id).collect();
        let Some(made) = Fill::new(job.bbox, ids, job.prompt.clone()) else {
            self.fill_failed("ComfyUI returned no image".into());
            return;
        };
        // In hand before the fill names them, so the Loupe draws at once
        // and a read-only folder's bakes have the pixels.
        self.patches
            .hand_in(&job.path, patches.into_iter().map(Arc::new), !job.written);
        let mut fills = self.fills_of(&job.path).to_vec();
        let index = match &job.target {
            Target::New => {
                if fills.len() >= MAX_FILLS {
                    self.set_status(StatusKind::Error, crate::i18n::t().fill_limit.into());
                    return;
                }
                fills.push(made);
                fills.len() - 1
            }
            Target::More(was) => {
                // Deleted or undone while generating: nothing to replace.
                let Some(i) = fills.iter().position(|f| f == was) else {
                    return;
                };
                if let Some(slot) = fills.get_mut(i) {
                    *slot = made;
                }
                i
            }
        };
        self.generative.unpruned.insert(job.path.clone());
        self.edit_fills(&job.path, fills);
        if matches!(job.target, Target::New) {
            self.generative.painting = None;
        }
        if self.shown.path() == Some(job.path.as_path()) {
            self.generative.picked = Some(index);
        }
        if let Some(why) = failed.first() {
            let notice = t
                .fill_partial
                .replace("{failed}", &failed.len().to_string())
                .replace("{of}", &(arrived + failed.len()).to_string())
                .replace(
                    "{why}",
                    &worded(why.clone(), &job.url, t.fill_generate_more),
                );
            self.set_status(StatusKind::Error, notice);
            self.comfy.check_now();
        }
        self.request_redraw();
    }

    /// When the frame loop should wake to probe the server again; `None`
    /// outside Touch Up.
    pub(crate) fn comfy_wake_at(&self) -> Option<Instant> {
        self.touchup_active()
            .then(|| self.comfy.probe_due())
            .flatten()
    }

    /// Each frame: notices Touch Up opening and closing, probes the server
    /// while it is open, and lands a finished job. True while a job, a
    /// probe or a download is out, so the frame loop keeps polling.
    pub(crate) fn poll_generative(&mut self) -> bool {
        let open = self.tool == LoupeTool::TouchUp;
        if open != std::mem::replace(&mut self.generative.was_open, open) {
            if open {
                self.comfy.check_now();
            } else {
                if self.generative.job.is_none() {
                    self.generative.painting = None;
                }
                self.generative.picked = None;
                self.prune_fill_variants();
            }
        }
        if open {
            self.comfy.probe_if_due();
        }
        // Settings shows which model files are in place, as of its opening.
        let settings = self.show_settings();
        if settings && !std::mem::replace(&mut self.generative.settings_was_open, settings) {
            self.comfy.scan_models();
        }
        self.generative.settings_was_open = settings;
        let status_before = self.fill_gate();
        let comfy_busy = self.comfy.poll();
        if self.fill_gate() != status_before {
            self.request_redraw();
        }
        if comfy_busy && self.show_settings() {
            // A download's progress bar moves.
            self.request_redraw();
        }

        let landed = self
            .generative
            .job
            .as_ref()
            .and_then(|job| match job.result.try_recv() {
                Ok(result) => Some(result),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => {
                    Some(Err("the fill stopped unexpectedly".into()))
                }
            });
        if let Some(result) = landed {
            if let Some(job) = self.generative.job.take() {
                self.land_job(job, result);
            }
            if !open {
                self.generative.painting = None;
                self.prune_fill_variants();
            }
        } else if self.generative.job.is_some() {
            // The spinner turns and the count moves.
            self.request_redraw();
        }
        comfy_busy || self.generative.job.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::folder_app;
    use crate::app::Shown;

    const URL: &str = "http://127.0.0.1:8765";
    const RECT: [f32; 4] = [0.0, 0.25, 0.5, 0.75];

    /// A 4x4 patch of one grey, painted only in its left column.
    fn patch(v: u8) -> Patch {
        let rgba = (0..16)
            .flat_map(|i| [v, v, v, if i % 4 == 0 { 255 } else { 0 }])
            .collect();
        Patch::new(4, 4, rgba).unwrap()
    }

    /// A job as `start_fill_job` leaves it, with no thread behind it.
    fn job(path: &Path, target: Target) -> Job {
        Job {
            path: path.to_path_buf(),
            target,
            url: URL.into(),
            bbox: RECT,
            pin: [0.1, 0.5],
            prompt: String::new(),
            written: false,
            stop: Arc::new(AtomicBool::new(false)),
            done: Arc::new(AtomicUsize::new(0)),
            result: std::sync::mpsc::channel().1,
        }
    }

    /// An App showing its one photo with Touch Up open.
    fn touch_up(tag: &str) -> (App, PathBuf, PathBuf) {
        let (mut app, dir, paths) = folder_app(tag, 1);
        app.shown = Shown::Preview(paths[0].clone(), 100, 100);
        app.tool = LoupeTool::TouchUp;
        app.poll_generative();
        (app, dir, paths[0].clone())
    }

    fn arrived(patches: &[Patch]) -> Result<Generated, String> {
        settle(patches.iter().cloned().map(Ok).collect())
    }

    #[test]
    fn a_job_keeps_the_variants_that_arrived_and_fails_only_with_none() {
        let (a, b) = (patch(1), patch(2));
        let all = settle(vec![Ok(a.clone()), Ok(b.clone()), Ok(a.clone())]).unwrap();
        assert_eq!(
            all.patches,
            [a.clone(), b.clone()],
            "a repeat is one variant"
        );
        assert!(all.failed.is_empty());

        let some = settle(vec![Ok(a.clone()), Err("boom".into()), Ok(b.clone())]).unwrap();
        assert_eq!(some.patches, [a.clone(), b]);
        assert_eq!(some.failed, ["boom"]);

        let none = settle(vec![Err("first".into()), Err("second".into())]);
        assert_eq!(none, Err("first".into()));
        assert!(settle(Vec::new()).is_err());
        let stopped = settle(vec![Ok(a), Err(STOPPED.into())]);
        assert_eq!(stopped, Err(STOPPED.into()), "a cancel keeps nothing");
    }

    #[test]
    fn a_finished_job_records_a_fill_pinned_on_its_paint() {
        let (mut app, dir, photo) = touch_up("fill-land");
        let (a, b, c) = (patch(1), patch(2), patch(3));
        app.land_job(
            job(&photo, Target::New),
            arrived(&[a.clone(), b.clone(), c]),
        );

        let fills = app.fills_of(&photo).to_vec();
        assert_eq!(fills.len(), 1);
        assert_eq!(fills[0].bbox(), RECT);
        assert_eq!(fills[0].variants().len(), 3);
        assert_eq!(fills[0].chosen(), a.id());
        // Shown disabled rather than hidden, with no server asked.
        app.comfy.set_enabled(true);
        let view = app.generative_view().unwrap();
        assert_eq!(
            view.pins,
            [[0.0625, 0.5]],
            "the paint's middle, not the rect's"
        );
        let picked = view.picked.unwrap();
        assert_eq!((picked.index, picked.variant, picked.variants), (0, 0, 3));
        drop(view);
        assert!(app.status_text().is_none(), "no notice for a whole job");

        app.cycle_fill_variant(false);
        assert_eq!(app.fills_of(&photo)[0].chosen(), patch(3).id(), "wraps");
        app.cycle_fill_variant(true);
        app.cycle_fill_variant(true);
        assert_eq!(app.fills_of(&photo)[0].chosen(), b.id());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_variant_that_failed_costs_a_notice_not_the_fill() {
        let (mut app, dir, photo) = touch_up("fill-partial");
        let (a, b) = (patch(1), patch(2));
        let result = settle(vec![Ok(a), Err(LOST.into()), Ok(b)]);
        app.land_job(job(&photo, Target::New), result);

        assert_eq!(app.fills_of(&photo)[0].variants().len(), 2);
        let notice = app.status_text().unwrap().to_string();
        assert_eq!(
            notice,
            "1 of 3 variations failed. ComfyUI stopped responding at 127.0.0.1:8765. \
             Start it again, then press Generate More."
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_job_with_no_variant_keeps_the_stroke_and_says_what_to_do() {
        let (mut app, dir, photo) = touch_up("fill-none");
        let mut raster = BrushRaster::new(1.0);
        let stroke = Stroke {
            radius: 0.1,
            feather: 0.5,
            flow: 1.0,
            erase: false,
            points: vec![[0.5, 0.5]],
        };
        raster.stamp(&stroke, 0);
        app.generative.painting = Some(Painting {
            path: photo.clone(),
            aspect: 1.0,
            coverage: Arc::new(raster.coverage()),
            stroke,
            raster,
            down: false,
            failed: None,
        });
        app.land_job(job(&photo, Target::New), Err(LOST.into()));

        assert!(app.fills_of(&photo).is_empty());
        let said = "ComfyUI stopped responding at 127.0.0.1:8765. \
                    Start it again, then press Generate.";
        assert_eq!(app.status_text(), Some(said));
        let kept = app.generative.painting.as_ref().unwrap();
        assert_eq!(kept.failed.as_deref(), Some(said), "kept for Generate");
        assert!(!said.contains("io"), "no transport text reaches the notice");

        // A cancel says nothing and keeps the stroke too.
        app.status = None;
        app.land_job(job(&photo, Target::New), Err(STOPPED.into()));
        assert!(app.status_text().is_none());
        assert!(app.generative.painting.is_some());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn undo_steps_back_through_fills_on_touch_ups_own_stack() {
        let (mut app, dir, photo) = touch_up("fill-undo");
        app.land_job(job(&photo, Target::New), arrived(&[patch(1)]));
        app.land_job(job(&photo, Target::New), arrived(&[patch(2)]));
        let two = app.fills_of(&photo).to_vec();
        assert_eq!(two.len(), 2);

        app.select_fill(0);
        app.delete_picked_fill();
        assert_eq!(app.fills_of(&photo), &two[1..]);

        app.undo_touchup();
        assert_eq!(app.fills_of(&photo), two.as_slice(), "the delete");
        app.undo_touchup();
        assert_eq!(app.fills_of(&photo), &two[..1], "the second fill");
        app.undo_touchup();
        assert!(app.fills_of(&photo).is_empty(), "the first fill");
        app.undo_touchup();
        assert!(app.fills_of(&photo).is_empty(), "nothing left to undo");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn generate_more_replaces_the_fill_it_was_asked_for() {
        let (mut app, dir, photo) = touch_up("fill-more");
        app.land_job(job(&photo, Target::New), arrived(&[patch(1), patch(2)]));
        let was = app.fills_of(&photo)[0].clone();
        let more = [patch(7), patch(8), patch(9)];
        app.land_job(job(&photo, Target::More(was.clone())), arrived(&more));
        let ids: Vec<_> = more.iter().map(Patch::id).collect();
        assert_eq!(app.fills_of(&photo).len(), 1);
        assert_eq!(app.fills_of(&photo)[0].variants(), ids.as_slice());

        // The fill was deleted while its job ran: nothing comes back.
        app.select_fill(0);
        app.delete_picked_fill();
        let gone = app.fills_of(&photo).to_vec();
        app.land_job(job(&photo, Target::More(was)), arrived(&[patch(4)]));
        assert_eq!(app.fills_of(&photo), gone.as_slice());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leaving_touch_up_keeps_only_the_variant_each_fill_shows() {
        let (mut app, dir, photo) = touch_up("fill-prune");
        let (a, b, c) = (patch(1), patch(2), patch(3));
        app.land_job(job(&photo, Target::New), arrived(&[a, b.clone(), c]));
        app.cycle_fill_variant(true);
        app.select_fill(0);
        app.delete_picked_fill();
        assert!(app.fills_of(&photo).is_empty());

        app.tool = LoupeTool::None;
        app.poll_generative();
        // The delete's undo step was pruned with the fills, so undoing it
        // brings back a fill that names only the file that is kept.
        app.tool = LoupeTool::TouchUp;
        app.undo_touchup();
        let back = app.fills_of(&photo).to_vec();
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].variants(), [b.id()]);

        app.tool = LoupeTool::None;
        app.poll_generative();
        assert_eq!(
            app.fills_of(&photo),
            back.as_slice(),
            "pruning twice is the same"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
