# Non-mac/wasm RAW decode: follow rawler's own demosaic (RapidRaw shape)

## Context

Compared this repo's three RAW decode paths against RawTherapee (real
C++ engine: dcraw-lineage decode, `camconst.json` calibration, 8
selectable demosaic algorithms including AMaZE/RCD, CPU-only
`ImProcFunctions` pipeline with LCMS2 output) and RapidRaw (real
Rust/rawler/wgpu RAW editor: decode → black/white normalize → WB →
demosaic [Speed/Quality] → camera color matrix → highlight compression
→ orientation → linear buffer to GPU, shader does live edits + final
gamma).

RawTherapee's engine can't be adopted directly — C++, LCMS2-dependent,
no wgpu, and this repo already ruled LCMS2 out for wasm32 (`color_profile.rs`
picked `moxcms` specifically because LCMS2 won't build there). RapidRaw's
stack (Rust, rawler, wgpu) is the actual match for this repo's mandate,
so its pipeline shape is the template.

**Scope of this doc**: decode-stage fidelity only (RapidRaw's steps 1–7).
Does **not** touch keeping the buffer linear through to the GPU shader
(RapidRaw's step 8) — that's a separate, much bigger change shared by
every platform including mac (`develop.rs`/`renderer.rs`/`shader.wgsl`
are one pipeline across mac/non-mac/wasm; mac's ImageIO decode already
hands back gamma-baked sRGB, not linear, so going linear-through-to-GPU
would fork mac and non-mac unless mac gets its own matching rework).
Explicitly out of scope here. mac's `image_decode.rs` is untouched by
this doc — it already delegates the entire RAW pipeline to ImageIO's
opaque `CGImageSourceCreateImageAtIndex` with zero decode options,
which already matches "use ImageIO."

## Finding that reshaped this plan

`rawler` 0.7.2 already ships real demosaic implementations for standard
Bayer sensors, unused outside their own files:

- `rawler::imgop::sensor::bayer::ppg::PPGDemosaic` — real edge-directed
  interpolation (Demosaic<f32,3>), full resolution. This is the exact
  algorithm `decode_raw_nonmac` (native non-mac CLI path, `image_decode.rs`)
  already uses today via `RawDevelop`'s `ProcessingStep::Demosaic`.
- `rawler::imgop::sensor::bayer::superpixel::Superpixel3Channel` — quarter-res
  2x2 bin (Demosaic<f32,3>), no interpolation. This is what
  `raw_fast_preview.rs`'s hand-rolled `bin_bayer_quarter_res` reimplements
  by hand, function-for-function.
- `RawImage::apply_scaling()` — per-Bayer-channel black/white-level
  rescale to [0,1], resets levels after. This is what `raw_fast_preview.rs`'s
  hand-rolled `normalize` closure (fused into both `bin_bayer_quarter_res`
  and `decimate_linear_rgb`) reimplements by hand.

So "follow RapidRaw's design" for the wasm/non-mac fast-preview path
mostly means: **stop hand-rolling stages rawler already provides**, and
use its own dispatch structure — `RawDevelop`'s own step order is
Rescale → Demosaic → CropActiveArea → (WhiteBalance+Calibrate fused,
applied together post-demosaic) → CropDefault → SRgb — as the reference
shape.

**2026-08-24 reset**: an earlier uncommitted layer on top of this file
added a per-camera DCP-profile-fetch system (`camera_profile.rs`,
`camera_profile_cache.rs`, a network fetch of RawTherapee's own `.dcp`
database, a tuned `LINEAR_EXPOSURE_GAIN = 2.4` empirical constant) that
didn't work out and has been discarded (`git reset --hard` + `git clean`,
confirmed by the user). The actual current baseline this doc now argues
from is simpler and already closer to RapidRaw's real design than that
discarded layer was: no camera-profile fetch at all, matrix comes
straight from `raw.color_matrix` (DNG-embedded calibration, exactly
RapidRaw's own step 5), tone mapping is a fixed generic gamma LUT
(`to_srgb_u8`), and there is no exposure-gain stage of any kind — this
doc and the plan it feeds are written against *that* file, not the
discarded one. No DCP/network-profile layer is being reintroduced by
this work.

## Design

Stages, in order, for the `cpp == 1` standard-Bayer path
(`raw_fast_preview.rs`'s current `bin_bayer_quarter_res`):

1. **Black/white normalize** — `raw.apply_scaling()` (rawler's own),
   replacing the hand-rolled `normalize` closure. Requires `raw` to be
   mutable at this point (currently bound immutable from `decode`) —
   restructure the call site accordingly.
2. **Demosaic** — new `DemosaicMode { Fast, Quality }`. `Fast` calls
   `Superpixel3Channel::demosaic()` (matches today's output/behavior —
   same quarter-res bin, just rawler's real implementation instead of
   ours). `Quality` calls `PPGDemosaic::demosaic()` (full-res, real
   interpolation). Both take `&Pix2D<f32>` (single-channel mosaic) +
   `CFA` + `PlaneColor` + `Rect` (roi), return `Color2D<f32,3>`. Deletes
   `bin_bayer_quarter_res`'s per-pixel interpolation loop; replaces it
   with: build the `Pix2D` view over `raw.data`, call the trait method,
   convert the returned `Color2D<f32,3>` into the rest of the pipeline's
   input shape.
   - `Quality` is implemented but **not wired into any call site** this
     round — stays available for a later UI-facing "full quality" toggle
     (export path, or a Loupe opt-in), per user's explicit scope choice.
   - `decimate_linear_rgb` (`cpp == 3`, already-demosaiced linear DNGs —
     no CFA, nothing to demosaic) is unaffected by this stage; still
     gets `apply_scaling()` for its black/white-normalize half.
3. **White balance** — stays pre-demosaic, same position as today (today's
   hand-rolled `normalize` closure multiplies each raw sample by
   `wb[cfa.color_at(row,col)]` before averaging). Factored into its own
   small standalone pass (`apply_white_balance_in_place`) over the
   `Pix2D<f32>` right after `apply_scaling()`, before the `Demosaic` call —
   *not* fused with the color matrix. (An earlier draft of this doc said
   WB would move to after demosaic, fused with the matrix, mirroring
   rawler's own `map_3ch_to_rgb`. Checked the math: WB is a per-channel
   scalar multiply and commutes with Superpixel averaging for identical
   G1/G2 coefficients, so that reorder would have produced the same
   output too — but keeping WB in today's exact position is simpler to
   reason about and lower-risk, so that's the design now.)
4. **Color matrix + highlight rolloff** — unchanged: `build_cam2rgb`
   (derives a camera-RGB→sRGB matrix from `raw.color_matrix`, the
   DNG-embedded calibration — RapidRaw's own step 5) and `apply_cam2rgb`
   (applies it, then `rawler::imgop::raw::clip_euclidean_norm_avg` as the
   soft highlight-rolloff — RapidRaw's step 6, already folded into this
   one function rather than split out). No separate exposure-gain stage
   exists in this codebase to extract or rename — that constant was part
   of the discarded DCP-profile layer, not this baseline.
5. **Gamma** — unchanged (`to_srgb_u8`, a fixed 1/2.2-gamma lookup table
   built once). No per-camera tone curve; this is RapidRaw's final
   linear→display gamma step, done here instead of at a GPU blit (this
   plan's explicit output-contract boundary — see below).
6. **Orientation** — unchanged (`apply_orientation`), already its own
   function.

`decode_raw_nonmac` (native non-mac, `image_decode.rs`) gets no code
change — it already delegates through `RawDevelop::default()` unmodified
(rescale → demosaic via `PPGDemosaic` → crop → WB+calibrate → crop →
sRGB gamma), the same rawler-native step machinery this plan brings to
the wasm path. It gets doc-comment alignment only, pointing at this
doc's stage vocabulary for consistency between the two non-mac paths.

## Output contract (unchanged)

Same as today: fully baked, gamma/tone-mapped RGBA8 buffer, same shape
ImageIO already hands back on mac. No linear-light output, no shader
changes, no `develop.rs`/`renderer.rs` changes. This is the boundary
that keeps this scoped to decode-stage fidelity only.

## Risk: rayon on wasm32-unknown-unknown

Both `PPGDemosaic` and `Superpixel3Channel` use `rayon::prelude`
internally. Spiked (2026-08-24): a throwaway call to both compiles and
codegens clean for `--target wasm32-unknown-unknown` via `cargo build`.
That only proves compile-time — this target has no real OS thread
support (no `+atomics` build flag anywhere in this repo's Cargo
config), so `rayon`'s parallel iterators spawning worker threads at
runtime is unverified and can't be checked by `cargo build`/`cargo
check` alone.

**Implication for the implementation plan**: the first task that wires
either `Demosaic` impl into the real wasm build must include an actual
in-browser smoke test (build the deployed wasm bundle, decode a real
RAW file, confirm no panic/hang) as its explicit verification step —
not deferred to a later task, not assumed from the compile-only spike
above.

## Testing

- `decode_probe.rs`'s existing brightness-comparison harness (against
  the ImageIO baseline) re-run before/after — this refactor changes
  *how* black/white-normalize and quarter-res demosaic happen but not
  their math for the `Fast` tier, so `Fast`-tier output should be
  byte-identical (or within existing float-rounding tolerance) to
  today's `bin_bayer_quarter_res` output on the same sample files.
- New: a same-file comparison between `Fast` and `Quality` tiers once
  `Quality` exists, purely to confirm `PPGDemosaic` doesn't panic/crash
  on real sample files — no fixed expected output, just "runs, produces
  a plausible image."
- The in-browser wasm smoke test from the risk section above.

## Explicitly out of scope

- Linear-light buffer kept through to GPU / shader-side edits+gamma
  (RapidRaw step 8) — separate doc, needs a matching mac-side decision.
- A tunable `highlight_compression` parameter, a per-camera tone curve,
  or any new camera-profile/DCP-fetch layer (network or baked-in) — this
  plan matches RapidRaw's actual design as-is: DNG-embedded calibration
  only (`raw.color_matrix`/`wb_coeffs`), generic gamma, no per-camera
  profile system of any kind. Reintroducing one is a different project.
- Wiring `Quality` demosaic into any UI call site.
- Any change to mac's `image_decode.rs` RAW path.
