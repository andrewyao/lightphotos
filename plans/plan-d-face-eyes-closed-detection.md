# Plan — Face + eyes-closed detection (Vision framework)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [x] Task 1: Vision FFI spike, validated standalone before any integration — a small test harness/binary running `VNImageRequestHandler` + `VNDetectFaceLandmarksRequest` against known open-eye and closed-eye photos, printing landmarks (files: src/facequality.rs, Cargo.toml) — harness is `cargo run --bin face_probe -- <images>`; FFI round trip covered by unit tests. **Pointing it at real open-eye/closed-eye photos is still a human step** (no face fixtures on this machine); see Task 8.
- [ ] Task 2: Factor the shared `VNImageRequestHandler` setup out of `featureprint.rs` into one place rather than duplicating it (requires Task 1) (files: src/featureprint.rs, src/facequality.rs)
- [ ] Task 3: `pub struct FaceQuality { faces: u32, min_eye_openness: Option<f32> }` plus the pure eye-aspect-ratio openness scoring, unit-tested against fabricated landmark arrays (framework glue separate from testable logic, mirroring the `coregraphics.rs`/`image_decode.rs` split) (files: src/facequality.rs)
- [ ] Task 4: Coarser `VNDetectFaceRectanglesRequest`-only fallback (face present/absent) for when landmarks prove too heavy or unreliable (requires Task 1) (files: src/facequality.rs)
- [ ] Task 5: `face_quality: HashMap<PathBuf, FaceQuality>` cache in `app.rs`, lazy-filled on thumbnail arrival like `sharpness`/`phashes`, gated to burst/duplicate-group members only (not whole-folder) since face detection is heavier than blur scoring (requires Task 3) (files: src/app/thumbs.rs, src/app/mod.rs)
- [ ] Task 6: `combined_score(sharpness: Option<f64>, eyes: Option<EyeState>) -> Option<f64>` in `burst.rs` — a pre-combining function called before `marks_for`, penalizing closed eyes, keeping `compute_marks` signal-agnostic; unit-tested with fabricated inputs (requires Task 3) (files: src/burst.rs)
- [ ] Task 7: "Eyes closed" filter chip/toggle and grid badge, reusing the existing badge-drawing pattern in `draw_grid` (requires Task 5) (files: src/ui/grid.rs, src/ui/toolbar.rs)
- [ ] Task 8: End-to-end — `cargo test`, then `cargo build --release` against a real burst containing a blink; confirm the open-eyed frame is preferred as Best (requires all above) (files: —)

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Reference

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs first in the remaining roadmap, after Plan C; Plan D2 runs right after this one and shares its Vision-framework setup). Auto-tone, the former Plan B, is dropped and not to be implemented.

**What**: Per-photo face detection + eye-openness estimate, feeding into best-of-burst/duplicate-group picking as a tiebreaker/hard filter (never suggest a "Best" frame with closed eyes if an open-eyed sibling exists), plus a standalone "eyes closed" badge/filter — Aftershoot's implied blink-detection culling signal. Eye openness is classic geometry (eye-aspect-ratio over landmark points), no model training.

**Effort/Priority**: Medium-to-large, the riskiest plan in the roadmap — new framework dependency, new crate, and the eye-openness heuristic needs validation against real photos before it can be trusted as a culling signal. Consider running on-demand at Loupe-open before committing to a background pass.

**Critical files**: `src/facequality.rs` (new), `src/coregraphics.rs` (pattern reference, not modified), `src/burst.rs` (scoring integration point), `src/app/`, `src/ui/`, `Cargo.toml`
