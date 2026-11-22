# src/ui (part B) — mask_overlay, mod, modals, photo_menu, presets, tabs, theme, toolbar, tour

## Scope

Every line of these files was read, plus `docs/UI.md`, `src/ui/form.rs` (rows, dialog, footer), `app/tour.rs`, the key handlers in `app/keys.rs` that front these dialogs, and `app/loupe.rs`, `app/adjust.rs` (coordinate mapping).

| File | Lines |
|---|---|
| src/ui/mask_overlay.rs | 634 |
| src/ui/mod.rs | 1311 |
| src/ui/modals.rs | 753 |
| src/ui/photo_menu.rs | 235 |
| src/ui/presets.rs | 253 |
| src/ui/tabs.rs | 22 |
| src/ui/theme.rs | 595 |
| src/ui/toolbar.rs | 1120 |
| src/ui/tour.rs | 305 |

## Summary

This is careful, well-commented UI code. I found no panics under the CLAUDE.md rules: there is no `unwrap` or `expect` in non-test code, and the one `sorts[..]` index is guarded. Every user-visible string goes through `i18n`. Colors come from `theme::Palette`, apart from deliberate on-photo overlay whites and the fixed label colors. The mask overlay's source-UV, straighten and rotation mapping is consistent, and crop is a view of the full texture, so it doesn't distort the mapping. The risks are functional gaps and layout robustness, not crashes.

- Purple color labels can't be assigned from anywhere in the UI.
- The Grid toolbar doesn't wrap or scroll.
- Long library preset names push the `⋯` menu out of the panel.
- The shipped Medium theme has low contrast for links and the wordmark, and the theme tests don't cover those cases.
- The Presets page is disabled wholesale in read-only folders.

Findings by severity: Critical 0, High 0, Medium 3, Low 13, Info 2.

## Findings

### [UI-B-01] Purple color label can't be set from the UI — Severity: Medium — Confidence: High
- Location: `src/ui/photo_menu.rs:126-139`; `src/sidecar.rs:63-72`; `src/ui/toolbar.rs:148-152`
- Evidence: the right-click Color Label submenu lists `Red, Yellow, Green, Blue`. `ColorLabel::from_digit` gives 6-9 to those four and says "Purple has no key, as in Lightroom". `UiAction::ToggleLabel` is raised only from this menu (`grep ToggleLabel` shows no other UI source). The toolbar's filter row has a Purple dot, and `label_name` and `label_color` have Purple arms.
- Impact: a user can filter by Purple but can't give a photo that label. Photos labeled Purple elsewhere (a sidecar from another tool, a command) show it, but it can't be added or cleared from the menu. The label set is inconsistent: the filter offers 5, the setter 4.
- Suggested fix: add `(ColorLabel::Purple, m.label_purple)` to the `labels` array at `photo_menu.rs:127`. Add a test that the menu and the toolbar filter list the same labels.

### [UI-B-02] Grid toolbar has no overflow handling on narrow windows — Severity: Medium — Confidence: Medium
- Location: `src/ui/toolbar.rs:483-512` (`grid_toolbar`), compare `selection_bar` at `:608`
- Evidence: the top row is one `ui.horizontal` with about 20 controls, three labels, the count and the Actions menu. It has no `ScrollArea` and no wrap. The bottom bar does ("a window too narrow for it scrolls sideways rather than hiding Export and Delete"). There is no `min_inner_size` anywhere in `src`.
- Impact: in a narrow window, or with the left (220 pt) and right (340 pt) panels open in a moderate window (a 1100-pt window leaves about 540 pt in the middle column), the tail of the row is clipped. That tail holds the sort combo, sort direction, `n of m photos` and the Actions menu, which carries Score All, Group Bursts, Auto Adjust All and the GPX import. Alt+= makes it worse, because the items scale. The browser build is the most exposed.
- Suggested fix: wrap the row in `ScrollArea::horizontal`, as `selection_bar` does, or use `horizontal_wrapped`. Add a headless-frame test at 700 pt width asserting the Actions button is reachable.

