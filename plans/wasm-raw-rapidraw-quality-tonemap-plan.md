# wasm32 RAW Loupe: RapidRaw's full pipeline, CPU demosaic + GPU tonemap

## Context

LightPhotos already has a mature wasm32 branch (Cargo.toml wasm target deps,
`Trunk.toml`/`index.html`, `src/bin/wasm_worker.rs` + `src/web_worker_pool.rs`
Web Worker pool, `src/app/web.rs` File System Access glue) that already
decodes and displays RAW files end to end in the browser, and a recent
session (commits `9b58598`/`3b04c86`/`d1d6b95`) already ported RapidRaw's real
sRGB gamma and its display brightness/contrast boost as a CPU lookup table,
plus root-caused a missing linear exposure gain.

Two gaps, confirmed by reading both codebases directly:

**1. Demosaic quality.** Every RAW decode on wasm32 — Grid *and* Loupe —
currently goes through `src/raw_fast_preview.rs`'s `DemosaicMode::Fast`
(`Superpixel3Channel`, a quarter-res 2x2 bin). `DemosaicMode::Quality`
(`PPGDemosaic`, real edge-directed interpolation — rawler's default, and
RapidRaw's own algorithm) already exists in the same file but is never
called. Verified against RapidRaw's source
(`/Users/andyyao/RapidRaw/src-tauri/src/raw_processing.rs`, `image_loader.rs`,
`lib.rs`) and its `rawler` fork (`imgop/develop.rs`): the command that
renders RapidRaw's actual editor/Loupe view, `generate_preview_for_path`
(`lib.rs:1540`), calls `develop_raw_image` with `use_fast_raw_dev = false` →
`DemosaicAlgorithm::default()` = `Quality`. `fast_demosaic = true` (→
`Speed`) appears exactly once, in `generate_all_community_previews` — preset
**thumbnail** generation. So RapidRaw's real algorithm is full PPG for the
main view, Speed only for thumbnails — Grid stays `Fast`, Loupe gets
`Quality`.

**2. Tonemap stage.** RapidRaw's pipeline is two stages: Stage A (CPU,
`rawler` decode + demosaic + white balance + color matrix + a highlight
rolloff, ending in **linear**, un-gamma'd `DynamicImage::ImageRgba32F`) and
Stage B (its own big WGSL compute shader: exposure, tone curve, and — for RAW
sources specifically — the display brightness/contrast boost). LightPhotos's
prior session ported Stage B's RAW-only piece as a **CPU lookup table**
applied at decode time, baked into u8 sRGB bytes before upload — because
lightphotos's renderer/shader.wgsl own a separate, established edit
pipeline (`develop.rs` + `GpuAdjust`, −100..100 sliders) shared across every
format, and that pipeline expects an already-tone-mapped sRGB texture.

The user wants the wasm RAW path to actually match RapidRaw's own two-stage
split: decode stops at Stage A (linear), a **second, RapidRaw-specific WGSL
shader** does Stage B's tonemap on the GPU — genuinely two `shader.wgsl`
files for now (mac ImageIO's existing one, unchanged; a new one for this
path), consolidated later. Confirmed scope for *this* pass: **display only**
— the new shader reproduces RapidRaw's default RAW tonemap (real sRGB gamma
+ its brightness/contrast boost), not lightphotos's own develop sliders
(exposure/contrast/WB/vibrance) — those stay off this path until a later
consolidation. Texture precision: `Rgba16Float` (filterable and
render-attachment-capable in WebGPU core, unlike `Rgba32Float`; half the
memory of a 32-bit float texture, which matters given wasm32's already-flagged
linear-memory pressure for large RAW files).

Out of scope, unchanged: mac's ImageIO path, native non-mac (Linux/Windows)
RAW decode (`image_decode.rs`'s `decode_raw_nonmac`, which keeps baking
gamma+boost into u8 sRGB exactly as today — this plan's new
`DecodedImage::pixel_format`/`Quality`-mode-produces-linear behavior is only
ever exercised from `wasm_worker.rs`), and any new RAW format support.

## Part A — `raw_fast_preview.rs`: Quality mode now produces linear output

