# Aftershoot-inspired feature roadmap: Cull + Edit — overview

## Context

Aftershoot (aftershoot.com) is a commercial photographer's tool built on 4 pillars: Cull (AI duplicate/blur/eyes-closed grouping + one-click review), Edit (AI style-matching profiles), Retouch (AI skin/hair/background touch-up), Deliver (client galleries + print store). lightphotos is a local, single-user macOS culling+develop tool with no client-facing/business layer, so this roadmap covers only **Cull + Edit** — Retouch and Deliver are explicitly out of scope (confirmed with user).

Aftershoot's "AI" implies trained ML models. lightphotos has zero ML today (only variance-of-Laplacian blur scoring in `sharpness.rs`). Every feature in this roadmap is **heuristic-first / on-device**: classic algorithms (perceptual hashing, gray-world auto-tone, XMP parsing) or calls into existing macOS system frameworks (Apple Vision for face/eye detection, segmentation, feature-print similarity) — no bundled or trained models.

## Plans in this folder

Each plan is its own file, self-contained, no ordering dependency between them (except where noted):
- `plan-a-presets.md` — Presets (with Lightroom `.xmp` import)
- `plan-b-auto-tone.md` — Auto-tone (single photo + bulk selection)
- `plan-c-duplicate-grouping-survey-mode.md` — Duplicate grouping + Survey Mode (dHash + `VNFeaturePrintObservation` refinement)
- `plan-d-face-eyes-closed-detection.md` — Face + eyes-closed detection
- `plan-d2-subject-segmentation-selection.md` — Subject/foreground segmentation as a selection (exploratory, split out from Plan D)
- `plan-e-clarity.md` — Clarity slider
- `plan-f-dehaze.md` — Dehaze slider
- `plan-g-hsl-panel.md` — HSL panel (8-band hue/saturation/luminance)
- `plan-h-tone-curve.md` — Tone Curve (full point-based spline) — deferred, unscheduled

Plans E-G exist because Lightroom presets can reference Clarity/Dehaze/HSL, which lightphotos doesn't have yet — those fields are currently unmapped/dropped on import in Plan A. Plan H (Tone Curve) also feeds Plan A's mapping but is deferred/unscheduled. Crop straighten (Lightroom's `CropAngle`, an arbitrary-degree rotate paired with the crop rectangle) is explicitly **deferred, no plan written** — lightphotos' crop rectangle shape already matches Lightroom's normalized `CropLeft/Top/Right/Bottom`, but lightphotos only has coarse 90°-step rotation (`catalog.rs`'s `rotation: u8`), no fine-angle straighten. Crop stays unmapped in Plan A's LR import until/unless a straighten feature is separately planned.

## Execution order (user-confirmed)

**Plan B → Plan C → Plan D → Plan D2 → (Plan E, F, G) → Plan A last.** Plan H (Tone Curve) is deferred — parked, no fixed slot in this sequence, picked up separately later.

Each plan is built, committed, and reviewed as its own separate unit before starting the next — not one big batch. For each plan: implement → run its own verification steps (unit tests + the end-to-end check listed in that plan's Verification section) → commit → review — before moving to the next plan. Presets (Plan A) is last in the active sequence since its full LR-import field coverage depends on Plans E-G existing (Clarity/Dehaze/HSL become mappable); Tone Curve mapping stays unavailable in Plan A until Plan H is eventually picked up.

## Shared architecture patterns

Confirmed from a codebase survey, reused rather than reinvented by every plan below:
- **Pure logic modules**: `burst.rs` + `sharpness.rs` are the template — total, unit-testable functions with no UI/filesystem/decode dependency, `#[cfg(test)]` tests inline.
- **Catalog columns/tables are additive**: `catalog.rs` uses `CREATE TABLE IF NOT EXISTS` + idempotent `ALTER TABLE ... ADD COLUMN` (see how `touchups` was added).
- **Per-photo signal caching in `app.rs`**: `ratings`/`sharpness` are `HashMap<PathBuf, T>`, lazily filled as thumbnails arrive (`score_pending_burst_thumbs`/`score_arrived_thumbs`).
- **`UiAction`/`BulkKind` enums** in `ui.rs` carry user actions back to `app.rs` (see `run_bulk`).
- **`develop.rs`'s `Adjustments`** is the single source of truth for edits, mirrored in `GpuAdjust` (shader uniform) and `apply_linear` (CPU histogram) — any new tone control must update all three plus the thumbnail-cache edit signature.
- **`hash.rs`'s FNV-1a is not a perceptual hash** — duplicate detection needs its own dHash-style module.
- **Framework bindings**: `coregraphics.rs` is the template for hand-wrapping an Apple framework not fully covered by existing `objc2-*` crates.

## Considered and dropped

- A separate Pick/Reject flag (tri-state, independent of star rating) — redundant with user's existing rating convention (0 = none, 1-2 = reject, 3 = neutral, 4-5 = pick). The one piece worth keeping — a **"Delete all Rejects" bulk action** (trash everything rated 1-2, reusing `trash.rs`'s move-to-Trash) — is folded into `plan-c-duplicate-grouping-survey-mode.md`, next to its "keep best, rate rest low" Survey Mode action.
- "Learns how you cull" adaptive rating — contradicts heuristic-first/no-training constraint.
- Composition scoring (rule-of-thirds-style) — too unreliable without a model.
- Basic HSL/curves as a bare backlog item — superseded; HSL and Tone Curve got their own plans (G, H) once Lightroom-preset compatibility made them worth doing properly.
