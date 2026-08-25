# Non-mac/wasm RAW Decode: Follow rawler's Own Demosaic Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Stop hand-rolling black/white-normalize and quarter-res Bayer demosaic in `raw_fast_preview.rs` (the wasm/non-mac fast-preview RAW decode path) — call rawler's own `RawImage::apply_scaling()` and its public `Demosaic` trait implementations (`Superpixel3Channel`, `PPGDemosaic`) instead, matching the shape `decode_raw_nonmac` (native non-mac) already gets for free via `RawDevelop`.

**Architecture:** `raw_fast_preview.rs` currently fuses black/white-normalize + white-balance + 2x2-bin-demosaic into one hand-rolled per-pixel closure, duplicated once for the Bayer path (`bin_bayer_quarter_res`) and once for the already-linear path (`decimate_linear_rgb`). This plan extracts the duplicated post-demosaic render step into one shared function, swaps the black/white-normalize math for rawler's own `apply_scaling()`, and swaps the Bayer path's hand-rolled 2x2 averaging for rawler's own `Superpixel3Channel`/`PPGDemosaic` (behind a new `DemosaicMode` enum, `Quality` implemented but not wired into any call site). Output contract is unchanged: same gamma-mapped RGBA8 buffer shape every platform already produces. No camera-profile/DCP-fetch system is involved anywhere in this plan — the current baseline has none (an earlier attempt at one was tried and discarded before this plan was written; see the spec's note), and this plan doesn't reintroduce one — matrix comes straight from `raw.color_matrix` (DNG-embedded calibration), matching RapidRaw's own design.

**Tech Stack:** Rust, `rawler` 0.7.2 (already a dependency), no new dependencies.

**Spec:** `plans/raw-decode-rapidraw-parity-design.md`

## Global Constraints

- Output contract unchanged: fully baked, gamma-mapped RGBA8 — no linear-light output, no `develop.rs`/`renderer.rs`/`shader.wgsl` changes (spec's explicit scope boundary).
- mac's `image_decode.rs` RAW path is untouched by this plan.
- No camera-profile/DCP-fetch layer, network or baked-in, gets added by this plan — matrix stays `raw.color_matrix` only, tone mapping stays the generic `to_srgb_u8` gamma LUT.
- `DemosaicMode::Quality` (PPG) is implemented but wired into zero call sites — stays available for a later UI toggle, per spec's explicit scope choice.
- Every task that touches `bin_bayer_quarter_res` or `decimate_linear_rgb` must keep the golden-hash regression tests from Task 1 passing (`Fast` tier only — `Quality` has no prior baseline to preserve).
- `decode_probe`'s `[[bin]]` target requires `--features raw-probe` on every `cargo test`/`cargo run` invocation (`Cargo.toml`: `required-features = ["raw-probe"]`) — every test-run command in this plan must include it.
- `rustc` toolchain floor and build commands: see root `CLAUDE.md` (`cargo build`, `cargo test`, pinned Rust stable ≥ 1.92).

---

## File Structure

- **Modify: `src/bin/decode_probe.rs`** — add module declarations for `raw_fast_preview` and `hash` (neither is currently pulled into this binary), a synthetic Bayer-CFA DNG fixture writer, and golden-hash regression tests (Task 1).
- **Modify: `src/raw_fast_preview.rs`** — the actual pipeline restructure (Tasks 2-5). Widen `decode_raw_fast_from_bytes`'s (and later, other newly-`pub(crate)` items') cfg gate so `decode_probe.rs` can reach them from a mac dev build, mirroring `image_decode.rs::decode_raw_via_rawler`'s existing `#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]` pattern exactly (Task 1). All behavioral changes stay within this file; no new files.
- **Modify: `src/image_decode.rs`** — doc-comment-only alignment on `decode_raw_nonmac` (Task 6).

---

### Task 1: Golden-hash regression fixtures (safety net before any behavior change)

**Files:**
- Modify: `src/bin/decode_probe.rs`
- Modify: `src/raw_fast_preview.rs`

**Interfaces:**
- Consumes: `raw_fast_preview::decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String>` (existing, `src/raw_fast_preview.rs:52` — **sync**, not `async`; no `pollster::block_on` needed anywhere in this plan), `hash::Fnv1a` (existing, `src/hash.rs`).
- Produces: `write_bayer_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()>` — a new synthetic RGGB Bayer-CFA DNG fixture writer, used by this task's test and by every later task's regression check.

- [ ] **Step 1: Widen `decode_raw_fast_from_bytes`'s cfg gate so `decode_probe.rs` can call it on mac**

In `src/raw_fast_preview.rs`, change:

```rust
#[cfg(not(target_os = "macos"))]
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
```

to:

