# Plan — Duplicate grouping + Survey Mode

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

**Status: COMPLETE** — shipped in commit `87a8101`. All boxes checked; kept for reference.

- [x] Task 1: New `src/phash.rs` — `dhash(rgba, width, height) -> u64` (9×8 luma downscale + row-wise gradient thresholding) and `hamming(a, b) -> u32`, sharing the downscale helper with `sharpness.rs` (files: src/phash.rs, src/image_ops.rs, src/sharpness.rs)
- [x] Task 2: Unit tests for `phash.rs` with synthetic buffers — near-identical images give small Hamming distance, clearly different ones give large (files: src/phash.rs)
- [x] Task 3: New `src/duplicates.rs` — pure `group_by_hash(hashes, max_distance) -> Vec<u32>` union-find grouping, mirroring `burst.rs`'s style, with a fixed distance threshold constant (no UI knob), plus inline tests (files: src/duplicates.rs)
- [x] Task 4: New `src/featureprint.rs` — Vision FFI over `VNGenerateImageFeaturePrintRequest` → `VNFeaturePrintObservation`, exposing a distance function; keeps the non-Send observation confined to its worker thread (files: src/featureprint.rs)
- [x] Task 5: Standalone check of `featureprint.rs` against real photo pairs (one true duplicate, one similar-but-different), confirming distance ordering before wiring it in (files: src/featureprint.rs)
- [x] Task 6: Feature-print refinement pass in `duplicates.rs` — merges/splits dHash candidate groups by anchor-relative feature distance, fixed threshold (requires Tasks 3, 4) (files: src/duplicates.rs)
- [x] Task 7: `app.rs` caches — `phashes: HashMap<PathBuf, u64>` and feature prints filled lazily off thumbnail arrival, feature prints only for dHash candidates; `dup_marks` recomputed like `burst_marks`; dHash whole-folder pass gated behind a toggle mirroring `bursts_on` (files: src/app/thumbs.rs, src/app/mod.rs)
- [x] Task 8: Grid duplicate badge — extend the existing burst-badge corner-drawing block in `draw_grid`; opens Survey Mode on click (files: src/ui/grid.rs)
- [x] Task 9: Survey Mode — side-by-side group view with focus nav, live rating hotkeys, and one-click "Keep Best, Reject Rest" (best frame rated up, siblings dropped to 1-2) (files: src/ui/survey.rs, src/ui/mod.rs)
- [x] Task 10: "Delete all Rejects" — folder-wide sweep of ★1-2 photos to Trash via `trash.rs`, independent of selection, no new catalog state (files: src/app/catalog.rs, src/ui/toolbar.rs)
- [x] Task 11: End-to-end — `cargo build --release` against a folder with known near-duplicates plus similar-but-distinct shots; confirm grouping separates them, Survey Mode shows groups side-by-side, "keep best" and "Delete all Rejects" behave (files: —)

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Reference

See `00-overview.md` for shared context, architecture patterns, and execution order. (Auto-tone, the former Plan B that once preceded this one, is dropped and not to be implemented.)

**What**: Groups visually-similar frames regardless of capture-time gap (today's `burst.rs` only groups by a 2-second time window, missing similar shots taken further apart or re-imported from multiple cards/cameras) — Aftershoot's "Duplicate Grouping With Intent". Paired with **Survey Mode**: a side-by-side review UI for one group, plus a one-click "keep best, rate rest low" action, and a general **"Delete all Rejects" bulk action** (trash everything currently rated 1-2, reusing the existing rating convention and `trash.rs`'s move-to-Trash — no new catalog state needed for this part).

**Grouping signal — two-tier, dHash first + Vision `VNFeaturePrintObservation` as a stronger second pass**: start with dHash (cheap, dependency-free, always available) to get a coarse candidate grouping, since it's enough to catch most near-duplicates from thumbnails alone. Then, for photos dHash marks as candidates (or borderline cases near the distance threshold), compute a `VNFeaturePrintObservation` (Apple's built-in learned image-similarity embedding) and compare via its native distance function — real learned similarity, not just pixel-gradient hashing, so it's meaningfully better at telling a true duplicate (same framing, same moment) apart from a creative variation (different pose/expression, similar framing). Two-tier keeps the common case cheap and only pays the heavier Vision cost where it matters.

**Effort/Priority**: Medium. The dHash pure modules are small and cheap to write/test; the `VNFeaturePrintObservation` refinement pass adds a real Vision-framework dependency (shared setup with Plan D if both land) but only runs on a small candidate subset, so its cost is bounded. Most UI effort is the Survey Mode view and the whole-folder background dHash scoring pass.

**Critical files**: `src/phash.rs`, `src/featureprint.rs`, `src/duplicates.rs`, `src/app/`, `src/ui/survey.rs`, `src/image_ops.rs`, `src/trash.rs` (reused, not modified)