### [UI-B-03] Library preset rows overflow, pushing the `⋯` menu out of the panel — Severity: Medium — Confidence: Medium
- Location: `src/ui/presets.rs:63-73`
- Evidence: each library row is `ui.horizontal(|ui| { preset_row(...); ui.with_layout(right_to_left, library_menu) })`. A horizontal layout doesn't wrap, so a long preset name's `Button::selectable` takes its natural width. Folder-preset rows at `:93` are in the vertical list and do wrap.
- Impact: a library preset with a long or CJK-heavy name (Lightroom imports are long, e.g. "Cinematic Teal Orange Warm Highlights") fills the panel. The row's Rename and Delete menu is clipped off the right edge and can't be reached, though the preset can still be applied.
- Suggested fix: give the name button `ui.available_width() - menu_width` and truncate it (`Button::truncate()` or `Label::truncate`), or lay the row out as a right-to-left menu first and then the left-aligned truncated name.

### [UI-B-04] Medium theme: hyperlink and wordmark contrast fall below AA, and the tests don't cover them — Severity: Low — Confidence: Medium
- Location: `src/ui/theme.rs:420` (`v.hyperlink_color = p.selection`), `:15` (`BRAND_BLUE`), `:227` (Medium is fixed at gray 96), tests `:429-549`
- Evidence: I computed WCAG ratios by hand. `SELECTION_BLUE` (90,160,255) on the Medium panel (about 92,96,102) and window (88 gray) is about 2.4-2.7:1. The Light header with BRAND_BLUE is about 3.1:1, and the Medium header (about 78 gray) is about 2.6:1. Links are `ui.link` for "Click to allow access" (`presets.rs:176`) and Skip tour (`tour.rs:178`). No test measures `selection`, `hyperlink` or `danger` as text on `panel` or `window`. The danger-as-text test checks only `widget[0]` at 3.0, and `danger` is used as small text in `form::error` and the red Delete menu items.
- Impact: link text is hard to read in the shipped Medium theme. A future palette edit could regress danger text unnoticed.
- Suggested fix: add tests for `selection` against `panel` and `window` at 4.5, and for `danger` as text on `panel` and `window` at 4.5. Give `hyperlink_color` its own palette entry for Medium. Optionally use a per-theme wordmark blue.

### [UI-B-05] Presets page is entirely disabled in a read-only folder — Severity: Low — Confidence: High
- Location: `src/ui/presets.rs:26`
- Evidence: the doc comment says "Applying a preset edits; the footer's library work doesn't". The code wraps the whole `list` in `editable`, so the search box, the group expand/collapse headers and the Rename/Delete `⋯` menus are disabled too. Only Apply edits the folder. Rename and Delete operate on the library, and `RenamePresetPrompt` and `RequestDeletePreset` are not in `edits_folder`.
- Impact: in a read-only folder (a second instance, a locked volume) the user can't search or browse the presets, or manage the library from the list.
- Suggested fix: apply `editable` only to the row buttons (`preset_row`), not to the container, search field or `library_menu`.

### [UI-B-06] Presets list does per-frame work proportional to the whole library, with no virtualization — Severity: Low — Confidence: Medium
- Location: `src/ui/presets.rs:34-100`
- Evidence: every frame it clones the query string, calls `to_lowercase()` on `t.my_presets` and on every library preset name, rebuilds a `BTreeMap` over all folder presets, and then lays out every row of every open group (`ScrollArea::show`, not `show_rows`). `LIMITS.files` is 5000 per folder, and a search opens every matching group.
- Impact: a large Lightroom preset folder with the search box non-empty means thousands of widgets per frame, and the UI thread stalls while typing. Typing also schedules frames.
- Suggested fix: use `show_rows` or a per-group row budget, and cache the lowercased library names or compute them in `App`.

### [UI-B-07] Status toast is a single no-wrap line with unscaled offsets — Severity: Low — Confidence: Medium
- Location: `src/ui/mod.rs:983-1013` (`.extend()` at `:1006`, `screen.max.y - 48.0` at `:991`)
- Evidence: the label is `Label::new(..).extend()`, deliberately unwrapped. A long message (an export error with a path, a model-download failure reason) is wider than a narrow window. The Area is only position-constrained, so the text is clipped at the window edge. `48.0`, `16` and `10` don't go through `font_size::px`.
- Impact: error messages can be unreadable on narrow windows. Under Alt+= the toast can overlap the bottom bar.
- Suggested fix: wrap at `min(screen.width() - margin, ~600 pt)`, and scale the offsets with `font_size::px`.

