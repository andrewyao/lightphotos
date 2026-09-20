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
- [ ] Task 10: Presets with Lightroom `.xmp` import — library, parser and UI done on `feat/develop-presets`; import wiring and a real-`.xmp` check remain. **Not gated behind Tasks 7-9** as this line used to claim: only the importer's field coverage is, and an unmapped field is reported rather than silently lost (files: plans/plan-a-presets.md)
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

**Auto-tone (formerly Plan B) is dropped** — user decision, 2026-08-11. Not to be implemented; no plan file exists and nothing in `src/` references it. Ignore the "Plan B" ordering language that survives in some of the older plan files' reference sections.

Aftershoot's "AI" implies trained ML models. lightphotos has zero ML today (only variance-of-Laplacian blur scoring in `sharpness.rs`). Every feature in this roadmap is **heuristic-first / on-device**: classic algorithms (perceptual hashing, XMP parsing) or calls into existing macOS system frameworks (Apple Vision for face/eye detection, segmentation, feature-print similarity) — no bundled or trained models.

Plans E-G exist because Lightroom presets can reference Clarity/Dehaze/HSL, which lightphotos doesn't have yet — those fields are named in the import's own note rather than silently dropped, and re-importing the file once a slider exists picks them up. Plan H (Tone Curve) also feeds Plan A's mapping but is deferred/unscheduled. Crop straighten (Lightroom's `CropAngle`, an arbitrary-degree rotate paired with the crop rectangle) is explicitly **deferred, no plan written** — lightphotos' crop rectangle shape already matches Lightroom's normalized `CropLeft/Top/Right/Bottom`, but lightphotos only has coarse 90°-step rotation (`catalog.rs`'s `rotation: u8`), no fine-angle straighten. Crop stays unmapped in Plan A's LR import until/unless a straighten feature is separately planned.

### Execution discipline (user-confirmed)

Each plan is built, committed, and reviewed as its own separate unit before starting the next — not one big batch. For each plan: implement → run its own verification steps (unit tests + the end-to-end check listed in that plan) → commit → review — before moving to the next plan.

### Shared architecture patterns

Confirmed from a codebase survey, reused rather than reinvented by every plan below:
- **Pure logic modules**: `burst.rs` + `sharpness.rs` are the template — total, unit-testable functions with no UI/filesystem/decode dependency, `#[cfg(test)]` tests inline.
- **Catalog columns/tables are additive**: `catalog.rs` uses `CREATE TABLE IF NOT EXISTS` + idempotent `ALTER TABLE ... ADD COLUMN` (see how `touchups` was added).
- **Per-photo signal caching in `app.rs`**: `ratings`/`sharpness` are `HashMap<PathBuf, T>`, lazily filled as thumbnails arrive (`score_pending_burst_thumbs`/`score_arrived_thumbs`).
- **`UiAction`/`BulkKind` enums** in `ui.rs` carry user actions back to `app.rs` (see `run_bulk`).
- **`develop.rs`'s `Adjustments`** is the single source of truth for edits, mirrored in `GpuAdjust` (shader uniform) and `apply_linear` (CPU histogram) — any new tone control must update all three plus the thumbnail-cache edit signature.
- **`hash.rs`'s FNV-1a is not a perceptual hash** — duplicate detection needs its own dHash-style module.
- **Framework bindings**: `coregraphics.rs` is the template for hand-wrapping an Apple framework not fully covered by existing `objc2-*` crates.

### Considered and dropped

- A separate Pick/Reject flag (tri-state, independent of star rating) — redundant with user's existing rating convention (0 = none, 1-2 = reject, 3 = neutral, 4-5 = pick). The one piece worth keeping — a **"Delete all Rejects" bulk action** — was folded into `plan-c-duplicate-grouping-survey-mode.md`.
- "Learns how you cull" adaptive rating — contradicts heuristic-first/no-training constraint.
- Composition scoring (rule-of-thirds-style) — too unreliable without a model.
- Basic HSL/curves as a bare backlog item — superseded; HSL and Tone Curve got their own plans (G, H) once Lightroom-preset compatibility made them worth doing properly.
