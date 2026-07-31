# Plan D — Face + eyes-closed detection (Vision framework)

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs third, after Plans B and C; Plan D2 runs right after this one and shares its Vision-framework setup).

**What**: Per-photo face detection + eye-openness estimate, feeding into best-of-burst/duplicate-group picking as a tiebreaker/hard filter (never suggest a "Best" frame with closed eyes if an open-eyed sibling exists), plus a standalone "eyes closed" badge/filter — Aftershoot's implied blink-detection culling signal.

**Implementation**:
- New `src/facequality.rs` (Vision FFI glue + pure scoring, mirroring the `coregraphics.rs`/`image_decode.rs` split — framework glue separate from testable logic): wraps `VNImageRequestHandler` + `VNDetectFaceLandmarksRequest` (eye landmark points → eye-aspect-ratio-style openness heuristic, classic geometry, no model training) with a coarser `VNDetectFaceRectanglesRequest`-only fallback (face present/absent only) if landmarks prove too heavy or unreliable.
  - Output: `pub struct FaceQuality { faces: u32, min_eye_openness: Option<f32> }` (or a simpler `EyeState` enum) — the scoring math is unit-testable against fabricated landmark arrays even though the FFI call itself isn't (same as `coregraphics.rs` having no tests while `sharpness.rs`'s math does).
- `app.rs`: `face_quality: HashMap<PathBuf, FaceQuality>` cache, same lazy-fill-on-thumbnail-arrival pattern as `sharpness`/`phashes`.
- `burst.rs`: keep `compute_marks` signal-agnostic — add a pre-combining function (`combined_score(sharpness: Option<f64>, eyes: Option<EyeState>) -> Option<f64>`, penalizing closed eyes) called before `marks_for`, rather than growing `compute_marks`'s own signature.
- `ui.rs`: "eyes closed" filter chip/toggle and a grid badge, reusing the existing badge-drawing pattern in `draw_grid`.
- Performance: gate to burst/duplicate-group members only initially (not whole-folder) since face detection is heavier than blur scoring; consider running on-demand at Loupe-open before committing to a background pass.

**Effort/Priority**: Medium-to-large, the riskiest plan in the roadmap — new framework dependency, new crate, and the eye-openness heuristic needs validation against real photos before it can be trusted as a culling signal.

**Critical files**: `src/facequality.rs` (new), `src/coregraphics.rs` (pattern reference, not modified), `src/burst.rs` (scoring integration point), `src/app.rs`, `src/ui.rs`, `Cargo.toml`

**Verification**: the Vision FFI spike validated standalone (a small test harness/binary running detection against known open/closed-eye photos) before any `compute_marks` integration. Once integrated: unit tests on the pure `combined_score` function with fabricated inputs, then `cargo build --release` end-to-end against a real burst containing a blink, confirming the open-eyed frame is preferred.
