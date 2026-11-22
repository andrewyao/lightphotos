# src/develop (part A) — base, brush, chroma, color, copy, coverage, curve, effects, fill_request

## Scope

All lines read, in full:

| File | Lines |
|---|---|
| `src/develop/base.rs` | 628 |
| `src/develop/brush.rs` | 577 |
| `src/develop/chroma.rs` | 286 |
| `src/develop/color.rs` | 467 |
| `src/develop/copy.rs` | 626 |
| `src/develop/coverage.rs` | 701 |
| `src/develop/curve.rs` | 441 |
| `src/develop/effects.rs` | 441 |
| `src/develop/fill_request.rs` | 475 |

Cross-checked against `src/shaders/base.wgsl`, `src/shaders/loupe_common.wgsl` (colorMix, oklab, toneCurve, effects, grain, vignette, baseAt/fieldAt, baseProbe), `develop/mask.rs` (Stroke sanitizing, falloff), `develop/parallel.rs`, `develop/image_ops.rs` (resample, pixel_linear), `sidecar.rs` and `persist/presets.rs` (load paths), `app/folder_store.rs` (RemoveCa paste contract), `app/optics.rs` (measure resolution).

## Summary

Overall quality is high. The pixel math is careful, and the CPU and GPU twins I diffed agree. That covers base/`cellSpan`/`overlap`/blur order, `colorMix`, the OkLab constants, `toneCurve`, vignette, the PCG grain and the `lattice` bitcast. Hostile-input handling is mostly done well: strokes are sanitized in `mask.rs`, the stamp budget bounds brush work, PNG limits are set and sizes are checked, and the `Detections` parse is tolerant. I found no panics in non-test code and no `unsafe`.

The real weaknesses are all at the sidecar and preset trust boundary, where a few numeric inputs are not scrubbed:

- `CaScale` is never clamped on load.
- A malformed curve point fails the whole record.
- `chroma::measure`'s search window is narrower than its advertised scale cap.

Findings by severity: Critical 0, High 0, Medium 1, Low 5, Info 3.

## Findings

### [DEV-A-01] Sidecar `chromatic_aberration` scales are never clamped; inf/NaN/huge values render garbage — Severity: Medium — Confidence: Medium
- Location: `src/develop/chroma.rs:13,134` (the only place `MAX_SCALE` is applied); `src/develop/mod.rs:847` (`Adjustments::clamped` covers `SLIDERS` and `straighten` only); `src/develop/mod.rs:748` (`CaScale::source_uv`); `src/develop/mod.rs:1248` (`a.chromatic_aberration.unwrap_or_default()` goes straight to the GPU uniform); `src/develop/base.rs:333-344`.
- Evidence: `MAX_SCALE` (0.005, "anything past it is a bad fit") bounds only what `measure` produces. A sidecar or preset can carry `{"chromatic_aberration":{"red":1e39,"blue":NaN-ish}}`. serde_json reads `1e39` as an infinite f32, as the `effects.rs` test comment itself notes. `source_uv` gives `(u-0.5)*inf+0.5`, which is `±inf`, or `NaN` at `u == 0.5` (`0*inf`).
- Impact / failure scenario: A hostile or hand-edited sidecar makes that photo render and export as NaN/garbage. Thumbnails, the Loupe and an export would all be wrong, with no error. A finite but large `k` (for example 0.5) also yields a plainly broken image rather than being treated as a bad scale. The `base.rs` comment ("a sidecar's runaway CA scale lands on an edge texel") is only true for finite huge values. See DEV-A-04 for inf/NaN.
- Suggested fix: Add a `CaScale::clamped()` (NaN to 0, clamp to `±MAX_SCALE`, or a looser documented bound) and call it from `Adjustments::clamped` and wherever a record or preset is adopted. Add a test with `1e39` and a NaN-producing value.

