# src/decode: image decode/encode, ImageIO/CoreGraphics FFI, rawler glue

## Scope

All of `src/decode/` was read line by line, 8,335 lines in total.

| File | Lines |
|---|---|
| `src/decode/mod.rs` | 12 |
| `src/decode/coregraphics.rs` | 128 |
| `src/decode/decode_budget.rs` | 318 |
| `src/decode/image_encode.rs` | 1076 |
| `src/decode/image_decode/mod.rs` | 1381 |
| `src/decode/image_decode/raw_filter.rs` | 301 |
| `src/decode/image_decode/fit.rs` | 701 |
| `src/decode/image_decode/nonmac_decode.rs` | 447 |
| `src/decode/rawler/mod.rs` | 1139 |
| `src/decode/rawler/raw_preview.rs` | 1120 |
| `src/decode/rawler/probe/main.rs` | 37 |
| `src/decode/rawler/probe/nonmac.rs` | 1675 |

Cross-checked against these (read only): `vendor/rawler-0.8.0` (`rawsource.rs`, `rawimage.rs`, `imgop/develop.rs`, `imgop/raw.rs`, `decoders/mod.rs`), `kamadak-exif-0.6` `reader.rs`, `image-0.25.10` limits, `develop/parallel.rs`, the `jobs/loader.rs` panic recovery, and the callers of `encode_jpeg`, `capture_stamp` and `sort_meta`.

## Summary

The code is careful and consistently follows the CLAUDE.md rules.
- There are no `unwrap`, `expect` or `panic` calls outside tests.
- Every `unsafe` block has a SAFETY comment, and the ones I traced match the real invariants.
- Header-derived sizes are bounded by `check_decode_size`, `check_rawler_size_limit`, `Plan::peak_bytes` and `decode_budget`.
- The slice and segment walkers (`splice_app1`, `with_png_meta`, `jpeg_is_progressive`, `fuji_dynamic_range_gain`) use `get()` and checked math, and have truncation tests.
- The macOS build keeps rawler out through the `#[cfg(not(target_os = "macos"))] pub mod rawler` gate.
- CF object retain/release is balanced: every `CFRetained::from_raw` sits on a null-checked +1 "Create" result, and the borrowed dictionary values are type-checked with `CFGetTypeID` before casting.

The risks are in behaviour and performance, not memory safety. Five Medium findings:
1. The macOS `capture_stamp` TIFF fallback can never succeed.
2. `RawSource::new` prefaults the whole file on every metadata read, and kamadak-exif reads whole TIFF files on every sort pass.
3. The browser's RAW export and scoring decodes bypass `decode_budget`.
4. CR3 and RAF have no size pre-check, so a crafted header can make rawler attempt a huge allocation.

Count by severity: Critical 0, High 0, Medium 5, Low 12, Info 5.

## Findings

### [DECODE-01] macOS `capture_stamp` TIFF `DateTime` fallback looks in the wrong dictionary and always returns None — Severity: Medium — Confidence: High
- Location: `src/decode/image_decode/mod.rs:657-661`
- Evidence: `let t = dict_string(&props, key!(kCGImagePropertyTIFFDateTime))?;`. `props` is the top-level ImageIO property dictionary, but `kCGImagePropertyTIFFDateTime` is a key inside the `{TIFF}` sub-dictionary. The sibling readers `read_capture_time` (line 634) and `read_capture_date` (line 679) correctly go through `dict_dictionary(&props, kCGImagePropertyTIFFDictionary)` first. The non-mac `stamp_from_exif` falls back to `Tag::DateTime`/`OffsetTime` correctly.
- Impact: on macOS, any file with only IFD0 `DateTime` and no `DateTimeOriginal` (software-edited JPEGs, many TIFF and PNG exports, scans) gets no capture stamp. Exports then drop the date on macOS but keep it on Linux, Windows and web. Upload instants are lost too. The test `an_export_carries_its_capture_date_in_exif` only uses `DateTimeOriginal`, so nothing catches it.
- Suggested fix: look up the TIFF dictionary, then the key inside it, as `read_capture_time` does. Add a test that exports with only IFD0 `DateTime` set.