```rust
// Gated on `feature = "raw-probe"` as well as `not(target_os = "macos")` so
// `decode_probe.rs`'s golden-hash regression tests can call this same
// function from a mac dev build via `cargo test --bin decode_probe
// --features raw-probe` — exact same reasoning and pattern as
// `image_decode.rs`'s `decode_raw_via_rawler` (see that function's doc
// comment). On mac+raw-probe this also compiles into the *main*
// `lightphotos` binary, where nothing calls it — only `decode_probe.rs`'s
// own copy of this module does.
#[cfg(any(not(target_os = "macos"), feature = "raw-probe"))]
#[allow(dead_code)]
pub(crate) fn decode_raw_fast_from_bytes(bytes: &[u8], max_px: u32) -> Result<DecodedImage, String> {
```

The module's top-of-file doc comment (`//! \`#[cfg(not(target_os = "macos"))]\`, like the rest of the non-mac RAW decode: ...`) should also get a one-line update noting the `raw-probe` exception, mirroring `image_decode.rs`'s equivalent note.

- [ ] **Step 2: Add module declarations to `decode_probe.rs`**

Near its existing `#[path]` declarations (top of file):

```rust
#[path = "../raw_fast_preview.rs"]
mod raw_fast_preview;
#[path = "../hash.rs"]
mod hash;
```

- [ ] **Step 3: Run a build to confirm the widened gate compiles clean on mac**

Run: `cargo build --bin decode_probe --features raw-probe`
Expected: builds clean (no errors — `raw_fast_preview.rs` has no macOS-specific code inside it, so widening its gate shouldn't hit anything mac-incompatible).

- [ ] **Step 4: Write `write_bayer_dng`, a synthetic RGGB Bayer DNG fixture**

Add this function near `write_linear_dng` (same file). It follows that function's exact proven pattern (see its own doc comment for the byte-layout rationale) but writes a single-channel Bayer mosaic instead of 3-channel linear RGB, with the DNG tags a Bayer decode path actually reads: `CFAPattern`, `BlackLevelRepeatDim`+`BlackLevels`, `WhiteLevel`, `AsShotNeutral` (for a deterministic non-NaN white balance — confirmed via `rawler`'s `DngDecoder::get_wb`, which inverts `AsShotNeutral` into `wb_coeffs`, and defaults to `[NaN; 4]` when the tag is absent).

```rust
/// Writes a minimal, valid Bayer-CFA DNG: an RGGB mosaic, single 16-bit
/// sample per pixel, with the DNG tags a real Bayer decode path reads
/// (`CFAPattern`, black/white levels, `AsShotNeutral` for white balance).
/// Test pattern: `v = 100 + ((x * 53 + y * 197) % 800)`, deterministic and
/// non-uniform (unlike a flat value, gives 2x2-bin/PPG demosaic something
/// real to interpolate/average). `width`/`height` must be even (Bayer 2x2
/// tiling).
///
/// Confirmed against the real `rawler` 0.7.2 source: `DngDecoder::get_cfa`
/// (`src/decoders/dng.rs`) reads only `TiffCommonTag::CFAPattern`
/// (0x828E) — `CFARepeatPatternDim` isn't consulted for the `CFA` object,
/// so it's omitted here. `CFAColor` numeric codes (`src/cfa.rs`) are
/// RED=0, GREEN=1, BLUE=2 — `CFAPattern = [0,1,1,2]` is RGGB.
fn write_bayer_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    use std::io::Write;

    const T_BYTE: u16 = 1;
    const T_SHORT: u16 = 3;
    const T_LONG: u16 = 4;
    const T_RATIONAL: u16 = 5;
    const T_SRATIONAL: u16 = 10;

    const ENTRY_COUNT: u16 = 19;
    const IFD_OFFSET: u32 = 8;

    fn inline_u16(v: u16) -> [u8; 4] {
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    }
    fn inline_u32(v: u32) -> [u8; 4] {
        v.to_le_bytes()
    }
    fn push_entry(buf: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: [u8; 4]) {
        buf.extend_from_slice(&tag.to_le_bytes());
        buf.extend_from_slice(&typ.to_le_bytes());
        buf.extend_from_slice(&count.to_le_bytes());
        buf.extend_from_slice(&value);
    }
    fn pad_to_even(buf: &mut Vec<u8>) {
        if buf.len() % 2 != 0 {
            buf.push(0);
        }
    }

    assert!(width % 2 == 0 && height % 2 == 0, "Bayer fixture needs even dimensions");

    let ifd_size = 2 + (ENTRY_COUNT as usize) * 12 + 4;
    let after_ifd = IFD_OFFSET as usize + ifd_size;

    // Out-of-line blocks, in ascending-tag order: BlackLevels (4x SHORT),
    // ColorMatrix1 (9x SRATIONAL), AsShotNeutral (3x RATIONAL).
    let blacklevels_offset = after_ifd as u32; // 4 x SHORT = 8 bytes
    let mut off = after_ifd + 8;
    if off % 2 != 0 {
        off += 1;
    }
    let colormatrix_offset = off as u32; // 9 x SRATIONAL = 72 bytes
    off += 72;
    if off % 2 != 0 {
        off += 1;
    }
    let asshotneutral_offset = off as u32; // 3 x RATIONAL = 24 bytes
    off += 24;
    if off % 2 != 0 {
        off += 1;
    }
    let pixel_offset = off as u32;

    let strip_byte_count = width * height * 2; // 1 sample/pixel, 16-bit

    let mut buf: Vec<u8> = Vec::with_capacity(pixel_offset as usize + strip_byte_count as usize);

    buf.extend_from_slice(&[0x49, 0x49, 0x2A, 0x00]);
    buf.extend_from_slice(&IFD_OFFSET.to_le_bytes());

    buf.extend_from_slice(&ENTRY_COUNT.to_le_bytes());
    push_entry(&mut buf, 254, T_LONG, 1, inline_u32(0)); // NewSubfileType
    push_entry(&mut buf, 256, T_LONG, 1, inline_u32(width)); // ImageWidth
    push_entry(&mut buf, 257, T_LONG, 1, inline_u32(height)); // ImageLength
    push_entry(&mut buf, 258, T_SHORT, 1, inline_u16(16)); // BitsPerSample
    push_entry(&mut buf, 259, T_SHORT, 1, inline_u16(1)); // Compression = none
    push_entry(&mut buf, 262, T_SHORT, 1, inline_u16(32803)); // PhotometricInterpretation = CFA
    push_entry(&mut buf, 273, T_LONG, 1, inline_u32(pixel_offset)); // StripOffsets
    push_entry(&mut buf, 277, T_SHORT, 1, inline_u16(1)); // SamplesPerPixel
    push_entry(&mut buf, 278, T_LONG, 1, inline_u32(height)); // RowsPerStrip
    push_entry(&mut buf, 279, T_LONG, 1, inline_u32(strip_byte_count)); // StripByteCounts
    push_entry(&mut buf, 284, T_SHORT, 1, inline_u16(1)); // PlanarConfiguration
    push_entry(&mut buf, 33422, T_BYTE, 4, [0, 1, 1, 2]); // CFAPattern = RGGB
    push_entry(&mut buf, 50706, T_BYTE, 4, [1, 4, 0, 0]); // DNGVersion
    push_entry(&mut buf, 50707, T_BYTE, 4, [1, 1, 0, 0]); // DNGBackwardVersion
    push_entry(&mut buf, 50713, T_SHORT, 2, inline_u32(0x0002_0002)); // BlackLevelRepeatDim = [2,2]
    push_entry(&mut buf, 50714, T_SHORT, 4, inline_u32(blacklevels_offset)); // BlackLevels
    push_entry(&mut buf, 50717, T_LONG, 1, inline_u32(1024)); // WhiteLevel
    push_entry(&mut buf, 50721, T_SRATIONAL, 9, inline_u32(colormatrix_offset)); // ColorMatrix1
    push_entry(&mut buf, 50728, T_RATIONAL, 3, inline_u32(asshotneutral_offset)); // AsShotNeutral
    buf.extend_from_slice(&0u32.to_le_bytes()); // next IFD = none

    debug_assert_eq!(buf.len(), after_ifd);

    // BlackLevels = [0, 0, 0, 0]
    for _ in 0..4 {
        buf.extend_from_slice(&0u16.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, colormatrix_offset);

    // ColorMatrix1 = identity 3x3 SRATIONAL
    const IDENTITY_3X3: [(i32, i32); 9] = [(1, 1), (0, 1), (0, 1), (0, 1), (1, 1), (0, 1), (0, 1), (0, 1), (1, 1)];
    for (num, den) in IDENTITY_3X3 {
        buf.extend_from_slice(&num.to_le_bytes());
        buf.extend_from_slice(&den.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, asshotneutral_offset);

    // AsShotNeutral = [1/1, 1/1, 1/1] (neutral -> wb_coeffs = [1,1,1,NaN])
    for _ in 0..3 {
        buf.extend_from_slice(&1i32.to_le_bytes());
        buf.extend_from_slice(&1i32.to_le_bytes());
    }
    pad_to_even(&mut buf);
    debug_assert_eq!(buf.len() as u32, pixel_offset);

    // Pixel data: deterministic non-uniform single-channel mosaic.
    for y in 0..height {
        for x in 0..width {
            let v = 100u16 + (((x * 53 + y * 197) % 800) as u16);
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    debug_assert_eq!(buf.len() as u64, pixel_offset as u64 + strip_byte_count as u64);

    let mut f = std::fs::File::create(path)?;
    f.write_all(&buf)?;
    Ok(())
}
```

- [ ] **Step 5: Write the golden-hash regression tests**

Add to `decode_probe.rs`'s `#[cfg(test)] mod tests` block:

```rust
/// Locks in `raw_fast_preview::decode_raw_fast_from_bytes`'s current
/// (pre-refactor) output on a synthetic Bayer fixture, via `hash::Fnv1a`
/// over width+height+rgba bytes (see `src/hash.rs` — chosen because it's
/// stable/deterministic across process runs, unlike `DefaultHasher`).
/// Every task in `plans/raw-decode-rapidraw-parity-plan.md` that touches
/// `bin_bayer_quarter_res` must keep this passing — the plan is
/// structure-only for the `Fast` tier, so output must not change.
///
/// The captured hash below was observed by running this test once with a
/// dummy value and reading the actual value off the `println!` output —
/// standard golden-snapshot practice, not hand-computed (a multi-stage
/// float pipeline's output isn't something to derive by hand).
#[test]
fn raw_fast_preview_bayer_fast_tier_matches_golden_hash() {
    let path = std::env::temp_dir().join(format!("lightphotos_bayer_dng_test_{}.dng", std::process::id()));
    let (width, height) = (8u32, 6u32);
    write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
    let bytes = std::fs::read(&path).expect("read fixture bytes");
    let _ = std::fs::remove_file(&path);

    let decoded = raw_fast_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
        .expect("decode_raw_fast_from_bytes failed on synthetic Bayer DNG fixture");

    let mut hasher = hash::Fnv1a::new();
    hasher.write(&decoded.width.to_le_bytes());
    hasher.write(&decoded.height.to_le_bytes());
    hasher.write(&decoded.rgba);
    let golden = hasher.finish();

    println!(
        "bayer golden hash: {golden:#x} ({}x{}, {} rgba bytes)",
        decoded.width,
        decoded.height,
        decoded.rgba.len()
    );
    assert_eq!(
        golden, 0x0000_0000_0000_0000,
        "Fast-tier Bayer decode output changed from the captured golden hash \
         (see this test's println! output above for the actual value) - if this \
         change is intentional, update the literal; if not, a task's supposedly \
         structure-only refactor changed real output"
    );
}

/// Same idea, for the already-linear (`cpp == 3`, `decimate_linear_rgb`)
/// path — reuses the existing `write_linear_dng` fixture (this file's
/// synthetic Linear DNG, above) as-is rather than adding a second new
/// fixture. That fixture has no `AsShotNeutral` tag, so `wb_coeffs` comes
/// back `[NaN; 4]` (`DngDecoder::get_wb`'s no-tag branch) — today's actual
/// behavior on it, NaN-poisoned output included, is exactly what this
/// golden hash locks in; this refactor doesn't change that (fixing it is
/// out of scope, see the spec's "Explicitly out of scope" section).
#[test]
fn raw_fast_preview_linear_fast_tier_matches_golden_hash() {
    let path = std::env::temp_dir().join(format!("lightphotos_linear_wb_dng_test_{}.dng", std::process::id()));
    let (width, height) = (16u32, 12u32);
    write_linear_dng(&path, width, height).expect("write_linear_dng failed");
    let bytes = std::fs::read(&path).expect("read fixture bytes");
    let _ = std::fs::remove_file(&path);

    let decoded = raw_fast_preview::decode_raw_fast_from_bytes(&bytes, u32::MAX)
        .expect("decode_raw_fast_from_bytes failed on synthetic Linear DNG fixture");

    let mut hasher = hash::Fnv1a::new();
    hasher.write(&decoded.width.to_le_bytes());
    hasher.write(&decoded.height.to_le_bytes());
    hasher.write(&decoded.rgba);
    let golden = hasher.finish();

    println!(
        "linear golden hash: {golden:#x} ({}x{}, {} rgba bytes)",
        decoded.width,
        decoded.height,
        decoded.rgba.len()
    );
    assert_eq!(
        golden, 0x0000_0000_0000_0000,
        "Fast-tier Linear decode output changed from the captured golden hash \
         (see this test's println! output above for the actual value)"
    );
}
```

- [ ] **Step 6: Run both tests once to capture the real golden hashes**

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview -- --nocapture`
Expected: both `FAIL` (dummy `0x0`), each printing its real `bayer golden hash: 0x...` / `linear golden hash: 0x...` line.

- [ ] **Step 7: Paste the captured hashes in, rerun to confirm PASS**

Replace each `0x0000_0000_0000_0000` with the value its own test printed.

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview`
Expected: both `PASS`.

- [ ] **Step 8: Commit**

```bash
git add src/bin/decode_probe.rs src/raw_fast_preview.rs
git commit -m "test: golden-hash regression fixtures for raw_fast_preview RAW decode"
```

---

### Task 2: Extract the shared post-demosaic render step

**Files:**
- Modify: `src/raw_fast_preview.rs`

**Interfaces:**
- Consumes: `apply_cam2rgb`, `to_srgb_u8` (both existing, this file).
- Produces: `render_rgb_sample(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 4]` — used by both `bin_bayer_quarter_res` and `decimate_linear_rgb` from here on.

Pure dedup, zero behavior change — `bin_bayer_quarter_res` (around line 272-280) and `decimate_linear_rgb` (around line 334-341) each inline the identical "apply matrix if present, else pass through -> `to_srgb_u8` x3 -> alpha=255" sequence. This task pulls it into one function both call.

- [ ] **Step 1: Add `render_rgb_sample`**

Add near `apply_cam2rgb` in `src/raw_fast_preview.rs`:

```rust
/// Renders one linear camera-RGB sample (already white-balanced) to
/// display RGBA8: color matrix (if the camera has calibration data) ->
/// gamma, alpha fixed opaque. Shared by both `bin_bayer_quarter_res` and
/// `decimate_linear_rgb` — was duplicated inline in both before this.
fn render_rgb_sample(rgb: [f32; 3], cam2rgb: &Option<[[f32; 4]; 3]>) -> [u8; 4] {
    let srgb = match cam2rgb {
        Some(m) => apply_cam2rgb(m, rgb),
        None => rgb,
    };
    [to_srgb_u8(srgb[0]), to_srgb_u8(srgb[1]), to_srgb_u8(srgb[2]), 255]
}
```

- [ ] **Step 2: Use it in `bin_bayer_quarter_res`**

Replace this block (current body, inside the `oy`/`ox` loop):

```rust
            let srgb = match &cam2rgb {
                Some(m) => apply_cam2rgb(m, rgb),
                None => rgb,
            };
            let idx = (oy * out_w + ox) * 4;
            rgba[idx] = to_srgb_u8(srgb[0]);
            rgba[idx + 1] = to_srgb_u8(srgb[1]);
            rgba[idx + 2] = to_srgb_u8(srgb[2]);
            rgba[idx + 3] = 255;
```

with:

```rust
            let px = render_rgb_sample(rgb, &cam2rgb);
            let idx = (oy * out_w + ox) * 4;
            rgba[idx..idx + 4].copy_from_slice(&px);
```

- [ ] **Step 3: Use it in `decimate_linear_rgb`**

Same replacement in `decimate_linear_rgb`'s equivalent block.

- [ ] **Step 4: Run the golden-hash tests to confirm no behavior change**

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview`
Expected: both `PASS` (same hashes as Task 1).

- [ ] **Step 5: Commit**

```bash
git add src/raw_fast_preview.rs
git commit -m "refactor: extract shared render_rgb_sample from raw_fast_preview's two demosaic paths"
```

---

### Task 3: Swap black/white-normalize to `RawImage::apply_scaling()`; extract white-balance into its own pass

**Files:**
- Modify: `src/raw_fast_preview.rs`

**Interfaces:**
- Consumes: `rawler::RawImage::apply_scaling(&mut self) -> rawler::Result<()>` (rawler's own — confirmed at `rawler-0.7.2/src/rawimage.rs:507`, rescales per-Bayer-channel black/white levels to `[0, ...)`, resets `blacklevel`/`whitelevel` to 0/1 after).
- Produces: `apply_white_balance_in_place(samples: &mut [f32], width: usize, cfa: &rawler::CFA, wb: [f32; 4])` (Bayer/CFA-position-based) and `apply_white_balance_linear_in_place(samples: &mut [f32], cpp: usize, wb: [f32; 4])` (LinearRaw/channel-index-based) — both new. `bin_bayer_quarter_res` and `decimate_linear_rgb` now take `raw: &mut rawler::RawImage` (was `&rawler::RawImage`) — ripples to `fast_preview` and its one caller in `decode_raw_fast_from_bytes`.

This keeps white balance in its *current* position (pre-demosaic, same as today's hand-rolled `normalize` closure) — see the spec's note on why WB stays here rather than moving to post-demosaic.

- [ ] **Step 1: Call `apply_scaling()` once, up front**

In `decode_raw_fast_from_bytes`, right after the `rawler::decode` call (the `raw` binding is already `let raw = ...` from a `catch_unwind` — change to `let mut raw = ...`):

```rust
    let mut raw = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| rawler::decode(&source, &params)))
        .map_err(|_| "panicked during RAW decode".to_string())?
        .map_err(|e| e.to_string())?;
    raw.apply_scaling().map_err(|e| e.to_string())?;
