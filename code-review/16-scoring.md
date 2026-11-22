# src/scoring — quality, Vision, face, sky, segmentation scoring

## Scope

All of `src/scoring/`, read line by line:

| File | Lines |
|---|---|
| `detect.rs` | 502 |
| `facequality.rs` | 866 |
| `judge.rs` | 172 |
| `mod.rs` | 30 |
| `quality.rs` | 728 |
| `seetaface.rs` | 285 |
| `segmentation.rs` | 473 |
| `sky.rs` | 893 |
| `vision.rs` | 73 |
| **Total** | **4022** |

Cross-checked callers in `jobs/score.rs`, `worker_pool.rs`, `app/loupe.rs`, `app/masks.rs`, `sidecar.rs`, `decode/image_decode/mod.rs` and `develop/image_ops.rs::orient_mask`. I also ran one throwaway Swift experiment in the scratchpad (not in the repo) to settle SCORE-01.

## Summary

The module is generally careful. Non-test code has no `unwrap`/`expect`/`panic`. Input-derived sizes are checked (`sky_coverage`, `seetaface::detect_gray`, `gray_of`, `analysis_luma`). NaN is handled deliberately in `score`, `face_bounds` and `raw_faces`. `unsafe` is small and mostly commented. The scoring math is pure and well unit-tested.

The biggest risks are correctness bugs at the Vision boundary, which the fabricated-input tests cannot catch:
1. Subject masks are rotated twice for any photo with a non-trivial EXIF orientation (verified empirically).
2. Eye-landmark coordinates are probably box-normalized, but the code treats them as image-normalized. That skews the blink metric by the image aspect ratio.
3. The long-lived `FacePool` workers call Vision with no autorelease pool, unlike `judge` and `detect`.

Findings: 1 High, 3 Medium, 7 Low, 3 Info.

## Findings

### [SCORE-01] Subject/person mask is rotated a second time for every EXIF-oriented photo — Severity: High — Confidence: High
- Location: `src/scoring/segmentation.rs:150-151` (doc), `:176-178` (`mask.oriented(orientation_of(path))`); `src/decode/image_decode/mod.rs:855-857` (doc of `orientation_of`); `src/scoring/vision.rs:7-10` (the contradicting note).
- Evidence:
  - `segment_subject` says "Vision ignores EXIF orientation, so we rotate the mask ourselves" and calls `Mask::oriented(orientation_of(path))`.
  - `vision.rs` says the opposite. Given a file, Vision applies EXIF orientation. Commit a724cb1 fixed this for face boxes: "which is what led face boxes to be rotated a second time".
  - I tested the real API. A 400x200 JPEG with `kCGImagePropertyOrientation = 1` gave a `VNGeneratePersonSegmentationRequest` mask of 2016x1512 (landscape). The same pixels with orientation 6 gave 1512x2016 (portrait).
  - So Vision already returns the mask in upright space, and `oriented()` rotates it again.
- Impact:
  - For any photo with orientation 2-8 (portrait shots from phones and cameras), the Loupe "Show Selection" overlay is rotated or mirrored relative to the image.
  - A Subject mask's coverage (`DetectInput::detect_subject`, then `Coverage::fit`) is wrong.
  - The wrong coverage is persisted as a content-addressed PNG under `.lightphotos/masks/`. "Photos keep the coverage they stored; Retry adopts" means these stay wrong until the user re-detects.
  - `sky_detect::detect` subtracts `subject.resampled(p.width, p.height)`, so the sky is cut by a mask of the wrong shape. An orientation-6 photo stretches a landscape mask onto a portrait frame.
  - Orientation 3 (180 degrees) and mirrored orientations are also wrong.
- Suggested fix:
  - Drop the `.oriented(...)` call and the `orientation_of` dependency in `segment_subject`. Correct the comments in `segmentation.rs` and `orientation_of`'s doc.
  - Add a regression test that writes an orientation-6 JPEG (landscape stored). `test_support::write_jpeg` has no orientation option, so extend it. Assert that `segment()` returns a mask with `height > width`.
  - Ideally also paint a distinctive blob and assert that its position follows the upright image.
  - Re-run detection for stored Subject coverages, or version the coverage id, so masks produced by the buggy build are not trusted.

