# Lightroom keymap

## Context

LightPhotos' shortcuts drifted from Lightroom Classic's defaults. The worst
cases: X exported where Lightroom rejects, C cropped where Lightroom opens
Compare, and Pick/Reject/Unflag had no keys at all. Switch every binding that
has a Lightroom equivalent to the Lightroom key. Drop the chords that only
existed for us.

**Blocked:** another session has uncommitted edits in `src/app/keys.rs`,
`src/i18n.rs`, `src/app/mod.rs` and the rail. Start after that work is
committed, and build on top of it.

## Before / after

### Culling

The flag and label keys apply to the whole selection, or to Compare picks,
with no confirm. This goes through `set_rating` / `set_flag`, as in 4a3d89f.

| Action | Before | After |
|---|---|---|
| Pick / Reject / Unflag | toolbar only | **P / X / U** |
| Toggle Pick ↔ Unflag | – | **`** |
| Rate 0–5 | 0–5 | 0–5 |
| Rate and advance | – | **Shift+0–5** (only with one photo selected) |
| Red / yellow / green / blue label | – | **6 / 7 / 8 / 9** (the same key again clears it) |

### Views

| Action | Before | After |
|---|---|---|
| Grid / Loupe | G / E | G / E |
| Develop (Sliders page) | – | **D** |
| Crop | C | **R** |
| Compare Photos in Stack | rail only | **C** (when the photo is in a stack) |
| Info / before-after | I / Y | I / Y |
| Cleanup / Masks | Q and K / K | **Q** / K |
| Zoom | Space, Cmd+0 fit, Cmd+1 100% | Space and **Z** (Lightroom's zoom toggle); **Cmd+0 and Cmd+1 removed** |

### Crop mode

| Action | Before | After |
|---|---|---|
| Commit | C or Enter | **R** or Enter |
| Rotate 90° | [ / ] | **Cmd+[ / Cmd+]** |
| Export while cropping | X | removed (X is Reject) |

### Editing

| Action | Before | After |
|---|---|---|
| Rotate 90° in the Loupe | [ / ] | **Cmd+[ / Cmd+]** |
| Brush size | [ / ] in Cleanup | [ / ] (its only meaning now) |
| Paste settings | Cmd+Shift+Y | **Cmd+Shift+V** |
| Auto Tone one photo / the selection | Cmd+U / Cmd+Shift+U | Cmd+U / **no key** (menu and toolbar only) |

### File and selection

| Action | Before | After |
|---|---|---|
| Export | X | **Cmd+Shift+E** |
| Deselect all | – | **Cmd+D** |
| Score photos | Cmd+Shift+S | **no key** (menu and toolbar only) |
| Auto Stack by Capture Time | Cmd+Alt+G | **no key** (menu only; Lightroom has none either) |
| Save preset | Cmd+Shift+P | **Cmd+Shift+N** |
| Stack / Unstack (Cmd+G / Cmd+Shift+G), Select all, Delete, Esc, Enter, Tab, F6, Cmd+, , ? | unchanged | unchanged |

## Implementation notes

- **`src/app/keys.rs`:** the bindings above. Colour labels reuse
  `App::set_label` and `ColorLabel` (`src/app/catalog.rs`, `src/catalog.rs`).
  Drop their `#[allow(dead_code)]`, and make `set_label` multi-selection
  aware like `set_flag`. Pressing a label the photo already has clears it.
  Retire `ColorLabel::from_digit` (Shift+1–5) in favour of 6–9.
- **`src/menu.rs`:** the menu replays each command's chord. Commands that lose
  their chord (Zoom to Fit, Actual Size, Auto Tone Selection, Score Photos,
  Auto Stack by Capture Time) either dispatch their action directly or leave the menu.
  Zoom to Fit and Actual Size go, since Z and Space cover zoom. Keep the
  others as items with no key equivalent. Update `chord()`, the Export,
  Paste Settings and Save Preset chords, and the `every_menu_chord_is_in_the_help`
  test.
- **Help overlay and i18n:** every shortcut string and tooltip, in both English
  and Chinese.
- **Tests:** rebind the tests that press C, X, [ ], Cmd+Shift+Y, Cmd+0 and
  Cmd+1. Add tests for P/X/U on a selection, 6–9 setting and toggling a
  label, Shift+digit rating and advancing, and R entering and committing a
  crop.
- **Website:** update `docs/shortcuts.astro` in `../lightphotos-app` to match.
  Edit it there, but don't commit without asking.
- **Memory:** update `keyboard-shortcut-cleanup.md` once this lands.

## Verification

1. `cargo fmt --check && cargo test && cargo build --release && cargo build --bins`
2. Drive scripts in the real app for P/X/U, 6–9, R crop, Cmd+Shift+E and D,
   with screenshots.
3. Check the macOS menu shows the new key equivalents, and none for the
   removed chords.
