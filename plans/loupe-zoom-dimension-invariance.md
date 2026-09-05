# Loupe zoom/pan: make it dimension-invariant across the preview→full swap

## Context

The Loupe shows an opened photo through up to four decode tiers (Thumb → Speed → screen-fit
Preview → full-resolution Full). All zoom/pan/fit math runs against `App::image_size()`
(`src/app/thumbs.rs:263`), which returns the true source pixel dimensions (`self.source_size`)
once a metadata read lands, but **falls back to the currently-uploaded texture's own pixel
dimensions** until then.

`self.zoom` is stored as an **absolute** source-pixel→screen-pixel ratio and `self.pan` as the
image's top-left corner in screen pixels. When `image_size()` transitions from preview
dimensions to true source dimensions **while the user has manually zoomed
(`fitted == false`)**, `loupe_transform()` (`src/app/loupe.rs:211`) recomputes `scale`/`offset`
from the new `iw/ih` against the unchanged `zoom`/`pan`:

- `denom = zoom * iw` grows by `k = source_longest / preview_longest`
- `scale` → ×`1/k` (image shrinks), `offset` → ×`1/k` (slides toward the texture origin)

Result: a visible jump — a 24MP photo in a 2560px preview snaps ~2.3× smaller and lurches
toward the top-left — at the moment the full decode (or, on native, the async EXIF read; on
wasm, `poll_web_full`'s `source_size` correction) lands. This is the bug the `TEMPORARY DEBUG`
tier-tint scaffolding (`src/renderer.rs:130`, `src/app/thumbs.rs:33`) was added to diagnose.

`upload_shown` (`src/app/thumbs.rs:200-224`) and the three `source_size` writers
(`on_exif_info` `src/app/thumbs.rs:776`, `poll_web_preview` `src/app/web.rs:517`,
`poll_web_full` `src/app/web.rs:680`) all re-fit **only when `fitted == true`**; none
compensate `zoom`/`pan` in the zoomed case.

**Intended outcome:** the pixels under the user's viewport do not move when a sharper tier
swaps in. Zooming in on a soft preview and having it resolve to full detail should change only
sharpness, never scale or position — cropped photos included.

## Approach: store the manual zoom relative to fit-scale

Replace the absolute `zoom` field with a **fit-relative** factor. The on-screen image span
then works out to `zoom_rel * loupe_area` on the fit-limiting axis — **independent of
`image_size()`** — so a preview→full dimension change produces an identical transform with no
compensation logic at any decode-result site.

### Core change — `src/app/loupe.rs` (+ field rename in `src/app/mod.rs`)

1. **`src/app/mod.rs:693`**: rename `zoom: f32` → `zoom_rel: f32` (multiple of fit-scale;
   `1.0` == fitted). Init `1.0` (`src/app/mod.rs:968`).

2. **New `fn fit_scale(&self) -> f32`** — factor the fit computation out of `fit_to_window`:
   `(ww / iw).min(wh / ih).clamp(MIN_ZOOM, MAX_ZOOM)` using `display_size()` and
   `loupe_area()`. Single source of truth for both `fit_to_window` and `zoom()`.

3. **New `fn zoom(&self) -> f32`** — `self.zoom_rel * self.fit_scale()`. The absolute zoom
   every existing consumer wants; replaces the five `self.zoom` reads in `loupe.rs`
   (`center` :150, `zoom_at` :156-158, `loupe_transform` :214-215, `full_wanted_for_zoom`
   :96). Bind `let z = self.zoom();` once per function.

4. **`fit_to_window` / `fit_for_crop`** (:52, :65): set `self.zoom_rel = 1.0` /
   `self.zoom_rel = MARGIN` respectively; `fitted = true`; `center()`; `push_transform()`.

5. **`reset_100`** (:77): `self.zoom_rel = 1.0 / self.fit_scale()` → `zoom()` == exactly 1.0
   (the clamp cancels). `fitted = false`.

6. **`zoom_at`** (:155): compute `new_abs = (self.zoom() * factor).clamp(MIN_ZOOM, MAX_ZOOM)`,
   do the existing cursor-anchor pan math with `self.zoom()` / `new_abs`, then store
   `self.zoom_rel = new_abs / self.fit_scale()`. `pan` stays in screen px.

7. **`rotate`** (:128): unchanged in spirit — `fit_to_window()` when fitted, else
   `center()` + `push_transform()`.

### Preserve absolute zoom across `loupe_area()` changes (behaviour parity)

When the viewport rect changes (window resize, compare split) while `fitted == false`, the
user expects the magnification to hold, not track the window. Two existing `!fitted` branches
need `zoom_rel` re-derived from the pre-change absolute zoom:

- **`src/app/mod.rs:1271`** (viewport-rect-changed, `else` branch): capture
  `let keep = self.zoom();` before assigning `self.loupe_viewport`, then after:
  `self.zoom_rel = keep / self.fit_scale();` and `push_transform()`.
- **`toggle_compare`** (`src/app/loupe.rs:258`, `!fitted` branch): same capture/restore around
  the `self.compare` flip, keeping the existing `self.pan.0 += (new_width - old_width) / 2.0`.

### Cropped images — no extra work, verify only

A committed crop is a normalized 0..1 rect in `Adjustments.crop`, applied by the shader
**downstream** of the transform (`src/shader.wgsl:176` discards outside-crop fragments).
`image_size()` / `fit_to_window` always use the full frame; the crop never enters the zoom/pan
math. Because the new transform is dimension-invariant, the crop rectangle and its overlay
handles (`loupe_tex_to_screen`, `src/app/loupe.rs:433`) stay put across the swap too. Crop mode
(`fit_for_crop`, `fitted == true`) is covered by the per-frame refit. The Shift-lock aspect
capture in `crop_grab` (`src/app/crop.rs:78`) reads `image_size()` for a pixel ratio — sub-pixel
drift only, and only if a decode swap lands mid-drag; left as-is.

### Out of scope

- **Aspect-ratio drift.** `fit_within` (`src/image_decode.rs:626`) floor-rounds `nw` and `nh`
  independently, so preview aspect can differ from source aspect by ~0.02-0.04% — sub-pixel,
  invisible. A non-mac embedded preview with a *genuinely* different aspect than the sensor
  would be a preview-decode correctness bug, separate from this and not observed; file it
  independently if it ever surfaces.
- **Removing the `TEMPORARY DEBUG` tier-tint scaffolding** (`src/renderer.rs:130-135`,
  `src/app/thumbs.rs:33-55`/`154-175`/call sites, `debug_tier_label`). Its comments say
  "remove once the Loupe zoom-refit fix is verified" — do it as a follow-up after the user
  confirms the fix, not in this change.

## Files

| File | Change |
|---|---|
| `src/app/mod.rs` | rename field `zoom` → `zoom_rel` (:693, :968); zoom-preserve shim at :1271 |
| `src/app/loupe.rs` | `fit_scale()` + `zoom()` accessors; rework `fit_to_window`, `fit_for_crop`, `reset_100`, `zoom_at`, `center`, `loupe_transform`, `full_wanted_for_zoom`, `toggle_compare` |

`self.zoom` is read only inside `loupe.rs` (plus the field decl/init in `mod.rs`) — `main.rs`
and `ui/` touch it only via `zoom_at`. Blast radius is two files.

## Testing / verification

1. **Extract a pure helper for unit testing.** Pull the `loupe_transform` arithmetic into a
   free `fn loupe_xform(image_size, loupe_area, zoom_rel, pan, rot) -> ([f32;2],[f32;2],[f32;4])`
   (mirrors the existing free `zoom_outruns_preview`). Add `#[cfg(test)]` cases in
   `src/app/loupe.rs`:
   - **dimension invariance:** same `zoom_rel` + `pan`, `image_size` = `(2560,1707)` then
     `(6000,4000)` (same 3:2 aspect) → `scale`/`offset` equal within 1e-4.
   - `reset_100` semantics: `zoom_rel = 1/fit_scale` ⇒ absolute `zoom()` == 1.0 for a range of
     window/image sizes.
   - fitted (`zoom_rel == 1.0`) ⇒ image exactly fills the fit-limiting axis.
2. `cargo test` — existing `zoom_outruns_preview` tests (`src/app/loupe.rs:484`) must still
   pass unchanged (that free fn and its `self.zoom()` caller are semantically identical).
3. `cargo build --release` and `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release
   --config Trunk.toml` — both targets compile.
4. **Manual (user runs their own instance — no GUI automation):**
   - Native: open a *folder* (so the EXIF read is genuinely async) of 24MP RAW/JPEG; in the
     Loupe, scroll-zoom to ~300% within the first moment, before it sharpens. The image must
     stay put as detail resolves — no shrink, no lurch to the top-left. Repeat with Alt+0
     (100%) and with Space+drag pan mid-swap.
   - Repeat on a photo with a committed crop: same test, plus confirm the crop framing/black
     margins don't shift.
   - Repeat in the wasm build (`./scripts/deploy-web.sh` or a local trunk serve): the
     `poll_web_full` `source_size` correction is the swap trigger there.
   - Toggle before/after compare (`\`) and resize the window while zoomed in: magnification
     holds, matching today's behaviour.
   - The tier-tint scaffolding stays in for this pass — use it to watch the Preview→Full
     transition while testing.

## Copy to repo

Per project convention, copy this plan to `lightphotos/plans/` for check-in once approved.