### [SCORE-02] Eye landmark points are very likely normalized to the face box, but treated as image-normalized — Severity: Medium — Confidence: Medium
- Location: `src/scoring/facequality.rs:22-24` and `:112-118` (`region_points`), `:240-280` (`eye_openness`), `:411-413` (`aspect_wh`), `judge.rs:103`.
- Evidence:
  - The doc says landmark points are "0..1 over the whole image, not the face box". `eye_openness` then divides y by the image aspect (`aspect_wh = image w/h`) to put y in x units.
  - Apple's `VNFaceLandmarkRegion2D.normalizedPoints` are relative to the face's `boundingBox`. That is why `VNImagePointForFaceLandmarkPoint(point, faceBoundingBox, w, h)` exists. The objc2 header comment doesn't say either way.
  - The tests (`openness_is_corrected_for_the_image_aspect_ratio`) use fabricated image-normalized points, so nothing exercises the real frame.
  - `CLOSED_EYE_RATIO`'s own comment says it is "Not yet checked against real photos".
- Impact:
  - If the points are box-normalized and Vision's box is about square in pixels, the correct y-scale is about 1, not `1/aspect`.
  - In a 3:2 landscape the openness is understated by 1.5x. Open eyes at 0.25-0.45 become 0.17-0.30, and a squint near 0.2 becomes 0.13 and is flagged "Closed".
  - In a 2:3 portrait it is overstated by 1.5x. A blink at 0.10-0.15 passes as Open.
  - The effect feeds the eyes-closed filter and the 70% `EyesClosed` score penalty. It is aspect-dependent and invisible in the unit tests.
- Suggested fix:
  - Verify with `face_probe` on a real portrait. Compare eye points to `bounding_box`. If the eye points span a large part of [0,1] while the face box is small, they are box-normalized.
  - If so, convert to image space with the box (`x_img = bx + bw*x`, `y_img = by + bh*y`) before the aspect correction, or use the box's pixel aspect `(bh*H)/(bw*W)` as the y-scale.
  - Invalidate `signals.json` entries when `min_eye_openness` semantics change, and fix the doc comment.
  - Add a test with a real face fixture, or at least a captured Vision observation.

### [SCORE-03] `FacePool` workers call Vision with no autorelease pool — Severity: Medium — Confidence: Medium
- Location: `src/scoring/facequality.rs:40-48` (`detect_faces`), `:452-466` (`with_workers`, setup `|| {}`); `src/worker_pool.rs` (no pool around `run`); `src/scoring/segmentation.rs:154-227`.
- Evidence:
  - `judge.rs:82-84` and `detect.rs:71-72` wrap Vision in `autoreleasepool` because "Worker threads have no autorelease pool of their own, and Vision autoreleases freely". `decode/image_decode/raw_filter.rs:91` does the same.
  - `analyze_job` → `detect_upright` → `detect_faces` → `vision::perform(Source::File)` is the only long-lived Vision caller with no pool.
  - A grep for `autorelease` shows only those three sites.
- Impact:
  - `FacePool` (1-2 permanent threads) runs a full-resolution Vision decode and landmark pass for every grouped photo in a folder.
  - Whatever Vision autoreleases (the NSError path, result arrays, possibly decoded buffers) accumulates until the thread exits, which is the app's lifetime. Memory grows with the number of photos scanned.
  - The magnitude is unmeasured, but the maintainers saw it in the sibling paths.
  - Segmentation threads are per-request and are cleaned at thread exit, so they are lower risk. `profile.rs` calls them on a long-lived thread.
- Suggested fix:
  - Wrap the body of `detect_faces` (request creation, `perform`, `faces_from`) in `objc2::rc::autoreleasepool`. It returns plain `Vec<RawFace>`, so nothing escapes the pool.
  - Do the same in `segment_subject`/`segment`, or put one pool in `WorkerPool`'s setup/run path on macOS.
  - Better still, add a helper in `vision.rs` so every caller gets this for free.

### [SCORE-04] Any detected face, however small or low-confidence, can mark a photo "eyes closed" — Severity: Medium — Confidence: Medium
- Location: `src/scoring/facequality.rs:283-301` (`face_quality`), `:28-36` (`confidence` field); `judge.rs:102-104`; `quality.rs:182-188`.
- Evidence:
  - `RawFace.confidence` is populated (Vision, and SeetaFace via `score/20`) but nothing outside `face_probe` reads it. `face_quality` takes the minimum openness over every face's eyes, with no face-size or confidence filter.
  - `eye_openness` needs only 3 points and is a ratio of a tiny contour.
- Impact:
  - A background bystander or a tiny face a few tens of pixels wide (the score runs on a 1024 px preview) has coarse, noisy landmarks. An openness under 0.15 there marks the whole frame `EyeState::Closed`.
  - That costs the photo up to 70% of its score (`EyesClosed` floor 0.3) and puts it in the eyes-closed filter, even though the subject's eyes are open.
  - It is a plausible false-positive source for culling, and the stated design ("one blinker marks a group shot") hides it.
