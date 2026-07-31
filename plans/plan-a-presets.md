# Plan A — Presets (with Lightroom `.xmp` import)

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

**Implementation**:
- `catalog.rs`: new `presets(id INTEGER PRIMARY KEY, name TEXT NOT NULL, adjustments TEXT NOT NULL, touchups TEXT, created_at INTEGER)` table in the existing `catalog.db`, reusing the existing JSON-serialize-`Adjustments` convention. `Catalog::save_preset(name, &Adjustments, &[TouchUp])`, `list_presets()`, `delete_preset(id)`.
- New module `src/lr_preset.rs` (pure parse/map functions, testable with fixture `.xmp` files): parse the RDF/XML `.xmp` file (a minimal namespaced-XML reader is enough — Camera Raw presets are flat key/value attributes on one `<rdf:Description>` node, no need for a full XMP toolkit), extract the fields in the table above, produce an `Adjustments` + `Vec<TouchUp>`. Any field not present in the file is left at `Adjustments::default()`'s value for that slot.
- `app.rs`: `presets: Vec<(i64, String, Adjustments, Vec<TouchUp>)>` loaded at startup; `apply_preset`/`apply_preset_to_selection` reuse the existing `apply_settings_to_selection` code path (today's copy/paste clipboard, generalized to a named multi-slot library); `import_lr_preset(path)` calls `lr_preset::parse` then `Catalog::save_preset`.
- `ui.rs`: presets panel in Develop (list, click to apply, "+" to save current sliders as new, "Import from Lightroom..." file-picker entry point), using existing modal machinery for the name-entry dialog.
- No "learns your style" auto-suggestion — stays user-driven only.

**Effort/Priority**: Quick-to-medium. The in-app preset library (save/list/apply) is small and reuses existing plumbing almost entirely. The `.xmp` parser is the new surface area — keep it minimal (flat attribute extraction, not a general XMP library) and lean on fixture-file unit tests against a few real exported Lightroom presets.

**Critical files**: `src/catalog.rs`, `src/develop.rs`, `src/lr_preset.rs` (new), `src/app.rs`, `src/ui.rs`

**Verification**: unit tests in `lr_preset.rs` against 2-3 real `.xmp` preset files exported from Lightroom (a plain tone preset, one with noise reduction, one with a spot-heal) confirming correct field extraction. End-to-end: `cargo build --release`, import a real preset, apply to a photo, confirm sliders match expected values and spot-heals land in roughly the right place.
