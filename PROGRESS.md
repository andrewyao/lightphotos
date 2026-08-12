# Progress Log

Run log for `plans/00-overview.md`, driven by the plan-runner loop. One line
per completed or blocked task, so an interrupted run resumes instead of
restarting. Overview tasks are logged as `Plan <file> / Task N`.

Verification gate for every task: `cargo test && cargo build --release`.

- [2026-08-11 21:35] plan-d / Task 1 done: `facequality.rs` Vision FFI (`VNDetectFaceLandmarksRequest` → `RawFace` with normalized eye-contour points) + `face_probe` validation binary. 113 tests pass, release builds. Real-photo landmark validation left to a human (no face fixtures available locally).
- [2026-08-12 00:05] plan-d2 / Task 3 BLOCKED: needs human visual check — `seg_probe` harness built and exercised against a real image (mask + tinted overlay written, pipeline proven), but no photo with a real subject is available here. Recorded finding: person segmentation returns a confident imaginary subject on abstract imagery, and no coverage statistic catches it.
- [2026-08-12 00:00] plan-d2 / Task 2 done: `image_ops::resample_bilinear_u8` (pixel-center mapping, 6 tests) + `orient_mask` (EXIF table, 2 tests) + `Mask::resized`/`oriented`. 140 tests pass, release builds.
- [2026-08-11 23:50] plan-d2 / Task 1 done: `src/segmentation.rs` — person segmentation with a foreground-instance fallback, `CVPixelBuffer` → packed `Mask` handling both single-channel formats and row padding. 130 tests pass, release builds.
- [2026-08-11 22:05] plan-d / Task 8 BLOCKED: needs human visual check — `cargo test` (126) and `cargo build --release` green, app launches clean, but validating the blink heuristic needs real photographs of faces, which this machine has none of. Recipe in the plan file. Tasks 1-7 merged to `main` anyway so the rest of the roadmap (Plan D2 shares the Vision setup) isn't held hostage to it.
- [2026-08-11 22:02] plan-d / Task 7 done: eyes-closed grid badge (bottom-right, `EYES_BADGE` blue) + toolbar filter chip stacking on the star filter, gated on Bursts/Duplicates. Fixed the toolbar keyboard-focus index mapping (stale since the Duplicates button landed). 126 tests pass, release builds, app launches clean.
- [2026-08-11 21:53] plan-d / Task 6 done: `burst::combined_score` (blink = hard demotion into `(-1, 0)`, monotonic in sharpness within that band) + 5 unit tests; both `recompute_burst_marks` and `recompute_dup_marks` now route through the new `App::culling_score`. 126 tests pass, release builds.
- [2026-08-11 21:47] plan-d / Task 5 done: `facequality::FacePool` + `face_quality`/`face_pending`/`face_failed` caches in `App`, `request_face_quality`/`poll_face_quality` driven per frame from `main.rs` and on both grouping toggles. Gated to burst members + duplicate groups of 2+. 121 tests pass, release builds.
- [2026-08-11 21:43] plan-d / Task 4 done: `detect_face_rects` (`VNDetectFaceRectanglesRequest`) fallback returning `RawFace`es with empty eye contours, so it drops into `face_quality` unchanged. 121 tests pass, release builds.
- [2026-08-11 21:41] plan-d / Task 3 done: pure eye-aspect-ratio scoring (`eye_openness`, `face_quality`, `FaceQuality`, `EyeState`, `CLOSED_EYE_RATIO = 0.15`), 7 new unit tests over fabricated contours — ordering-independent, roll-invariant, aspect-corrected via new `image_decode::pixel_size`. 120 tests pass, release builds.
- [2026-08-11 21:36] plan-d / Task 2 done: shared `VNImageRequestHandler` setup extracted to `src/vision.rs::perform_request`; `featureprint.rs` and `facequality.rs` now only build their request and read its results. 113 tests pass, release builds.
