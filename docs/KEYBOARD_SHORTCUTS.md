# Keyboard shortcuts

Press `?` (`Shift`+`/`) in the app for the built-in overlay. This file lists the
same bindings in the same order, plus the ones the overlay leaves out. `Cmd` is
the primary modifier on macOS. On Linux, Windows and the browser it is `Ctrl`.

## Navigate

| Key | Action |
| --- | --- |
| `E` / `G` | Editor (Loupe) / library (Grid); `E` opens the selected photo from the Grid |
| `Enter` or `Space` | Open the selected photo (in the library) |
| `Esc` | Step back one level; from the folder list, prompts to quit (not in the browser) |
| `←` / `→` | Previous / next photo (in the editor); move the selection (in the library grid) |
| `↑` / `↓` | Move the selection (in the library grid); zoom in / out in 10% steps (in the editor) |
| `PageUp` / `PageDown` | Previous / next photo while the detail or develop panel has focus |
| `Cmd`+`O` | Open the folder picker |

## Select

| Key | Action |
| --- | --- |
| `Cmd`+`A` | Select all in the current folder |
| `Shift`+click | Range-select |
| `Cmd`+click | Toggle one cell's selection |
| `Shift`+arrows | Extend the selection (library) |

## Rate and label

| Key | Action |
| --- | --- |
| `0` `1` `2` `3` `4` `5` | Set star rating 0-5 |
| `Shift`+`1`-`5` | Set color label (red / yellow / green / blue / purple) |
| `Shift`+`0` | Clear color label |

## Zoom and pan (in the editor)

| Key | Action |
| --- | --- |
| `Space` | Cycle zoom: fit, then 2× fit, then 100% |
| `Cmd`+`0` | Fit to window |
| `Cmd`+`1` | 100% (1:1 pixel) |
| `Cmd`+`=` | Zoom in (20% step) |
| `Cmd`+`−` | Zoom out (20% step) |
| `↑` / `↓` | Zoom in / out (10% step) |
| Scroll | Zoom at the cursor |
| `Shift`+scroll | Pan horizontally |
| `Alt`+scroll | Pan vertically |
| `Shift`+`Alt`+scroll | Trackpad zoom |
| `Space`+drag | Pan |

## Edit

| Key | Action |
| --- | --- |
| `[` / `]` | Rotate image −90° / +90° |
| `C` | Crop |
| `Y` | Before / after compare |
| `Cmd`+`U` | Auto Tone this photo |
| `Cmd`+`Shift`+`U` | Auto Tone the selection (after a confirm) |
| `Cmd`+`Shift`+`C` | Copy develop settings |
| `Cmd`+`Shift`+`Y` | Apply settings to the selection (after a confirm) |
| `X` | Open or close the export form: a folder (`Exports/` by default) or an Immich server, and an output size. `Enter` exports, `Esc` closes. Edits are baked in and files are never overwritten |
| `Delete` | Move the selection to the Trash, after a confirm; in the browser, deletes permanently |

## Culling

Turned off in the shipped build. `SHOW_GROUPING_TOOLS` in `src/app/mod.rs`
gates the toolbar buttons, the two keys below and the in-app overlay's Culling
section together, so nothing here is reachable until it is flipped on. It is
off because the thresholds behind the two features, `CLOSED_EYE_RATIO` and
`DEFAULT_MAX_FEATURE_DISTANCE`, have not been checked against real photos.

| Key | Action |
| --- | --- |
| `B` | Best-of-burst badges (library) |
| `D` | Duplicate-group badges (library) |
| Click a badge | Survey the group |

In Survey, `←` / `→` pick which photo the rating keys apply to. `Enter` rates the
group's best frame, the one the grid badges, 5 stars and every other member 1
star. `Esc` closes.

## Keyboard focus

| Key | Action |
| --- | --- |
| `F6` / `Shift`+`F6` | Cycle focus between regions (folders, grid, toolbar, develop, filmstrip) |
| `Tab` / `Shift`+`Tab` | Next / previous item in the focused region |
| `?` | Show or hide the help overlay |

## Modes with their own keys

Crop and Touch Up take over the keyboard while they are active. Other keys do
nothing, so a stray arrow or digit cannot move the selection out from under the
edit.

| Key | Action |
| --- | --- |
| `C` / `Enter` | Commit the crop |
| `Esc` | Cancel the crop, or leave Touch Up |
| `Delete` / `Backspace` | Delete the selected touch-up |
| `Cmd`+`Z` | Delete the selected touch-up, or the last one if none is selected |

While cropping, drag the edges to resize, hold `Shift` to keep the ratio, and
press `X` to open the export form.
