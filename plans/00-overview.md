# Plan — Aftershoot-inspired roadmap (Cull + Edit) overview

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

**This is the entry point — point plan-runner here, not at an individual plan file.**
Each task below is one whole plan file, executed as a unit. For each task:

1. Open the plan file named in `files:` and run *its* checklist as a nested
   single-worktree loop — implement each of its tasks, verify, tick its boxes
   as you go. That file is the durable record of partial progress if the run
   stops mid-task.
2. Verify the plan as a whole: `cargo test` and `cargo build --release` must
   both pass clean, plus the plan's own final end-to-end task.
3. Merge the work (see "Branch and merge per plan" below).
4. Only then check the box here and append the commit/PR to `PROGRESS.md`.

If the plan's own checklist can't be finished, leave this box unchecked and
mark it `BLOCKED: <reason>` — its sub-checklist keeps the partial progress.

- [x] Task 1: Auto-tone, single photo + bulk selection — dropped from the roadmap on 2026-08-11 (user decision), then built anyway on 2026-09-14 outside the roadmap, commit `239af85`. Shipped as `src/autotone.rs` (pure analysis) + `src/app/autotone.rs` (wiring and the batch window); paced through a 32-photo window in `6e50544`. No plan file was ever written (files: —)
- [x] Task 2: Duplicate grouping + Survey Mode — done, commit `87a8101` (files: plans/plan-c-duplicate-grouping-survey-mode.md)
- [ ] Task 3: Native Linux/Windows port — Phase 1 (cfg-gate peripheral cluster) + Phase 2 (cross-platform decode/encode backend), verified on macOS only this pass; real Linux/Windows runtime and real-camera RAW fidelity left BLOCKED for the user's own follow-up on real Linux hardware. HEIC stays out of scope for non-mac (files: plans/plan-i-native-linux-windows-port.md)
- [ ] Task 4: Web/WASM port feasibility gate — memo only, no checklist yet. Run the decode-harness verification described in the memo (sustained decode throughput for HEIC/RAW in-browser) before treating this as a go (files: plans/web-wasm-port-feasibility.md)
- [ ] Task 5: Face + eyes-closed detection (requires Task 2) (files: plans/plan-d-face-eyes-closed-detection.md) — **BLOCKED: needs human visual check.** Tasks 1-7 of that plan are done and merged (`plan-d-face-quality` → `main`); its Task 8 needs a real burst with a blink. See that file for the two-step recipe.
- [ ] Task 6: Subject/foreground segmentation as a selection, exploratory (requires Task 5 — shares its Vision setup) (files: plans/plan-d2-subject-segmentation-selection.md) — **BLOCKED: needs human visual check.** Tasks 1, 2, 4, 5, 6 of that plan are done and merged (`plan-d2-segmentation` → `main`); Tasks 3 and 7 need real photographs with subjects in them. See that file.
- [ ] Task 7: Clarity slider (requires Task 6; parallelizable with Tasks 8-9) (files: plans/plan-e-clarity.md)
- [ ] Task 8: Dehaze slider (requires Task 6; parallelizable with Tasks 7, 9) (files: plans/plan-f-dehaze.md)
- [ ] Task 9: HSL panel, 8-band hue/saturation/luminance (requires Task 6; parallelizable with Tasks 7-8) (files: plans/plan-g-hsl-panel.md)
- [ ] Task 10: Presets with Lightroom `.xmp` import — library, parser, UI and native import all done on `feat/develop-presets`; the browser picker, a real-`.xmp` check and the wider field coverage remain. **Not gated behind Tasks 7-9** as this line used to claim: only the importer's field coverage is, and an unmapped field is reported rather than silently lost (files: plans/plan-a-presets.md)
- [ ] Task 11: DEFERRED — Tone Curve (full point-based spline); parked, no fixed slot, pick up separately (files: plans/plan-h-tone-curve.md)

Standalone plans in this folder, not part of the roadmap sequence:

