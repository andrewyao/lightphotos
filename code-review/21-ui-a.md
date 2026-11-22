# src/ui (part A) — develop_panel, export_panel, font_size, form, grade_wheel, grid, home, info_panel, loupe

## Scope

Every line of these files was read, plus `docs/UI.md`. Cross-references were checked in `src/app/{nav,accessors,export,masks,histogram}.rs`, `src/ui/mod.rs`, `src/develop/{curve,mask}.rs`, `src/decode/image_decode/nonmac_decode.rs`, and egui 0.34/0.36 sources.

| File | Lines |
|---|---|
| src/ui/develop_panel.rs | 1777 |
| src/ui/export_panel.rs | 628 |
| src/ui/font_size.rs | 89 |
| src/ui/form.rs | 856 |
| src/ui/grade_wheel.rs | 138 |
| src/ui/grid.rs | 785 |
| src/ui/home.rs | 222 |
| src/ui/info_panel.rs | 425 |
| src/ui/loupe.rs | 1741 |

## Summary

The code is careful and consistent about the CLAUDE.md rules. There is no `unsafe` and no `unwrap`/`expect`/`panic` in non-test code. Slicing and indexing is guarded or fixed-size. Float-to-int casts rely on Rust's saturating `as`. `form.rs` and `font_size.rs` are well built and tested.

The real risks are:
- **A spurious edit.** Selecting a radial mask can push a `SetMask` with no user interaction. That means an undo entry and a sidecar write.
- **Per-frame work that grows with the selection.** The Info panel and the Export footer clone every selected path each frame.
- **Wrong shutter speeds.** `format_shutter` shows wrong values for 0.4 to 0.99 s exposures.
- **Accessibility.** Most custom-painted widgets have no accessible name, and the focus ring is missing on the segmented control.
- **UI.md rule drift.** Hard-coded literals bypass `font_size::px`, and fixed 300 px side panels don't scale.

There are also a few small interaction bugs in the grade wheel, the folder sidebar sizing and the crop Grid overlay. Test coverage is thin outside `form.rs`, `info_panel.rs` and the crop guides.

Findings: 0 Critical, 0 High, 3 Medium (UI-A-01, 02, 09), 11 Low, 3 Info.

## Findings

### [UI-A-01] Selecting a radial mask can push a `SetMask` with no user input — Severity: Medium — Confidence: Medium
- Location: `src/ui/develop_panel.rs:1263-1278`; sink at `src/app/masks.rs:735-748` and `put_masks`.
- Evidence:
  - The radial feather is shown through a percent round trip: `let mut percent = *feather * 100.0; develop_slider(.., &mut percent, ..); *feather = (percent / 100.0).clamp(0.0, 1.0);`.
  - After that, `if next != *mask { out.actions.push(UiAction::SetMask(i, next)); }` runs every frame.
  - `(x*100)/100 != x` in f32 for about 15.7% of arbitrary values. I checked 1e6 random f32 in numpy. All of them reach a fixed point after one extra iteration.
  - Radial feathers come from `feather_wheel` (`feather + d*0.0025`, so arbitrary floats) and from imported sidecars.
- Impact: Merely picking a radial mask (or opening a photo that has one selected) can emit one `SetMask` carrying a feather that differs in the last ulp. `set_mask` sees `*slot != mask`, so `put_masks` records an Undo step and writes the sidecar. The user sees an unexplained undo entry and a modified photo. The `develop_slider` code already documents this exact hazard for the Auto Tone values ("producing an edit without any user interaction").
- Suggested fix: Only push when a slider reports `changed` or `interacted`. Collect the return values of `develop_slider` and the feather slider, then compare `next != *mask` only if one of them fired. Or compare the percent value before and after (`percent != original_percent`) and only then write `feather`. A test should draw a radial mask with feather `0.1234567` and assert no action.