### [DECODE-02] Every RAW metadata read on Linux prefaults the whole file (`RawSource::new` uses `populate()`) — Severity: Medium — Confidence: Medium
- Location: `src/decode/rawler/mod.rs:564` (`fill_raw_metadata_from_path`) and `:300` (`decode_raw_nonmac`). The vendored `rawsource.rs:31` does `MmapOptions::new().populate().map(&file)`, then `advise(WillNeed)`.
- Evidence: `fill_raw_metadata_from_path` only needs headers, but `RawSource::new(path)` forces the kernel to read the entire RAW (25-80 MB) into the page cache on Linux. `read_metadata` runs per shown photo (`jobs/loader.rs:624`), and `app/geotag.rs:689` and `app/commands.rs:497` call it in bulk.
- Impact: opening an info panel, or a geotag pass over N RAWs, costs N full-file reads instead of N small header reads. This is a large I/O and latency regression on spinning disks and network shares.
- Related hazard: a mapping is also a SIGBUS risk if the file is truncated, or the card is pulled, mid-read. A SIGBUS kills the process and is not catchable by the panic guards. For a culling tool that is routinely run on SD cards, it is worth noting even though `populate` narrows the window.
- Suggested fix: for metadata, read the first N MB (or the TIFF IFD region) into a `Vec` and use `RawSource::new_from_slice`. Or add a lazy-mapping constructor in the vendored rawler. For full decodes, consider `read_capped` plus `new_from_shared_vec`.

### [DECODE-03] kamadak-exif `read_from_container` reads the whole file for TIFF-family files, and the sort pass runs it for every photo — Severity: Medium — Confidence: Medium
- Location: `src/decode/image_decode/nonmac_decode.rs:113-122` (`exif_sort_fields`), `:151-158` (`capture_stamp`), `:202-206` (`read_metadata`).
- Evidence: kamadak-exif 0.6 `reader.rs:141-143` reads the first 4096 bytes, and if `tiff::is_tiff` it does `reader.read_to_end(&mut buf)`, which is unbounded. `exif_sort_fields` skips only RAW extensions, so `.tif` and `.tiff` photos go through it. `sort_meta` is requested for the whole folder on load (`app/nav.rs:119`, `jobs/loader.rs:631`).
- Impact: on Linux and Windows a folder of 100-500 MB scanned TIFFs reads and holds every byte in RAM per decode worker just to learn the capture time. This bypasses the `MAX_PHOTO_BYTES` cap used by `read_photo`.
- Suggested fix: for TIFF, read only a bounded prefix (for example 16 MB, or walk the IFD with `Seek`) and call `read_raw`. Or skip EXIF in the sort pass for TIFF and fall back to mtime.

### [DECODE-04] Browser RAW export and RAW scoring decodes take no `decode_budget` grant — Severity: Medium — Confidence: Medium
- Location: `src/decode/rawler/mod.rs:294-360` (`decode_raw_nonmac*`). Callers: `src/export/mod.rs:335` (`bake_export_from_shared_vec`) and `src/scoring/detect.rs:231`. `src/web/web_decode.rs:219-222` acknowledges it: "that one takes no `decode_budget` grant".
- Evidence: `decode_raw_nonmac_from_source` runs `check_rawler_size_limit` and `normalize_linear_levels` but never calls `decode_budget::acquire`. `raw_preview::decode_raw_preview`, the Loupe path, does. An export decodes at full size (`max_dim = u32::MAX`), then `develop_intermediate` clones (per the comment at `mod.rs:396`). The peak is several hundred MB to over 1 GB at 24-45 MP.
- Impact: on wasm32 this is exactly the failure `decode_budget.rs` was written to prevent. One export racing up to six Loupe decodes that each hold part of the 1.5 GB budget can exhaust the shared 4 GB heap. The allocation failure aborts the thread and strands its memory.
- Suggested fix: take a grant in `decode_raw_nonmac_from_source` under `cfg(target_arch = "wasm32")`, sized from `raw.width * raw.height` (about 40-50 bytes per pixel for Export) and grown after the sensor size is known, as `decode_raw_preview` does. Alternatively route the browser export through `raw_preview`.

### [DECODE-05] CR3 and RAF have no pre-decode size check, so a crafted header can reach an unbounded allocation in rawler — Severity: Medium — Confidence: Medium
- Location: `src/decode/rawler/mod.rs:21-42` (the doc comment states the gap) and `:74-83`.
- Evidence: "Containers that aren't TIFF (CR3, RAF) have no size tags to read here and pass unchecked." The comment also says 0.8.0 "tries the allocation, which wraps or aborts". `check_rawler_size_limit` is the only guard before `rawler::decode`.
- Impact: Rust allocation failure is an abort, not a panic, so `catch_unwind` and the loader's panic recovery do not help. A 100-byte crafted RAF/CR3 can take down the whole native app or a wasm worker, which breaks the "malformed input never crashes" rule. Native only fails this way when the allocation is refused. On Linux with overcommit it may succeed lazily and then thrash.
- Suggested fix: add a RAF size read next to `fuji_dynamic_range_gain` (the RAF header already carries the sensor dimensions). For CR3, read the `CRAW`/`CMT` size from the ISO-BMFF boxes. Or cap the file-size-to-pixel ratio as a sanity bound until then.

