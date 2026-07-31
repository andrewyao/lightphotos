# Plan E — Clarity slider

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group, after Plan D2).

**What**: Local (midtone) contrast enhancement — the classic "punch without changing exposure" slider. −100..=100, 0 = identity, same convention as the other tone sliders.

**Architecturally different from the other sliders** — this is the one thing to get right before starting: every existing tone slider (`exposure`, `contrast`, `highlights`, etc.) is a pure per-pixel formula — the fragment shader reads one texel and computes its new value with no knowledge of neighboring pixels. Clarity is a *local-contrast* operation: it needs a blurred (low-frequency) version of the image to compare each pixel against (unsharp-mask style: `pixel + amount * (pixel - blurred(pixel))`), which existing sliders never require. This means Clarity cannot just add a field to `GpuAdjust` and a formula to `shader.wgsl`/`apply_linear` the way `contrast` did — it needs an actual blur pass.

**Implementation**:
- `src/renderer.rs`: add a downsample+blur render pass (a small Gaussian or box blur over a few mip levels of the loupe texture, or a single fixed-radius blur scaled relative to image size) producing a low-frequency texture the main fragment shader can sample alongside the source texel. This is the first multi-pass addition to the renderer — read `renderer.rs`'s existing single-pass pipeline setup before starting, since bind group layout will need a second texture binding.
- `src/shader.wgsl`: fragment shader samples both the source texel and the blurred texture, applies `clarity` as `pixel + amount * (pixel - blurred)`, gated by the new `GpuAdjust::clarity` field.
- `develop.rs`: add `clarity: f32` to `Adjustments` (same `TONE_RANGE`, `is_zero` skip convention as the others), to `GpuAdjust` (mind the 16-byte alignment comment — pad to a multiple of 4 fields), to `edit_signature_with_touchups`'s tone array.
- `apply_linear` (CPU histogram mirror): needs its own blur step to stay faithful to the shader — likely a simple box-blur over the downscaled histogram-source buffer (same downscale helper `sharpness.rs`/`image_ops.rs` use), since the histogram doesn't need pixel-perfect Gaussian match, just the same qualitative curve.
- `image_ops.rs`/export: the baked full-resolution export path needs the same blur+combine step applied at full res (or a good-enough downsampled blur upsampled back) — this is the one place performance matters most since export isn't real-time.
- `ui.rs`: new slider in the Develop panel tone group, alongside Contrast.

**Effort/Priority**: Medium-large — the slider/UI/serde parts are trivial and match existing patterns exactly; the blur pass is genuinely new renderer architecture and the part to prototype/de-risk first.

**Critical files**: `src/renderer.rs`, `src/shader.wgsl`, `src/develop.rs`, `src/image_ops.rs`, `src/ui.rs`

**Verification**: unit test the CPU blur+combine math in `apply_linear`'s style against a synthetic step-edge image (clarity should visibly punch up the transition without moving the flat regions' brightness). End-to-end: `cargo build --release`, drag Clarity on a real photo with fine texture (foliage, fabric), compare Loupe live-render vs exported JPEG for visual parity — this is the one feature where live-render/export mismatch is most likely to sneak in given the extra pass.