### [UI-A-02] Per-frame `Vec<PathBuf>` of the whole selection in Info panel and Export footer — Severity: Medium — Confidence: High (code), Medium (impact)
- Location: `src/ui/info_panel.rs:198` (`app.selected_paths().len()`), `src/ui/export_panel.rs:106` (`app.export_blocker()` calls `self.action_paths()`, then `pending_among` over them; `src/app/export.rs:241-243`). Also `selected_path()` is called twice per frame in the Info page (`info_panel.rs:194` and `histogram.rs:213`).
- Evidence: `selected_paths()` is `selected_path_refs().map(to_path_buf).collect()`. `src/app/nav.rs:376` says the `_refs` and `action_count()` variants exist "for readers that run every frame".
- Impact: With Select All on a large folder (10k to 50k photos) and the Info or Export page open, every repaint allocates and frees N `PathBuf`s. Mouse movement over egui causes repaints at display rate. That is millions of allocations per second on the UI thread, and it makes the Info page janky. The export blocker also walks all N paths each frame.
- Suggested fix: Info page: use `app.selection_count()` or `action_count()`. Export footer: add an `export_blocker` that works on `action_path_refs()`, or cache the blocker per selection/edit generation.

### [UI-A-03] `format_shutter` shows wrong values for 0.4–0.99 s and an empty row for 0 s — Severity: Low — Confidence: High
- Location: `src/ui/loupe.rs:708-719`; used at `src/ui/info_panel.rs:71` and `loupe.rs:698`.
- Evidence:
  - For `seconds < 1.0` it always prints `1/{round(1/seconds)}s`.
  - 0.8 s gives 1/1.25 → "1/1s". 0.6 s gives "1/2s" (that is 0.5 s). 0.4 s gives "1/3s". 0.7 s gives "1/1s". 0.8", 0.6", 0.5" and 0.3" are standard 1/3-stop shutter speeds.
  - `seconds <= 0.0` returns `""`. The Info panel wraps that in `Some(...)`, so the "Shutter" row appears with a blank value. `exposure_parts` drops it via `strip_suffix('s')`, so the two displays disagree. EXIF `ExposureTime = 0` does occur.
- Impact: Wrong exposure data is shown to the user for slow exposures. A blank row appears for 0 or negative values.
- Suggested fix: Use the 1/N form only when `1/seconds` is within about 2% of an integer (or `seconds <= 0.5`). Otherwise show decimals ("0.8s"). Return `Option<String>` and drop non-positive values, so the row is omitted. Add unit tests for 0.8, 0.6, 0.3333, 1/250, 0, 1.0 and 2.5.

### [UI-A-04] Color-grade wheel: a click on the disc that completes in one frame is ignored; the `clicked_by` branch is dead — Severity: Low — Confidence: High (egui behavior), Medium (frequency)
- Location: `src/ui/grade_wheel.rs:96-106` (`primary_press_on_disc`).
- Evidence:
  - `primary = response.clicked_by(Primary) || (is_pointer_button_down_on && primary_down)`, then `origin = i.pointer.press_origin()`.
  - egui clears `press_origin` while processing the release event (`egui-0.36.2/src/input_state/mod.rs:1197`).
  - So on the frame where `clicked_by` is true, `press_origin()` is `None` and the function returns false. The first branch can never contribute.
- Impact: Normally the press frame (button down) already set the hue, so it works. If press and release arrive in one frame (low frame rate, a busy UI thread, a wasm tab under load, a quick tap), the disc click only picks the wheel and does not set hue or saturation.
- Suggested fix: Take the origin from `response.interact_pointer_pos()` (valid on the click frame) or `ctx.input(|i| i.pointer.interact_pos())`. Add a pure `wheel_value(pos, center, radius) -> (hue, sat)` function with tests for the angle and clamp math.

### [UI-A-05] Folder sidebar can grow to window width minus 96 px — Severity: Low — Confidence: Medium
- Location: `src/ui/grid.rs:15-17`.
- Evidence: `panel_width = (content_width + 24.0).max(220.0).min((ui.available_width() - 96.0).max(220.0))`. The panel is `resizable(false)`, and `draw_left_panel` runs before the right panel and the central grid in `ui::draw`.
- Impact: A deeply expanded tree with long names (CJK or full-sentence folder names) can take nearly the whole window. The Develop/Export panel is squeezed and the Grid gets about 96 px, less than one 192 pt cell. There is no way to shrink the sidebar. The literals `24.0`, `220.0` and `96.0` are not scaled by `font_size::px`, although `home.rs` uses `px(220)` for the same panel, so the home layout and the real Grid layout differ at non-default font sizes.
- Suggested fix: Cap at about 35–40% of the window width, use `font_size::px` for the minimum, and make the panel resizable or add a horizontal scroll.

