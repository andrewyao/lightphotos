# Plan — Dehaze slider

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [ ] Task 1: Add `dehaze: f32` to `Adjustments`, `GpuAdjust`, and `edit_signature_with_touchups`'s tone array, same −100..=100 / 0 = identity conventions as the existing sliders (files: src/develop.rs)
- [ ] Task 2: Implement the per-pixel formula in the fragment shader — shadow-weighted contrast stretch plus a saturation boost proportional to the dehaze amount (requires Task 1) (files: src/shader.wgsl)
- [ ] Task 3: Mirror the identical formula in `apply_linear` so histogram and shader agree (the same "two mirrors must agree" discipline every existing slider follows) (requires Task 1) (files: src/develop.rs)
- [ ] Task 4: Unit test the CPU formula against a synthetic low-contrast/washed-out image — dehaze should measurably increase contrast and saturation (requires Task 3) (files: src/develop.rs)
- [ ] Task 5: Wire the new field through the shared tone-pipeline path used by export/thumbnail bake — per-pixel like the rest, so no separate export-time work beyond the wiring (requires Task 1) (files: src/image_ops.rs)
- [ ] Task 6: Dehaze slider in the Develop panel (requires Task 1) (files: src/ui/develop_panel.rs)
- [ ] Task 7: End-to-end — `cargo build --release`, run on a real hazy/foggy photo, confirm Loupe and exported JPEG match (requires all above) (files: —)

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

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group; it doesn't share Plan E's blur-pass risk, so it can land before or alongside it).

**What**: Reduces (or, at negative values, adds) atmospheric haze — Lightroom's Dehaze slider. −100..=100, 0 = identity.

**Simpler than Clarity, architecturally**: unlike Clarity, a good approximation of dehaze does *not* need a blur/local pass — a common practical approximation is a per-pixel contrast-stretch weighted toward shadows (haze mostly lifts blacks and desaturates), combined with a saturation boost proportional to the dehaze amount. This fits the existing per-pixel-formula shader pattern exactly like `contrast`/`blacks` do, just a more involved formula (not a true dark-channel-prior removal, which *would* need multi-pixel/neighborhood analysis — explicitly not doing that here, matching the heuristic-first constraint from the rest of this roadmap).

**Effort/Priority**: Quick-to-medium — same shape as adding any existing tone slider (three-mirror sync), no new renderer architecture.

**Critical files**: `src/develop.rs`, `src/shader.wgsl`, `src/image_ops.rs`, `src/ui/develop_panel.rs`