### [DECODE-06] `offset_seconds` accepts embedded signs, so malformed offsets pass validation — Severity: Low — Confidence: High
- Location: `src/decode/image_decode/mod.rs:577-588`.
- Evidence: after stripping the leading sign, `h` and `m` are parsed as `i64` and only `h <= 14 && m < 60 && rest.len() == 5` is checked. `"+-5:00"` (`rest = "-5:00"`, length 5), `"+05:-5"` and `"+5:+0"` all parse and pass, and `CaptureStamp::new` keeps the original string.
- Impact: a garbled `OffsetTimeOriginal` is written into the export's EXIF verbatim as `+-5:00`. `instant()` would also compute a nonsense offset for uploads.
- Suggested fix: require `h >= 0 && m >= 0` (or parse as `u8` and require two ASCII digits on each side). Add `"+-5:00"` to `an_out_of_range_exif_year_or_offset_is_rejected_without_overflow`.

### [DECODE-07] `decimate_linear_rgb` (cpp==3 DNG, wasm tiers) never applies the default crop — Severity: Low — Confidence: Medium
- Location: `src/decode/rawler/raw_preview.rs:928-1033`.
- Evidence: `demosaic_cfa` explicitly applies `raw.crop_area` ("Skipping it lets dark edge pixels into the mip chain"). `decimate_linear_rgb` uses only `active_area` and has no `crop_area` handling. Native `RawDevelop` runs `CropDefault` in `LINEAR_DEVELOP_STEPS`, and `developed_size` reports the cropped size.
- Impact: for a Linear DNG with a `DefaultCropSize` smaller than the active area (phone ProRAW and Samsung linear DNGs often have one), the browser preview has a different size and aspect than the `source_size` the Loupe was told, and includes edge pixels. The Loupe keeps its zoom across tiers, so there will be a visible jump.
- Suggested fix: apply `default_crop(raw, area)` to the area before decimating, as the Bayer path does.

### [DECODE-08] `build_cam2rgb` picks an arbitrary illuminant when there is no D65 matrix, and skips Bradford adaptation — Severity: Low — Confidence: Medium
- Location: `src/decode/rawler/raw_preview.rs:530-550` (`.or_else(|| raw.color_matrix.iter().next())`).
- Evidence: `color_matrix` is a `HashMap<Illuminant, _>` (`rawimage.rs:245`), so `iter().next()` is nondeterministic across runs. Rawler's own `RawDevelop` (`imgop/develop.rs:264-300`) uses a fixed priority list and Bradford-adapts a non-D65 matrix to D65.
- Impact: for a file with only A or D50 matrices, the browser Fast and Quality tiers can differ in color from run to run and from the native develop. An empty matrix `Vec` gives an all-zero `xyz2cam`, and `pseudo_inverse` then yields NaN (a division by zero).
- Suggested fix: reuse `color_matrix_find_first` with the same priority list, adapt as rawler does, and return `None` for fewer than 9 values.

### [DECODE-09] The wasm Bayer and RGB preview paths reject sample buffers longer than `w*h`, while `bayer_mosaic` and the native half-res path tolerate them — Severity: Low — Confidence: Medium
- Location: `src/decode/rawler/raw_preview.rs:742` and `:953` (`data.len() != …`). Compare `:271-276`, where `bayer_mosaic` accepts `samples >= n`, and `rawler/mod.rs:416-420`, which truncates a longer buffer.
- Evidence: `bayer_mosaic` documents "at least this many", and a test (`surplus_samples_bin_without_panicking`) covers the native develop. The wasm path then returns `None`, which surfaces as "unsupported RAW layout".
- Impact: a RAW whose decoder returns a padded buffer decodes natively but fails in the browser. This is a platform divergence.
- Suggested fix: `data.truncate(width * height)` (or `>=`) before the check, as `half_res_image` does.