```

- [ ] **Step 2: Update `fast_preview`'s signature to take `&mut RawImage`**

```rust
fn fast_preview(
    raw: &mut rawler::RawImage,
    orientation: rawler::decoders::Orientation,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => bin_bayer_quarter_res(raw),
        3 => decimate_linear_rgb(raw),
        _ => None,
    }?;
    Some(apply_orientation(orientation, w, h, rgba))
}
```

Update its one call site in `decode_raw_fast_from_bytes` to pass `&mut raw` instead of `&raw`.

- [ ] **Step 3: Add the two white-balance helpers**

Add near `build_cam2rgb`:

```rust
/// Multiplies each raw sample by its CFA-position's white-balance
/// coefficient, in place. Pre-demosaic — matches this file's pre-refactor
/// behavior exactly (today's hand-rolled `normalize` closure applied WB
/// at this same point, fused with black/white-normalize; this factors it
/// out as its own pass now that black/white-normalize is
/// `apply_scaling()`'s job instead).
fn apply_white_balance_in_place(samples: &mut [f32], width: usize, cfa: &rawler::CFA, wb: [f32; 4]) {
    for (idx, v) in samples.iter_mut().enumerate() {
        let (row, col) = (idx / width, idx % width);
        *v *= wb[cfa.color_at(row, col)];
    }
}

