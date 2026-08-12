# Plan — Tone Curve (full point-based spline)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

**Status: DEFERRED / unscheduled** — parked, no fixed slot in the current execution order, picked up separately later. Do not start these tasks without confirming the plan has been un-parked.

- [ ] Task 1: Prototype the draggable-point curve widget in isolation first — egui custom painting: curve line, draggable point handles, click-to-add / right-click-to-remove, clamped monotonic in x; check egui's existing examples for a suitable widget before hand-rolling (files: src/ui/develop_panel.rs)
- [ ] Task 2: `pub struct ToneCurve { pub points: Vec<(f32, f32)> }` (normalized 0..1 both axes, default = `(0,0)`+`(1,1)` identity line) and `ToneCurves { rgb, red, green, blue }`, added to `Adjustments` (files: src/develop.rs, src/tonecurve.rs)
- [ ] Task 3: Pure `fn bake_lut(curve: &ToneCurve) -> [f32; 256]` via a monotonic spline (Catmull-Rom or monotonic cubic Hermite, to avoid overshoot/banding), in its own `#[cfg(test)]`-friendly module mirroring the `sharpness.rs`/`burst.rs` convention (requires Task 2) (files: src/tonecurve.rs)
- [ ] Task 4: Unit test `bake_lut` against known shapes — identity line gives the identity ramp; a simple S-curve is monotonic with the expected midpoint behavior (requires Task 3) (files: src/tonecurve.rs)
- [ ] Task 5: LUT texture upload path in the renderer — a small 1D texture (256×1, or 4×256×1 for composite+RGB) uploaded whenever the curve changes, not every frame; new resource management alongside the existing loupe-image texture (requires Task 3) (files: src/renderer.rs)
- [ ] Task 6: Shader samples the LUT via `textureSample`/`textureLoad` instead of computing a formula, keeping shader cost constant regardless of curve complexity (requires Task 5) (files: src/shader.wgsl)
- [ ] Task 7: `apply_linear` evaluates the same baked LUT (array index/lerp, not a live spline re-evaluation) so histogram and shader stay pixel-identical (requires Task 3) (files: src/develop.rs)
- [ ] Task 8: Hash the curve's quantized control points into `edit_signature_with_touchups` so the thumbnail cache invalidates on curve edits (requires Task 2) (files: src/develop.rs)
- [ ] Task 9: Wire the prototyped curve widget into the Develop panel against the real `Adjustments` state (requires Tasks 1, 2) (files: src/ui/develop_panel.rs)
- [ ] Task 10: End-to-end — `cargo build --release`, drag curve points on a real photo, confirm Loupe live-render matches the exported JPEG, and confirm the thumbnail cache invalidates when only curve points change with no other slider touched (requires all above) (files: —)

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

See `00-overview.md` for shared context and architecture patterns.

**What**: A draggable-control-point tone curve (RGB composite + optional per-channel R/G/B curves), matching Lightroom's main Tone Curve tab and its `ToneCurvePV2012`/`ToneCurvePV2012Red/Green/Blue` XMP lists directly — chosen over a simpler parametric curve specifically for future Presets/LR-import fidelity.

**Architecturally**: the curve itself (a monotonic spline through user-placed control points) is evaluated once per changed control-point-set into a 256-entry lookup table, not per-pixel-recomputed — the standard technique, keeping the shader cost identical to a single texture sample regardless of curve complexity.

**Effort/Priority**: Large — the biggest single item across the whole roadmap. Curve math is small and testable; the custom draggable-curve UI widget and the LUT-texture upload path are both new, nontrivial surfaces. Prototype the UI widget in isolation before wiring it to the full develop/export/histogram pipeline.

**Critical files**: `src/develop.rs`, `src/tonecurve.rs` (new, optional split), `src/renderer.rs`, `src/shader.wgsl`, `src/ui/develop_panel.rs`