### [UI-A-06] Long folder names are clipped with no tooltip or horizontal scroll — Severity: Low — Confidence: Medium
- Location: `src/ui/grid.rs:292-347`, `58-64`.
- Evidence: The names are `flat_toggle` buttons in a `ui.horizontal` row (wrap mode Extend). The `ScrollArea` is vertical only and the panel is not resizable.
- Impact: A name wider than the capped panel is cut off. There is no hover text and no way to read the end of the name (long names or depth indent, CJK, narrow windows).
- Suggested fix: Truncate with `Label::truncate` and add `on_hover_text(path)`.

### [UI-A-07] An open folder with no photos shows a blank Grid — Severity: Low — Confidence: Medium
- Location: `src/ui/grid.rs:112-116`.
- Evidence: `no_matches` is shown only when `len == 0 && (filter || eyes filter)`. With `len == 0` and no filter, the code falls through to an empty `show_rows`. `en.rs` has no "folder has no photos" string.
- Impact: The user sees nothing and cannot tell "empty" from "still loading" or "failed". I did not verify whether `App::open` refuses empty folders elsewhere.
- Suggested fix: Add an empty state ("No supported photos in this folder") that also distinguishes the loading state.

### [UI-A-08] Crop "Grid" overlay emits an unbounded number of lines for elongated crop boxes — Severity: Low — Confidence: Medium
- Location: `src/ui/loupe.rs:927-939`.
- Evidence: The cell is `min(w,h)/8` and the step iterator is `(1..).map(i*cell/len).take_while(f < 1-1e-4)`, so the line count is about `8*max/min` on the long side. The count is not capped.
- Impact: A thin Custom-aspect box, for example 1500x10 on-screen, yields about 1200 vertical polylines per frame (each a `Vec` plus a `Shape`). The loop is infinite only if `len` were infinite. It cannot be today, because `r` is intersected with the finite `central`, but the code relies on that implicitly.
- Suggested fix: Cap steps (for example `.take(64)`) or switch to a minimum cell size.

### [UI-A-09] Accessibility: custom-painted controls lack accessible names, and the segmented control has no visible keyboard-focus ring — Severity: Medium — Confidence: High
- Location:
  - Thumbnail cell: `grid.rs:433`.
  - Folder chevron and glyph: `grid.rs:217-221, 251-255`.
  - Collapsing-header chevron: `form.rs:202-203`.
  - Segmented segments: `form.rs:578` has `widget_info`, but no focus ring is painted.
  - `toggle_switch`: `develop_panel.rs:813-815`, empty label.
  - `tri_state_checkbox`: `develop_panel.rs:569`, `Checkbox::without_text`.
  - Compare tile stars: `loupe.rs:1436`, 1471, 1513.
  - Curve graph: `develop_panel.rs:1648`, mouse only.
  - Glyph-only step buttons: `develop_panel.rs:608`.
  - Develop slider tracks have no label of their own (`form::slider` draws the label as a separate `ui.label`).
- Evidence: These use `allocate_exact_size`/`ui.interact` with `Sense::click` and no `widget_info`, or an empty name. Grid cells have no file-name tooltip either. egui's own focus ring is not drawn for hand-painted widgets.
- Impact: Screen-reader users get unnamed buttons. Keyboard users tabbing through the segmented control (Theme, crop Aspect, Export amount, Mixer axis) can't see which segment has focus. The chevron and the folder glyph add redundant focus stops.
- Suggested fix:
  - Add `widget_info` (a label, plus selected/expanded state) to the custom widgets. Use the file name for grid cells.
  - Paint `ui.painter().rect_stroke(..)` when `resp.has_focus()` in `segmented`.
  - Give `toggle_switch` the row label.
  - Make the folder glyph `Sense::hover`, or merge it with the label.
  - Give the curve graph a keyboard alternative.

### [UI-A-10] UI.md spacing rule violated by unscaled literals; fixed-width side panels don't scale — Severity: Low — Confidence: High
- Location:
  - `info_panel.rs:201` (`add_space(4.0)`), `develop_panel.rs:369` (`add_space(6.0)`), `grid.rs:189` (`add_space(8.0)`).
  - `develop_panel.rs:1359, 1374`: exposure row `+ 6.0` and inset `4.0`.
  - Spinners at `develop_panel.rs:967, 1180` (`size(12.0)`).
  - `grid.rs:15-17` (see UI-A-05).
  - Overlay literals in `loupe.rs`: pin radii at 325-331, the 28.0 spinner at 335, `pad = 8.0` at 1268 and 509, `EDGE_GRAB_PX = 24.0` at 861, and the strip height at 22.
  - `info_panel.rs:177-181` and `export_panel.rs:37-41` use `default_size(340.0)` and `.resizable(false).default_size(300.0)` in the Grid.
