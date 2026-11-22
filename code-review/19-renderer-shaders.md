# src/renderer and src/shaders (wgpu)

## Scope

Every line of these files was read.

| File | Lines |
|---|---|
| `src/renderer/mod.rs` | 2250 |
| `src/renderer/raw_render.rs` | 92 |
| `src/renderer/match_test.rs` | 1432 |
| `src/shaders/loupe_common.wgsl` | 938 |
| `src/shaders/shader.wgsl` | 107 |
| `src/shaders/raw_shader.wgsl` | 65 |
| `src/shaders/base.wgsl` | 119 |
| `src/shaders/mipgen.wgsl` | 36 |

Cross-checked against these files, but not reviewed in full:

- `GpuAdjust`, `GpuTouchUp` and `GpuCurve` in `src/develop/mod.rs`
- `GpuMask` in `src/develop/mask.rs`
- `GpuFill`, `FillSet::atlas` and `shelf_pack` in `src/develop/fill.rs`
- `Grid` in `src/develop/base.rs`
- `Effects::new` and `Sharpen::new`
- callers in `src/app/{adjust,loupe,thumbs,fullscreen}.rs`
- wgpu 29.0.3 sources (default error handler, `CurrentSurfaceTexture`, the WebGPU `create_pipeline_layout`)

## Summary

The renderer is carefully built. It has no `unwrap` or `unsafe`, and no per-frame bind-group or buffer creation. The base and fine layer caching is keyed correctly. The WGSL uniform and storage layouts match the Rust structs: I computed `Adjust` as 448 bytes, `Mask` as 96, `TouchUp` as 48, `Fill` as 32, `FillTable` as 1536 and `Overlay` as 32. A GPU-versus-CPU parity matrix covers most develop features.

The main weakness is failure handling. The code never installs a wgpu uncaptured-error handler or a device-lost callback, and wgpu's native default is to panic. Several renderer entry points take unchecked inputs. A GPU out-of-memory error, an oversized texture or a short pixel buffer would therefore kill the app on the UI thread. That breaks the project's "never panic" rule for non-test code. The second weakness is test reach. Parity is checked only on macOS/Metal, and the RAW shader is checked for identity and fills only.

Findings by severity: High 1, Medium 3, Low 6, Info 4.

## Findings

### [REND-01] No uncaptured-error or device-lost handler, so any wgpu error panics the UI thread  — Severity: High — Confidence: High (default behaviour), Medium (how likely a trigger is)
- Location: `src/renderer/mod.rs:387-439` (`Renderer::new`), and every `create_texture`, `write_texture`, `write_buffer` and `submit` in the file.
- Evidence:
  - `grep` finds no `on_uncaptured_error`, `set_device_lost_callback` or error scope anywhere in `src/`.
  - wgpu 29.0.3 `backend/wgpu_core.rs:685` has `default_error_handler(err) -> ! { panic!("wgpu error: {err}") }`.
- Impact / failure scenario:
  - On native, any validation or out-of-memory error from the renderer takes the default handler and panics on the UI thread. `jobs::loader`'s catch-all does not cover this.
  - Likely triggers:
    - A `PixelFormat::LinearF16` RAW at the adapter's `max_texture_dimension_2d` (16384² RGBA16F is about 2 GB plus mips) on an integrated GPU or a small-VRAM card. `set_image` is called with the image plus the previous image still alive.
    - A device reset or eGPU unplug.
    - Any of the unchecked inputs in REND-02.
  - A panic also loses the slider edit in flight (`unsaved_edit` has not reached the sidecar).
  - On wasm the handler only logs to the console, and every later call fails. That leaves a permanently blank canvas with no message to the user.
- Suggested fix:
  - After `request_device`, call `device.on_uncaptured_error(Arc::new(|e| ...))`. Log the error and set a flag `Renderer::take_error()`. The app then shows "GPU error" and drops the current image (`clear_image`). Treat out-of-memory as "image too large for this GPU" and fall back to the thumbnail or preview tier.
  - Call `device.set_device_lost_callback(...)` and surface it. Recreating the device is better, but at least show an error.
  - Wrap `create_texture` in `push_error_scope(OutOfMemory)` / `pop_error_scope` inside `set_image`.