### [DEV-A-02] One out-of-range curve point makes the whole sidecar record, or the whole preset library, unreadable — Severity: Low — Confidence: High
- Location: `src/develop/curve.rs:19-20` (`#[serde(from = "Vec<[u8; 2]>")]`); consumers `src/sidecar.rs:547` (`parse_record`) and `src/persist/presets.rs:~60` (`Document`).
- Evidence: The `From<Vec<[u8;2]>>` impl is documented as "so a hand-edited sidecar can't break a render". The `[u8; 2]` deserialization happens first, though. `[[256,10],[255,255]]`, `[[1.5,2]]`, `[[-1,0]]` or `[[1,2,3]]` is a serde error, not a clamp or drop. The test `a_bad_point_list_loads_as_something_renderable` only covers structurally valid lists.
- Impact / failure scenario: A single bad point in one photo's sidecar returns `ReadError::Unreadable`. Per `read_record`'s contract the app then refuses to overwrite it, so that photo can't be rated or edited until the file is fixed by hand. In a preset document the same point flags the entire library `corrupt` and refuses all writes. The failure is safe and reported, but it is out of proportion to one bad number, and it contradicts the module's own resilience claim and the tolerant per-entry parsing used for masks, fills and detections.
- Suggested fix: Deserialize through `Vec<[serde_json::Value; 2]>` or a lenient helper that clamps or drops bad points, then run the existing sort, dedup and thin step. Add malformed-number tests.

### [DEV-A-03] `chroma::measure`'s ±3 px search cannot reach its own `MAX_SCALE` — Severity: Low — Confidence: Medium
- Location: `src/develop/chroma.rs:13,19,134,143`; `src/app/optics.rs:18` (`MEASURE_PX = 2560`).
- Evidence: `radial_shift` searches `±MAX_SHIFT = 3 px` and drops the sample when the best match is at the end of the range. The photo is measured at 2560 px on its long edge (half-diagonal about 1536 px), so a scale `k` gives a shift of `k*r`, which is 7.7 px at the corner for `k = 0.005`. Only edges with `k*r < 3` can be measured. The `clamp(-MAX_SCALE, MAX_SCALE)` is then effectively unreachable, and the doc "anything past it is a bad fit" does not describe what happens.
- Impact / failure scenario: Moderate to strong CA (`k` of about 0.003 or more) is measured only from the inner edges and drops below `MIN_EDGES`. The result is `0.0` ("no correction"), not a clamped correction, and the user sees "Remove CA" do nothing on exactly the photos that need it. For mild CA the result is fine. The tests only exercise `k` up to 0.002 on a 1200x800 image, where the maximum shift is 1.4 px.
- Suggested fix: Scale `MAX_SHIFT` with the image (for example `MAX_SCALE * r_max + 1`), or measure on a smaller image (shifts shrink with resolution). Add a test at `k = 0.004` on a 2560 px image.

### [DEV-A-04] `base::bilinear` yields NaN for inf/NaN coordinates despite its comment — Severity: Low — Confidence: High
- Location: `src/develop/base.rs:329-344` (comment at 333); compare `src/develop/coverage.rs:287` (which guards non-finite `uv`).
- Evidence: `sx = u*w - 0.5; fx = sx.floor(); tx = sx - fx`. For `u = NaN` or `±inf`, `tx` is NaN (`inf - inf`). The index casts saturate to 0 or the edge, but the blend `a*(1-tx)+b*tx` is NaN. `Coverage::at_uv` handles this with an early `0.0`, but `BaseLayer::at_uv` (and the coordinates reaching it from `caUv`, see DEV-A-01) do not.
- Impact / failure scenario: A NaN base value feeds Clarity, Dehaze or Texture. CPU produces NaN there; the GPU `vec2<i32>(NaN)` is implementation-defined, so CPU and GPU can disagree. This is reachable only via DEV-A-01-style input.
- Suggested fix: Treat non-finite `u`/`v` as 0 (or clamp the coordinates) before computing `tx`/`ty`, and fix the comment. Add a NaN/inf test next to `at_uv_at_a_cell_center_is_that_cell`.