- Suggested fix:
  - Ignore faces below a confidence threshold and below a minimum box size in `face_quality`. For example, drop faces with `bounding_box` width or height under about 4% of the image, or under 40 px at the analysis scale.
  - Or weight the openness by face prominence.
  - If `confidence` stays unused, remove the field or document it as reserved.

### [SCORE-05] Vision failure silently degrades the score and the result is stored — Severity: Low — Confidence: Medium
- Location: `src/scoring/judge.rs:16` (`vision_signals(...).unwrap_or_default()`), `:96-100`; `src/jobs/score.rs:72`.
- Evidence:
  - The faces and aesthetics requests run in one `performRequests`. If either fails (no Neural Engine/GPU, as the test comments note for CI VMs; an aesthetics-only failure), the whole `Err` is swallowed. Both signals are dropped, even if the face request had populated results.
  - The score then comes back `Basis::TechnicalOnly`.
- Impact:
  - `ScorePool::new` returns `None` off macOS precisely because "a technical-only score would mislead". On macOS the same degrade happens invisibly on a Vision failure.
  - The result is persisted in the sidecar tagged `technical_only`, so the user sees a different scale (flat 50 base) with no warning or retry.
- Suggested fix:
  - Return the Vision error up to `score_photo` (so the job reports a failure), or surface `vision_failed` on `QualityScore`.
  - Alternatively, run the two requests separately so a failed aesthetics request does not discard face data.

### [SCORE-06] `StoredScore` has no forward/lenient deserialization — one unknown variant fails the whole sidecar read — Severity: Low — Confidence: Medium
- Location: `src/scoring/quality.rs:53-94` (`Basis`, `Penalty`, `QualityScore`); `src/sidecar.rs:119` and `:247-253` (`score: Option<StoredScore>`, `#[serde(flatten)]`).
- Evidence:
  - `Penalty` and `Basis` are closed enums without `#[serde(other)]`. The `score` field has no `deserialize_with`, unlike `touchups`, `fills`, `rotation` and `location`, which got lenient readers.
  - A sidecar written by a newer build with a new `Penalty` (or an out-of-range `value`) makes `serde_json::from_slice::<ImageRecord>` fail (`sidecar.rs:547`).
- Impact:
  - The score is derived, disposable data, but a bad one makes the user's edits unreadable in an older or downgraded build (`read_record` returns `Err`).
  - This is the same failure the lenient helpers were added to prevent.
- Suggested fix:
  - Add a `deserialize_with` helper for `score` that maps any parse error to `None` (a stale score is merely recomputed).
  - Add `#[serde(other)] Unknown` variants if the UI can render them.

### [SCORE-07] `analysis_luma` block-averages by whole factors, so measurements jump at 1024/1025 px — Severity: Low — Confidence: High
- Location: `src/scoring/quality.rs:13-15`, `:283-290`.
- Evidence: `block = ceil(long_side / 1024)`, so 1024 px is measured at full resolution but 1025-2048 px is averaged 2x2 down to 512-1024 px (a 1025 px image is measured at 512). The doc says "a score means the same thing whatever size the preview came in".
- Impact:
  - 2x2 averaging halves the noise sigma and changes focus, and `clip_hi`/`clip_lo` count averaged pixels.
  - `jobs/score.rs::preview` accepts any decode result with `long >= 1024`. If `decode_at_size` ever returns an unscaled preview (for example a 1620 px embedded JPEG), that photo is scored at a different effective resolution from its siblings.
  - Baked crops or rotations can also change the size.
- Suggested fix: Resample to a fixed long side with fractional-area (or bilinear/area) averaging, or at least test that `decode_at_size` always returns exactly `ANALYSIS_PX`. Add a test that scores a 1024 px image and its 1025 px twin and asserts they are close.

### [SCORE-08] A frame too small to analyze scores as "dark and soft" instead of "unknown" — Severity: Low — Confidence: High
- Location: `src/scoring/quality.rs:260-263` and `:289-292` (`Technical::default()` on `None`); `judge.rs:15`.
- Evidence: a degenerate or short input returns `Technical::default()` (all zeros). `score()` treats that as measured values: focus 0 gives `SoftFocus`, mean_luma 0 gives `Dark` and `ShadowsCrushed` is not triggered but `Dark` is.
- Impact: an edited preview after an extreme crop (under 3 cells after the block division) or a very thin panorama (for example 1024x2) gets a score of about 10 with the penalties "Soft focus" and "Dark". The tests assert `technical()` returns zeros but never what `score` does with them.
- Suggested fix: Have `technical` return `Option<Technical>`/`Result`, and have `judge` return `Err("too small to score")`.

