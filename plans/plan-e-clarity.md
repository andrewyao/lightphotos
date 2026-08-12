# Plan — Clarity slider

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [ ] Task 1: Prototype/de-risk the blur pass first — read `renderer.rs`'s existing single-pass pipeline setup, then add a downsample+blur render pass (small Gaussian/box blur over a few mip levels, or one fixed-radius blur scaled to image size) producing a low-frequency texture, with a second texture binding in the bind group layout (files: src/renderer.rs)
- [ ] Task 2: Fragment shader samples both the source texel and the blurred texture and applies `pixel + amount * (pixel - blurred)`, gated by a new `GpuAdjust::clarity` field (requires Task 1) (files: src/shader.wgsl)
- [ ] Task 3: Add `clarity: f32` to `Adjustments` (same `TONE_RANGE` and `is_zero` skip convention as the other sliders), to `GpuAdjust` (mind the 16-byte alignment comment — pad to a multiple of 4 fields), and to `edit_signature_with_touchups`'s tone array (files: src/develop.rs)
- [ ] Task 4: CPU mirror in `apply_linear` — its own box-blur over the downscaled histogram-source buffer (reusing the `image_ops.rs` downscale helper), faithful in qualitative curve rather than pixel-exact Gaussian match (requires Task 3) (files: src/develop.rs, src/image_ops.rs)
- [ ] Task 5: Unit test the CPU blur+combine math against a synthetic step-edge image — clarity should visibly punch up the transition without moving flat regions' brightness (requires Task 4) (files: src/develop.rs)
- [ ] Task 6: Baked full-resolution export path gets the same blur+combine step (at full res, or a good-enough downsampled blur upsampled back) — the one place performance matters most since export isn't real-time (requires Task 3) (files: src/image_ops.rs, src/export.rs)
- [ ] Task 7: Clarity slider in the Develop panel tone group, alongside Contrast (requires Task 3) (files: src/ui/develop_panel.rs)
- [ ] Task 8: End-to-end — `cargo build --release`, drag Clarity on a real photo with fine texture (foliage, fabric), compare Loupe live-render vs exported JPEG for visual parity; live-render/export mismatch is most likely here given the extra pass (requires all above) (files: —)

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

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group, after Plan D2).

**What**: Local (midtone) contrast enhancement — the classic "punch without changing exposure" slider. −100..=100, 0 = identity, same convention as the other tone sliders.

**Architecturally different from the other sliders** — this is the one thing to get right before starting: every existing tone slider (`exposure`, `contrast`, `highlights`, etc.) is a pure per-pixel formula — the fragment shader reads one texel and computes its new value with no knowledge of neighboring pixels. Clarity is a *local-contrast* operation: it needs a blurred (low-frequency) version of the image to compare each pixel against (unsharp-mask style: `pixel + amount * (pixel - blurred(pixel))`), which existing sliders never require. This means Clarity cannot just add a field to `GpuAdjust` and a formula to `shader.wgsl`/`apply_linear` the way `contrast` did — it needs an actual blur pass, the first multi-pass addition to the renderer.

**Effort/Priority**: Medium-large — the slider/UI/serde parts are trivial and match existing patterns exactly; the blur pass is genuinely new renderer architecture and the part to prototype/de-risk first.

**Critical files**: `src/renderer.rs`, `src/shader.wgsl`, `src/develop.rs`, `src/image_ops.rs`, `src/ui/develop_panel.rs`