### [UI-B-08] Save/Rename preset button is enabled for a blank name and does nothing — Severity: Low — Confidence: High
- Location: `src/ui/modals.rs:174-184` with `src/app/presets.rs:105`
- Evidence: `commit_preset_name` returns early on `name.trim().is_empty()`, leaving the prompt open with no message. The footer button is `Button::new(save, Role::Primary)`, always enabled.
- Impact: clicking Save, or pressing Enter, on an empty field looks broken. There is no feedback of any kind, and the code comment calls the early return "not a silent no-op".
- Suggested fix: set `enabled: !text.trim().is_empty()`, and show `form::hint` or `form::error` text when blank.

### [UI-B-09] Destructive confirms show no default-action focus, while Enter confirms — Severity: Low — Confidence: Medium
- Location: `src/ui/modals.rs:8-39`, `:97-123`; `src/app/keys.rs:~76` (Enter -> `confirm_pending`)
- Evidence: only `delete_group_modal` draws a focus ring (`footer_with_focus`). For `confirm_modal` (bulk Delete, `Role::Danger`) and `delete_preset_modal`, `keys.rs` makes Enter confirm, but nothing in the dialog shows that.
- Impact: a user who presses Delete, then Enter out of habit, trashes the selection. The Trash is recoverable on desktop, and I didn't check the browser. The deleted preset is not recoverable.
- Suggested fix: pass `footer_with_focus(.., Some(Role::Cancel))` for Danger confirms, and make Enter activate the focused button. Or require a click for `Role::Danger`.

### [UI-B-10] Mask handles and pins are culled when off the image, which makes some unreachable — Severity: Low — Confidence: Medium
- Location: `src/ui/mask_overlay.rs:456-474` (`handle_area`), `:433-440` (Rotate handle at `top + 24px`), `COORD_RANGE` in `src/develop/mask.rs:31`
- Evidence: `handle_area` returns `None` when `!central.contains(at)`. Mask coordinates may legally be -10..11.
- Impact: a radial near the top edge has its Rotate handle off-image, so its rotation can't be changed. A center dragged past the image edge loses its Move handle on release (and its pin, when deselected), and it can then only be reached through the panel list or undo. When a linear's Start and End are close together, its Move handle is created last and covers them.
- Suggested fix: clamp handle positions into `central.shrink(HANDLE_PT)` rather than culling. Optionally clamp dragged centers to the image.

### [UI-B-11] Literal sizes bypass `font_size::px`, and several hit targets are small — Severity: Low — Confidence: High
- Location: `toolbar.rs:257` (star `.size(20.0)`, every sibling uses `px(20.0)`); `modals.rs:396,402,403` (help `add_space(10.0)`, `min_col_width(140.0)`, `spacing [14,6]`); `mask_overlay.rs:12,465` (`HANDLE_PT` 6, so a 16x16 pt target); `mod.rs:915-918` (fullscreen button about 13.5 pt); `toolbar.rs:768` (item_spacing 1.0); `tour.rs:124` (margin 16); `presets.rs:19` (default_size 340.0)
- Evidence: UI.md says spacing "comes from the constants in `form.rs` through `font_size::px`, never from literal `add_space` numbers, so Alt+= and Alt+- scale it".
- Impact: Alt+= leaves these elements behind, and the Grid toolbar's stars no longer line up with the other controls. The fullscreen button, the mask handles and the filter dots are below the usual 24 pt pointer target.
- Suggested fix: route through `font_size::px`, enlarge the fullscreen and mask-handle hit areas, and keep the visuals small.