- Evidence: UI.md says "Spacing comes from the constants in `form.rs` through `font_size::px`, never from literal `add_space` numbers".
- Impact: At Alt+= sizes (up to 2.15x) the vertical rhythm and hit targets (grab distance, pin reach) stay at the 13 pt scale, and the Grid's Export and Info panels are fixed 300 px wide and cannot be resized. Develop's panel is resizable, so the three right-hand pages differ.
- Suggested fix: Route through `font_size::px`. Make the Grid's Info and Export panels resizable, or scale their default width.

### [UI-A-11] Home and Grid disagree on the left-panel width at non-default font size — Severity: Low — Confidence: Medium
- Location: `home.rs:27, 33` (`px(220)`) versus `grid.rs:16` (`.max(220.0)`).
- Evidence: `home.rs` documents "the Grid's minimum for its folder tree".
- Impact: The placeholder layout the tour points at shifts when the first folder opens at font sizes other than 13.
- Suggested fix: Share one const and one scaled helper (see UI-A-05).

### [UI-A-12] `+ Linear` / `+ Radial` / `+ Brush` at MAX_MASKS are greyed with no reason — Severity: Low — Confidence: High
- Location: `src/ui/develop_panel.rs:1116-1117`.
- Evidence: `ui.add_enabled(room, ..)` followed by `.on_hover_text(tip)`. egui's `on_hover_ui` uses `Tooltip::for_enabled` and shows nothing on a disabled widget (`egui-0.34.3/src/response.rs:633-642`). The detect buttons below it correctly use `on_disabled_hover_text`.
- Impact: With 16 masks the add buttons go dead without explanation, unlike the Subject/Sky buttons and the ComfyUI controls, which UI.md says must say why.
- Suggested fix: Use `on_disabled_hover_text` with a "maximum of N masks" string (new i18n key in all tables).

### [UI-A-13] Hover hints "Esc" and "Enter" bypass i18n — Severity: Info — Confidence: High
- Location: `develop_panel.rs:975, 994, 1003`. Also the English-letter hemisphere suffixes in `loupe.rs:766-780` and units in `loupe.rs` (`" mm"`, `"ISO"`, `" s"`, `"EV"`, `"KB"`) bypass `i18n`.
- Impact: Cosmetic for non-English locales. The key names are conventional, so this is low priority.
- Suggested fix: Add a `key_esc`/`key_enter` string, or accept as proper names.

### [UI-A-14] Export text fields: `MAX_STEM_BYTES` used as a char limit; commit-on-blur always saves — Severity: Low — Confidence: Medium
- Location: `export_panel.rs:216` (`char_limit(MAX_STEM_BYTES)`) and `export_panel.rs:92-94`.
- Evidence:
  - `TextEdit::char_limit` counts chars. `MAX_STEM_BYTES = 240` is a byte cap, so CJK names (3 bytes per char) can be typed up to 720 bytes. `export/mod.rs:235` then silently truncates, and the example name only shows the truncated result if the user notices.
  - `field.lost_focus()` always sets `next`, even when the text did not change, which calls `save_export_settings` (a disk write) and `resume_immich()` (key-store lookup and possible connect) on every blur.
- Impact: Minor. Silent truncation of long non-ASCII templates, and redundant writes on focus changes.
- Suggested fix: Only set `next` when `unsaved` or the text differs from the saved text. Explain truncation in the hint, or count bytes in a custom limiter.

### [UI-A-15] Immich/prompt `TextEdit`s rely on auto-ids; sections with conditional rows can shift them — Severity: Low — Confidence: Low
- Location: `export_panel.rs:485, 498, 608`; `develop_panel.rs:956`.
- Evidence: Only `setting_text` uses `.id_salt(id)`. The Immich URL, API key and album-name fields and the generative Prompt field have none. The Prompt field sits after a conditional "Spots" section in the same page `ui`.
- Impact: If a conditionally drawn row above one of these appears or disappears while the field has focus (for example a spot list changing as a fill finishes), the auto id changes and the field loses focus mid-typing. I could not find a concrete trigger, so the risk is low.
- Suggested fix: Give each a stable `id_salt`.

