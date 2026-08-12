# Plan — Split app.rs and ui.rs into submodules

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

**Status: COMPLETE** — shipped in commit `6a917e3`. All boxes checked; kept for reference.

- [x] Task 1: Turn `src/app.rs` into `src/app/mod.rs` — `App` struct + fields, `Shown` enum, `new`/`open`/`load_folder` init, `redraw` per-frame glue, `apply_ui_actions` dispatch, free fns (`digit_of`, `file_label`, `range_set`, `remap_positions`), `#[cfg(test)]` tests; declare the sibling `mod` list (files: src/app/mod.rs)
- [x] Task 2: Move selection/navigation core + region/focus system + directional key handlers (files: src/app/nav.rs)
- [x] Task 3: Move the keyboard dispatch impl block — `nav_key_should_fall_through`, `handle_key` (files: src/app/keys.rs)
- [x] Task 4: Move the read-only accessor impl block used by ui.rs (files: src/app/accessors.rs)
- [x] Task 5: Move ratings/bulk persistence — `set_rating`, `run_bulk`, `delete_selection`, `copy/apply_settings*`, filters (files: src/app/catalog.rs)
- [x] Task 6: Move develop/adjustments actions + white-balance picker (files: src/app/adjust.rs)
- [x] Task 7: Move crop editing (files: src/app/crop.rs)
- [x] Task 8: Move export triggering (files: src/app/export.rs)
- [x] Task 9: Move histogram build/recompute (files: src/app/histogram.rs)
- [x] Task 10: Move loupe zoom/pan/transform math (files: src/app/loupe.rs)
- [x] Task 11: Move working-set/thumbnail/loader polling (files: src/app/thumbs.rs)
- [x] Task 12: Widen field visibility to `pub(super)` (or crate-visible equivalent) where sibling submodules now need it — mechanical, no semantic change (requires Tasks 1-11) (files: src/app/mod.rs)
- [x] Task 13: Turn `src/ui.rs` into `src/ui/mod.rs` — `theme` mod, `UiAction`, `BulkKind`, `FrameOutput`, top-level `draw` dispatcher, `status_toast`, `star_string`, `toolbar_focus_sync`, `region_focus_marker` (files: src/ui/mod.rs)
- [x] Task 14: Move `global_toolbar` — mode switch, star filters, bulk-action buttons (files: src/ui/toolbar.rs)
- [x] Task 15: Move `confirm_modal`, `quit_modal`, `help_modal`, `rating_histogram` (files: src/ui/modals.rs)
- [x] Task 16: Move `draw_folders_panel`, `folder_content_width`, `draw_grid`, `folder_node`, `thumbnail_cell`, `grid_cell` (files: src/ui/grid.rs)
- [x] Task 17: Move `draw_loupe` (incl. inline filmstrip panel), the overlays (`loupe_touchup_overlay`, `loupe_wb_picker_overlay`, `loupe_compare_overlay`, `loupe_crop_overlay`), `draw_loupe_info_bar` + text formatters, `nearest_edge`, `dist_to_segment`, `filmstrip_cell` (files: src/ui/loupe.rs)
- [x] Task 18: Move `draw_develop_panel`, `draw_histogram` (files: src/ui/develop_panel.rs)
- [x] Task 19: `cargo build` and `cargo build --release` both succeed with no new warnings; `cargo test` passes (existing `range_set`/`remap_positions` tests, moved into `app/mod.rs`, unchanged) (requires all above) (files: —)
- [x] Task 20: Manual smoke test — grid browse, filmstrip scroll, loupe zoom/pan/rotate, rating/filter, crop, export, develop sliders, compare mode; any behavior change indicates a mistake in the split (files: —)
- [x] Task 21: `git diff --stat` shows renames/moves rather than large rewritten hunks — a file showing as mostly-new content means logic was altered in transit (files: —)

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

### Context

app.rs (3787 lines) and ui.rs (1931 lines) were the two files every feature plan touches. At the time, the roadmap (`plans/00-overview.md`) queued Plan B (auto-tone — since dropped, not to be implemented) → C (duplicates/survey mode) → D (face/eyes) → D2 (segmentation) → then E/F/G in parallel → A (presets) last. B, C, D, and D2 **all add new code to both app.rs and ui.rs** — new caches, new bulk actions, new UI panels/badges. E/F/G/H add sliders/panels to ui.rs only and barely touch app.rs, so they were indifferent to this refactor's timing.

Doing the split before Plan B was cheap because the seams already existed:
- app.rs already had 3 separate `impl App` blocks (main logic ~2830 lines, read-only UI accessors ~200 lines, keyboard dispatch ~250 lines), and its top-of-file doc comment already anticipated per-domain extraction "in later steps."
- ui.rs already had a narrow, mechanical interface into `App`: confirmed zero direct `app.field = ...` writes anywhere in the file — every read goes through a `pub(crate)` accessor method, every user-driven mutation is expressed as a `UiAction` returned in `FrameOutput` and dispatched centrally in `app.rs::apply_ui_actions`.

This made the split a pure move (file boundaries, `mod` declarations, method bodies unchanged) — no interface redesign, no behavior change.

### Approach

Pure structural refactor using Rust's multi-file-module support (an `impl App` block can live in any submodule as long as the type is in scope). Each ui file's functions keep taking `&mut App` / `&App` exactly as before — no change to the accessor-based interface.

### Out of scope

- No change to `UiAction` variants, `apply_ui_actions` dispatch shape, or any accessor signatures.
- No change to `develop.rs`, `image_ops.rs`, `renderer.rs`, `shader.wgsl`, `catalog.rs`, or any other module.
- Not attempting to reduce `App`'s field count or redesign ownership — purely relocating existing code.