### [REND-02] `set_image` and friends trust their caller about dimensions and buffer length  — Severity: Medium — Confidence: High
- Location:
  - `set_image`, `src/renderer/mod.rs:1280-1365` (no size check).
  - `upload_egui_image`, `:1603-1697`: checks only `rgba.len() < (4*w*h) as usize`, in `u32` arithmetic.
  - `set_touchups`, `:1831-1841`.
  - `upload_thumb`, `:1192-1241`.
- Evidence:
  - `set_image` creates a `w x h` texture and calls `queue.write_texture(&img.rgba, bytes_per_row = bpp*w, rows_per_image = h)`. It checks neither `w, h <= self.max_dim` nor `img.rgba.len() == w*h*bpp`. `DecodedImage.rgba` and `pixel_format` are both public.
  - `upload_egui_image` and `upload_thumb` also never compare against `max_dim`.
  - `set_masks` (`:1850`) and `set_fills` (`:1901`) defensively cap their slices to `MAX_MASKS` / `MAX_FILLS`. `set_touchups` does not cap to `MAX_TOUCHUPS`; its `write_buffer` goes into a buffer of exactly `64 * 48` bytes.
  - Today the app keeps spot lists at or below 64 (sidecar load truncates, `apply_touchups` rejects). That is an unenforced cross-module invariant.
- Impact / failure scenario:
  - A decoder path that returns a short buffer, or one that forgets to clamp to `max_dim`, reaches wgpu validation, which under REND-01 means a panic. The fit-to-`max_dim` step lives elsewhere (`fit_within`, per-decoder). Any decoder path that skips it (a new RAW path, the wasm worker) crashes the app instead of showing a failed-decode message.
  - Several `u32` products can overflow when `max_dim` is 32768, as on NVIDIA Vulkan:
    - `4*w*h` and `padded_row*h` in `upload_egui_image`
    - `padded_row * h` in `read_offscreen_blocking`
  - These are unlikely, but they are input-derived arithmetic that CLAUDE.md asks to be checked.
- Suggested fix: at the top of each upload entry point, return early (`clear_image()` plus a logged error, or a `Result`) unless `w, h` are in `1..=max_dim` and the length is `w as usize * h as usize * bpp` (checked math, in `usize`). Cap `touchups` with `.get(..MAX_TOUCHUPS)` as the other two setters do.

### [REND-03] Surface `Lost` and `Validation` are mishandled, and `Suboptimal` is ignored  — Severity: Medium — Confidence: Medium
- Location: `src/renderer/mod.rs:2119-2133`.
- Evidence:
  - `C::Outdated | C::Lost => surface.configure(..); return false`.
  - `C::Timeout | C::Occluded | C::Validation => return false`.
  - wgpu's own docs for `Lost` (`wgpu-29.0.3/src/api/surface_texture.rs:74`) say "needs to be recreated … call `Instance::create_surface()`, then `configure`". The renderer keeps no `Instance` and only reconfigures.
- Impact / failure scenario:
  - A really lost surface (some Vulkan/Wayland compositor restarts, display hot-unplug, macOS display sleep edge cases) returns `Lost` on every frame forever. The window stays blank and the caller keeps retrying, because the doc says "The caller must retry".
  - `Validation` and `Occluded` also return `false` immediately, so a caller that re-requests a redraw on `false` busy-loops.
  - `Suboptimal` is treated like `Success` and never reconfigured. After a resize some drivers keep delivering a mis-sized swapchain.
- Suggested fix:
  - Keep the `wgpu::Instance` and the `Arc<Window>`, and recreate the surface on `Lost`.
  - Reconfigure after presenting a `Suboptimal` frame.
  - Return a tri-state (`Presented`, `Retry`, `Occluded`) so the caller can idle on `Occluded` instead of spinning.

