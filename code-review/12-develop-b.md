# src/develop (part B) — fill, grade, image_ops, local, mask, mod, parallel, sharpen, side_file

## Scope

All files were read in full, including their test modules. I also read the shader code they mirror (`src/shaders/loupe_common.wgsl`, `shader.wgsl`, `raw_shader.wgsl`) and the callers and neighbours needed to verify contracts: `paths.rs` (write_atomic, read_capped, refuse_linked_sidecar_dir), `sidecar.rs` (touchups, fills, masks loaders), `app/folder_store.rs` (sweep), `app/fill.rs`, `jobs/loader.rs`, `develop/coverage.rs` and `develop/chroma.rs`.

| File | Lines |
|---|---|
| src/develop/fill.rs | 1124 |
| src/develop/grade.rs | 279 |
| src/develop/image_ops.rs | 1821 |
| src/develop/local.rs | 158 |
| src/develop/mask.rs | 1126 |
| src/develop/mod.rs | 2637 |
| src/develop/parallel.rs | 263 |
| src/develop/sharpen.rs | 379 |
| src/develop/side_file.rs | 121 |

## Summary

This is high-quality, defensive code. There is no `unsafe` and there are no non-test `unwrap`/`expect`/`panic`. Input-derived numbers are almost always clamped or sanitized, indexing is guarded by up-front length checks, every file write goes through `write_atomic_in_dir`, and locks tolerate poisoning.

I found no CPU/GPU math divergence for in-range values. I compared `apply_linear_impl`, `exposure_curve`, `tone`, `localTone`, `maskWeight`, `colorGrade`, `localAdjust`, `sharpenAdjust`, `applyFills`, `applyTouchups`, the denoise kernel, and the `GpuAdjust`/`GpuMask`/`GpuFill` layouts line by line against the WGSL. The 448-byte `GpuAdjust` layout and its alignments are consistent with the shader struct.

The real risks are at the edges, not in the core math:
- `TouchUp` is the one sidecar-derived type with no sanitizer, and an infinite radius turns the bake black.
- A generative fill whose patch file is missing or corrupt is silently dropped from exports, thumbnails and scores, unlike masks, which fail the bake.
- Fills are not forward-compatible with newer builds, and the orphan sweep can then delete their patch files.
- A few places on the CPU bake path read unclamped `Adjustments`, so out-of-range sidecar values render differently on export than in the Loupe.

Findings by severity: Critical 0, High 0, Medium 3, Low 6, Info 4.

## Findings

### [DEV-B-01] TouchUp is the only sidecar-derived edit with no sanitizer; infinite radius or opacity renders the whole photo black — Severity: Medium — Confidence: High (mechanism), Medium (exact visual outcome on GPU)
- Location: `src/develop/mod.rs:588` (struct `TouchUp`), `src/sidecar.rs:256-263` (`at_most_max_touchups` only truncates), `src/develop/image_ops.rs:657-679` (`apply_touchups`), the WGSL twin `applyTouchups` at `loupe_common.wgsl:256-280`.
- Evidence: the sidecar loader does `Vec::<TouchUp>::deserialize(..)` then `truncate(MAX_TOUCHUPS)`, and nothing else touches the values. `Crop` has `sanitized()` and `Mask` has `sanitized_within()`, but `TouchUp` has neither. The repo's own tests state that serde reads `1e39` as an f32 infinity. In `apply_touchups`:
  ```
  let radius = t.radius * w.min(h);              // inf
  if distance >= radius { continue }             // finite < inf -> not skipped
  let feather = (radius * clamp).max(1.0);       // inf
  let mask = ((radius - distance) / feather)     // inf/inf = NaN
      .clamp(0.0, 1.0);                          // f32::clamp keeps NaN
  ```
- Impact: a sidecar (or preset or paste) holding `"radius": 1e39` yields a NaN `mask` at every pixel. The bake turns NaN into black (`S::encode(NaN)` gives 0 and `max(0.0)` swallows NaN). Exports, edited thumbnails, scores and the histogram show an all-black photo, while the sidecar still looks fine to the user. `"opacity": 1e39` produces `0 * inf = NaN` wherever the smoothstep is 0. On the GPU, `clamp(NaN, 0, 1)` is implementation-defined in WGSL, so the Loupe can disagree with the export. A very large finite radius or opacity is merely extreme, not NaN.
- Suggested fix: add `TouchUp::sanitized(self) -> Option<TouchUp>`. It should reject non-finite fields, clamp `center` and `source` to `COORD_RANGE`, `radius` to roughly 1e-4..2, `feather` to 0..1, `opacity` to 0..1, and `delta` to -1..1. Apply it in `at_most_max_touchups`, preset import, and the drive/MCP paths. Add a test with `radius: 1e39` that currently bakes black.

