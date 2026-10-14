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
| `Cmd`+`G` | Group the selected cells into one stack; a selected stack brings all its members |
| `Cmd`+`Shift`+`G` | Ungroup the selected stacks; their photos come back selected |
| `Shift`+`Delete` | Delete Group: for the selected stacks, either Remove Group (keep every photo as a single) or move every member to the Trash |

## Rate, flag and label

| Key | Action |
| --- | --- |
| `0` `1` `2` `3` `4` `5` | Set star rating 0-5 |
| `Shift`+`0`-`5` | Set the rating and go to the next photo |
| `P` / `X` / `U` | Pick / reject / unflag |
| `` ` `` | Pick, or unflag a picked photo |
| `6` `7` `8` `9` | Red, yellow, green or blue label; press again to clear |

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
| `Cmd`+`Z` | Undo the last develop change: a slider drag, a reset, a crop, a preset, Auto Tone or a pasted look. Ratings, flags, labels and rotation are not undone |
| `Cmd`+`[` / `Cmd`+`]` | Rotate image −90° / +90° |
| `R` | Crop |
| `C` | Compare: 2 to 6 selected photos, grouped or not, or else the photos in the selected stack, with a shared zoom square |
| `Y` | Before / after compare |
| `M` | Linear Gradient mask: drag on the photo from where the effect is full to where it ends. `M` again puts the tool down |
| `Shift`+`M` | Radial Gradient mask: drag out from its center. A click without a drag adds a default-sized one |
| `Cmd`+`U` | Auto Tone this photo |
| `Cmd`+`Shift`+`U` | Auto Tone the selection (after a confirm) |
| `Cmd`+`Shift`+`C` | Copy develop settings: choose the sections to carry, then `Enter` copies and `Esc` cancels. The choice is remembered |
| `Cmd`+`Shift`+`Y` | Apply settings to the selection (after a confirm) |
| `Cmd`+`Shift`+`E` | Open or close the export form: a folder (`Exports/` by default) or an Immich server, and an output size. `Enter` exports, `Esc` closes. Edits are baked in and files are never overwritten |
| `Delete` | Move the selection to the Trash, after a confirm; a selected stack counts as all its photos. In the browser, deletes permanently |

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
| `R` / `Enter` | Commit the crop |
| `Esc` | Cancel the crop, or leave Touch Up |
| `Delete` / `Backspace` | Delete the selected touch-up |
| `Cmd`+`Z` | Delete the selected touch-up, or the last one if none is selected |

While cropping, drag the edges to resize and hold `Shift` to keep the ratio.

On the Masks page, a mask tool or a picked mask takes these keys. The others
work as usual.

| Key | Action |
| --- | --- |
| `Esc` | Put the mask tool down, or let go of the picked mask |
| `Delete` / `Backspace` | Delete the picked mask, not the photo |
| `Shift`+scroll | Over a radial mask's center, change its feather |
| `Cmd`+`Z` | Undo the last mask change; a whole handle drag is one step |
