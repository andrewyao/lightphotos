# Plan G — HSL panel (8-band hue/saturation/luminance)

See `00-overview.md` for shared context, architecture patterns, and execution order (this plan runs in the E/F/G group; independent of Clarity/Dehaze, can be built in parallel).

**What**: Per-hue-band adjustment — 8 color bands (red/orange/yellow/green/aqua/blue/purple/magenta, matching Lightroom's convention), each with independent Hue/Saturation/Luminance sliders (24 values total). Maps directly to Lightroom's `HueAdjustmentRed` etc. XMP fields for future Presets import coverage.

**Architecturally**: a per-pixel operation like the existing tone sliders (no blur pass needed) — convert the pixel's RGB to HSL, determine which band(s) its hue falls into (with smooth blending between adjacent bands so there's no hard edge artifact at band boundaries), apply that band's H/S/L offsets, convert back to RGB. Bigger than existing sliders mainly in *data volume* (24 floats vs 1-2 per slider) and *UI surface* (8 rows × 3 sliders, likely tabbed by Hue/Saturation/Luminance the way Lightroom's panel is), not in fundamental shader architecture.

**Implementation**:
- `develop.rs`: new `pub struct Hsl { pub bands: [HslBand; 8] }` (or `[f32; 24]`) added to `Adjustments` as `hsl: Hsl` (default = all-zero = identity, needs its own `is_identity`-style check folded into `Adjustments::is_identity()`). `GpuAdjust` grows by 24 floats (mind 16-byte alignment — already a multiple of 4, fine) or is passed via a separate small uniform/storage buffer if `GpuAdjust`'s single-uniform-buffer approach gets unwieldy at this size (worth checking wgpu's uniform-buffer size limits, though 24 f32 = 96 bytes is trivial).
- `shader.wgsl`: RGB→HSL conversion, band-membership weighting function (e.g. a raised-cosine or triangular falloff between the 8 band centers spaced 45° apart), apply per-band deltas, HSL→RGB back. This is the most math-dense single addition in the roadmap but entirely per-pixel/stateless.
- `apply_linear`: same conversion+band math in Rust, mirroring the shader exactly (same discipline as every other slider).
- `edit_signature_with_touchups`: extend the tone-quantization loop to include the 24 HSL values.
- `ui.rs`: new HSL panel/tab in Develop, 8 rows × 3 sliders (or a per-channel tab switcher), likely its own collapsible section given the size.

**Effort/Priority**: Medium — no new renderer architecture (unlike Clarity), but the largest single data/UI surface of the sliders in this group. Independent of Clarity/Dehaze, can be built in parallel.

**Critical files**: `src/develop.rs`, `src/shader.wgsl`, `src/ui.rs`

**Verification**: unit test the RGB↔HSL round-trip and band-weighting function in isolation (a pure-red pixel should land ~100% in the Red band, ~0% in Green/Blue bands) plus a couple of synthetic-pixel adjustment checks. End-to-end: `cargo build --release`, push a single band's saturation to an extreme on a photo with that color present, confirm only that color shifts and the band boundary doesn't show a hard seam.