### [DEV-B-02] A missing or corrupt fill patch is silently dropped from exports, thumbnails and scores, while the edit signature still claims the fill — Severity: Medium — Confidence: High (behavior), Medium (whether intended)
- Location: `src/develop/fill.rs:401-413` (`FillSet::read` logs with `eprintln!` and returns `None`), `fill.rs:655-670` (`FillInput::resolve` "Never fails"), `src/jobs/loader.rs:132-155` (bake job), `src/app/export.rs:624`, `src/app/fill.rs:535-550` (web equivalent), `src/develop/mod.rs:1062-1070` (signature hashes every fill).
- Evidence: `MaskInput::resolve` returns `Err`, and the loader comment says "the thumbnail is never cached without a mask its signature claims". `FillInput::resolve` has no error path. A fill whose file is gone, truncated or fails its hash is logged to stderr and left out. `edit_signature_with_touchups` hashes `f.bbox()` and `f.chosen()` for every fill in the sidecar, whether or not its patch could be read.
- Impact:
  - A folder copied without `.lightphotos/fills`, or a patch file damaged by a failed sync, gives an export that silently lacks the generative fill. The user is never told: stderr in a GUI app, `console.warn` on web.
  - Edited thumbnails and cached scores are keyed by a signature that includes the fill. When the patch later reappears, the cache never refreshes.
  - The loader's design rule for masks is not applied to fills.
- Suggested fix: make `FillInput::resolve` return a `Result<FillSet, PatchError>` or a `(FillSet, Vec<PatchError>)`. At minimum, have export fail or warn visibly per photo. Have thumbnail and score bakes treat "a listed fill had no readable patch" as a failed (uncached) bake, as masks do. Alternatively, fold the missing-patch state into the signature or `FillSet::key()`.