/// Same idea for already-demosaiced linear data (`cpp == 3`, no CFA — each
/// sample's channel is just `idx % cpp`, cycling R,G,B).
fn apply_white_balance_linear_in_place(samples: &mut [f32], cpp: usize, wb: [f32; 4]) {
    for (idx, v) in samples.iter_mut().enumerate() {
        *v *= wb[idx % cpp];
    }
}
```

- [ ] **Step 4: Rewrite `bin_bayer_quarter_res` to take `&mut RawImage`, use `apply_white_balance_in_place`, drop the old black/white math**

Replace the function's signature and its `normalize` closure + black/white setup:

```rust
fn bin_bayer_quarter_res(raw: &mut rawler::RawImage) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::RawImageData;

    let cfa = raw.camera.cfa.clone();
    let wb = raw.wb_coeffs;
    let width = raw.width;

    let RawImageData::Float(data) = &mut raw.data else {
        return None; // apply_scaling always leaves Float data
    };
    if data.len() != width * raw.height {
        return None;
    }
    apply_white_balance_in_place(data, width, &cfa, wb);

    let sample_at = |row: usize, col: usize| -> f32 { data[row * width + col] };

    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, raw.height),
    ));
    let (x0, y0) = (area.p.x - (area.p.x % 2), area.p.y - (area.p.y % 2));
    let (aw, ah) = (area.d.w - (area.d.w % 2), area.d.h - (area.d.h % 2));

    let out_w = aw / 2;
    let out_h = ah / 2;
    let mut rgba = vec![0u8; out_w * out_h * 4];
    let cam2rgb = build_cam2rgb(raw);

    for oy in 0..out_h {
        for ox in 0..out_w {
            let (row, col) = (y0 + oy * 2, x0 + ox * 2);
            let mut rgb = [0f32; 3];
            let mut g_count = 0f32;
            for (dr, dc) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
                let ch = cfa.color_at(row + dr, col + dc);
                let v = sample_at(row + dr, col + dc);
                match ch {
                    0 => rgb[0] = v,
                    2 => rgb[2] = v,
                    _ => {
                        rgb[1] += v;
                        g_count += 1.0;
                    }
                }
            }
            if g_count > 0.0 {
                rgb[1] /= g_count;
            }
            let px = render_rgb_sample(rgb, &cam2rgb);
            let idx = (oy * out_w + ox) * 4;
            rgba[idx..idx + 4].copy_from_slice(&px);
        }
    }

    Some((out_w as u32, out_h as u32, rgba))
}
```

Note: `build_cam2rgb(raw)` still takes `&rawler::RawImage` — passing the `&mut RawImage` parameter `raw` where it's expected auto-reborrows, no change needed to `build_cam2rgb` itself.

- [ ] **Step 5: Rewrite `decimate_linear_rgb`'s normalize the same way**

Replace its `black`/`white`/`wb` setup and per-pixel normalize with `apply_white_balance_linear_in_place` (black/white already done upfront in Step 1 via `apply_scaling`), keeping its sampling loop otherwise the same (direct per-channel reads now that scaling+WB already happened), and its `render_rgb_sample` call from Task 2 unchanged. Signature becomes `fn decimate_linear_rgb(raw: &mut rawler::RawImage) -> Option<(u32, u32, Vec<u8>)>`.

- [ ] **Step 6: Run the golden-hash tests**

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview`
Expected: both `PASS`, same hashes as Task 1/2 (this step is pure restructure — same math, same order, just factored differently).