### [UI-B-12] Hover text on disabled controls never shows, and some disabled states give no reason — Severity: Low — Confidence: High
- Location: `mod.rs:761` (Undo/Redo `on_hover_text`); `presets.rs:206-208` (Save current as preset); `toolbar.rs:786-795` (Apply Adjustment)
- Evidence: in egui 0.34 `Response::on_hover_text` goes through `Tooltip::for_enabled`, which requires `response.enabled()`; only `on_disabled_hover_text` shows on disabled widgets.
- Impact:
  - The Undo/Redo tooltip, which carries the shortcut, doesn't appear while the button is disabled. This is harmless on its own.
  - "Save current as preset…" is disabled with a selection of 0 or 2+ photos and says nothing about why.
  - Apply Adjustment in a read-only folder with something copied shows `apply_settings_needs_copy` ("copy first"), which is wrong.
- Suggested fix: add `on_disabled_hover_text` with the real reason for each (needs exactly one photo, the folder is read-only).

### [UI-B-13] `ToggleHelp` is a non-idempotent dismiss — Severity: Low — Confidence: Low
- Location: `src/ui/modals.rs:419,423`, `src/app/mod.rs:1751`, `src/app/keys.rs:263-269`
- Evidence: Help closes via `ToggleHelp` from the Close button and from `should_close`, and also via the key handler's `show_help = false`. Settings correctly uses the idempotent `CloseSettings`. Today `keys.rs` clears the state before the next frame, so the modal never sees Esc.
- Impact: any future path that delivers two dismiss signals in one frame, such as a click plus Esc, would close and reopen Help.
- Suggested fix: add `UiAction::CloseHelp`. Also, the photo menu's "Export" entry (`photo_menu.rs:222`) uses `ToggleExportForm`, so choosing it while the export panel is open closes the panel.

### [UI-B-14] `TourStep::ALL` and `tour_steps` are linked only by convention — Severity: Low — Confidence: High
- Location: `src/ui/tour.rs:68,135`, `src/i18n.rs:347`
- Evidence: `TourStep::ALL[index]` and `t.tour_steps[index]` are direct indexes. `ALL` has 9 entries and `tour_steps: [_; 9]` is a separate hard-coded size. `tour_next` bounds on `ALL.len()`.
- Impact: adding a tour stop without growing every language table gives an index panic on the last stop, which CLAUDE.md prohibits. Compiler help is absent, and the test walks only the first two steps.
- Suggested fix: use `.get(index)` and return, plus `const _: () = assert!(TourStep::ALL.len() == 9)` tied to the i18n array size.

### [UI-B-15] Stars, flags and filter toggles have no accessibility labels, and stars have no tooltip — Severity: Low — Confidence: Medium
- Location: `toolbar.rs:645-667` (strip stars are raw `allocate_exact_size` with painted glyphs, no `widget_info`, no hover text); `toolbar.rs:257` (filter stars are click-sensing Labels, not buttons); `flag_toggle`, `label_dot` and `unrated_button` are painted, with hover text only
- Impact: screen readers see unnamed or label-typed elements. Users get no "Rate 3 stars" tooltip.
- Suggested fix: `resp.widget_info(|| WidgetInfo::labeled(Button, true, ..))`, plus hover text on the strip stars.

### [UI-B-16] `Theme::set` swallows a save failure — Severity: Low — Confidence: Medium
- Location: `src/ui/theme.rs:376-381`
- Evidence: `eprintln!("could not save the theme")` is the only reaction. `eprintln!` on wasm32 goes nowhere.
- Impact: a full disk or a blocked store silently reverts the theme on next launch, which is exactly the "full disk must give an error the user can act on" case.
- Suggested fix: return the error and let the caller `set_status(Error, ...)`.

### [UI-B-17] The `medium()` dark-text branch can't be reached in production — Severity: Info — Confidence: High
- Location: `src/ui/theme.rs:227,309-351`
- Evidence: `Theme::Medium` is hard-wired to `medium(Color32::from_gray(96))`, which is always `LightText`. The `DarkText` arm is exercised only by tests, and the Settings UI offers no gray slider.
- Impact: dead code that looks supported. My measured danger text contrast for that arm on its window is about 3.3:1, below 4.5.
- Suggested fix: delete the arm, or ship the slider and add its contrast tests.