### [DEV-B-03] Fills are not forward-compatible: unreadable fills and variants beyond three are dropped, and the orphan sweep then deletes their patch files — Severity: Medium — Confidence: Medium
- Location: `src/sidecar.rs:268-279` (`readable_fills` silently drops entries `Fill` rejects), `src/develop/fill.rs:105-122` (`Fill::new` truncates variants to `MAX_VARIANTS` and the prompt), `fill.rs:60-74` (custom `Serialize` writes only four fields), `src/app/folder_store.rs:213-225` (sweep builds `fills` from loaded images and reaps unclaimed files).
- Evidence: masks and detections are deliberately forward-compatible. `MaskList` keeps foreign entries verbatim and `Detections.extra` keeps unknown targets "so the orphan sweep keeps their files". Fills have no equivalent. A fill entry this build cannot parse (a later build's new shape, or a 4th variant) is not carried through. Its patch ids are absent from the sweep's `named` map. Any such file older than `ORPHAN_REAP_AFTER` (1 h) is removed when the folder is opened writable. Entries `Fill` rejects are omitted from the next sidecar write, and the custom `Serialize` drops unknown fields on any rewrite.
- Impact: opening a shared folder with an older build after a newer one can permanently delete generated patch PNGs (which cost a ComfyUI run each), even if the older build never rewrites the sidecar. A variant dropped by `truncate(MAX_VARIANTS)` is swept the same way.
- Suggested fix: apply the `MaskList` approach to fills. Keep unreadable entries as `Box<RawValue>` and write them back. For files an unreadable entry might name, either extract any 16-hex ids from the raw JSON for the sweep's `named` set, or skip the fills sweep when any entry was unreadable.

### [DEV-B-04] The CPU bake reads some sliders from unclamped `Adjustments`, so out-of-range sidecar values export differently from the Loupe — Severity: Low — Confidence: High
- Location: `src/develop/image_ops.rs:387` (`adj.denoise > 0.0`), `image_ops.rs:415` (`develop::denoise_sample(adj, ..)` uses `adj.denoise` raw), `image_ops.rs:389` (`LocalAmounts::of(adj)` unclamped), `src/develop/mod.rs:1219-1236` (`GpuAdjust::for_image` passes raw `adj.sharpening`, `adj.vignette`, `adj.grain`), `mod.rs:843-858` (`clamped()` is applied only inside `Develop::new` and `GpuAdjust::from`). No caller of `bake_edited*` clamps first.
- Evidence: the GPU path gets `denoise` through `GpuAdjust::from(&adj.clamped())`, so 0..100. The bake calls `denoise_sample(adj, ..)` with the raw value: `denoise: 500` gives `sigma_r = 0.02 + 1.5 = 1.52` on export versus 0.32 in the Loupe. With masks present, `global = LocalAmounts::of(adj)` is raw, so `1e30 + w * (-4)` clamps to 5 on the CPU but `5 - 4 = 1` on the GPU.
- Impact: only for hostile or edited sidecar values (the UI clamps), but it breaks the principle that "a render never trusts its values" and the "export == Loupe" guarantee. `Sharpen::new` and `Effects::new` clamp ranges but not NaN. NaN cannot come from JSON, so this only matters for non-JSON sources.
- Suggested fix: in `bake_with_copy`, do `let adj = &adj.clamped();` at the top so every consumer sees one clamped copy. In `GpuAdjust::for_image`, build from the clamped copy too.

### [DEV-B-05] `pixel_linear` and `sample_linear` index without bounds checks; `chroma::measure` reaches them with no length check — Severity: Low — Confidence: Medium
- Location: `src/develop/image_ops.rs:620-643`. `pixel_linear` is documented "the caller keeps x and y inside the image", but it also needs `rgba.len() >= w*h*bpp`. Callers in `src/develop/chroma.rs:85,196` (`radial_edges`, `bilinear`) are reached from `chroma::measure` at `chroma.rs:37`, which checks only `w` and `h` against `MARGIN`. The pub `sample_linear` has the same hole for a zero-sized or short image.
- Evidence: `Source::new`, `downsample_linear`, `to_srgb8`, `fit_long_edge` and `rotate_rgba` all defend against a short or empty buffer, and there is a test named "a_truncated_image_bakes_to_nothing". `chroma::measure(&img)` (called from `app/optics.rs:239` and `web/web_decode.rs:241`) has no such guard.
- Impact: if any decoder path ever yields a `DecodedImage` shorter than `width*height*bpp`, "Remove Chromatic Aberration" panics instead of failing. I could not confirm that `DecodedImage` construction always validates (I could not locate `new_tracked`), so this is a latent rather than demonstrated crash.
- Suggested fix: give `pixel_linear` and `sample_linear` a checked path (`Source::new`-style validation up front, or `get()` returning a default). Add the same length guard to `chroma::measure`, and a truncated-image test.

### [DEV-B-06] `OutputSharpening::apply` is single-threaded and holds three full-size float planes — Severity: Low — Confidence: High
- Location: `src/develop/sharpen.rs:200-257`.
- Evidence: `luma: Vec<f32>` plus the horizontal pass result plus the final `blur` are all live at once (`pass(&pass(&luma, true), false)`), which is 12 bytes per pixel. Each pass does `i % w` and `i / w` per sample and walks 5 taps, serially. The crate already has `parallel::for_each_light_row`.
- Impact: a 45 MP export needs about 540 MB of extra transient memory on top of the output buffer, and about 1-2 s of single-thread work. On wasm32 the memory is the tight constraint.
- Suggested fix: do the horizontal pass in row bands via `for_each_light_row`, then the vertical pass. Write the horizontal result into a reused buffer so only two planes are live. Precompute `1.0 / total`.

### [DEV-B-07] Fill patches are decoded and hash-verified from disk by every bake job; the atlas can reach 256 MB — Severity: Low — Confidence: Medium
- Location: `src/develop/fill.rs:405-413` (`FillSet::read`), `fill.rs:338-348` (`read`), `fill.rs:449-487` (`atlas`).
- Evidence: each native bake job (`FillInput::OnDisk`) calls `read` per fill: a capped file read, a PNG decode, and an FNV-1a pass over up to 16 MiB. There is no cross-job cache, and every edited-thumbnail re-bake while the user drags a slider repeats it. `atlas()` allocates `width*height*4` at up to 8192 x 8192, which is 256 MB for 16 full-size patches, then uploads it.
- Impact: 16 fills of 2048 px means about 1 s of avoidable decode per thumbnail bake. Typical patches are far smaller, so this is an edge case.
- Suggested fix: keep a small `Arc<Patch>` cache keyed by id (ids are content hashes, so it never goes stale). Consider capping the atlas to what the surface can show.

### [DEV-B-08] Orphan sweeping misses some files — Severity: Low — Confidence: Medium
- Location: `src/develop/side_file.rs:31-38` (`parse_file_name` requires UTF-8), `side_file.rs:66-69` (non-UTF-8 entries are skipped), `src/app/folder_store.rs:213-225`, `src/web/web_folder_store_fs.rs:425-440` (web writes coverage and patches but nothing sweeps them; only `web_thumb_cache::sweep_orphans` is wired).
- Evidence and impact:
  - Side files for photos with non-UTF-8 names (possible on Linux) are never reaped.
  - On the web build, discarded fill variants (`keep_chosen` drops up to two per fill) and replaced coverages accumulate forever in the user's folder, up to about 20 MB each.
  - `write()` also skips an existing identical file without refreshing its mtime. A deterministic regenerate that matches an old orphan reuses a file that already looks 1 h old to another instance's sweep.
- Suggested fix: sweep by `OsStr` bytes, or percent-encode names. Add a web sweep for the `masks` and `fills` dirs. Touch the mtime in `write()` when the file already exists.

### [DEV-B-09] Test gaps — Severity: Low — Confidence: High
- Details:
  - `moving_any_one_edit_changes_the_signature` (`mod.rs:1864`) checks 4 of about 25 edit kinds. Nothing asserts that sharpening, vignette, grain, texture/clarity/dehaze, touchup geometry, masks, strokes or fills each move the signature. A forgotten hash field means silent stale thumbnails. A loop over `SLIDERS` (with the documented exceptions: mixer when mono, gray mix when not mono, sub-sliders at amount 0) would catch it.
  - `edit_signature_stable` can only fail on nondeterminism.
  - No test feeds `TouchUp` non-finite values (DEV-B-01).
  - `fit_long_edge` and `OutputSharpening::apply` have no 16-bit (`u16`) tests, no tall-image tests, and no 1xN or Nx1 edge tests.
  - `side_file::sweep_orphans`'s stale `.tmp` branch (via `atomic_tmp_target`) is not exercised by any test; the fill and coverage sweep tests only use `.png` files.
  - `parallel` has no wasm test, so the `run_with_helpers` pool path is unexercised.
  - No test for `crop_bounds` with `w == 0`; it is safe only because `Source::new` returns first.

### [DEV-B-10] Minor robustness and maintainability — Severity: Info — Confidence: High
- `resample_bilinear_u8` (`image_ops.rs:723`) and `orient_mask` (`image_ops.rs:760`) multiply `u32 * u32` (`(sw * sh) as usize`, `(dw * dh) as usize`). A 100k x 100k mask would wrap or panic in debug. Inputs today are bounded by `MAX_COVERAGE_SIDE` and Vision output, so this is latent. Prefer `usize` math with `checked_mul`.
- `Sections::contains/toggled` (`mod.rs:127-134`) use `1 << section as u16` on a `u16` bitset. There are 11 sections, so it is safe, but nothing statically asserts the enum stays under 16 variants.
- `GpuAdjust::_pad0` doubles as the touch-up count (documented), and `for_image` applies `.min(MAX_MASKS)` and `.min(MAX_FILLS)` but not `.min(MAX_TOUCHUPS)`. All current callers are bounded by the sidecar loader and `apply_touchups`, so it is safe, but the asymmetry is easy to trip over.
- `Adjustments::clamped()` does not touch `chromatic_aberration` scales. They are protected downstream (`bilinear` uses saturating casts, tested), but a scale of -1.0 collapses red or blue to a single texel.
- `deserialize_crop` silently turns an invalid crop into "no crop" (documented).
- Duplicate helpers: `is_zero` is defined in both `mod.rs:599` and `mask.rs:161`.
- `neutralize_gray`, `apply_raw_preview_boost`, `Straighten::largest_crop` and `denoise_sample` do not guard NaN inputs. They are fed finite, UI-derived values today.

## Test coverage assessment

Strong:
- `mod.rs`: slider-name round-trip, out-of-range slider/straighten/crop clamping, exposure-curve properties (monotonicity, continuity at 0, finiteness), RAW look-knot parity with the shader text, denoise behaviour, Straighten geometry.
- `image_ops.rs`: bit-exact sRGB LUT checks, truncated-image bake, CA fringe correction, mask baking under a crop, fill drawing/feathering/format coverage, the missing-patch bake.
- `parallel.rs`: each row and chunk runs exactly once, a scratch is created per thread, zero `row_len`.
- `fill.rs` and `mask.rs`: malformed JSON, PNG validation (truncated, gray, RGB, too wide, wrong hash), atlas packing, sanitizer behaviour.
- CPU/GPU parity is further covered by `renderer/match_test.rs`, which is outside this unit.

Weak or missing: see DEV-B-09. Tests for sanitization and hostile values are good for crop, masks and fills, but absent for touch-ups.

## Things done well / verified-safe areas

- No `unsafe`; no non-test `unwrap`, `expect` or `panic!`. Locks use `unwrap_or_else(PoisonError::into_inner)`. The thread scope tolerates spawn failure.
- All file writes use `write_atomic_in_dir` (atomic tmp+rename, `create_new`, symlinked-dir refusal). Reads use `read_capped`, which refuses non-regular files and over-size files before opening. The orphan sweep refuses a symlinked `.lightphotos` and only deletes names matching the strict `<photo>.<16 hex>.png` pattern, with an age guard.
- Patch decode is bounded on three counts: file size (20 MiB), PNG decoder byte limit, and dimension caps. It is hash-verified against its id and rejects non 8-bit RGBA.
- `Fill::new`, `Crop::sanitized`, `Mask::sanitized_within` and `Stroke::sanitized` cap counts, clamp ranges, reject non-finite values and cut strings at char boundaries. The brush-point budget is shared across masks.
- `bilinear` and the touch-up/fill `texel` closures clamp indices with saturating casts, so NaN, inf and huge coordinates land on edge texels (tested).
- `rgba_len` and `Source::new` use checked arithmetic. `rotate_rgba` and `fit_long_edge` reject wrong-sized buffers.
- Division by zero is avoided in `Sharpen::new` (`short <= 0`), `falloff` (feather 0), the denoise `wsum` (center weight is 1), and `Radial` radii (clamped to at least 1e-4).
- `GpuAdjust` is 112 floats (448 bytes), 16-byte aligned. Field order and vec2/array alignment match the WGSL `Adjust` struct. `GpuMask` and `GpuFill` match their WGSL structs. Coverage packing offsets are byte offsets consistent with `coverageTexel`.
- Parity points checked against the shader: WB gains, exposure rational curve, tone/contrast/shadows/highlights formulas, mask weight (aspect scaling, falloff, invert, zero-length gradient), local tone clamps, Color Grading (OkLab weights), sharpen taps and weights, denoise weights, fill bilinear and clamp-in-patch, and touch-up feather/opacity.

## Open questions for maintainers

1. Is silent omission of an unreadable fill patch (DEV-B-02) deliberate? The doc says "the bake still succeeds", but the signature then claims a fill that was never drawn, and exports gain no user-visible error.
2. Should fills follow the `MaskList` forward-compatibility model (keep unknown entries and variants verbatim), given that the sweep deletes unclaimed patch files after an hour (DEV-B-03)?
3. Where should out-of-range `Adjustments` be normalized: once at the bake entry (preferred, DEV-B-04) or at each consumer? Today it is a mix.
4. Does every `DecodedImage` constructor guarantee `rgba.len() >= w*h*bpp`? If so, `pixel_linear` should say so in its contract; if not, DEV-B-05 is live.
5. Should the web build sweep `.lightphotos/masks` and `.lightphotos/fills` (DEV-B-08)? Native does on folder open.
6. Is the alpha handling of `Srgb8` images intentional? `unpremul_to_linear` un-premultiplies, `bake` writes opaque output, and `px_to_srgb8` returns the raw premultiplied bytes, so translucent PNGs export over black and `to_srgb8` disagrees with the bake.