### [DEV-A-05] `copy::merge` completeness depends on `SLIDERS` and is barely tested — Severity: Low — Confidence: Medium
- Location: `src/develop/copy.rs:377-398` and the tests at 466-478.
- Evidence: Every `Adjustments` field that isn't `curve`, `crop`, `straighten` or `chromatic_aberration` is carried only if some `SLIDERS` row points at it. I checked the count: 13 + 24 + 8 + 14 + 4 + 1 + 4 + 3 = 71, so today it is complete. The "everything copies" test builds a source with only about ten non-default fields (`source()`), so a dropped or mis-mapped row (for example `SharpenMasking`, one gray band, grade `global`, `GrainRoughness`) would pass.
- Impact / failure scenario: A future `Adjustments` field added without a `SLIDERS` row, or with the wrong `CopyItem::of` mapping, silently isn't pasted or applied by presets. Nothing fails.
- Suggested fix: Add a test that sets every `SLIDERS` field to a distinct non-default value and asserts `merge(default, src, all) == src` (apart from CA). Also assert that each item copies exactly the fields of its sliders.

### [DEV-A-06] Brush cache and raster size are keyed by an aspect that can differ between decodes of one photo — Severity: Low — Confidence: Low
- Location: `src/develop/brush.rs:53-71,241-255`; callers `src/app/loupe.rs:637`, `src/develop/mask.rs:556`.
- Evidence: `raster_size(aspect)` rounds from an `f32` aspect, and the key includes `(w, h)`. A preview and a full decode of the same photo can report slightly different pixel dimensions (embedded JPEG against sensor crop, for example 1.7783 against 1.7774), so the raster size can differ by a texel.
- Impact / failure scenario: The Loupe and the export can use different (sub-texel different) rasters, and each costs its own rasterization (up to the one-second budget) and its own cache slot, which can evict a live raster. Visually negligible. Related: `BrushRaster::new` derives `scale` from the rounded raster aspect rather than the true aspect, so dabs are elliptical by up to about 0.2%.
- Suggested fix: Quantize the aspect (for example to 1/1024 in the key and size) or key off the source's canonical dimensions. Optional.

### [DEV-A-07] Exhausting the stamp budget silently drops later dabs, erases included — Severity: Info — Confidence: High
- Location: `src/develop/brush.rs:139-141,171-179`.
- Evidence: Once `budget` hits 0 every later dab is skipped with no signal. That includes erase strokes and strokes in other masks after the first heavy one. The test `an_erase_after_many_sweeps_of_the_largest_brush_still_lands` shows the allowance is about 750 largest dabs, so it is hard to hit while painting.
- Impact / failure scenario: A sidecar from elsewhere (or heavy painting) shows a mask that differs from what was painted, with no hint. This is the deliberate trade against the DoS cap, noted so a maintainer can decide whether the UI should say "mask too complex".
- Suggested fix: Expose `budget == 0` (for example via `BrushRaster::exhausted()`) so the panel can warn.

### [DEV-A-08] `width as usize * height as usize` unchecked in public `Coverage` constructors — Severity: Low — Confidence: Low
- Location: `src/develop/coverage.rs:221` (`Coverage::fit`); `src/develop/image_ops.rs:723-728` (`sw * sh` is u32 arithmetic in `resample_bilinear_u8`).
- Evidence: `fit` is `pub` and takes arbitrary `u32` sides. On wasm32, `usize` is 32 bits, so `w*h` can wrap (a panic in debug builds). `resample_bilinear_u8` multiplies `u32`s, which overflow beyond about 65535².
- Impact / failure scenario: Only reachable with a detector plane of 64k x 64k or larger, which no current caller produces, so mostly theoretical. `decode()` is safe because the sides are capped first.
- Suggested fix: Use `checked_mul` (returning `CoverageError::Corrupt`) in `fit`, or reject sides above a sane bound up front.

### [DEV-A-09] Misleading `CoverageError::Missing` when the sidecar path can't be built — Severity: Info — Confidence: High
- Location: `src/develop/coverage.rs:362-364`, used by `write` at 383-391.
- Evidence: `file_path` returns `Missing` when `photo` has no parent or file name. `write()` propagates it, so a failure to save reads as "the mask's coverage file is missing".
- Impact / failure scenario: Confusing text for a write failure, only for a pathological photo path.
- Suggested fix: Map to `CoverageError::Io("cannot place the mask beside this photo")` in `write`.

## Test coverage assessment

