# Plan — Face + eyes-closed detection (Vision framework)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [x] Task 1: Vision FFI spike, validated standalone before any integration — a small test harness/binary running `VNImageRequestHandler` + `VNDetectFaceLandmarksRequest` against known open-eye and closed-eye photos, printing landmarks (files: src/facequality.rs, Cargo.toml) — harness is `cargo run --bin face_probe -- <images>`; FFI round trip covered by unit tests. **Pointing it at real open-eye/closed-eye photos is still a human step** (no face fixtures on this machine); see Task 8.
- [x] Task 2: Factor the shared `VNImageRequestHandler` setup out of `featureprint.rs` into one place rather than duplicating it (requires Task 1) (files: src/featureprint.rs, src/facequality.rs) — landed as a new `src/vision.rs` (`perform_request`) instead of inside either caller, so Plan D2 can reuse it too
- [x] Task 3: `pub struct FaceQuality { faces: u32, min_eye_openness: Option<f32> }` plus the pure eye-aspect-ratio openness scoring, unit-tested against fabricated landmark arrays (framework glue separate from testable logic, mirroring the `coregraphics.rs`/`image_decode.rs` split) (files: src/facequality.rs) — also `EyeState`, `CLOSED_EYE_RATIO`, and `analyze()`. The ratio needed an image-aspect correction (Vision normalizes x and y against different denominators), so `image_decode::pixel_size` was added to supply it.
- [x] Task 4: Coarser `VNDetectFaceRectanglesRequest`-only fallback (face present/absent) for when landmarks prove too heavy or unreliable (requires Task 1) (files: src/facequality.rs)
- [x] Task 5: `face_quality: HashMap<PathBuf, FaceQuality>` cache in `app.rs`, lazy-filled on thumbnail arrival like `sharpness`/`phashes`, gated to burst/duplicate-group members only (not whole-folder) since face detection is heavier than blur scoring (requires Task 3) (files: src/app/thumbs.rs, src/app/mod.rs)
- [x] Task 6: `combined_score(sharpness: Option<f64>, eyes: Option<EyeState>) -> Option<f64>` in `burst.rs` — a pre-combining function called before `marks_for`, penalizing closed eyes, keeping `compute_marks` signal-agnostic; unit-tested with fabricated inputs (requires Task 3) (files: src/burst.rs)
- [x] Task 7: "Eyes closed" filter chip/toggle and grid badge, reusing the existing badge-drawing pattern in `draw_grid` (requires Task 5) (files: src/ui/grid.rs, src/ui/toolbar.rs) — chip is gated on Bursts/Duplicates being on, since Task 5's cost gate means nothing else fills the cache it reads. Also fixed the pre-existing off-by-one in the toolbar keyboard-focus mapping (Duplicates was never reachable).
- [ ] Task 8: End-to-end — `cargo test`, then `cargo build --release` against a real burst containing a blink; confirm the open-eyed frame is preferred as Best (requires all above) (files: —) — **BLOCKED: needs human visual check.** `cargo test` (126 pass) and `cargo build --release` are green, and the app launches and opens a folder clean, but no photographs of faces exist on this machine, so the half that matters is untested. To close it:
  1. `cargo run --bin face_probe -- /path/to/burst/*.jpg` and read the printed openness per eye. Confirm blinking frames land clearly below `CLOSED_EYE_RATIO` (0.15) and open ones clearly above; retune the constant in `src/facequality.rs` if the two populations sit elsewhere.
  2. Open that folder in the app, turn on **Bursts**, and confirm the open-eyed frame takes the Best badge over a sharper blink, that blinking frames get the bottom-right eye badge, and that the **Eyes closed** chip narrows the grid to them.

  Everything the heuristic rests on — the threshold, whether Vision's landmarks are trustworthy on real faces, whether profile shots false-positive — is unvalidated until this runs. Until then the pipeline is proven to *work*, not proven to be *right*.

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