- [ ] **Step 7: Commit**

```bash
git add src/raw_fast_preview.rs
git commit -m "refactor: black/white-normalize via rawler's apply_scaling, WB as its own pass"
```

---

### Task 4: Swap Bayer demosaic to rawler's `Demosaic` trait (`Superpixel3Channel`/`PPGDemosaic`)

**Files:**
- Modify: `src/raw_fast_preview.rs`

**Interfaces:**
- Consumes: `rawler::imgop::sensor::bayer::{Demosaic, superpixel::Superpixel3Channel, ppg::PPGDemosaic}` (confirmed public in rawler 0.7.2, both `impl Demosaic<f32, 3>`), `rawler::pixarray::Pix2D`.
- Produces: `pub(crate) enum DemosaicMode { Fast, Quality }` — `Fast` is the only value used by any call site in production code; `Quality` is exercised only by this task's own smoke test. `bin_bayer_quarter_res` becomes `pub(crate)` (was private) so `decode_probe.rs`'s smoke test (Step 5) can call it directly.

This is the task the spec's risk section calls out: both `Demosaic` impls use `rayon` internally, and this repo's wasm target has no thread-pool setup. The compile+codegen spike (2026-08-24) passed; this task adds the runtime half that spike couldn't cover.

- [ ] **Step 1: Add `DemosaicMode`**