### [SCORE-09] "Breakdown adds up" holds only when no unlisted penalty fired — Severity: Low — Confidence: High
- Location: `src/scoring/quality.rs:109`, `:217-230`; the test at `:427-449`; `src/ui/grid.rs:636-650`.
- Evidence: a penalty with factor in [0.95, 1) lowers `value` but is never listed (`LISTED_BELOW`). The shown "base - listed deductions" then does not equal `value`. The test passes only because it uses penalties well past the threshold.
- Impact: the tooltip can show base 80, deductions totalling 30, and a score of 48. This is cosmetic but confusing.
- Suggested fix: Either skip applying factors above `LISTED_BELOW` (treat them as zero penalty), or list them, or fold the remainder into an "other" row. Add a test with a 0.97-factor penalty.

### [SCORE-10] On macOS 12-13 a photo with no person reports a failure instead of "no subject" — Severity: Low — Confidence: Medium
- Location: `src/scoring/segmentation.rs:166-179`, `:209-213`; `src/scoring/detect.rs:266`.
- Evidence: when person segmentation finds no one, `segment_foreground(path)?` runs. On macOS under 14, `require_class(..."14.0")` returns `Err`, which `?` propagates.
- Impact: on macOS 12/13 (supported, since LSMinimumSystemVersion is 11) every non-portrait photo yields `DetectError::Failed("...needs macOS 14.0")`. The result is not persisted, so Retry re-runs it. "Show Selection" is silently empty (`Err(_) => current_selection = None`), but Subject detection shows an error where "nothing found" is the true answer.
- Suggested fix: When the foreground request is unavailable, return `Ok(None)` from the fallback path (the person request ran fine and found nobody). Reserve `Err` for the case where neither request could run.

### [SCORE-11] Mask buffer stride is not validated against width — Severity: Low — Confidence: Low
- Location: `src/scoring/segmentation.rs:232-300`.
- Evidence: `copy_u8_rows`/`copy_f32_rows` read `width` elements per row from `base + y*stride` with no `stride >= width * bytes_per_pixel` check. The SAFETY comment relies on "bytes_per_row as the buffer reports them".
- Impact: Vision's own buffers satisfy this. It is defense in depth only, since a bad buffer would read past the allocation. CLAUDE.md treats external numbers as hostile.
- Suggested fix: Return `Err` if `stride < width * size_of::<T>()`. Use `checked_mul` for `width * height`.

### [SCORE-12] Non-UTF-8 paths cannot be passed to Vision — Severity: Low — Confidence: High
- Location: `src/scoring/vision.rs:35`.
- Evidence: `path.to_str().ok_or("path is not valid UTF-8")?`.
- Impact: a photo whose filename has bytes that are not valid UTF-8 never gets faces, selection or detect results, with a generic error. This is rare on APFS, which normalizes to UTF-8, but possible on network or foreign volumes.
- Suggested fix: Build the URL from the file-system representation (`NSURL::fileURLWithFileSystemRepresentation` / `CFURLCreateFromFileSystemRepresentation`) using the `OsStr` bytes.

### [SCORE-13] Latent coupling and edge cases in `sky.rs` — Severity: Info — Confidence: Medium
- Location: `src/scoring/sky.rs:442` (`l.checked_sub(2)`), `:331` (`first = max(labels) + 1`), `:340` (`low_row = gh - gh/5`), `:131-133` vs `quality.rs` luma coefficients.
- Evidence and notes:
  - `add_clouds` hard-codes "label 1 is the sky, clouds start at 2". That is true only because `grow_sky` calls `add_clouds` only when `any_blue`, which guarantees a non-empty sky. If the sky were ever empty, `first` would be 1 and the cloud indices would shift by one.
  - For grids under 5 rows, `gh/5 == 0`, so `low_row == gh` and `clear_of_bottom` is always true. Panoramas under about 5 cells tall treat every pale region as "above the bottom".
  - `sky.rs` uses Rec.601 luma, `quality.rs` Rec.709.
  - The guided filter allocates roughly 15 full-size f32 buffers. At `MAX_PIXELS` (16M) that is about 1 GB, but callers cap at 1024 px, so this is only a note.
- Suggested fix: Return the first label from `label_regions` instead of hard-coding 2. Add a unit test for a very flat panorama.

