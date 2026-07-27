# Split app.rs and ui.rs into submodules (before Plan B)

## Context

app.rs (3787 lines) and ui.rs (1931 lines) are the two files every feature plan touches. The roadmap (`plans/00-overview.md`) queues Plan B (auto-tone) → C (duplicates/survey mode) → D (face/eyes) → D2 (segmentation) → then E/F/G in parallel → A (presets) last. B, C, D, and D2 **all add new code to both app.rs and ui.rs** — new caches, new bulk actions, new UI panels/badges. E/F/G/H add sliders/panels to ui.rs only and barely touch app.rs, so they're indifferent to this refactor's timing.

Doing the split now, before Plan B, is cheap because the seams already exist:
- app.rs already has 3 separate `impl App` blocks (main logic ~2830 lines, read-only UI accessors ~200 lines, keyboard dispatch ~250 lines), and its top-of-file doc comment already anticipates per-domain extraction "in later steps."
- ui.rs already has a narrow, mechanical interface into `App`: confirmed zero direct `app.field = ...` writes anywhere in the file — every read goes through a `pub(crate)` accessor method, every user-driven mutation is expressed as a `UiAction` returned in `FrameOutput` and dispatched centrally in `app.rs::apply_ui_actions` (lines 3118-3253).

This means the split is a pure move (file boundaries, `mod` declarations, method bodies unchanged) — no interface redesign, no behavior change. If we instead do it after B/C/D/D2 land, those four plans keep adding entangled code to the monolith (new bulk-action variants, new caches, new panel functions), so the eventual split deals with a bigger, more coupled diff and has to fight rebase conflicts against whichever plan branches are still in flight. Doing it first means B/C/D/D2's new code goes directly into the right submodule once, instead of landing in the monolith and being moved later.

## Approach

Pure structural refactor — no logic changes, no behavior changes. Turn `src/app.rs` into `src/app/mod.rs` + siblings, and `src/ui.rs` into `src/ui/mod.rs` + siblings, using Rust's multi-file-module support (an `impl App` block can live in any submodule as long as the type is in scope).

### `src/app/` split (from concern groups already identified)

| New file | Contents (from current app.rs line ranges) |
|---|---|
| `app/mod.rs` | `App` struct + fields, `Shown` enum, `new`/`open`/`load_folder` init, `redraw` (per-frame glue), `apply_ui_actions` dispatch, free fns (`digit_of`, `file_label`, `range_set`, `remap_positions`), `#[cfg(test)]` tests |
| `app/nav.rs` | Selection/navigation core + region/focus system + directional key handlers (current ~624-1354) |
| `app/keys.rs` | Keyboard dispatch impl block (current ~3456-3708): `nav_key_should_fall_through`, `handle_key` |
| `app/accessors.rs` | The read-only accessor impl block for ui.rs (current ~3256-3454) |
| `app/catalog.rs` | Ratings/bulk persistence (current ~1354-1671): `set_rating`, `run_bulk`, `delete_selection`, `copy/apply_settings*`, filters |
| `app/adjust.rs` | Develop/adjustments actions + white-balance picker (current ~1809-2170) |
| `app/crop.rs` | Crop editing (current ~1967-2124) |
| `app/export.rs` | Export triggering (current ~2170-2313) |
| `app/histogram.rs` | Histogram build/recompute (current ~2313-2424) |
| `app/loupe.rs` | Loupe zoom/pan/transform math (current ~2424-2698) |
| `app/thumbs.rs` | Working-set/thumbnail/loader polling (current ~1671-1809, ~2698-2970) |

All become `impl App { ... }` blocks in their own file; `mod nav; mod keys; ...` declared in `app/mod.rs`. Field visibility: fields currently private to the single file need `pub(super)` or a crate-visible equivalent so sibling submodules can reach them — mechanical, no semantic change.

### `src/ui/` split

| New file | Contents (from current ui.rs line ranges) |
|---|---|
| `ui/mod.rs` | `theme` mod, `UiAction`, `BulkKind`, `FrameOutput`, top-level `draw` dispatcher, `status_toast`, `star_string`, `toolbar_focus_sync`, `region_focus_marker` |
| `ui/toolbar.rs` | `global_toolbar` (mode switch, star filters, bulk-action buttons) |
| `ui/modals.rs` | `confirm_modal`, `quit_modal`, `help_modal`, `rating_histogram` |
| `ui/grid.rs` | `draw_folders_panel`, `folder_content_width`, `draw_grid`, `folder_node`, `thumbnail_cell`, `grid_cell` |
| `ui/loupe.rs` | `draw_loupe` (incl. inline filmstrip panel), overlays (`loupe_touchup_overlay`, `loupe_wb_picker_overlay`, `loupe_compare_overlay`, `loupe_crop_overlay`), `draw_loupe_info_bar` + text formatters, `nearest_edge`, `dist_to_segment`, `filmstrip_cell` |
| `ui/develop_panel.rs` | `draw_develop_panel`, `draw_histogram` |

Same pattern: each file's functions keep taking `&mut App` / `&App` exactly as today — no change to the accessor-based interface, since it's already narrow and clean.

### Out of scope

- No change to `UiAction` variants, `apply_ui_actions` dispatch shape, or any accessor signatures.
- No change to `develop.rs`, `image_ops.rs`, `renderer.rs`, `shader.wgsl`, `catalog.rs`, or any other module.
- Not attempting to reduce `App`'s field count or redesign ownership — purely relocating existing code.

## Verification

- `cargo build` and `cargo build --release` both succeed with no warnings introduced.
- `cargo test` passes (existing `range_set`/`remap_positions` tests, moved into `app/mod.rs`, still pass unchanged).
- Manual smoke test via `./target/release/lightphotos <folder>` and `./target/release/lightphotos <photo>`: grid browse, filmstrip scroll, loupe zoom/pan/rotate, rating/filter, crop, export, develop sliders, compare mode — confirm no regressions, since this is a pure move and any behavior change indicates a mistake in the split (e.g. a missed field visibility fix or accidentally reordered borrow).
- `git diff --stat` should show renames/moves rather than large rewritten hunks — if a file shows as mostly-new content instead of moved content, double check no logic was altered in transit.
