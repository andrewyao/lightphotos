# Plan H — Tone Curve (full point-based spline)

See `00-overview.md` for shared context and architecture patterns. **This plan is deferred/unscheduled** — parked, no fixed slot in the current execution order, picked up separately later.

**What**: A draggable-control-point tone curve (RGB composite + optional per-channel R/G/B curves), matching Lightroom's main Tone Curve tab and its `ToneCurvePV2012`/`ToneCurvePV2012Red/Green/Blue` XMP lists directly — chosen over a simpler parametric curve specifically for future Presets/LR-import fidelity.

**Architecturally**: the curve itself (a monotonic spline through user-placed control points, e.g. Catmull-Rom or a monotonic cubic Hermite to avoid overshoot/banding) is evaluated once per changed control-point-set into a 256-entry lookup table (LUT), not per-pixel-recomputed — this is the standard technique and keeps the shader cost identical to a single texture sample regardless of curve complexity.

**Implementation**:
- `develop.rs`: `pub struct ToneCurve { pub points: Vec<(f32, f32)> }` (normalized 0..1 both axes, default = two points `(0,0)` and `(1,1)` = identity line) — one for composite, optionally three more for R/G/B channels (`pub struct ToneCurves { pub rgb: ToneCurve, pub red: Option<ToneCurve>, pub green: Option<ToneCurve>, pub blue: Option<ToneCurve> }`). Added to `Adjustments`. New pure function `fn bake_lut(curve: &ToneCurve) -> [f32; 256]` (or `[u8; 256]` if 8-bit precision is acceptable) — this is the natural `#[cfg(test)]`-friendly pure module, could live in `develop.rs` itself or a new `src/tonecurve.rs` mirroring the `sharpness.rs`/`burst.rs` pure-module convention.
- GPU: the baked LUT needs to reach the shader as a small 1D texture (256×1, or 4×256×1 for composite+RGB) uploaded whenever the curve changes (not every frame) — this is new resource-management work in `renderer.rs` (a texture upload path alongside the existing loupe-image texture), plus a `textureSample`/`textureLoad` call in `shader.wgsl` instead of a formula.
- `apply_linear`: evaluate the same baked LUT (just an array index/lerp, not a live spline re-evaluation) so histogram and shader stay pixel-identical.
- `edit_signature_with_touchups`: hash the curve's control points (quantized) so the thumbnail cache invalidates correctly on curve edits.
- `ui.rs`: the biggest new UI surface in this roadmap — a draggable-point curve widget (egui custom painting: draw the curve line, draggable point handles, click-to-add/right-click-to-remove points, clamped to stay monotonic in x). Check egui's existing examples/whether a suitable widget exists before hand-rolling one from scratch.

**Effort/Priority**: Large — the biggest single item across the whole roadmap. Curve math is small and testable; the custom draggable-curve UI widget and the LUT-texture upload path are both new, nontrivial surfaces. Recommend prototyping the UI widget in isolation before wiring it to the full develop/export/histogram pipeline.

**Critical files**: `src/develop.rs`, `src/tonecurve.rs` (new, optional split), `src/renderer.rs`, `src/shader.wgsl`, `src/ui.rs`

**Verification**: unit test `bake_lut` against known curve shapes (identity line → LUT is the identity ramp; a simple S-curve → verify monotonicity and expected midpoint behavior). End-to-end: `cargo build --release`, drag curve points on a real photo, confirm Loupe live-render matches exported JPEG, and confirm the thumbnail cache correctly invalidates when only curve points change (no other slider touched).