### [REND-04] Parity tests cover the RAW shader only for identity and fills, and run only on macOS/Metal  — Severity: Medium — Confidence: High
- Location: `src/renderer/match_test.rs:982-1024` (`loupe_fills_match_cpu_bake_on_half_float_photos`), `:906-932` (`headless`, `no_adapter`), `:800-819` (`tolerance`).
- Evidence:
  - The full matrix (every slider at both ends, curves, mixer, grade, masks, vignette and the rest) runs on `PixelFormat::Srgb8` fixtures only.
  - For `LinearF16` and `TonedF16`, the test filters to `identity` and the five `fill*` cases (`assert_eq!(cases.len(), 6)`).
  - `raw_shader.wgsl` differs from `shader.wgsl` by the sRGB working gamma and the `apply_raw_preview_boost` knots. These feed tone, curve, mixer and grade, none of which is parity-tested on the RAW path. This is the only path non-mac users get for RAW.
  - The tolerances were measured on an Apple M-series GPU (comment at `:800`). Linux skips because lavapipe is up to 11 levels off, and Windows/DX12 and WebGPU are never exercised.
- Impact / failure scenario:
  - A RAW-only regression, such as a changed knot, a `linear_to_srgb` branch error for negative values, or an unboosted channel in `developGamma`, passes CI. It would show up as an on-screen mismatch with export, histogram and Auto Tone.
- Suggested fix:
  - Run a representative subset (for example `everything`, `mixer_all`, `grade_all`, `curve_rgb`, exposure and contrast extremes, one mask) on a `LinearF16` ramp, in a test that is already macOS-gated.
  - Add `ramp` values above 1.0 and below 0, since linear RAW data can have them.
  - Consider a non-Metal CI job with a real or software Vulkan device, even if only for smoke render.

### [REND-05] Premultiplied-alpha macOS images are blended as straight alpha  — Severity: Low — Confidence: Medium
- Location: `src/shaders/shader.wgsl:82`, `src/shaders/raw_shader.wgsl:64`, `src/renderer/mod.rs:733` and `:776` (`BlendState::ALPHA_BLENDING`), `src/decode/image_decode/mod.rs:117-120`.
- Evidence:
  - `DecodedImage` documents that alpha is premultiplied on macOS and straight elsewhere.
  - The shaders output `vec4(srgb, texel.a)` and the pipeline uses `ALPHA_BLENDING` (`src*α + dst*(1-α)`).
  - The `Rgba8UnormSrgb` hardware decode and the mip box filter also treat premultiplied bytes as straight.
- Impact / failure scenario: a semi-transparent PNG, HEIC or WebP shows a darker, wrongly composited fringe on macOS (colour × α twice) and a correct one on Windows, Linux and the web. Opaque photos (the normal case) are unaffected.
- Suggested fix: pick one convention for the Loupe. Either un-premultiply in `decode_imageio`, or composite over `loupe_bg` and force alpha to 1. Keep the same behaviour on every platform.

### [REND-06] The base/fine reduce pass is one unbounded-per-fragment draw  — Severity: Low — Confidence: Low
- Location: `src/shaders/base.wgsl:70-88` (`fs_base_reduce`), `src/renderer/mod.rs:1497-1532` (`encode_base`).
- Evidence:
  - Each grid cell loops over every source texel it covers: `(src.x/grid.x) * (src.y/grid.y)` texels, each calling `preAt`. That is 3 samples for chromatic aberration (CA), plus the touch-up and fill loops.
  - For a 61 MP image with a 256-cell short side that is about 1400 texels per fragment.
  - For a 16384² image it is about 268 M `preAt` calls, with up to 800 M texture samples in a single draw.
- Impact / failure scenario:
  - On weak iGPUs this can run for seconds in one submission. Windows' watchdog (TDR) and the browser's GPU watchdog can reset the device, and under REND-01 that is fatal.
  - The same pass reruns on every touch-up edit or CA slider change.
- Suggested fix: split the reduce into row bands (several scissored draws or several passes), or first mip-downsample into an intermediate texture. A band count scaled to `src / grid` keeps each draw under about 50 ms.