### [UI-A-16] `font_size::base` indexes a map — Severity: Info — Confidence: High
- Location: `src/ui/font_size.rs:38`.
- Evidence: `style.text_styles[&TextStyle::Body].size` panics on a missing key (map `Index`).
- Impact: Not reachable today, since `apply` always installs Body. The style is also the app's own.
- Suggested fix: `.get(&TextStyle::Body).map_or(DEFAULT, |f| f.size)`.

### [UI-A-17] Misattached doc comment — Severity: Info — Confidence: High
- Location: `src/ui/loupe.rs:413-419`.
- Evidence: The first doc paragraph ("On-screen radius of a touch-up…") belongs to `touchup_screen_radius` (line 455). It was stranded above `brush_cursor`'s doc. `export_panel.rs:18` also mentions a `develop_tab` mode that no longer exists.
- Suggested fix: Move or delete the stale text.

## Test coverage assessment

- **Well covered:** `form.rs` (label column and top alignment, section spacing, footer order and widths, disabled buttons, segmented hit areas, `row_sizes` wrapping), `font_size.rs` (ratios, clamp, NaN), `info_panel.rs` (grouping, missing fields, GPS with stored fallback, clock source), and `loupe::crop_guides` and the spiral.
- **No tests at all:**
  - `develop_panel.rs` (1777 lines): sliders, the mask section, curve interaction, `develop_slider` write-back, the radial feather round trip (UI-A-01), step rows, the generative panel.
  - `export_panel.rs`: draft versus commit semantics of `setting_text`, the Immich states.
  - `grid.rs`: `thumbnail_cell` geometry, the scroll-to-selection logic, `folder_content_width`, the badge hit-test and `click_action`.
  - `home.rs`: the card centring and discard loop.
  - `grade_wheel.rs`: angle and saturation math (UI-A-04).
- **Weak or missing at the edges:** `format_shutter` and the other formatters are covered only incidentally (UI-A-03), and nothing tests the 0-second or NaN paths. The test helper `form::tests::click` is a good pattern. The same simulated-pointer approach could drive the wheel, the segmented control and the curve.

## Things done well / verified-safe areas

- No `unsafe`. No `unwrap`/`expect`/`panic!` in non-test code.
- Every indexing/slicing site I examined is guarded or fixed-size:
  - `form::segmented`: `per_row` is non-empty (`rows.max(1)`) and `choices[i..i+count]` sums to `n`.
  - `smooth_bins` and `bins_peak` slice `[f32; 256]`.
  - `UNITS[unit]` is bounded by `unit < len-1`.
  - `compare chunks(cols)` has `cols >= 1`.
- Float-to-int casts rely on saturation and are bounded (`as i8` on `px()`, `min(127.0)` in `home.rs`, `as usize` on `floor().max(1.0)`). `font_size::apply` handles NaN, inf and out-of-range input.
- `home.rs`'s card-height discard handshake is guarded (`> 0.5` threshold, egui pass limit).
- Grid virtualization uses `show_rows` and only the visible rows build cells. The scroll-to-selection logic is edge-triggered, so it doesn't fight manual scrolling.
- Numeric egui widgets defend against hostile values (`DragValue` `clamp_existing_to_range(false)`, `Slider` `Edits` clamping), and `grade_wheels` clamps the radius to at least a small disc.
- Role-based footer button ordering (`order`) is a pure function with a test.
- The Export panel disables the whole form while a batch runs, and the text-draft path keeps typing off the disk until blur.
- The generative panel never shows transport text, in line with UI.md.

## Open questions for maintainers

1. Is a folder with zero supported photos intended to show a blank Grid (UI-A-07), or does `App::open` surface a message elsewhere that I did not find?
2. Is AccessKit meant to be supported (eframe enables it by default)? If so, UI-A-09 is a bigger gap than "Low". If not, say so in docs/UI.md.
3. Should `SetMask` from the mask sliders be gated on slider interaction, as `SetAdjustments` is on `changed`? Today the "diff the whole struct" approach is the only place doing this.
4. Is the Grid's non-resizable 300 px Info/Export panel deliberate, given Develop's is resizable and all three share the id `"develop"` in the Loupe?