### [SCORE-14] Misleading or contradictory documentation — Severity: Info — Confidence: High
- Location: `src/scoring/facequality.rs:101-106` (`# Safety` on a safe private fn); `src/scoring/segmentation.rs:151` and `src/decode/image_decode/mod.rs:855-857` (contradict `vision.rs`); `src/scoring/facequality.rs:22-24` (see SCORE-02).
- Impact: the contradictory Vision-orientation comments are how SCORE-01 survived. A `# Safety` section on a safe function suggests a caller obligation that does not exist.
- Suggested fix: Fix the three comments together with SCORE-01 and SCORE-02, and keep one authoritative statement of Vision's orientation and coordinate conventions in `vision.rs`.

### [SCORE-15] The blink threshold is unvalidated, and `face_probe` feeds the aspect ratio from stored size — Severity: Info — Confidence: Medium
- Location: `src/scoring/facequality.rs:122-125`; `src/bin/face_probe.rs:73` (`pixel_size(path)`).
- Evidence: `CLOSED_EYE_RATIO` says "Not yet checked against real photos". The probe takes `aspect_wh` from the stored pixel size, which for orientation 5-8 is inverted relative to the upright frame that Vision's landmarks use. The app path correctly uses `upright_size`.
- Impact: tuning the threshold with the probe on portrait-oriented files would give wrong numbers.
- Suggested fix: Use `upright_size` in the probe (make it `pub`), or call `facequality::analyze`.

## Test coverage assessment

- **Strong:**
  - `quality.rs` has good behavior tests: blur lowers focus, over-exposure raises clipping, grain is not read as focus, size independence at 2x, an NaN/inf sweep, and a JSON round trip.
  - `sky.rs` has scene tests plus hostile-input and tiny-image tests.
  - `seetaface.rs` tests the box math, the edge clamping and the NaN score.
- **Weak:**
  - Vision-boundary behavior has no test with real or realistic data.
  - There is no EXIF-rotated segmentation test, which is exactly SCORE-01.
  - There is no real-face landmark test, which is SCORE-02. The aspect-ratio test uses fabricated image-normalized points, so it can only confirm the arithmetic.
  - There is no test of the `FacePool` autorelease behavior, or of `judge` when Vision fails.
  - The Vision tests skip on VMs without a model, so CI covers little of this on the mac side.
- **Gaps:**
  - `score()` with unlisted small penalties (SCORE-09).
  - `technical()` degenerate output feeding `score()` (SCORE-08).
  - `segment_subject` on a foreground-unavailable runtime (SCORE-10).
  - Face filtering by size and confidence (SCORE-04).
  - A sidecar with an unknown `Penalty` (SCORE-06).
- `a_vision_pass_over_pixels_finds_no_face_in_a_flat_frame` swallows `Err` into `eprintln!`. On a machine where Vision always fails it passes without exercising anything.

## Things done well / verified-safe areas

- `sky_coverage` (`sky.rs:78-115`) validates sizes before allocating with `saturating_mul`/`checked_mul`, caps at `MAX_PIXELS`, and returns zeros rather than panicking. The `dilate`/`box_mean` sliding-window index arithmetic holds at the edges (I traced it for `r > width`).
- `quality::score` cannot underflow (`points(before) - points(value)` is monotone, since every factor is at most 1 and at least the floor). NaN and inf measures are filtered, and `points` clamps before the `as u8`.
- `seetaface` is safe against bad input: length mismatch gives `Err`, tiny images give an empty result, the model is loaded once per thread, and the box clamps handle NaN.
- `vision::require_class` correctly guards `ClassType::new` for classes newer than the 11.0 floor. In `judge.rs` and `detect.rs` the autoreleasepool closures return plain data, and `?` inside the closure is fine.
- `region_points` null/empty checks are sound. `pixel_buffer_to_mask` always unlocks the buffer before propagating the error.
- The non-mac cfg arms (`vision_signals`, `segment`, `detect_subject`, `detect_faces`) are consistent. rawler is not referenced from `scoring/` (the wasm `detect_upright` goes through `decode::rawler`, which is non-mac).

## Open questions for maintainers

1. Was Vision's landmark coordinate frame (face-box versus image) ever checked against a real portrait with `face_probe`? The doc says "whole image", but Apple's API implies the face box.
2. Is the person/foreground mask orientation fix intended to invalidate stored Subject coverages, or should existing `.lightphotos/masks/` PNGs from rotated photos be re-detected?
3. Should a failed Vision pass in `judge` fail the score job, or is the silent technical-only fallback intended on macOS?
4. Is there a policy on minimum face size and confidence for the eyes-closed filter?