### [REND-07] Memory peak in `set_image`, and a fresh texture and bind group per selection-mask update  — Severity: Low — Confidence: Low
- Location: `src/renderer/mod.rs:1280-1357`, `:1738-1812`.
- Evidence:
  - `set_image` creates the new texture, its mips and the grid textures while `self.image_bind`, `self.base`, `self.fine` and `self.image_view` still hold the previous image. `self.fine = None; self.image_view = None` is done early. The old `image_bind` and `base` are replaced only at `:1355-1357`.
  - `set_selection_mask` allocates a new R8 texture and bind group on every call. The app calls it when the mask id changes (every brush stroke edit in `sync_mask_overlay`).
- Impact / failure scenario:
  - For 16k² RAW (about 2.8 GB with mips) two images can be resident at once. That makes REND-01's out-of-memory path more likely.
  - The mask texture churn is small per call, but it creates garbage during a brush drag.
- Suggested fix:
  - Call `self.image_bind = None; self.base = None;` before the `create_texture`. wgpu defers the actual free until in-flight work completes, so this only helps if a frame is not still using it.
  - Reuse the mask texture when `(w, h)` is unchanged and use `write_texture`.

### [REND-08] Rust/WGSL struct layout agreement is only checked at runtime, and `16` is hard-coded in the shader  — Severity: Low — Confidence: High
- Location:
  - `src/develop/mod.rs:1119` (`GpuAdjust`), `src/develop/mask.rs:703`, `src/develop/mod.rs:1080` and `src/develop/fill.rs:582`.
  - `src/shaders/loupe_common.wgsl:101` (`array<Fill, 16>`) and `:225` (`min(u32(adj.fill_count), 16u)`).
- Evidence:
  - The only compile-time size check is `assert_eq!(size_of::<GpuCurve>(), 4096)`.
  - wgpu's `min_binding_size` validation catches a Rust buffer smaller than the WGSL struct. It does not catch reordered fields, or a Rust struct larger than WGSL.
  - Reordered same-size fields would give wrong colours, with no error, on every platform.
  - If `MAX_FILLS` is raised, the shader would silently clamp at 16 and `FILL_TABLE_BYTES` would grow past the declared struct.
- Impact / failure scenario: a later edit to either side silently mis-maps uniforms. `match_test` would catch it, but only on a GPU runner.
- Suggested fix:
  - Add const assertions: `size_of::<GpuAdjust>() == 448`, `GpuMask` 96, `GpuTouchUp` 48, `GpuFill` 32, `OverlayParams` 32, `Transform` 32, `BasePass` 16. Add a few `offset_of!` checks.
  - Add a test that reads `array<Fill, N>` and `16u` from the WGSL text and compares them with `MAX_FILLS`, as is already done for the Gaussian kernels.

### [REND-09] Output surface format choice is "first non-sRGB", and readback supports only 8-bit RGBA/BGRA  — Severity: Low — Confidence: Low
- Location: `src/renderer/mod.rs:444-451`, `:1119-1123`.
- Evidence:
  - `caps.formats.iter().find(|f| !f.is_srgb())` can return `Rgba16Float` or `Rgb10a2Unorm` if a backend lists one before `Bgra8Unorm`.
  - `read_offscreen_blocking` returns `None` for anything but 8-bit RGBA/BGRA.
  - Because the shaders hand-encode sRGB, a 10- or 16-bit float surface would also change how egui dithers.
- Impact / failure scenario: `--drive` `shot` silently produces nothing on such a driver, and the window gets an unintended HDR surface.
- Suggested fix: prefer an explicit allowlist (`Bgra8Unorm`, `Rgba8Unorm`) and fall back to the first non-sRGB format.

### [REND-10] `read_offscreen_blocking` ignores a failed map  — Severity: Low — Confidence: Medium
- Location: `src/renderer/mod.rs:1155-1158`.
- Evidence: `slice.map_async(.., |_| {})`, then `get_mapped_range()`. `get_mapped_range` panics if the map failed (for example out-of-memory or device lost). The result of `device.poll` is checked, but the map callback result is not. The buffer is never `unmap`ped.
- Impact / failure scenario: `--drive` `shot` panics instead of failing with an exit code and a message.
- Suggested fix: capture the callback result in a `Cell` or channel and return `None` on `Err`.