### [UI-B-18] Small maintainability notes — Severity: Info — Confidence: High
- `src/ui/tabs.rs` holds only `footer_height` (22 lines), so the name is misleading.
- The `mod.rs` test module tests only `loupe::` formatters (`mod.rs:1212`), so those belong in `loupe.rs`.
- `theme::colors(ctx)` takes a `ctx.data` read lock and rebuilds a ~300-byte `Palette` on every call, and it is called per cell and per widget. Caching the palette next to the theme in `apply` would remove that.
- Per-frame `String` allocations: the gps modal's `source.display().to_string()` and `zone_text`, and the help modal's `keys(key)`. This is only an issue while those modals are open.

## Test coverage assessment

- `theme.rs`: good. It covers AA contrast for label, value, button text, filled buttons, pill, stack card and toast across the themes plus Medium at gray 150. It misses `danger` or selection as text on panel and window, hyperlink, and `BRAND_BLUE` on the header (UI-B-04).
- `mask_overlay.rs`: only three pure-math tests (radial drawn on the ellipse, rotate follows the pointer, axis handle sizes one radius). Nothing for screen-to-source mapping under zoom, rotation or straighten, for handle culling, for the draw/brush state machines (`far`, `index`, disarm on short drags), or for the Linear `Move`/`Start`/`End` drag.
- `toolbar.rs`: the two headless-frame tests (hover doesn't move the row, the F6 cursor visits every control left to right) are useful behavioral tests. Nothing covers narrow widths (UI-B-02).
- `modals.rs`: one test, on footer ordering. Nothing on the preset-name Enter/blank handling, Help column splitting, or the Settings scroll.
- `photo_menu.rs`, `presets.rs`, `tabs.rs`: no tests. A "menu and toolbar list the same labels" check would have caught UI-B-01.
- `tour.rs`: `callout_place` is tested, and `app/tour.rs` has click-through tests. There is no test of the last step, a Loupe-mode missing anchor, or the array-length coupling (UI-B-14).

## Things done well / verified-safe areas

- No panicking constructs in non-test code. Index operations are bounded: `sorts[(i + 1) % sorts.len()]` (`Name` is always present), `CopyGroup::ALL.split_at(4)` on a 12-element array, `ALL[index]` bounded by the app.
- Divisions in the mask overlay are protected by the early return on `!(u1 > 0.0 && v1 > 0.0)`. The brush drag and the ring math are NaN-safe, and `normalized()` of a zero vector is zero.
- `centered_row` inside `ScrollArea::horizontal` is safe: in egui 0.34 the content max width is the viewport (`if true { /* try to fit */ }` branch), not infinity.
- Mask coordinate mapping goes source UV, then `Straighten::to_canvas`, then texture UV with the rotation matrix, and the inverse. The drag math (`dragged`) works in source UV with the shorter-side units, and the tests confirm the rotate-handle angle convention. Crop is a view of the full texture, so it doesn't distort mapping.
- Press-origin handling for draw drags (`press_origin`), the same-frame press-and-release brush dab, the Enter-before-`request_focus` ordering in the name prompts, and the Esc double-handling avoided by `keys.rs` clearing state before the modal's next frame are all careful.
- Modal dismissal is consistent: every modal has Esc and a backdrop close, `should_close` maps to Cancel, and destructive confirms use `Role::Danger`.
- i18n discipline is excellent. The only literals are brand names ("Light", "Photos", "ComfyUI"), glyphs and `"?"`.
- `edits_folder()` covers the editing actions. I checked that geotag and fills have their own read-only checks (`check_can_geotag`, `coverage.read_only_photo`).
- Right-click menu opens on the Loupe only when the pointer isn't over egui, so it can't fire through the mask tools or a modal.

## Open questions for maintainers

1. Is omitting Purple from the photo menu intentional (e.g. mirroring Lightroom's keys), or an oversight? As written, no UI path can set it.
2. Is `Theme::Medium` meant to stay at gray 96 for good? If so, the `DarkText` branch and the gray-150 test fixture can go.
3. Should `Enter` on a `Role::Danger` confirm be the default action? `keys.rs` currently confirms on Enter with no visible focus.
4. What minimum window size or width is supported (especially in the browser build)? That decides whether UI-B-02 and UI-B-03 deserve ScrollArea, wrap or truncate, or are accepted.
5. Is bulk Delete reversible on wasm32? If not, the Enter-confirms behavior is a larger risk there.