`bin_bayer_quarter_res` (Bayer/cpp==1) and `decimate_linear_rgb`
(already-linear DNG/cpp==3) both currently emit u8 sRGB-boosted pixels via
`render_rgb_sample` → `to_srgb_u8`, regardless of `DemosaicMode`. Change: at
each function's pixel-emission point, branch on `mode`:

- `DemosaicMode::Fast` → unchanged: `render_rgb_sample` (gain → color matrix
  → real sRGB gamma → RapidRaw boost) → `[u8; 4]`, 4 bytes/pixel.
- `DemosaicMode::Quality` → new `render_rgb_sample_linear(rgb, cam2rgb) ->
  [f32; 3]`: gain (`image_decode::LINEAR_EXPOSURE_GAIN`) → color matrix +
  highlight rolloff (`apply_cam2rgb`'s existing `clip_euclidean_norm_avg`) —
  same as today, **stops there**, no gamma/boost/quantize. Convert each
  channel to `half::f16` (new dependency, see Part E) and write 8
  bytes/pixel (`f16::to_le_bytes()` × 4 channels, alpha = `f16::from_f32(1.0)`).

`fast_preview`/`bin_bayer_quarter_res`/`decimate_linear_rgb` already take
`mode` (from Part A's earlier demosaic-quality wiring — `fast_preview`
currently hardcodes `DemosaicMode::Fast` at its one call site and needs `mode`
threaded through from its caller, same change as before); this section
additionally makes the *pixel encoding*, not just the demosaic algorithm,
depend on it.

`decode_raw_preview_from_bytes(bytes, max_px, mode)` (the shared body behind
both public entry points — see below) constructs the final `DecodedImage`
with `pixel_format: if mode == Quality { LinearF16 } else { Srgb8 }` (Part B).
For `Quality`, **skip the `fit_within`/Lanczos3 resize-to-`max_px` step**:
that resize runs through the `image` crate's `RgbaImage<u8>`, which can't
represent f16 data, and PPG demosaic is already full-resolution (unlike
`Fast`'s built-in 4x reduction), so there's no cheap resize path to reuse.
Upload the full demosaiced resolution; the renderer's existing GPU mip chain
handles minification. This is a real memory/upload-size tradeoff (full-res
16-bit-per-channel vs. previously quarter-res 8-bit) — accept it for now
(matches the already-accepted CPU-speed tradeoff for `Quality`), flag it for
the verification pass, don't design around it.

Public entry points (unchanged naming from the demosaic-quality wiring):
```rust
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Fast)
}
pub(crate) fn decode_raw_quality_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
    decode_raw_preview_from_bytes(bytes, max_px, DemosaicMode::Quality)
}
```
`decode_raw_fast_from_bytes` stays byte-identical in name, signature, and
output shape — `decode_probe.rs`'s existing golden-hash tests for the Fast
tier need no changes. Drop the stale `#[allow(dead_code)]` on
`DemosaicMode::Quality` now that it's referenced.

## Part B — `DecodedImage` gains a `pixel_format` field

In `src/image_decode.rs` (where `DecodedImage` is defined, ~line 42):
```rust
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum PixelFormat {
    /// Tightly packed sRGB-gamma-encoded RGBA8 — every existing decode path.
    #[default]
    Srgb8,
    /// Tightly packed linear-light RGBA, 2 bytes/channel (`half::f16`) —
    /// produced only by `raw_fast_preview::decode_raw_quality_from_bytes`
    /// (wasm32 Loupe RAW decode). The renderer tonemaps this on the GPU via
    /// `raw_shader.wgsl` instead of expecting it pre-baked.
    LinearF16,
}

pub struct DecodedImage {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,       // raw bytes; layout depends on pixel_format
    pub pixel_format: PixelFormat,
}
```
Mechanical follow-up: every existing `DecodedImage { .. }` struct literal
(mac decode, non-mac JPEG/RAW, thumbnail extraction, `image_ops.rs`/test
fixtures, `web_worker_pool.rs`'s worker-message reconstruction — about 20
sites across `image_decode.rs`, `thumbnail.rs`, `raw_fast_preview.rs`,
`loader.rs`, `image_ops.rs`, `web_worker_pool.rs`) needs
`pixel_format: PixelFormat::Srgb8` added (or `..Default::default()` where a
literal doesn't already name every field). All of them keep meaning exactly
what they mean today — this is additive, not a behavior change, for every
site except the new `Quality`-mode branch in Part A.

## Part C — `raw_shader.wgsl` + renderer two-pipeline support

New file `src/raw_shader.wgsl`: a small, focused port of RapidRaw's RAW-only
tonemap block (the same math already ported as a CPU LUT in
`image_decode.rs`'s `apply_raw_preview_boost`, now expressed in WGSL against
genuinely linear GPU data instead of a precomputed table):
```wgsl
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var tex: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;

fn linear_to_srgb(c: vec3<f32>) -> vec3<f32> { /* standard EOTF^-1, ported from shader.wgsl's own or RapidRaw's */ }

const RAW_PREVIEW_BRIGHTNESS_GAMMA: f32 = 1.1;
const RAW_PREVIEW_CONTRAST_MIX: f32 = 0.75;

fn apply_raw_preview_boost(v: f32) -> f32 {
    let brightened = pow(v, 1.0 / RAW_PREVIEW_BRIGHTNESS_GAMMA);
    let contrast_curve = brightened * brightened * (3.0 - 2.0 * brightened);
    return clamp(brightened + (contrast_curve - brightened) * RAW_PREVIEW_CONTRAST_MIX, 0.0, 1.0);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let texel = textureSample(tex, samp, in.uv); // Rgba16Float: already linear, no hardware sRGB decode
    if (in.uv.x < 0.0 || in.uv.x > 1.0 || in.uv.y < 0.0 || in.uv.y > 1.0) {
        return vec4<f32>(0.12, 0.12, 0.13, 1.0);
    }
    let srgb = linear_to_srgb(texel.rgb);
    let boosted = vec3<f32>(
        apply_raw_preview_boost(srgb.r), apply_raw_preview_boost(srgb.g), apply_raw_preview_boost(srgb.b),
    );
    return vec4<f32>(boosted, texel.a);
}
```
Constants and formula copied verbatim from `image_decode.rs`'s
`RAW_PREVIEW_BRIGHTNESS_GAMMA`/`RAW_PREVIEW_CONTRAST_MIX`/
`apply_raw_preview_boost` — keep a `MUST stay in sync with` comment pointing
each direction, same convention `shader.wgsl`/`develop.rs` already use for
their own CPU/GPU pairs. No `Adjust`/`TouchUp`/crop/pan-zoom-uniform
handling needed in this module's own declarations — pan/zoom keeps working
because the pipeline's **vertex** stage is `shader.wgsl`'s existing `vs_main`
(wgpu allows a pipeline's vertex and fragment stages to come from different
shader modules as long as their `VsOut`/input interface matches by
`@location`), so `raw_shader.wgsl` only needs to declare that same `VsOut`
struct shape for its `fs_main` parameter.

`src/renderer.rs` changes:
- `const LINEAR_IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;`
  alongside the existing `IMAGE_FORMAT` const.
- In `new()`: load `raw_shader.wgsl` as a second `wgpu::ShaderModule`. Build
  `raw_pipeline` reusing the **exact same** `pipeline_layout` (all 4 groups —
  tex/xform/adj/touch) as the main `pipeline`, `vertex: { module: &shader,
  entry_point: "vs_main" }` (unchanged, cross-module reuse), `fragment: {
  module: &raw_shader, entry_point: "fs_main", targets: same
  `ColorTargetState` as the main pipeline (surface `format`, alpha blend) }`.
  Reusing the full 4-group layout even though `fs_main` only reads group 0
  means `render()`'s draw code needs **no branching on which groups to
  bind** — a shader is allowed to use a subset of its pipeline layout's
  groups. Also build `mip_pipeline_linear`: same `mip_shader`/`mipgen.wgsl`
  module (already fully format-agnostic — verified, it just samples and
  returns `textureSample(src_tex, src_samp, in.uv)`), new
  `ColorTargetState { format: LINEAR_IMAGE_FORMAT, .. }`.
- New `Renderer` field `image_pixel_format: PixelFormat` (or a plain bool),
  set at the end of `set_image`.
- `set_image`: branch near the top on `img.pixel_format` for `(texture
  format, bytes_per_pixel, mip pipeline)`:
  `Srgb8 → (IMAGE_FORMAT, 4, &self.mip_pipeline)`,
  `LinearF16 → (LINEAR_IMAGE_FORMAT, 8, &self.mip_pipeline_linear)`.
  Thread the chosen format into the `create_texture` call and the chosen
  `bytes_per_pixel` into `write_texture`'s `bytes_per_row: Some(bytes_per_pixel
  * w)`; thread the chosen mip pipeline into the existing per-level mip-gen
  loop (already parameterized by a pipeline value, just currently always
  `&self.mip_pipeline`). No other structural change — texture creation,
  mip-view creation, and bind-group creation are otherwise untouched, since
  `tex_bind_layout` (`TextureSampleType::Float{filterable:true}`) is already
  compatible with both formats.
  **Row-alignment note**: `set_selection_mask` (same file, R8Unorm path) pads
  its `write_texture` rows to `wgpu::COPY_BYTES_PER_ROW_ALIGNMENT` with an
  explicit comment that `write_texture` needs row alignment and 1-byte/texel
  "bites almost every time." The existing RGBA8 `set_image` path has never
  needed this padding in practice (multiple-of-64-width images are common
  enough it hasn't surfaced) — for the new 8-bytes/pixel path, apply the same
  padding pattern `set_selection_mask` already uses (round `bytes_per_row` up
  to `COPY_BYTES_PER_ROW_ALIGNMENT`, pad the buffer) rather than assuming
  arbitrary RAW widths happen to align. Cheap insurance, one established
  in-file pattern to copy.
- `render()`: in the `draw_into` closure and the `None`-viewport arm, change
  the captured `pipeline` from unconditionally `&self.pipeline` to `if
  self.image_pixel_format == PixelFormat::LinearF16 { &self.raw_pipeline }
  else { &self.pipeline }`. Every `set_bind_group` call stays exactly as
  written today.

## Part D — `app/histogram.rs`: skip for linear images

`build_hist_sample` (`src/app/histogram.rs:18`) reads `img.rgba` assuming
4-byte u8 sRGB pixels — wrong length/interpretation for `LinearF16`'s 8
bytes/pixel. Add an early guard: `if img.pixel_format != PixelFormat::Srgb8 {
clear the sample, mark dirty, return; }` — matches this pass's "display
only" scope (histogram is a develop-panel feature, deferred like the edit
sliders). `push_adjustments` (`src/app/adjust.rs`) needs no guard: it writes
to `adj_buf`/`touch_buf` unconditionally, which is harmless even though
`raw_shader.wgsl`'s `fs_main` never reads those bindings.

## Part E — `src/bin/wasm_worker.rs`, `src/web_worker_pool.rs`

Same wiring as the demosaic-quality plan, now carrying `pixel_format` across
the Worker postMessage boundary too:

- `wasm_worker.rs`'s `decode(bytes, max_px, is_raw, quality: bool)`: for
  `is_raw && quality`, call `decode_raw_quality_from_bytes` (now
  `LinearF16`-producing per Part A) instead of `decode_raw_fast_from_bytes`.
  `onmessage`: read `let quality = get_bool(&data, "quality");`, pass through.
  In the `Ok` result-posting branch, add a field for the decoded image's
  `pixel_format` (e.g. `"linear": bool`) alongside the existing
  `id`/`ok`/`width`/`height`/`rgba`.
- `web_worker_pool.rs`: `QueuedJob` gets a `quality: bool` field, derived
  inside `submit()` from `kind == JobKind::Preview` (`Preview` = Loupe →
  Quality/linear, `Thumb` = Grid → stays Fast/Srgb8) — **no signature
  change**, so both `app/web.rs` call sites (`request_web_thumbs:172`,
  `request_web_preview:322`) need zero edits. `pump()` posts the new
  `"quality"` field. `handle_worker_message`'s `Ok` branch reads the new
  `"linear"` field and sets `DecodedImage.pixel_format` accordingly instead
  of the current hardcoded `DecodedImage { width, height, rgba }` (needs the
  new field added there too, per Part B).

`app/web.rs`, `loader.rs`: no changes. `try_show`/`upload_shown`
(`app/thumbs.rs`) already pull whatever `Arc<DecodedImage>` landed in
`loader.rs`'s generic preview cache and hand it to `renderer.set_image`/
`build_hist_sample` unchanged — both now branch internally on
`pixel_format`, so the single full/preview/thumb tier cascade in `try_show`
needs no new branches or parallel caches.

## Part F — `Cargo.toml`

Add `half` (small, `no_std`-friendly, wasm32-compatible f16 conversion) to
the same `[target.'cfg(not(target_os = "macos"))'.dependencies]` block that
already carries `rawler`/`image`/`mozjpeg-rs`/`kamadak-exif`.

## Part G — `decode_probe.rs`

`raw_fast_preview_bayer_quality_tier_runs_without_panicking` currently calls
`bin_bayer_quarter_res` directly with `DemosaicMode::Quality` and checks the
returned buffer as u8 RGBA — update its assertions for the new 8-bytes/pixel
f16 layout (decode via `half::f16::from_le_bytes`, assert finite/plausible
values rather than non-zero u8 bytes). Add a small smoke test for the new
`decode_raw_quality_from_bytes` entry point itself (reusing the existing
`write_bayer_dng` fixture), asserting `pixel_format == PixelFormat::LinearF16`
and `rgba.len() == width * height * 8`. `decode_raw_fast_from_bytes`'s
existing golden-hash tests need no changes (Fast tier output is unchanged).

## Doc-comment updates (claims that go stale)

- `raw_fast_preview.rs`'s module doc and `DemosaicMode`'s own doc: "Fast is
  what every caller uses today... Quality isn't wired to any call site yet"
  → Fast is Grid-only, Quality is Loupe-only and now also changes output
  encoding (linear f16, not sRGB-boosted u8).
- `decode_raw_fast_from_bytes`'s doc: "Both the grid and the Loupe call this
  identically" → Grid-only now.
- `image_decode.rs`'s `apply_raw_preview_boost`/gamma constants: note they
  now have a WGSL twin in `raw_shader.wgsl` for the wasm Loupe path, in
  addition to their existing CPU LUT callers (`decode_raw_nonmac`,
  `raw_fast_preview`'s `Fast` tier).
- `plans/raw-decode-rapidraw-parity-design.md` (~lines 91-93, ~179): its
  "Quality demosaic wiring is out of scope" note is superseded by this plan —
  add a pointer so the two docs don't contradict each other.

## Verification

1. **Native, shared CPU logic** (fast iteration, no wasm toolchain):
   ```sh
   cargo test --bin decode_probe --features raw-probe
   ```
   Covers Fast-tier golden hashes (must stay byte-identical), the updated
   Quality-tier f16 test, the new `decode_raw_quality_from_bytes` smoke test,
   and the existing CFA-rejection test.
2. **wasm32 compile check**:
   ```sh
   cargo check --target wasm32-unknown-unknown --bins
   ```
   `--bins` matters: `wasm_worker` is a separate `[[bin]]` target with its
   own `#[path]`-included copy of `raw_fast_preview.rs`.
3. **End-to-end**: `trunk build`, then open a real RAW file in the Loupe.
   Confirm: (a) it renders — no wasm trap (`panic=abort` risk this codebase's
   comments repeatedly flag), (b) visibly sharper real demosaic vs. the old
   quarter-res artifacts, (c) the RAW-only brightness/contrast boost still
   reads the same as before the shader move (side-by-side against the
   current CPU-baked version, or against a native-mac ImageIO render of the
   same file, if available) — this is the actual test that the CPU→GPU move
   didn't change the picture, only where the math runs, (d) rough wall-clock
   sanity on decode+upload latency for a typical large RAW, given the two
   accepted tradeoffs here (PPG ~4-5x slower than native in an earlier spike;
   full-resolution 16-bit-per-channel upload instead of quarter-res 8-bit) —
   a gut check, not a benchmark gate, (e) Grid thumbnails unchanged
   (still Fast/quarter-res/sRGB8) — confirm `JobKind::Thumb` never flips
   `quality` to `true`.