### [REND-11] Compare view reads the base layer built from the "after" half only  — Severity: Info — Confidence: Medium
- Location: `src/renderer/mod.rs:1711-1719`, `:1523-1527`, `src/app/loupe.rs:470-488`.
- Evidence:
  - `base_wanted` and `from_b` come from whichever of `set_adjustments` or `set_adjustments_b` ran last.
  - The "before" half has `_pad0 = mask_count = fill_count = 0`, Texture, Clarity and Dehaze at 0, and no CA. So today it never reads the layer.
  - `push_compare` runs every frame in compare mode, so `from_b` stays true.
  - This holds only because `push_compare` builds `before` as defaults plus crop and straighten.
- Impact / failure scenario: if "before" ever shows an arbitrary edit state (A/B compare of two settings), Clarity, Dehaze and Texture on the A half would read a layer built for B.
- Suggested fix: key the layer on "union of what A and B need" or build one per half. At minimum assert the invariant next to `from_b`.

### [REND-12] Stale or misleading comments, and `_pad0` doubling as the touch-up count  — Severity: Info — Confidence: High
- Location:
  - `src/renderer/mod.rs:1934-1942`: the doc for `render()` ("Render the image pass, then the egui pass…") is attached to `set_clear_color`.
  - `:146-148`: `touch_gen` says it is bumped "by `set_image`". It is not. `set_image` resets `layers_built_for`, which is equivalent.
  - `:349`: `image_view` is documented as the "level-0 view" but is an all-mips view.
  - `:165`: `touch_count: a._pad0 as u32`. A field named `_pad0` carries the touch-up count (also `loupe_common.wgsl:25` and `:260`).
- Impact: reader confusion. Someone "cleaning up" the underscore-named pad field would break touch-ups.
- Suggested fix: rename to `touch_count` in `GpuAdjust` and WGSL, and move the doc comments.

### [REND-13] Mip chain from a 2x2 bilinear tap drops the last texel of odd levels  — Severity: Info — Confidence: Medium
- Location: `src/shaders/mipgen.wgsl:33-36`, `src/renderer/mod.rs:1292` and `:1535-1597`.
- Evidence:
  - Level sizes are `floor(n/2)`. A destination texel centre at `(i+0.5)*src/dst` is only exactly a 2x2 box for even sizes.
  - For odd `2k+1` the last source column and row never contribute, and the samples drift by up to one texel across the level.
  - The drift compounds with each odd level.
  - For a 3-row level the single output row is just the middle row.
- Impact: zoomed-out views of images with odd dimensions have a very slight half-pixel shift or edge bias, and extreme aspect-ratio panoramas lose edge rows in deep mips. It is not visible in normal photos.
- Suggested fix: sample four explicit taps at `uv ± 0.25*texel_src` (or use a compute downsample) so the footprint is exact for any size.

### [REND-14] `overlay_pipeline_layout` passes `None` for group 0, which becomes JS `null` on WebGPU  — Severity: Info — Confidence: Low
- Location: `src/renderer/mod.rs:750-760`.
- Evidence: wgpu 29's WebGPU backend maps a `None` bind-group layout entry to `JsValue::NULL` (`backend/webgpu.rs:2136-2149`). Null layouts in `createPipelineLayout` are a recent WebGPU addition.
- Impact: if a shipping browser doesn't support null layouts yet, `createPipelineLayout` fails, the overlay pipeline is invalid, and any pass that sets it is invalidated. That could blank the Loupe, not just the overlay.
- Suggested fix: use an empty `BindGroupLayout` for slot 0 (or bind `tex_bind_layout`) on wasm. Check the minimum Chrome, Safari and Firefox versions you support.

## Test coverage assessment

