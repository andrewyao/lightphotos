# Generic embedded-preview fallback for RAW thumbnails (tier 1)

## Context

`raw_fast_preview.rs`'s X-Trans demosaic path (`bin_bayer_quarter_res` → `XtransFastDemosaic`, plus the `downsample_xtrans_mosaic`/`map_xtrans_coord` active-area math) has been through ~9 roborev review-fix cycles and still has open correctness findings (active-area↔reduced-space coordinate mapping, per-phase black-level loss in `apply_scaling`, trailing-block CFA-phase clamp). It's also load-bearing for *every* Fuji RAF request, grid and Loupe both — there's no bypass today because RAF's proprietary container magic (`"FUJIFILM..."`) fails the existing generic embedded-thumbnail extractor (`thumbnail.rs`'s `embedded_preview_from_bytes`, which needs TIFF/JPEG magic at byte 0), so every RAF file always falls into the buggy demosaic path.

Investigation found this isn't actually a Fuji-specific gap. Checked rawler 0.7.2's `Decoder` trait across every format lightphotos supports:

- **Tier 2 (full decode/quality)** — `raw_image()` is *required* on the trait (no default), universal across every format already. `nonmac_decode.rs`'s `RawDevelop::develop_intermediate()` already calls it generically. No gap.
- **Tier 3 (fast decode)** — generic at the CFA-layout level, not per-manufacturer: `Superpixel3Channel` (quarter-res bin) already covers every Bayer camera (Canon/Nikon/Sony/Panasonic/Olympus/Pentax/Samsung). X-Trans is the only non-Bayer CFA in real use, and it's a correctness bug in that one branch, not a missing-format gap. **Decision: leave as-is, TODO for later** (see below) — this plan does not touch `raw_fast_preview.rs`'s demosaic code.
- **Tier 1 (thumbnail)** — two independent mechanisms: (a) `thumbnail.rs`'s existing generic `kamadak-exif` TIFF/JPEG-magic sniff, already works for any TIFF-outer-container format; (b) rawler's own `Decoder::full_image()` — a generic trait method (default `Ok(None)`), overridden per-decoder when that format carries an embedded JPEG/preview. Checked which lightphotos-supported extensions have a TIFF outer container (verified against `decoders/mod.rs`'s `get_decoder` dispatch): 8 of 10 (cr2, nef, arw, dng, rw2, orf, pef, srw) are TIFF-based and already work via (a). Exactly 2 are not — **cr3** (ISO-BMFF `ftyp`/`crx` box) and **raf** (proprietary header) — and both of those happen to already have rawler's `full_image()` override (`cr3.rs:399`, `raf.rs:433`). So the real fix is generic, not Fuji-specific: try (a), and when it fails, try (b) unconditionally — no per-format gating needed. This closes the tier-1 gap for CR3 and RAF both, for free.

## Scope

- Add one generic tier-1 fallback function, tried for any RAW format, not gated to RAF/Fuji.
- Wire it into both the wasm/web decode path and the native non-mac path (same shared code, per existing architecture).
- Leave `raw_fast_preview.rs`'s X-Trans demosaic bugs unfixed — add a `TODO` comment there recording the deferral and pointing at the roborev finding history (active-area coordinate mapping, per-phase black level, trailing-block phase mismatch), so it's not lost. It remains the correct fallback for any RAW file with no usable embedded image (corrupted file, no `full_image()` override for that format, decode error).

## Implementation

### 1. `src/thumbnail.rs` — new generic fallback function

Add a sibling to `embedded_preview_from_bytes`, same `#[cfg(not(target_os = "macos"))]` gate (mac's ImageIO path already handles this correctly and never touches `raw_fast_preview.rs`):

```rust
/// Generic fallback when the TIFF/JPEG-magic-based embedded thumbnail above
/// can't even open the container (CR3's ISO-BMFF wrapper, RAF's proprietary
/// header, ...): asks rawler's own `Decoder::full_image()` — a per-format
/// trait override (default `Ok(None)`) that reads whatever embedded
/// JPEG/preview that format's container carries, without touching the
/// CFA/sensor block or running any demosaic. Not gated to any specific
/// format — whichever decoder rawler recognizes the bytes as, this asks it
/// generically; formats with no override just get `None` and fall through
/// to the caller's next fallback.
///
/// Returns `None` on any failure (unrecognized format, decoder construction
/// failed, no embedded image, embedded blob doesn't decode) so callers can
/// always continue to `raw_fast_preview`/full-decode fallbacks unconditionally.
#[cfg(not(target_os = "macos"))]
pub(crate) fn rawler_full_image_from_bytes(bytes: &[u8], max_px: u32) -> Option<DecodedImage> {
    let run = std::panic::AssertUnwindSafe(|| -> Option<DecodedImage> {
        let source = rawler::rawsource::RawSource::new_from_slice(bytes);
        let params = rawler::decoders::RawDecodeParams::default();
        let decoder = rawler::get_decoder(&source).ok()?;

        let dynamic = decoder.full_image(&source, &params).ok().flatten()?;
        let img = dynamic.into_rgba8();
        let (w, h) = (img.width(), img.height());
        if w == 0 || h == 0 {
            return None;
        }

        // Orientation from raw_metadata(), not the embedded image's own EXIF
        // (may be absent or describe only the sub-image) — same source
        // nonmac_decode.rs/raw_fast_preview.rs's real_orientation already use.
        let orientation = decoder
            .raw_metadata(&source, &params)
            .ok()
            .and_then(|meta| meta.exif.orientation)
            .unwrap_or(1) as u8;

        let (nw, nh) = crate::image_decode::fit_within(w, h, max_px);
        let rgba = if (nw, nh) == (w, h) {
            img.into_raw()
        } else {
            image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Lanczos3).into_raw()
        };
        // Resize before orienting — same order embedded_preview_from_bytes uses.
        Some(crate::image_decode::apply_exif_orientation(
            DecodedImage { width: nw, height: nh, rgba, pixel_format: PixelFormat::Srgb8 },
            orientation,
        ))
    });
    std::panic::catch_unwind(run).ok().flatten()
}
```

Notes:
- No format gate (`FormatHint` etc.) — deliberately generic per the scope decision above.
- Every fallible step uses `.ok()`/`.ok().flatten()`, never `unwrap`/`expect`/`?` into a panicking context; wrapped in `catch_unwind` for defense-in-depth (mirrors `raw_fast_preview.rs`'s own convention — largely a no-op on `wasm32-unknown-unknown` since panic=abort there, but rawler's `full_image()` implementations use checked `Result`-returning byte reads, not raw slice indexing, so they're not panic-prone to begin with).

### 2. `src/web/wasm_worker.rs` — wire into `decode()`

In the `is_raw` branch, chain the new fallback between the existing baseline thumb attempt and the `raw_fast_preview` fallback:

```rust
if is_raw {
    if let Some(preview) = thumbnail::embedded_preview_from_bytes(bytes, max_px) {
        if preview.width.max(preview.height) * 2 >= max_px {
            return Ok(preview);
        }
    }
    if let Some(preview) = thumbnail::rawler_full_image_from_bytes(bytes, max_px) {
        return Ok(preview); // full-resolution embedded image; no "too small" check needed
    }
    return if quality {
        raw_fast_preview::decode_raw_quality_from_bytes(bytes, max_px)
    } else {
        raw_fast_preview::decode_raw_fast_from_bytes(bytes, max_px)
    };
}
```

Serves both `JobKind::Thumb` (grid) and `JobKind::Preview` (Loupe) since both flow through this same function. Update the doc comment to mention the new tier.

### 3. `src/thumbnail.rs` — wire into native non-mac path

```rust
#[cfg(not(target_os = "macos"))]
fn try_extract_embedded_preview(path: &Path, max_px: u32) -> Option<DecodedImage> {
    let bytes = fs::read(path).ok()?;
    embedded_preview_from_bytes(&bytes, max_px)
        .or_else(|| rawler_full_image_from_bytes(&bytes, max_px))
}
```

`thumbnail()`'s existing fallback to a full decode (`nonmac_decode::decode_raw_nonmac`) is unchanged, still catches every format/file where both embedded-image mechanisms return `None`.

### 4. `src/raw/fast_preview.rs` — TODO marker for the deferred X-Trans fix

Add a doc comment near `is_supported_xtrans_layout`/`downsample_xtrans_mosaic` recording:
- The three still-open roborev findings (active-area coordinate mapping in `map_xtrans_coord`, per-phase X-Trans black level lost through `apply_scaling`'s Bayer-only `correct_blacklevel_cfa`, trailing-block CFA-phase mismatch in the clamp).
- That this path is now only reached when a RAF file has no usable embedded image (rare — corrupted file, decode error) rather than on every request, lowering urgency but not correctness — still wrong when it does run.
- `// TODO: X-Trans demosaic still has open correctness bugs (see roborev job history on this file); deferred, not blocking since tier-1 embedded-preview fallback now covers the common case.`

### 5. `src/raw/probe.rs` — test coverage for the new generic fallback

Extend the existing DNG fixture-writer pattern (`write_linear_dng`/`write_bayer_dng`, using the file's own `push_entry`/`inline_u32`/`inline_u16`/`pad_to_even` helpers) rather than building a RAF/CR3 fixture — DNG's `full_image()` (`dng.rs:103`) is the cheapest to construct: it just needs a `SubIFDs`-referenced sub-IFD with `NewSubFileType=1`, `ImageWidth`/`ImageLength`, `SamplesPerPixel=3`, `BitsPerSample=8`, and an uncompressed RGB strip (read via `dynamic_image_from_ifd` in `decoders/mod.rs:512`, no JPEG compression needed) — no new dependency, no nested container.

- `write_dng_with_preview_subifd(...)` (or extend an existing writer with an optional preview sub-IFD) producing a small root IFD (CFA or linear, doesn't matter) plus one `NewSubFileType=1` preview sub-IFD.
- Test: `rawler_full_image_from_bytes` on that fixture returns `Some` with the expected preview dimensions/pixels.
- Negative test: existing `write_linear_dng`/`write_bayer_dng` fixtures (no preview sub-IFD) → `rawler_full_image_from_bytes` returns `None`, proving the function doesn't crash or misbehave when a format has no embedded image, and that the generic (non-gated) design doesn't accidentally succeed on the wrong thing.

## Verification

- `cargo test` — new probe.rs tests above, plus the existing regression suite (`cargo test --bin decode_probe --features raw-probe`) to confirm no behavior change for the Bayer-only path.
- Build check for `wasm32-unknown-unknown` (this is the primary target for the changed call sites) — `cargo build --target wasm32-unknown-unknown` or the project's existing wasm build script if one exists.
- Manual spot-check with any real RAW file on hand (CR2/NEF/ARW/DNG more likely available than RAF) through the actual app to confirm the new tier is reached and produces a correct, correctly-oriented preview — not required to gate merge, but worth doing once if a sample file exists.