Near the top of `src/raw_fast_preview.rs`:

```rust
/// `Fast` = rawler's `Superpixel3Channel` (quarter-res 2x2 bin, matches
/// this file's pre-Task-4 hand-rolled output). `Quality` = rawler's
/// `PPGDemosaic` (full-res, real edge-directed interpolation — the same
/// algorithm `decode_raw_nonmac`, the native non-mac path, already uses
/// via `RawDevelop`). `Quality` isn't wired into any call site yet — see
/// `plans/raw-decode-rapidraw-parity-design.md`'s explicit scope note.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DemosaicMode {
    Fast,
    #[allow(dead_code)]
    Quality,
}
```

- [ ] **Step 2: Give `bin_bayer_quarter_res` a `mode: DemosaicMode` parameter, swap the manual averaging loop for the `Demosaic` trait call, make it `pub(crate)`**

Replace the function body from Task 3 (keep the WB/scaling-guard prologue, but read the CFA from `raw.photometric` instead of `raw.camera.cfa` — matches rawler's own `RawDevelop::develop_intermediate` reference exactly, and correctly accounts for an `ActiveArea` origin shift, which `raw.camera.cfa` doesn't; for the even-origin fixture Task 1 uses, the two are identical, so this doesn't change the golden hash):

```rust
pub(crate) fn bin_bayer_quarter_res(raw: &mut rawler::RawImage, mode: DemosaicMode) -> Option<(u32, u32, Vec<u8>)> {
    use rawler::imgop::sensor::bayer::{Demosaic, ppg::PPGDemosaic, superpixel::Superpixel3Channel};
    use rawler::pixarray::Pix2D;
    use rawler::{RawImageData, RawPhotometricInterpretation};

    let RawPhotometricInterpretation::Cfa(config) = &raw.photometric else {
        return None;
    };
    let cfa = config.cfa.clone();
    let colors = config.colors.clone();
    let wb = raw.wb_coeffs;
    let width = raw.width;

    let RawImageData::Float(data) = &mut raw.data else {
        return None;
    };
    if data.len() != width * raw.height {
        return None;
    }
    apply_white_balance_in_place(data, width, &cfa, wb);

    let pixels = Pix2D::new_with(data.clone(), width, raw.height);
    let area = raw.active_area.unwrap_or(rawler::imgop::Rect::new(
        rawler::imgop::Point::new(0, 0),
        rawler::imgop::Dim2::new(width, raw.height),
    ));

    let demosaiced = match mode {
        DemosaicMode::Fast => Superpixel3Channel::new().demosaic(&pixels, &cfa, &colors, area),
        DemosaicMode::Quality => PPGDemosaic::new().demosaic(&pixels, &cfa, &colors, area),
    };

    let cam2rgb = build_cam2rgb(raw);
    let (w, h) = (demosaiced.width, demosaiced.height);
    let mut rgba = vec![0u8; w * h * 4];
    for (i, &rgb) in demosaiced.pixels().iter().enumerate() {
        let px = render_rgb_sample(rgb, &cam2rgb);
        rgba[i * 4..i * 4 + 4].copy_from_slice(&px);
    }
    Some((w as u32, h as u32, rgba))
}
```

- [ ] **Step 3: Update callers — `fast_preview` passes `DemosaicMode::Fast`**

```rust
fn fast_preview(
    raw: &mut rawler::RawImage,
    orientation: rawler::decoders::Orientation,
) -> Option<(u32, u32, Vec<u8>)> {
    let (w, h, rgba) = match raw.cpp {
        1 => bin_bayer_quarter_res(raw, DemosaicMode::Fast),
        3 => decimate_linear_rgb(raw),
        _ => None,
    }?;
    Some(apply_orientation(orientation, w, h, rgba))
}
```

`decode_raw_fast_from_bytes` and its production behavior are unchanged — `Fast` is what it already produced.

- [ ] **Step 4: Run the Bayer golden-hash test**

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview_bayer`
Expected: `PASS` — `Superpixel3Channel`'s own 2x2-averaging math (confirmed by reading its source: RGGB dispatch is `[p[0], (p[1]+p[2])/2.0, p[3]]`, same formula the old hand-rolled loop used) should reproduce Task 1's captured hash exactly for this RGGB, even-origin fixture.

If it does **not** match: don't force it green. Compare `rgba` byte-for-byte between old and new (e.g. temporarily print both) — the likely culprits, in order: (a) `config.cfa` (from `raw.photometric`) vs the old `raw.camera.cfa` genuinely differing for this fixture (shouldn't, per this step's note, but verify), (b) an off-by-one in `area`/`roi` alignment. Fix the actual cause; only update the golden literal if you've confirmed the new output is correct and the old one wasn't (which the spec's scope doesn't call for here).

- [ ] **Step 5: Add a `Quality`-tier smoke test (no prior baseline — just confirms it runs)**

Add to `decode_probe.rs`'s test module:

```rust
/// `DemosaicMode::Quality` (`PPGDemosaic`) has no golden hash to match
/// (this plan doesn't wire it into any call site) — this just confirms it
/// decodes without panicking and produces a plausible, non-degenerate
/// image on the same fixture Task 1 uses.
#[test]
fn raw_fast_preview_bayer_quality_tier_runs_without_panicking() {
    let path = std::env::temp_dir().join(format!("lightphotos_bayer_quality_dng_test_{}.dng", std::process::id()));
    let (width, height) = (8u32, 6u32);
    write_bayer_dng(&path, width, height).expect("write_bayer_dng failed");
    let bytes = std::fs::read(&path).expect("read fixture bytes");
    let _ = std::fs::remove_file(&path);

    // decode_raw_fast_from_bytes always uses DemosaicMode::Fast internally
    // (Step 3) - exercise Quality directly via rawler::decode + the same
    // apply_scaling/bin_bayer_quarter_res call chain that function makes.
    let source = rawler::rawsource::RawSource::new_from_slice(&bytes);
    let params = rawler::decoders::RawDecodeParams::default();
    let mut raw = rawler::decode(&source, &params).expect("rawler::decode failed on fixture");
    raw.apply_scaling().expect("apply_scaling failed");

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        raw_fast_preview::bin_bayer_quarter_res(&mut raw, raw_fast_preview::DemosaicMode::Quality)
    }));
    let (w, h, rgba) = result.expect("PPGDemosaic panicked").expect("bin_bayer_quarter_res returned None");

    assert!(w > 0 && h > 0, "degenerate output dimensions");
    assert!(rgba.iter().any(|&b| b != 0), "output looks all-zero/degenerate");
}
```

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview_bayer_quality`
Expected: `PASS`.

