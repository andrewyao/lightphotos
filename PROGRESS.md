# Progress Log

Run log for `plans/00-overview.md`, driven by the plan-runner loop. One line
per completed or blocked task, so an interrupted run resumes instead of
restarting. Overview tasks are logged as `Plan <file> / Task N`.

Verification gate for every task: `cargo test && cargo build --release`.

- [2026-08-11 21:35] plan-d / Task 1 done: `facequality.rs` Vision FFI (`VNDetectFaceLandmarksRequest` → `RawFace` with normalized eye-contour points) + `face_probe` validation binary. 113 tests pass, release builds. Real-photo landmark validation left to a human (no face fixtures available locally).
- [2026-08-11 21:41] plan-d / Task 3 done: pure eye-aspect-ratio scoring (`eye_openness`, `face_quality`, `FaceQuality`, `EyeState`, `CLOSED_EYE_RATIO = 0.15`), 7 new unit tests over fabricated contours — ordering-independent, roll-invariant, aspect-corrected via new `image_decode::pixel_size`. 120 tests pass, release builds.
- [2026-08-11 21:36] plan-d / Task 2 done: shared `VNImageRequestHandler` setup extracted to `src/vision.rs::perform_request`; `featureprint.rs` and `facequality.rs` now only build their request and read its results. 113 tests pass, release builds.