### [DECODE-10] macOS `encode_jpeg` writes in place, unlike the non-mac version which uses `write_atomic` — Severity: Low — Confidence: High
- Location: `src/decode/image_encode.rs:36-89` (macOS) versus `:153-165` (non-mac, `write_atomic`).
- Evidence: the macOS path goes through `CGImageDestination::with_url(&url, …)` then `finalize` directly on `out`. The thumbnail cache caller works around it with a `.tmp` file plus rename (`jobs/thumbnail.rs:509-525`), but the function's doc comment does not say so. The other macOS callers are mostly tests.
- Impact: CLAUDE.md requires atomic writes. A new caller on macOS gets a partial file after a crash or a full disk, and the two platforms disagree.
- Suggested fix: write via `encode_imageio` into memory (`CGImageDestination::with_data`) and `write_atomic`, as the other formats already do, and drop the caller workaround.

### [DECODE-11] `raw_filter::decode` cannot detect a failed CoreImage render and returns an all-black image as a success — Severity: Low — Confidence: Low
- Location: `src/decode/image_decode/raw_filter.rs:158-186`.
- Evidence: `render_toBitmap_rowBytes_bounds_format_colorSpace` returns `()`. The buffer is zeroed first by `pixel_buffer`. After `clamp_to_display_range` every pixel's alpha must be exactly 1.0 (`0x3C00`).
- Impact: a GPU reset, out-of-memory or lost context during the render leaves a valid-looking all-zero `TonedF16` image, which the loader then caches as the photo's Loupe image.
- Suggested fix: after the render, check that the last pixel's alpha half is 1.0. Return `Err` otherwise, so the ImageIO fallback runs.

### [DECODE-12] `apply_exif_orientation` is slow and memory-hungry on large images, and orientation logic is implemented twice — Severity: Low — Confidence: Medium
- Location: `src/decode/image_decode/mod.rs:905-942` and `src/decode/rawler/raw_preview.rs:402-444`.
- Evidence: the first allocates a second full-size buffer (`vec![0u8; len]`) and does `d % nw` and `d / nw` per pixel, so a 100 MP image costs 200M integer divisions. It also ignores the `spare` buffer reuse that the rest of the decode path uses. The second is a separate flips-then-transpose implementation with different structure and no shared test.
- Impact: on macOS an EXIF-rotated camera JPEG pays an extra full-resolution buffer and a non-vectorised loop. I checked the math for all eight orientations and it is correct, so this is not a correctness bug.
- Suggested fix: iterate rows and columns instead of dividing, and consider an in-place or buffer-reusing path. Share one implementation, or add a cross-check test that the two agree for 1..=8.

### [DECODE-13] HEIC/HEIF are listed as photos on every platform, but only macOS can decode them — Severity: Low — Confidence: Medium
- Location: `src/decode/image_decode/mod.rs:49-51` (`IMAGE_EXTS`), used by `navigation.rs:338`, `web/web_fs.rs:439` and MCP `headless.rs`.
- Evidence: CLAUDE.md says "HEIC … macOS-only", and the non-mac decoder is the `image` crate with `jpeg`, `png` and `tiff` features only. `is_image_extension` does not depend on the platform.
- Impact: on Linux, Windows and the browser a folder with HEIC files shows photos that always fail to decode (error tiles). Conversely, exported WebP and AVIF are not in the list, so they cannot be reopened.
- Suggested fix: gate `heic`/`heif` with `cfg!(target_os = "macos")` in the list, or confirm the failure UX is intended.

### [DECODE-14] `normalize_linear_levels` divides by zero positions if the black-level vector is empty — Severity: Low — Confidence: Low
- Location: `src/decode/rawler/mod.rs:174-178`.
- Evidence: with `black.len() == 0`, `0 % cpp == 0` passes `is_multiple_of`, `positions = 0`, and the mean is `0.0 / 0.0 = NaN`, giving NaN black levels. rawler's own `BlackLevel::default` holds one zero level, so this requires a decoder to produce an empty vector, which I could not show happens.
- Suggested fix: add `black.is_empty()` to the reject condition. It costs nothing and makes the function total.

### [DECODE-15] `decode_raw_via_rawler` is `pub` and bypasses the size cap and levels normalisation — Severity: Low — Confidence: High
- Location: `src/decode/rawler/mod.rs:258-261`.
- Evidence: it calls `rawler::decode_file(path)` directly, with no `check_rawler_size_limit` and no `normalize_linear_levels`. Only the `decode_probe` binary calls it.
- Impact: a future caller gets the exact hazards the module's comments describe. The probe itself will also OOM or panic on a crafted file.
- Suggested fix: add the two guards inside it, or mark it `#[doc(hidden)]` with a comment saying it is probe-only.