- [ ] **Step 6: The runtime risk check — build + run the actual wasm bundle**

This is the step the spec's risk section requires before this task is considered done: `PPGDemosaic`/`Superpixel3Channel` are now reachable from `raw_fast_preview.rs`, which compiles into `wasm_worker` — the compile-only spike from brainstorming doesn't cover whether `rayon`'s thread-pool init panics at runtime in a browser with no `+atomics` build flag.

Per this project's own convention (no GUI/browser automation from an agent — the user runs their own live instance), this step is manual:

1. Build: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml` (same command `scripts/deploy-web.sh` uses), or `trunk serve` for a local dev server.
2. Open the built app in a real browser, open a real Bayer-CFA RAW file (CR2/NEF/ARW — any camera RAW, not a DNG) through it.
3. Open devtools console. Confirm: no panic/error logged, the RAW file's grid thumbnail and/or Loupe view actually renders (not blank/black).
4. Report back what happened before this task is marked done.

If it panics: the fallback is gating `DemosaicMode` construction (or the whole `Superpixel3Channel`/`PPGDemosaic` call path) behind `#[cfg(not(target_arch = "wasm32"))]`, keeping today's hand-rolled loop as a wasm-only fallback — that's a real follow-up task, not covered by this plan, since it changes this plan's premise. Stop and report if this happens rather than silently patching around it.

- [ ] **Step 7: Commit**

```bash
git add src/raw_fast_preview.rs src/bin/decode_probe.rs
git commit -m "feat: demosaic via rawler's own Superpixel3Channel/PPGDemosaic (DemosaicMode)"
```

---

### Task 5: Name the highlight-rolloff stage boundary

**Files:**
- Modify: `src/raw_fast_preview.rs`

**Interfaces:** no signature changes — doc-comment-only, on `apply_cam2rgb` (existing).

- [ ] **Step 1: Expand `apply_cam2rgb`'s doc comment**

