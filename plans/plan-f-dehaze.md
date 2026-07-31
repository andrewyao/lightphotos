# Plan F — Dehaze slider

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group; doesn't share Plan E's blur-pass risk, so can land before or alongside it).

**What**: Reduces (or, negative value, adds) atmospheric haze — Lightroom's Dehaze slider. −100..=100, 0 = identity.

**Simpler than Clarity, architecturally**: unlike Clarity, a good approximation of dehaze does *not* need a blur/local pass — a common practical approximation is a per-pixel contrast-stretch weighted toward shadows (haze mostly lifts blacks and desaturates), combined with a saturation boost proportional to the dehaze amount. This fits the existing per-pixel-formula shader pattern exactly like `contrast`/`blacks` do, just a more involved formula (not a true dark-channel-prior removal, which *would* need multi-pixel/neighborhood analysis — explicitly not doing that here, matching the heuristic-first constraint from the rest of this roadmap).

**Implementation**:
- `develop.rs`: add `dehaze: f32` to `Adjustments`/`GpuAdjust`/`edit_signature_with_touchups`'s tone array, same conventions as existing sliders.
- `shader.wgsl` + `apply_linear`: implement the same per-pixel formula in both (this is the same "two mirrors must agree" discipline every existing slider already follows — no new architecture, just a new formula in the two places that already exist).
- `image_ops.rs`: reachable via the shared tone-pipeline path already used by export/thumbnail bake — since it's per-pixel like the rest, no separate export-time work needed beyond wiring the new field through.
- `ui.rs`: new slider in Develop panel.

**Effort/Priority**: Quick-to-medium — same shape as adding any existing tone slider (three-mirror sync), no new renderer architecture. Good candidate to land before or alongside Plan E since it doesn't share Clarity's blur-pass risk.

**Critical files**: `src/develop.rs`, `src/shader.wgsl`, `src/image_ops.rs`, `src/ui.rs`

**Verification**: unit test the CPU formula in `apply_linear`'s module against a synthetic low-contrast/washed-out image (dehaze should measurably increase contrast and saturation). End-to-end: `cargo build --release`, run on a real hazy/foggy photo, confirm Loupe and exported JPEG match.
