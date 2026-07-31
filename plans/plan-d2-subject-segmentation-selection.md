# Plan D2 — Subject/foreground segmentation as a selection (exploratory)

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs right after Plan D, sharing its Vision-framework setup).

**What**: Given the current photo, generate a foreground/subject mask via Vision (`VNGeneratePersonSegmentationRequest`, falling back to `VNGenerateForegroundInstanceMaskRequest` for non-person subjects) and surface it as a **selection** the user can see and toggle — deliberately scoped down from a full masked-editing pipeline. Goal is to understand how the selection mechanic feels/behaves (what the mask looks like on real photos, how it should be previewed, whether foreground-vs-background is the right split) before committing to wiring it into tone-adjustment blending. No `Adjustments`/`MaskTarget`/shader-blend work in this plan — that's deliberately deferred to a later plan once the selection UX is validated here.

**Implementation**:
- New `src/segmentation.rs` (Vision FFI glue, same pattern as Plan D's `facequality.rs` — if both plans land, factor the shared `VNImageRequestHandler` construction into one helper rather than duplicating it): wraps `VNGeneratePersonSegmentationRequest`/`VNGenerateForegroundInstanceMaskRequest`, returns a single-channel alpha buffer at the request's native resolution (typically lower than the working image — needs upsampling via simple bilinear resize to match display size).
- Not persisted: this is derived/computed data, not a user edit — recompute on demand (e.g. when Loupe opens a photo, or via an explicit "Compute Selection" action), not stored in the catalog. No `develop.rs`/`Adjustments` changes at all in this plan.
- `app.rs`: a transient `current_selection_mask: Option<Vec<u8>>` (or similar), computed for the currently-open Loupe photo only — not a per-folder background pass like `sharpness`/`phashes`, since the point here is to inspect one photo's selection at a time.
- `ui.rs`/`renderer.rs`: render the mask as a visual overlay on the Loupe image (standard masking-UI convention — e.g. red/green tint or marching-ants-style outline over the foreground region) so the user can actually see and judge the selection quality. This likely means uploading the mask as a texture and compositing it in `shader.wgsl` as an overlay pass — simpler than a full masked-blend idea since here it's just visualization, not affecting the rendered tone at all.
- A simple toggle: "Show Selection" on/off, and maybe an invert (foreground ↔ background) — no editing hooked up yet.

**Effort/Priority**: Small-to-medium — smaller than a full masking feature specifically because it stops at visualization; no shader-blend integration, no `Adjustments` model changes, no persistence. Right-sized as a first step to validate the concept before a later plan wires masks into actual tone-adjustment blending.

**Critical files**: `src/segmentation.rs` (new), `src/app.rs`, `src/ui.rs`, `src/renderer.rs`, `src/shader.wgsl` (overlay-only), `Cargo.toml`

**Verification**: a standalone check of `segmentation.rs` against a handful of real portrait/subject photos, confirming the mask roughly matches the subject outline. End-to-end: `cargo build --release`, open a real portrait in Loupe, toggle "Show Selection," confirm the overlay visually tracks the subject; try a non-person photo to see the foreground-instance fallback behavior.