### [DECODE-16] Misplaced doc comments and `#[hotpath::measure]` in `nonmac_decode.rs` — Severity: Low — Confidence: High
- Location: `src/decode/image_decode/nonmac_decode.rs:21-35`.
- Evidence: the "Decodes `path`, downscaling …" doc block, `#[cfg(...)]` and `#[hotpath::measure]` all precede a second doc block, and all attach to `decode_reusing`. `decode` itself has no docs and no hotpath measurement.
- Impact: misleading rustdoc, and the hotpath profile reports `decode_reusing` instead of `decode`. This is the kind of comment mistake that hides real behaviour.
- Suggested fix: move the first doc block and the attributes to `decode`.

### [DECODE-17] 32-bit `usize` arithmetic on input-derived sizes in wasm32 helper loops — Severity: Low — Confidence: Low
- Location: `src/decode/rawler/raw_preview.rs:459-464` and `:502-507` (`(oy + 1) * h`, `y * w + x`), and `src/decode/image_encode.rs:138-139` (`width as usize * 4`, `bytes_per_row * height as usize`, in `encode_jpeg_to_vec`, which wasm32 uses).
- Evidence: on wasm32, `usize` is 32 bits. `check_decode_size` bounds only `w*h` (500 MP), not `h * out_h`. A crafted very tall image (for example 1000 x 70000) with `out_h` close to `h` can wrap `oy * h` in release builds. `encode_jpeg_to_vec` uses unchecked multiplications where `Pixels::check` uses `checked_mul`.
- Impact: this needs pathological dimensions and gives a wrong index, which panics and aborts the wasm thread. The risk is low, but the fix is cheap.
- Suggested fix: use `u64` for the row boundary math or `checked_mul`. Make `encode_jpeg_to_vec` call `Pixels::check`.

### [DECODE-18] `fit_within(w, h, 0)` returns `(0, 0)`, which several consumers do not expect — Severity: Info — Confidence: High
- Location: `src/decode/image_decode/mod.rs:1017-1025`.
- Evidence: `.max(1).min(max_dim)` with `max_dim = 0` gives 0. The mac path rejects it ("target dimensions must be non-zero"). The rawler paths then run `box_downsample_rgb` with `out_w = 0` (division by zero in `ox * w / out_w` is avoided only because the loop is empty), and `Plan::new` computes a factor of 256 for a 0x0 output.
- Impact: nothing reaches this today (callers pass real sizes), but it is a latent edge.
- Suggested fix: treat `max_dim == 0` as an error in `fit_within`, or clamp to 1.

### [DECODE-19] Cross-platform metadata and capture-time semantics differ — Severity: Info — Confidence: Medium
- Location: `nonmac_decode.rs:94-147` versus `mod.rs:593-637`, and `rawler/mod.rs:670-674`.
- Evidence: mac `sort_meta` takes capture time from EXIF `DateTimeOriginal` then TIFF `DateTime`, for RAW too. Non-mac takes `DateTimeOriginal` only, and sends RAW straight to mtime. The non-mac RAW `capture_date` falls back to `create_date`, but the non-mac JPEG falls back to IFD0 `DateTime`.
- Impact: burst grouping and date sorts can differ between platforms for the same folder. Documented for RAW, not for the others.

### [DECODE-20] Tests: several assert too little or can never fail — Severity: Info — Confidence: High
- Orientation tests (`mod.rs:1296-1380`) use only a 2x1 image. Orientations 5 and 6 produce the same result for a one-row image, and 7 and 8 do too, so a swapped 5/6 or 7/8 would pass. Orientations 3, 4, 5 and 7 are untested. Verify with a 3x2 asymmetric image for all eight.
- `decodes_known_image_to_nonblank_rgba` (`mod.rs:1203`) silently returns when `/tmp/iv-test/a.png` is missing, which is every machine but the author's.
- The golden-hash tests in `probe/nonmac.rs` (`:1160`, `:1199`) pin f32 pipeline output to a hash. rawler uses `multiversion` SIMD (AVX2, SSE, NEON), so a CPU-dependent last-bit difference could break CI on a different runner. They also assert implementation rather than behaviour.
- Two `#[ignore]` diagnostics hardcode `/Users/andyyao/Desktop/...` (`probe/nonmac.rs:1501`, `:1649`).
- Not covered at all: `decimate_linear_rgb` with a crop, the X-Trans path, the positive `embedded_image_from_bytes` path (no RAF/CR3 fixture), the macOS TIFF-`DateTime` capture stamp (DECODE-01), `build_cam2rgb` fallback, and `decode_budget` behaviour on a panicking holder.

