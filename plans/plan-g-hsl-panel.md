# Plan — HSL panel (8-band hue/saturation/luminance)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [ ] Task 1: New `pub struct Hsl { pub bands: [HslBand; 8] }` (or `[f32; 24]`) added to `Adjustments` as `hsl: Hsl`, default all-zero = identity, with its own identity check folded into `Adjustments::is_identity()` (files: src/develop.rs)
- [ ] Task 2: Grow `GpuAdjust` by the 24 floats (already a multiple of 4, so 16-byte alignment holds; 24×f32 = 96 bytes is trivial for a uniform buffer — fall back to a separate small uniform/storage buffer only if the single-buffer approach gets unwieldy) (requires Task 1) (files: src/develop.rs, src/renderer.rs)
- [ ] Task 3: Pure band-weighting function — raised-cosine or triangular falloff between the 8 band centers spaced 45° apart — plus RGB↔HSL conversion, unit-tested in isolation (a pure-red pixel lands ~100% in the Red band, ~0% in Green/Blue) (files: src/develop.rs)
- [ ] Task 4: Shader implementation — RGB→HSL, band-membership weighting, per-band H/S/L deltas, HSL→RGB back; the most math-dense addition in the roadmap but entirely per-pixel/stateless (requires Tasks 2, 3) (files: src/shader.wgsl)
- [ ] Task 5: Mirror the same conversion + band math in `apply_linear`, matching the shader exactly (requires Task 3) (files: src/develop.rs)
- [ ] Task 6: Extend `edit_signature_with_touchups`'s tone-quantization loop to include the 24 HSL values so the thumbnail cache invalidates on HSL edits (requires Task 1) (files: src/develop.rs)
- [ ] Task 7: Synthetic-pixel adjustment unit tests — pushing one band's saturation moves only pixels of that hue (requires Tasks 3, 5) (files: src/develop.rs)
- [ ] Task 8: HSL panel in Develop — 8 rows × 3 sliders, likely tabbed by Hue/Saturation/Luminance the way Lightroom's panel is, in its own collapsible section given the size (requires Task 1) (files: src/ui/develop_panel.rs)
- [ ] Task 9: End-to-end — `cargo build --release`, push a single band's saturation to an extreme on a photo with that color present; confirm only that color shifts and the band boundary shows no hard seam (requires all above) (files: —)

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

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group; independent of Clarity/Dehaze, can be built in parallel).

**What**: Per-hue-band adjustment — 8 color bands (red/orange/yellow/green/aqua/blue/purple/magenta, matching Lightroom's convention), each with independent Hue/Saturation/Luminance sliders (24 values total). Maps directly to Lightroom's `HueAdjustmentRed` etc. XMP fields for future Presets import coverage.

**Architecturally**: a per-pixel operation like the existing tone sliders (no blur pass needed) — convert the pixel's RGB to HSL, determine which band(s) its hue falls into (with smooth blending between adjacent bands so there's no hard edge artifact at band boundaries), apply that band's H/S/L offsets, convert back to RGB. Bigger than existing sliders mainly in *data volume* (24 floats vs 1-2 per slider) and *UI surface* (8 rows × 3 sliders), not in fundamental shader architecture.

**Effort/Priority**: Medium — no new renderer architecture (unlike Clarity), but the largest single data/UI surface of the sliders in this group.

**Critical files**: `src/develop.rs`, `src/shader.wgsl`, `src/ui/develop_panel.rs`
