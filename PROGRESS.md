# Progress Log

Run log for `plans/00-overview.md`, driven by the plan-runner loop. One line
per completed or blocked task, so an interrupted run resumes instead of
restarting. Overview tasks are logged as `Plan <file> / Task N`.

Verification gate for every task: `cargo test && cargo build --release`.

- [2026-08-11 21:35] plan-d / Task 1 done: `facequality.rs` Vision FFI (`VNDetectFaceLandmarksRequest` → `RawFace` with normalized eye-contour points) + `face_probe` validation binary. 113 tests pass, release builds. Real-photo landmark validation left to a human (no face fixtures available locally).