- [x] Task 12: Zoom/pan support in compare mode — done, commit `2cdbf19` (files: plans/compare-mode-zoom.md)
- [x] Task 13: Split app.rs and ui.rs into submodules — done, commit `6a917e3` (files: plans/refactor-split-app-ui-modules.md)
- [ ] Task 14: Fix saved adjustments not applied until compare toggle (files: plans/fix-adjustments-not-applied-on-open.md)
- [ ] Task 15: Defects and inconsistencies found in the 2026-09-20 Vision/ML audit — 21 items across runtime bugs, unvalidated constants, dead code, efficiency, observability, stale docs, and CI gaps. Its Task 19 corrects two false claims in *this* file, so read that before trusting the Reference section below (files: plans/defects-and-inconsistencies.md)

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Running this file

### Verification gate

The verification command for every task, at both levels (a sub-plan's individual
tasks and a whole plan before its box is checked here):

```sh
cargo test && cargo build --release
```

### Branch and merge per plan

Each plan is built, committed, and reviewed as its own unit before the next
starts — not one big batch. Branch per plan (`plan-d-face-quality`, etc.), run
its checklist there, merge to `main` once the gate above passes, then check the
box here. Tasks 7-9 (Clarity / Dehaze / HSL) are the only mutually independent
set — they can run as parallel worktrees, but note they *all* touch
`src/develop.rs`, `src/shader.wgsl`, and `src/ui/develop_panel.rs`, so under the
plan-runner worktree rules they must either be sequenced or merged one at a time
with a rebase between. Everything else is strictly sequential.

### Manual verification steps — the one thing that can't run unattended

Several plans end in a step no autonomous loop can judge: "open a real portrait
and confirm the overlay tracks the subject" (Plan D2), "confirm Loupe live-render
matches the exported JPEG" (Plans E/F/G/H), "confirm the open-eyed frame is
preferred" (Plan D). An unattended run should treat these as a checkpoint: do
everything up to that point, leave the final box unchecked and marked
`BLOCKED: needs human visual check`, and move on. Don't let the loop self-certify
a visual result it can't see.

Tasks 3-4 (Linux/Windows and WASM port feasibility) are gates, not plans yet:
each memo is "not executable, no checklist" until its open verification
question is run and answered. An unattended run should read the memo, do
nothing else, and leave the box unchecked — the decision needs a human.

Plan `fix-adjustments-not-applied-on-open.md` (Task 14) is the strongest case of
this — its whole approach is "instrument, run, observe, then decide the fix," so
Tasks 4-5 there need a human at the terminal reading the log output.

---

## Reference

### Context

Aftershoot (aftershoot.com) is a commercial photographer's tool built on 4 pillars: Cull (AI duplicate/blur/eyes-closed grouping + one-click review), Edit (AI style-matching profiles), Retouch (AI skin/hair/background touch-up), Deliver (client galleries + print store). lightphotos is a local, single-user macOS culling+develop tool with no client-facing/business layer, so this roadmap covers only **Cull + Edit** — Retouch and Deliver are explicitly out of scope (confirmed with user).

**Auto-tone (formerly Plan B) was dropped, then shipped anyway.** The user dropped it on 2026-08-11. It was built outside the roadmap on 2026-09-14 in commit `239af85` and is in `main` today as `src/autotone.rs` (pure analysis) and `src/app/autotone.rs` (wiring and the batch window), 1580 lines between them (`wc -l src/autotone.rs src/app/autotone.rs`). No plan file was ever written. Ignore the "Plan B" ordering language that survives in some of the older plan files' reference sections, and do not treat this section's earlier "not to be implemented" wording as still binding.

Aftershoot's "AI" implies trained ML models. lightphotos ships no model of its own and trains nothing, and that constraint still holds. It does call Apple's trained models through Vision, on macOS only. `VNGenerateImageFeaturePrintRequest` refines duplicate groups (`featureprint.rs`), `VNDetectFaceLandmarksRequest` drives blink detection (`facequality.rs`), and `VNGeneratePersonSegmentationRequest` with `VNGenerateForegroundInstanceMaskRequest` as fallback produces the selection overlay (`segmentation.rs`). Those models ship with the OS and run on device, so nothing leaves the machine and nothing is bundled here. Every non-mac arm of those three modules returns `Err`. The rest of the roadmap stays **heuristic-first / on-device**: dHash in `phash.rs`, variance-of-Laplacian blur scoring in `sharpness.rs`, capture-time grouping in `burst.rs`, and XMP parsing.