Strong areas:
- **Banding and grid:** banded vs one-band sums are bit-identical, the grid shapes are tested, and the Gaussian tables are checked against both their formulas and the shader text.
- **Brush:** live stamping equals whole-stroke stamping, the hostile-strokes test bounds time and checks NaN/inf/negative values, and raster sizes are checked across extreme aspects.
- **Coverage PNGs:** tamper, truncated, wrong colour type, wrong bit depth and oversize-header cases are covered, and so is the sweep.
- **Color and effects:** seam-free hue sweep, clamped sliders, finite output for inf sliders, zero frames.
- **Copy preferences:** a round trip plus v1/v2/v3 migration.
- **Fill request:** painted-area math and basic hostile input.

Gaps:
- No NaN/inf test for `BaseLayer::at_uv`, `CaScale` application or `Curve::eval` (NaN input returns NaN here, whereas GPU `vec3<u32>(NaN)` indexes entry 0).
- `curve` has no malformed-number JSON test (DEV-A-02).
- `chroma` is only tested at `k <= 0.002`, with no strong-CA, no downscaled-large-image and no real-photo case (DEV-A-03).
- `fill_request::decode_rgb` has no tests for oversize replies (`> MAX_RESULT_SIDE`), palette or 16-bit or gray+alpha PNGs, or APNG/interlaced. Only truncated and garbage input are covered.
- `copy::merge` completeness (DEV-A-05).
- No test that CPU `ColorOps::apply`/`Effects::apply` equal the GPU outputs here. This presumably lives in `renderer/match_test.rs` (not reviewed).

## Things done well / verified-safe areas

- **base.rs:** the `cell_span`/`overlap` u64 arithmetic matches the WGSL u32 version, and the `h`/`w` divisors are guarded because `Grid` can't have a zero side. There is no unchecked indexing from input: every texel index is clamped or built from the grid. Per-cell sums keep the GPU's summation order.
- **brush.rs:**
  - `Stroke::sanitized` bounds flow, feather, radius, coordinates and point count before the raster sees them.
  - `stamp` is itself safe for NaN/inf radius or points: `f32::max` and the saturating casts avoid `n == 0` loops.
  - The clip, step and budget logic is sound.
  - The cache lock uses `PoisonError::into_inner`.
- **coverage.rs:**
  - Content-addressed, hash-verified, write-once, atomic; size-capped reads (`read_capped`); PNG `Limits`; sides checked before allocation.
  - `Detections` parse is tolerant and keeps unknown targets.
  - The corrupt-read recovery path (`coverage::remove` in `app/coverage.rs`) is wired.
- **curve.rs:** the Fritsch-Carlson tangents are standard, the strictly increasing x invariant is maintained by `insert`/`remove`/`move_point`/`From`, and the `move_point` `±1` arithmetic can't overflow under that invariant. The unused-slot-zero invariant keeps derived `Eq`/`Hash` correct, and the LUT nodes coincide with the 8-bit control points.
- **effects.rs:** integer PCG and lattice match the shader exactly, including the `i32` to `u32` bit reinterpretation; every slider is clamped; zero frames are guarded.
- **fill_request.rs:** crop rect validation, truncated-PNG handling, the decoder byte limit and the `Err` on a short plane (`Request::new` with a truncated image) are all done without panics.
- No rawler references and no `unsafe` in these files.

## Open questions for maintainers

1. Is `CaScale` meant to be clamped anywhere on load? `Adjustments::clamped` does not mention it, and `MAX_SCALE` is local to the measurer (DEV-A-01).
2. Should `CopyItem::Masks` really persist across sessions in `to_pref`? Its docs say "off unless chosen: drawn for one photo's framing", yet unlike `PER_COPY` items it is remembered once chosen. Presumably deliberate (there is a round-trip test), but the rationale for `PER_COPY` applies equally.
3. `color::hue_rgb` clamps negative degrees to 0 (red) rather than wrapping. That is correct for the current callers (0..TAU rim, clamped wheel hue), but the doc says "at HSV hue `deg`". Intended?
4. Is the 2560 px measuring size for CA (`MEASURE_PX`) tuned against real lens data? With `MAX_SHIFT = 3`, 2560 px caps measurable CA at roughly `k` of 0.002-0.004 (DEV-A-03).