Add above its existing doc comment (don't replace it):

```rust
/// Pipeline stage note: the `clip_euclidean_norm_avg` call below is this
/// file's counterpart to RapidRaw's standalone `highlight_compression`
/// stage — a soft highlight rolloff applied right after the color-matrix
/// multiply, same position RapidRaw's version occupies. Folded into this
/// one function rather than split into its own, since there's no separate
/// exposure/gain step in this codebase to share a boundary with (unlike
/// an earlier, discarded attempt at this file that had one — see
/// `plans/raw-decode-rapidraw-parity-design.md`'s reset note).
```

- [ ] **Step 2: Run the golden-hash tests (doc-only change, must still pass)**

Run: `cargo test --bin decode_probe --features raw-probe raw_fast_preview`
Expected: both `PASS`.

- [ ] **Step 3: Commit**

```bash
git add src/raw_fast_preview.rs
git commit -m "docs: name the highlight-rolloff pipeline stage boundary"
```

---

### Task 6: Doc-comment alignment on `decode_raw_nonmac`

**Files:**
- Modify: `src/image_decode.rs`

**Interfaces:** none — doc-only.

`decode_raw_nonmac` (native non-mac path) gets no code change per the spec — it already delegates black/white/WB/demosaic to `RawDevelop::default()`, unmodified. This task just points its doc comment at the same stage vocabulary Tasks 2-5 established, for consistency between the two non-mac paths.

- [ ] **Step 1: Add a doc-comment note to `decode_raw_nonmac`**

Near its existing doc comment (`src/image_decode.rs:202-219`), add:

```rust
/// Stage vocabulary note: this delegates black/white-normalize, white
/// balance, and demosaic to `RawDevelop`'s own `ProcessingStep`s (this is
/// literally the `PPGDemosaic`/`apply_scaling`-equivalent machinery
/// `raw_fast_preview.rs`'s `Fast`/`Quality` `DemosaicMode` now also calls
/// directly — see `plans/raw-decode-rapidraw-parity-design.md`). Neither
/// path applies any per-camera profile — both use `raw.color_matrix`
/// (DNG-embedded calibration) directly, matching RapidRaw's own design.
```

- [ ] **Step 2: Run the full test suite (sanity — doc-only change)**

Run: `cargo test --features raw-probe`
Expected: all `PASS`, no regressions from earlier tasks either.

- [ ] **Step 3: Commit**

```bash
git add src/image_decode.rs
git commit -m "docs: align decode_raw_nonmac's doc comment with the new stage vocabulary"
```

---

## Self-Review Notes

- **Spec coverage**: black/white normalize (Task 3), demosaic Fast+Quality via rawler (Task 4), WB kept pre-demosaic per the spec's correction (Task 3), color matrix/orientation unchanged (untouched, verified via golden hashes throughout), highlight-rolloff stage naming (Task 5), `decode_raw_nonmac` doc alignment (Task 6), wasm+rayon runtime risk check (Task 4 Step 6), output contract unchanged (no task touches `develop.rs`/`renderer.rs`/`shader.wgsl`), mac untouched (no task touches `image_decode.rs`'s RAW *code*, only a doc comment in Task 6), no camera-profile layer reintroduced (verified against the actual reset baseline throughout — no task references `camera_profile`/`CameraProfile`/any DCP concept). All covered.
- **Placeholder scan**: the two `0x0000_0000_0000_0000` golden-hash literals are the one deliberate exception — real, unavoidable (nobody can hand-compute an FNV hash of this pipeline's output), and each has a concrete, executable step (Step 6/7 of Task 1) that fills in the real captured value before the task is done. Not a "TBD."
- **Type consistency**: `bin_bayer_quarter_res`/`decimate_linear_rgb`/`fast_preview` all take `&mut rawler::RawImage` consistently from Task 3 onward; `render_rgb_sample`'s signature (Task 2, `(rgb: [f32;3], cam2rgb: &Option<[[f32;4];3]>) -> [u8;4]`, no profile params — corrected against the actual reset baseline, which has no `CameraProfile` type) is used identically in both call sites through every later task; `DemosaicMode` (Task 4) has exactly one variant (`Fast`) actually constructed anywhere in production code, matching the "not wired into any call site" constraint; `bin_bayer_quarter_res` is `pub(crate)` from Task 4 onward so `decode_probe.rs`'s Quality smoke test can reach it.
- **Verified against the actual current codebase** (2026-08-24, post-reset to commit `adc5b30`): read `src/raw_fast_preview.rs`, `src/bin/decode_probe.rs`, and `src/image_decode.rs` in full at their current (reset) content before writing this revision — `decode_raw_fast_from_bytes` is sync (not `async`), there is no `camera_profile`/`CameraProfile` system anywhere, tone mapping is `to_srgb_u8` (fixed gamma LUT, no per-camera curve), there is no exposure-gain stage/constant, `decode_probe.rs` does not yet declare a `raw_fast_preview` or `hash` module, and `decode_probe`'s `[[bin]]` target needs `--features raw-probe` on every invocation (`test = false` alone does not exclude it from an explicit `--bin decode_probe` test run — confirmed by actually running `cargo test --bin decode_probe --features raw-probe -- --list`, which listed all 14 existing tests). An earlier revision of this plan was written against an uncommitted WIP layer (camera-profile fetch, `LINEAR_EXPOSURE_GAIN`, an `async fn`) that has since been reset away entirely; this revision supersedes it.
