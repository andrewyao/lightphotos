# Plan — Presets (with Lightroom `.xmp` import)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [x] Task 1: `src/prefs.rs` — global key/value storage, native file per key under the OS config dir, `localStorage` per key in the browser; `i18n`'s own copy of that path logic deleted (files: src/prefs.rs, src/i18n.rs)
- [x] Task 2: `src/presets.rs` — `Preset { id, name, adjustments, notes }` + `PresetStore` (add/rename/remove, `unique_name` suffixing, `{"version":1,"presets":[…]}` envelope). A document that will not parse loads empty and refuses every write, so a corrupt library is never replaced by an empty one (files: src/presets.rs)
- [x] Task 3: `nav_key_should_fall_through` also excludes a focused text field, via `text_edit_focused` — not `egui_wants_keyboard_input`, which is true of any focused widget and strands the arrows after a slider drag (files: src/app/keys.rs)
- [x] Task 4: `App.presets` loaded at startup; `save_preset_from_shown`/`apply_preset`/`apply_preset_to_selection`/`delete_preset` in `src/app/presets.rs`; the crop-preserving merge factored out of `apply_settings_to_selection` into `apply_tone_to`, shared by the clipboard and by presets (files: src/app/presets.rs, src/app/catalog.rs, src/app/mod.rs)
- [x] Task 5: Presets block in the Develop panel — a collapsed `CollapsingHeader` above Touch Up, scrolling row list, `+` to save, per-row menu for rename and delete, its own delete-confirm modal (files: src/ui/develop_panel.rs, src/ui/modals.rs, src/ui/mod.rs, src/i18n.rs)
- [x] Task 6: Name entry — `preset_name_edit` on `App`, a `TextEdit` modal that takes focus itself (Tab is stripped before egui sees it), Enter read before re-requesting focus, and a `handle_key` guard so a lost-focus prompt can't let `x` export or a digit re-rate (files: src/ui/modals.rs, src/app/keys.rs, src/app/presets.rs, src/i18n.rs)
- [x] Task 7: `BulkKind::ApplyPreset(u64)` + selection-bar dropdown, so the grid can apply one look across a selection behind the usual confirmation (files: src/ui/toolbar.rs, src/app/catalog.rs, src/i18n.rs)
- [x] Task 8: `src/lr_preset.rs` — anchored `crs:` scanner reading both the attribute and child-element spellings, mapping table keyed by `SliderId` and clamped to each slider's own range, unsupported fields reported into `Preset::notes`. No XML crate (files: src/lr_preset.rs)
- [ ] Task 9: Native import — `dialog::pick_xmp_files` over `rfd::FileDialog::pick_files`, `App::import_lr_presets(paths)` writing the store once for the whole batch, an Import entry in the Presets block, and a status line naming what was dropped (files: src/dialog.rs, src/app/presets.rs, src/ui/develop_panel.rs, src/i18n.rs)
- [ ] Task 10: Browser import — `showOpenFilePicker` reached the way `web_fs.rs` reaches `showDirectoryPicker` (`js_sys::Reflect`, no new Cargo entry), async so it needs the `spawn_local` + channel + `poll_*` shape `request_folder_pick` already establishes (files: src/web/web_fs.rs, src/app/web.rs, src/app/mod.rs)
- [ ] Task 11: **BLOCKED: needs real files.** Import two or three real exported Lightroom `.xmp` presets, apply one, confirm the sliders match and that the note names what was dropped. Task 8's fixtures are hand-written from the documented `crs:` names, so they prove the scanner and not the field semantics. No Adobe CameraRaw or Lightroom settings directory exists on this machine (files: —)

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

See `00-overview.md` for shared context and execution order. **This plan is not gated behind Plans E/F/G**, as that file used to say. Only the importer's field *coverage* depends on them, and an unmapped field is reported rather than silently lost.

**What**: Named, user-saved `Adjustments` snapshots — plain saved slider states, not AI-trained ("save current sliders as 'Golden Hour', reapply later to one photo or a whole selection"). Plus **import of Lightroom `.xmp` presets**, mapping the overlapping fields and naming the rest.

**Where presets live**: global to the install, not per folder, so `<app support>/LightPhotos/presets` rather than a catalog sidecar. `Catalog` is scoped to one active directory and keys its records by filename, so it is the wrong home. The earlier version of this plan specified a SQLite `presets` table; there is no SQLite any more.

**What a preset carries**: the eleven tone fields only. `Adjustments::tone_only()` is exactly that set. No crop, no rotation, no touch-ups — those are decisions about one frame, and the earlier `crs:RetouchInfo` → `TouchUp` task was dropped for the same reason.

**Lightroom field mapping** (import only, no export):

| Lightroom XMP field (Camera Raw namespace) | lightphotos field | Notes |
|---|---|---|
| `Exposure2012` | `exposure` | exact; both are stops, both clamp to -5..5 |
| `Contrast2012`, `Highlights2012`, `Shadows2012`, `Whites2012`, `Blacks2012` | same names | exact; both -100..100 |
| `Vibrance`, `Saturation` | same names | exact; both -100..100 |
| `IncrementalTemperature`, `IncrementalTint` | `temp`, `tint` | exact; both relative -100..100. Written when the preset leaves white balance As Shot, the common case |
| `Temperature`, `Tint` (absolute) | — | **dropped and reported.** Kelvin against one camera's as-shot reference, while `temp` here is a relative nudge. Any conversion would mis-white-balance every photo while looking like it worked |
| `ConvertToGrayscale="True"` | `saturation = -100` | approximate; our closest monochrome. Overrides a mapped `Saturation`, as Lightroom's B&W mode does |
| `LuminanceSmoothing`, `ColorNoiseReduction` | `denoise` | approximate; different algorithm, same 0..100 intent, the larger of the two wins. Pinned by a test |
| `Clarity2012`, `Dehaze`, 24 `HueAdjustment*`/`SaturationAdjustment*`/`LuminanceAdjustment*`, `ToneCurvePV2012*`, `Sharpness`, `CropAngle` | — | **dropped and reported.** No slider exists yet; Plans E/F/G/H add them, and re-importing the file afterwards picks them up. A field Lightroom left at its default is not reported, so the note never cries wolf |

**`.lrtemplate` is out of scope.** It is a Lua table, a second parser for a format Lightroom stopped writing in 7.3 (2018) and can re-export as `.xmp` from its own Presets panel.

**Effort/Priority**: done except the import wiring. The library reused existing plumbing almost entirely; the scanner was the new surface area and is covered by fourteen unit tests over inline fixtures.

**Critical files**: `src/presets.rs`, `src/prefs.rs`, `src/lr_preset.rs`, `src/app/presets.rs`, `src/ui/develop_panel.rs`, `src/ui/modals.rs`