### [DECODE-21] `decode_probe` fixture directory is a predictable path in the shared temp dir — Severity: Info — Confidence: Medium
- Location: `src/decode/rawler/probe/nonmac.rs:33-37` (`temp_dir().join("lightphotos-decode-probe")`), `write_linear_dng` using `File::create`.
- Impact: on a shared Linux machine another user can pre-create that path as a symlink, and `File::create` follows it. This is a dev tool, so the risk is small.
- Suggested fix: use a process-unique directory, as the tests do.

## Test coverage assessment

Strong:
- Header and size bombs (`check_decode_size`, `check_rawler_size_limit`, the 65535x65535 PNG test).
- Truncation fuzz for the RAF header, the JPEG APP1 splice and the PNG chunk walker.
- Block-shrink maths, with edge crop and alpha weighting.
- The decode budget: ordering, growth, dead-thread release.
- A synthetic DNG fixture suite for the rawler glue.
- Several explicit "returns Err instead of panicking" tests for rawler layouts.

Weak or missing:
- Orientation (DECODE-20), and the macOS-only ImageIO paths: `read_metadata`, `sort_meta`, GPS, and HEIC are exercised only in the app, not unit-tested here.
- No test for the CFURL, bitmap-context and CGImage ownership code beyond round-trip smoke tests.
- The `raw_filter` tests need a real RAW from an environment variable and are skipped in CI.
- Platform-divergence checks: the two orientation implementations, and the mac and non-mac capture-time rules, are not tested against each other.

## Things done well / verified-safe areas

- **FFI**:
  - `coregraphics.rs`: the bitmap-context buffer contract is documented on the `unsafe fn`, and every caller passes `rows * bytes_per_row`.
  - The ownership order in `Prepared::cg_image` (context dropped before its buffer) is right for `CGBitmapContextCreateImage`'s copy-on-write behaviour.
  - `dict_*` helpers and `read_orientation` check `CFGetTypeID` before every cast, which defends against crafted files.
  - The CIRAWFilter decode wraps work in an `autoreleasepool` and checks `u16` alignment before the render.
- **Budgeting**:
  - `fit.rs` sizes the wasm grant from the header before any allocation, and `Plan::peak_bytes` tracks the real pipeline (tested).
  - The decode budget's FIFO ticket plus single-grower design avoids both starvation and mutual-wait deadlock, and handles poisoning.
- **macOS must not contain rawler**: `decode/mod.rs` gates the module, and all other rawler references in this directory live under `rawler/` or non-mac-only files. `Cargo.toml` puts `rawler` in the non-mac dependency table, and `[patch.crates-io]` is not a dependency edge.
- **Color**: embedded ICC profiles are converted to sRGB on non-mac (moxcms), with a parse-failure fallback and a test. TIFF and WebP export carry an explicit sRGB profile, and PNG gets an `sRGB` chunk only when the encoder named no colour space.
- **No `std::fs::write`** and no unwraps in non-test code in this directory.

## Open questions for maintainers

1. HEIC on macOS: does `CGImageSourceCreateImageAtIndex` return pixels with the HEIF `irot` transform already applied? If so, `decode_imageio`'s `apply_exif_orientation(read_orientation(...))` rotates twice for rotated HEICs. This is easy to check with a portrait iPhone HEIC. The test suite has none.
2. Is the premultiplied-alpha (macOS) versus straight-alpha (elsewhere) difference documented on `DecodedImage` handled consistently by the Loupe and thumbnail upload for transparent PNGs? It looks intentional but is easy to get wrong.
3. DECODE-04: is it deliberate that the browser export and scoring decodes bypass the budget (for example because only one export runs at a time)? The `web_decode.rs` comment suggests it was considered, but nothing bounds Loupe decodes against it.
4. Should `RawSource::new` (populate plus mmap) be replaced for the metadata-only paths? A cheap alternative is to patch the vendored `rawsource.rs`, which already carries local patches.
5. Is there a plan for a CR3 and RAF size pre-check? The doc comment on `check_rawler_size_limit` lists it as a known gap.