Plans E-G exist because Lightroom presets can reference Clarity/Dehaze/HSL, which lightphotos doesn't have yet — those fields are named in the import's own note rather than silently dropped, and re-importing the file once a slider exists picks them up. Plan H (Tone Curve) also feeds Plan A's mapping but is deferred/unscheduled. Crop straighten (Lightroom's `CropAngle`, an arbitrary-degree rotate paired with the crop rectangle) is explicitly **deferred, no plan written** — lightphotos' crop rectangle shape already matches Lightroom's normalized `CropLeft/Top/Right/Bottom`, but lightphotos only has coarse 90°-step rotation (`catalog.rs`'s `rotation: u8`), no fine-angle straighten. Crop stays unmapped in Plan A's LR import until/unless a straighten feature is separately planned.

### Execution discipline (user-confirmed)

Each plan is built, committed, and reviewed as its own separate unit before starting the next — not one big batch. For each plan: implement → run its own verification steps (unit tests + the end-to-end check listed in that plan) → commit → review — before moving to the next plan.

### Shared architecture patterns

Confirmed from a codebase survey, reused rather than reinvented by every plan below:
- **Pure logic modules**: `burst.rs` + `sharpness.rs` are the template — total, unit-testable functions with no UI/filesystem/decode dependency, `#[cfg(test)]` tests inline.
- **Catalog records are additive**: there is no SQL schema to migrate. Since `016f0af` the catalog is one JSON sidecar per photo holding an `ImageRecord` whose every field is `#[serde(default, skip_serializing_if = ..)]` (`src/catalog.rs:31-44`), so a new signal is added by adding a field and an older sidecar still deserializes. The `CREATE TABLE IF NOT EXISTS` and `ALTER TABLE ... ADD COLUMN` pattern this list used to name went with the SQLite catalog. A new field must also be added to the hand-written `ImageRecord::is_empty` (`src/catalog.rs:77-83`), or a cleared photo keeps writing a sidecar instead of deleting it.
- **Per-photo signal caching in `src/app/`**: `ratings` and `sharpness` are `HashMap<PathBuf, T>` fields on `App` (`src/app/mod.rs:547`, `:609`), lazily filled as thumbnails arrive (`score_arrived_thumbs`, `src/app/thumbs.rs:623`).
- **`UiAction` / `BulkKind` enums** in `src/ui/mod.rs` (`:38`, `:116`) carry user actions back to `src/app/` (see `run_bulk`, `src/app/catalog.rs:217`).
- **`develop.rs`'s `Adjustments`** is the single source of truth for edits, mirrored in `GpuAdjust` (shader uniform) and `apply_linear` (CPU histogram) — any new tone control must update all three plus the thumbnail-cache edit signature.
- **`hash.rs`'s FNV-1a is not a perceptual hash** — duplicate detection needs its own dHash-style module.
- **Framework bindings**: `coregraphics.rs` is the template for hand-wrapping an Apple framework not fully covered by existing `objc2-*` crates.

### Considered and dropped

- A separate Pick/Reject flag (tri-state, independent of star rating) — redundant with user's existing rating convention (0 = none, 1-2 = reject, 3 = neutral, 4-5 = pick). The one piece worth keeping — a **"Delete all Rejects" bulk action** — was folded into `plan-c-duplicate-grouping-survey-mode.md`.
- "Learns how you cull" adaptive rating — contradicts heuristic-first/no-training constraint.
- Composition scoring (rule-of-thirds-style) — too unreliable without a model.
- Basic HSL/curves as a bare backlog item — superseded; HSL and Tone Curve got their own plans (G, H) once Lightroom-preset compatibility made them worth doing properly.