- **Strong:**
  - GPU versus CPU bake parity (`loupe_shader_matches_cpu_bake`): every slider at both ends, curves, crop, straighten, CA, touch-ups, linear, radial and coverage masks, brush, vignette, grain, texture/clarity/dehaze, sharpening, mixer, grade and fills, across 5 fixtures. Tolerances are tight, at 2 levels.
  - Base layer parity per probe.
  - Layer rebuild caching (`only_the_spots_and_the_ca_scales_rebuild_the_base_layer`, `each_layer_builds_only_for_its_reader`).
  - Snapshot equals frame.
  - A missing adapter fails on CI instead of silently passing.
- **Gaps:**
  - RAW shader parity beyond identity and fills (REND-04).
  - Only the landscape branch of `maskWeight`. All fixtures are 96x64, or a larger landscape.
  - Portrait images, 90/180/270 `rot`, zoomed or panned transforms, and compare viewports are untested.
  - Overlay, `set_selection_mask`, `upload_thumb`, `upload_egui_image` and `resize` clamping.
  - Odd-size mips, the `max_dim` clamp, the coverage-buffer regrow path with a second growth, and multi-frame error paths.
  - `read_offscreen_blocking`'s BGRA branch. The headless renderer is hard-wired to `Rgba8Unorm`.
  - No test for malformed inputs (short buffer, zero or huge dimensions), although CLAUDE.md asks for synthetic-malformed tests when fixing crashes.
  - CPU-only layout assertions (REND-08).
- **Environment:** the whole parity suite is skipped on Linux (no GPU runner) and never runs on Windows or the browser.

## Things done well / verified-safe areas

- No `unwrap`, `expect`, `panic!` or `unsafe` in non-test renderer code. All indexing is on buffers the renderer sized itself, or is guarded.
- Layout verification:
  - `Adjust` is 448 bytes, which matches `GpuAdjust` field by field (the `vec2` members land on 8-byte alignment and the arrays on 16).
  - `FillTable` is 1536 bytes against `FILL_TABLE_BYTES`.
  - `TouchUp` is 48, `Mask` is 96 and `Overlay` is 32.
- Resource design:
  - Bind groups are created only when something changes (coverage regrow, fills, image, fine layer), never per frame.
  - `Rgba32Float` grid textures use `textureLoad` only, which keeps the base layer within core WebGPU.
  - Group 3 stays within WebGPU's four-bind-group and eight-storage-buffer defaults.
  - The mip sampler avoids reading the level being written.
  - The ping-pong order in `encode_base` never reads and writes the same texture in one pass.
- `Renderer::new` returns `Err` for missing surface, adapter, device, format or alpha mode. It requests the adapter's real limits and clamps the surface to `max_dim`.
- `resize` guards zero size and clamps to `max_dim`. `render` applies egui texture uploads even for skipped frames, which avoids a known egui panic.
- WGSL safety:
  - Division by zero is guarded (`len2 < 1e-12`, zero-size coverage, `atan2` at `c < 1e-6`, the denoise `wsum` always at least 1, `vig_axis` floored on the CPU side).
  - Derivative calls are placed before any early return.
  - The 5 integer overflows in `base.wgsl` are bounded (`cells <= 8192`, `size <= 32768`).
  - WGSL's saturating float-to-int conversions protect the noise lattice and fill indexes.
- GPU and CPU agree on negative linear values: the WGSL `linear_to_srgb` leaves negatives below 0, and `apply_raw_preview_boost` clamps them to the same result as `srgb_encode`.

## Open questions for maintainers

1. Is a panic on wgpu error intended on native (REND-01)? If you want to keep it as the fail-fast policy, say so in CLAUDE.md. It currently contradicts the "missing GPU, full disk … never a panic" rule.
2. What is the oldest browser the web build must support? That decides REND-14 and whether the `Rgba32Float` render-attachment requirement is safe everywhere.
3. Should the Loupe composite transparent images over `loupe_bg` identically on all platforms (REND-05)?
4. NVIDIA Vulkan reports `max_texture_dimension_2d = 32768`, and that value becomes `full_target` for the loader. Is full-size RAW decode up to 32768 px intended on those machines? A 32768x16384 RGBA16F texture is about 8.6 GB.
5. Is there appetite for a GPU runner (macOS is the only parity check today), or a looser, separate tolerance set for Vulkan/DX12?
