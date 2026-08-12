# Plan — Subject/foreground segmentation as a selection (exploratory)

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

- [x] Task 1: New `src/segmentation.rs` — wraps `VNGeneratePersonSegmentationRequest` with a `VNGenerateForegroundInstanceMaskRequest` fallback for non-person subjects, returning a single-channel alpha buffer at the request's native resolution; reuses the shared `VNImageRequestHandler` helper from Plan D rather than duplicating it (requires plan-d Task 2) (files: src/segmentation.rs, Cargo.toml)
- [x] Task 2: Bilinear upsample of the native-resolution mask to display size, unit-tested on a synthetic mask (files: src/segmentation.rs, src/image_ops.rs) — `image_ops::resample_bilinear_u8` + `Mask::resized`. Also added `image_ops::orient_mask`, since Vision works in stored orientation and the mask has to land in display orientation to line up with anything.
- [ ] Task 3: Standalone check of `segmentation.rs` against a handful of real portrait/subject photos, confirming the mask roughly matches the subject outline (requires Task 1) (files: src/segmentation.rs) — **BLOCKED: needs human visual check.** Harness built and exercised end to end: `cargo run --bin seg_probe -- <photos>` writes `<stem>.mask.jpg` (raw mask) and `<stem>.overlay.jpg` (photo with the foreground tinted) into the current directory. Verified against a real image, so the plumbing — request → CVPixelBuffer → packed mask → EXIF reorientation → bilinear resize to display size → composite — is proven. What's missing is a photo with an actual subject in it.

  **Finding worth knowing before that run**: on Apple's abstract `iMac Blue` wallpaper, which contains no person whatsoever, `VNGeneratePersonSegmentationRequest` returned a confident, well-formed, entirely imaginary person (13.2% mean / 13.4% solid coverage). No coverage statistic separates that from a real subject, so `EMPTY_COVERAGE` only catches the *nothing* case and person-free photos may never reach the foreground-instance fallback. Whether that matters on real photographs is exactly what this task is for — check a non-person subject (Task 7 asks for one too) and see whether the person path hijacks it.
- [x] Task 4: Transient `current_selection_mask: Option<Vec<u8>>` in `app.rs`, computed for the currently-open Loupe photo only — not persisted (derived data, not a user edit), not a per-folder background pass (requires Task 1) (files: src/app/loupe.rs, src/app/mod.rs)
- [x] Task 5: Upload the mask as a texture and composite it in an overlay pass — red/green tint or marching-ants-style outline over the foreground region; visualization only, does not affect rendered tone (requires Task 4) (files: src/renderer.rs, src/shader.wgsl)
- [x] Task 6: "Show Selection" on/off toggle, plus an invert (foreground ↔ background); no editing hooked up (requires Task 5) (files: src/ui/loupe.rs, src/ui/mod.rs) — in the Loupe info bar, not the Develop panel: this is a way of looking at the photo, not an edit. The toggle's label doubles as status (`Selection…` while computing, `No subject` when Vision found nothing).
- [ ] Task 7: End-to-end — `cargo build --release`, open a real portrait in Loupe, toggle "Show Selection", confirm the overlay visually tracks the subject; try a non-person photo to see the foreground-instance fallback behavior (requires all above) (files: —) — **BLOCKED: needs human visual check.** `cargo test` (140) and `cargo build --release` are green, the app launches into the Loupe clean, and the overlay pipeline passes wgpu/naga validation at runtime — but whether the tint actually lands on the subject is not something this run can see. To close it: open a portrait, hit **Show Selection** in the info bar, and check that the green tint follows the subject through zoom, pan, rotate and crop; then hit **Invert** and confirm it swaps to the background; then open a non-person photo (a pet, a plate of food) and see whether the foreground-instance fallback engages at all, given the finding recorded under Task 3.

  Since this plan is exploratory, the questions worth answering while looking are the ones it exists to ask: is a soft tint the right visualization, or does this want marching ants? Is foreground-vs-background the right split, or is per-instance selection the thing you actually reach for? Is accurate-quality segmentation fast enough on demand, or does the Loupe need to feel it coming?

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

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs right after Plan D, sharing its Vision-framework setup).

**What**: Given the current photo, generate a foreground/subject mask via Vision and surface it as a **selection** the user can see and toggle — deliberately scoped down from a full masked-editing pipeline. Goal is to understand how the selection mechanic feels/behaves (what the mask looks like on real photos, how it should be previewed, whether foreground-vs-background is the right split) before committing to wiring it into tone-adjustment blending.

**Explicitly out of scope**: no `Adjustments`/`MaskTarget`/shader-blend work, no `develop.rs` changes, no persistence. That's deferred to a later plan once the selection UX is validated here.

**Effort/Priority**: Small-to-medium — smaller than a full masking feature specifically because it stops at visualization. Right-sized as a first step to validate the concept before a later plan wires masks into actual tone-adjustment blending.

**Critical files**: `src/segmentation.rs` (new), `src/app/`, `src/ui/`, `src/renderer.rs`, `src/shader.wgsl` (overlay-only), `Cargo.toml`
