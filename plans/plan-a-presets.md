# Plan — Presets (with Lightroom `.xmp` import)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [ ] Task 1: Add `presets(id INTEGER PRIMARY KEY, name TEXT NOT NULL, adjustments TEXT NOT NULL, touchups TEXT, created_at INTEGER)` table + `save_preset`/`list_presets`/`delete_preset`, following the existing additive `CREATE TABLE IF NOT EXISTS` convention (files: src/catalog.rs)
- [ ] Task 2: New `src/lr_preset.rs` — minimal namespaced-XML reader that extracts flat Camera Raw attributes off the single `<rdf:Description>` node, with unit tests against 2-3 real exported `.xmp` fixtures (files: src/lr_preset.rs, tests/fixtures/*.xmp)
- [ ] Task 3: Map the direct tone/WB/color fields (`Exposure2012`, `Contrast2012`, `Highlights2012`, `Shadows2012`, `Whites2012`, `Blacks2012`, `Temperature`, `Tint`, `Vibrance`, `Saturation`) into `Adjustments`, verifying stop/Kelvin ranges match; unmapped fields keep `Adjustments::default()` values (requires Task 2) (files: src/lr_preset.rs)
- [ ] Task 4: Map `LuminanceSmoothing`/`ColorNoiseReduction` → `denoise` through a scaling function (not a 1:1 copy — different algorithm, same slider intent), with a unit test pinning the scaling (requires Task 3) (files: src/lr_preset.rs)
- [ ] Task 5: Map `crs:RetouchInfo` heal-type circles (position/radius) → `TouchUp` list, defaulting `feather`/color-delta which have no LR equivalent; skip clone-type entries (requires Task 3) (files: src/lr_preset.rs, src/develop.rs)
- [ ] Task 6: `presets: Vec<(i64, String, Adjustments, Vec<TouchUp>)>` loaded at startup; `apply_preset`/`apply_preset_to_selection` reusing the existing `apply_settings_to_selection` path (requires Task 1) (files: src/app/adjust.rs, src/app/mod.rs, src/app/catalog.rs)
- [ ] Task 7: `import_lr_preset(path)` — calls `lr_preset::parse` then `Catalog::save_preset` (requires Tasks 1, 2, 6) (files: src/app/adjust.rs)
- [ ] Task 8: Presets panel in Develop — list, click to apply, "+" saves current sliders as a new preset (existing modal machinery for name entry), "Import from Lightroom..." file-picker entry point; no "learns your style" auto-suggestion, stays user-driven (requires Tasks 6, 7) (files: src/ui/develop_panel.rs, src/ui/mod.rs)
- [ ] Task 9: Extend the mapping table to `Clarity2012` → `clarity`, `Dehaze` → `dehaze`, and the 24 `HueAdjustment*`/`SaturationAdjustment*`/`LuminanceAdjustment*` fields → `hsl`, each only once its slider exists (requires plan-e, plan-f, plan-g) (files: src/lr_preset.rs)
- [ ] Task 10: End-to-end — `cargo test`, then `cargo build --release`, import a real preset, apply to a photo, confirm sliders match expected values and spot-heals land in roughly the right place (requires all above) (files: —)

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

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs last, after Plans E-G).

**What**: Named, user-saved `Adjustments` snapshots — plain saved slider states, not AI-trained ("save current sliders as 'Golden Hour', reapply later to one photo or a whole selection"). Additionally: **import Lightroom `.xmp` presets**, mapping overlapping fields into lightphotos' `Adjustments`/`TouchUp` model. Reframes Aftershoot's "Instant AI Profiles" honestly within the heuristic-first constraint.

**Lightroom field mapping** (import only — no export, confirmed with user):

| Lightroom XMP field (Camera Raw namespace) | lightphotos field | Notes |
|---|---|---|
| `Exposure2012` | `exposure` | direct, verify stop range matches (-5..5) |
| `Contrast2012` | `contrast` | direct |
| `Highlights2012` | `highlights` | direct |
| `Shadows2012` | `shadows` | direct |
| `Whites2012` | `whites` | direct |
| `Blacks2012` | `blacks` | direct |
| `Temperature` | `temp` | direct (verify Kelvin vs lightphotos' unit/range) |
| `Tint` | `tint` | direct |
| `Vibrance` | `vibrance` | direct |
| `Saturation` | `saturation` | direct |
| `LuminanceSmoothing`/`ColorNoiseReduction` | `denoise` | approximate — different algorithm, same slider intent, needs a scaling function not a 1:1 copy |
| `crs:RetouchInfo` heal-type circles (position/radius) | `TouchUp` list | only heal-type entries map (no clone source point in lightphotos' model); `feather`/color-delta have no LR equivalent — default them on import |
| `Clarity2012` | `clarity` | mappable once Plan E lands; unmapped/dropped until then |
| `Dehaze` | `dehaze` | mappable once Plan F lands; unmapped/dropped until then |
| `HueAdjustmentRed`...`SaturationAdjustment*`...`LuminanceAdjustment*` (24 fields) | `hsl` | mappable once Plan G lands; unmapped/dropped until then |
| `ToneCurvePV2012`/`...Red/Green/Blue` | `tone_curve` | mappable once Plan H lands; unmapped/dropped until then |
| Not mapped | — | sharpening, `CropAngle`/crop straighten, `Orientation` — crop straighten explicitly deferred (see `00-overview.md`); lightphotos' crop *rectangle* already matches LR's normalized coordinates, only the angle is missing |

**Effort/Priority**: Quick-to-medium. The in-app preset library (save/list/apply) is small and reuses existing plumbing almost entirely. The `.xmp` parser is the new surface area — keep it minimal (flat attribute extraction, not a general XMP library) and lean on fixture-file unit tests against a few real exported Lightroom presets.

**Critical files**: `src/catalog.rs`, `src/develop.rs`, `src/lr_preset.rs` (new), `src/app/`, `src/ui/`
