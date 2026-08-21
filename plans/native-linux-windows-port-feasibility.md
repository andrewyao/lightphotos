# Research: LightPhotos as a native Linux/Windows port

**Status: open question on scope, decided on HEIC. Not executable — no checklist here.**
Written 2026-08-21.

## Context

LightPhotos today is macOS-only end to end — not by feature gap but by
construction. Zero `cfg(target_os)` gating exists anywhere in the codebase;
every objc2/AppKit/ImageIO/Vision dependency is unconditional
(`Cargo.toml:34-63`, `main.rs:26-50`). The ask started narrow ("make the
Mac-specific part optional to compile") and clarified to the real goal: run
on Linux/Windows.

Those are different sizes of job, because the mac-specific code splits into
two clusters that are not equally "optional":

1. **Genuinely peripheral** — `macos_delegate.rs` (Finder open-doc hook),
   `trash.rs` (NSFileManager Trash), and the Vision-feature cluster
   (`vision.rs`, `featureprint.rs`, `facequality.rs`, `segmentation.rs` —
   duplicate refinement, face/blink scoring, subject-segmentation overlay).
   These can be `cfg(target_os = "macos")`-gated with stub or alternate
   implementations elsewhere, no rewrite of core logic required.
2. **Load-bearing, not optional** — `coregraphics.rs`, `image_decode.rs`,
   `image_encode.rs`, `thumbnail.rs`. This is the *only* decode/encode/
   thumbnail path in the app, consumed by `loader.rs`, `renderer.rs`,
   `export.rs`, `app/*`, `ui/loupe.rs`. Cfg-gating this cluster out doesn't
   make it optional — it makes the app unable to open a photo on
   Linux/Windows. Making it actually work there means replacing it with a
   cross-platform codec stack, which is the real work and the only place
   this memo carries technical risk.

The repo already has `plans/web-wasm-port-feasibility.md`, which mapped this
exact "decode wall" problem for a *browser* port and settled on a
verify-before-committing approach rather than committing to a rewrite on
priors. This memo repeats that discipline for a **native** Linux/Windows
port, which starts from a materially better position than WASM — no 4GB
linear-memory ceiling, no `SharedArrayBuffer`/Worker constraints, real
native codec libraries available — but shares the same core question:
does a cross-platform decode/encode/thumbnail backend match ImageIO's
speed and fidelity closely enough to keep culling fast.

## Verdict

**The renderer, event loop, and every pure-logic module port at
near-parity — this half is genuinely easy on native, easier than the WASM
case. The decode/encode/thumbnail backend is 100% new work with real
format-coverage and fidelity risk, concentrated in RAW; HEIC is out of
scope by decision, not measurement.**

| | Native Linux/Windows parity? |
|---|---|
| Renderer, egui chrome, zoom/pan/develop sliders | **Yes — wgpu/winit/egui already target these platforms as primary, not secondary** |
| JPEG/PNG/TIFF decode+encode | **Yes, with a backend swap — mature crates exist** |
| RAW decode | **Probably — `rawler` is production-proven (see RapidRAW below), fidelity/coverage still needs its own check** |
| HEIC | **Not attempted for v1 — decided out of scope, see below** |
| Vision-based features (subject mask, face/blink, duplicate featureprint) | **No equivalent shipped for v1 — deferred, mac-only in the meantime** |

## What ports at parity

**The renderer.** `renderer.rs`, `shader.wgsl` compile through wgpu to
Vulkan (Linux) and DX12 (Windows) essentially unchanged — these are wgpu's
*primary* targets, more mature than its WebGPU backend. Zoom/pan stays a
uniform write, never a re-decode, exactly as on mac today.

**The egui chrome and winit event loop.** Both are already cross-platform
first-class targets; nothing here is mac-specific except the Finder
open-document hook (`macos_delegate.rs`, addressed below).

**All the pure logic.** `develop.rs`, `image_ops.rs`, `burst.rs`,
`sharpness.rs`, `phash.rs`, `duplicates.rs` (logic), `hash.rs`,
`navigation.rs` have no platform dependency and need no changes. Native
Linux/Windows is the *easy* case for this half of the app — no WASM memory
ceiling, no worker-spawn tax, real OS threads already what `loader.rs`
uses today (`available_parallelism() - 2` sizing translates directly).

## The decode wall

Per-format backend candidates, and the concrete answer to "how do we
replace core decode/encode":

### JPEG — mature options, need to pick an encoder

Decode: `zune-jpeg` (pure Rust) or the `image` crate (uses zune-jpeg as of
0.25) — low risk either way. Encode: `mozjpeg-rs`, a pure-Rust
reimplementation of mozjpeg with no C toolchain dependency, giving quality
parity with libjpeg-turbo — this is what RapidRAW (see Prior art) ships,
preferred over C bindings (`mozjpeg-sys`) for build simplicity. The `image`
crate's built-in JPEG encoder is a fallback with a lower quality ceiling.

### PNG/TIFF — low risk

`image` crate (backed by `png`/`tiff`) — mature, no open question here.

### Fast thumbnails — not free from any library

ImageIO's speed trick (`CGImageSourceCreateThumbnailAtIndex` pulling an
embedded preview instead of decoding the full image) has no drop-in
equivalent. It requires hand-rolled JFIF/EXIF/HEIF marker parsing
(`kamadak-exif` or `img-parts` as a starting point) to locate and extract
the embedded preview. RapidRAW confirms this is real, necessary work, not
something a crate hands you for free (see Prior art).

### RAW — the real open question

`rawler` (pure Rust, actively maintained) is the strongest candidate,
validated by RapidRAW shipping it in production covering ~40 RAW formats.
Alternative is `libraw-rs`/libraw C bindings — broader/more battle-tested
format coverage, but a real system-dependency and packaging burden,
especially on Windows (no system package manager norm, means vendoring or
requiring a build toolchain).

Fidelity risk applies regardless of library choice: RAW formats are largely
reverse-engineered and undocumented (Canon CR2, for instance). The WASM
memo raised this same caveat against Photopea's own "opens ~95% of files"
admission. A culling tool pointed at a whole folder surfaces the failing
tail as visibly broken frames, not a rounding error — this needs its own
verification pass before committing (see Verification below), not an
assumption that `rawler`'s coverage is good enough.

### HEIC — decided, mac-only for v1

LightPhotos supports HEIC today via ImageIO (`image_decode.rs:3`,
`navigation.rs:11`, advertised in `README.md:73`). The port keeps that
working on macOS exactly as-is and does not extend it to Linux/Windows.

No pure-Rust HEIC decoder exists; the only real option is `libheif-rs`
bindings to libheif + libde265/dav1d, which drags in a genuine
system-dependency and Windows-packaging cost for a format most non-mac
photo libraries won't be dominated by (HEIC's primary source is iPhone
capture). RapidRAW — a shipping competitor solving the same problem —
ships with **zero** HEIC/HEIF support on any platform, including macOS
(confirmed by a repo-wide grep of their source; no heic/heif string
anywhere). LightPhotos' position is easier than theirs, since HEIC already
works on mac and isn't being added — just not carried to non-mac. This is
a decision, not something the verification pass below needs to settle;
revisit only if real user demand shows up post-launch.

## No cross-platform equivalent (deferred, not blocking)

- **Vision cluster** (`segmentation.rs`, `facequality.rs`,
  `featureprint.rs`) — candidate replacement is ONNX Runtime via the `ort`
  crate (mature on native, more so than the WASM ONNX-Runtime-Web story)
  with a U²-Net/MODNet-class model for subject segmentation, a
  face-landmark model for blink scoring, and an embedding model for
  duplicate feature-prints. Real work: model download/bundling, re-tuned
  thresholds calibrated against Apple's Vision scores in `burst.rs`/
  `duplicates.rs`. Recommend `cfg(target_os = "macos")`-gating this cluster
  and shipping it as a later phase — subject-selection overlay, face/blink
  scoring, and duplicate refinement stay mac-only in the meantime.
- **`trash.rs`** — clean answer, unlike the browser case (no OS-level
  equivalent to a soft-delete for the File System Access API). The
  cross-platform `trash` crate supports both Windows Recycle Bin and the
  Linux/freedesktop trash spec, and is what RapidRAW ships (see Prior
  art). Straightforward swap.
- **`macos_delegate.rs`** — mac-only Finder "open document" integration.
  Linux/Windows "open with" already works through the existing
  CLI-path-argument flow the binary supports
  (`lightphotos /path/to/photo.jpg`), so this module can simply be
  `cfg(target_os = "macos")`-gated with no replacement needed for v1.

## Cfg strategy

`[target.'cfg(target_os = "macos")'.dependencies]` in `Cargo.toml` for the
existing objc2 crates, and symmetrically
`[target.'cfg(not(target_os = "macos"))'.dependencies]` for the new
cross-platform crates (`image`, `mozjpeg-rs`, `rawler`, `ort`, `trash`,
...; no `libheif-rs` — HEIC is out of scope for non-mac). Pair with
`#[cfg(target_os = "macos")]` / `#[cfg(not(target_os = "macos"))]` on the
two implementations behind a new `ImageBackend`-style trait covering
decode/encode/thumbnail. Idiomatic Rust per-platform compilation — no
Cargo feature flag needed for the OS split itself.

This gives two properties worth calling out explicitly:
- **Mac bundle is unaffected.** Cross-platform crates never enter the
  dependency graph for a macOS build — `scripts/bundle.sh` output is
  unchanged in size and composition. `rawler`/`ort`/`mozjpeg-rs` are
  Linux/Windows-only weight.
- **Mac behavior is unchanged.** The macOS implementation stays the
  existing ImageIO/CoreGraphics/Vision code, untouched — not reimplemented
  against the new trait's semantics. Same speed, same fidelity, same
  feature set including HEIC and the Vision cluster.

This cfg split alone (with no decode-backend work) is also what satisfies
the *original, narrower* ask — "Mac-specific compilation optional" in the
sense of a clean non-mac `cargo build` — and can land as Phase 1
independent of everything else in this memo.

## Recommendation: phased path

1. **Cfg-gate the peripheral cluster.** `macos_delegate.rs`, `trash.rs` →
   cross-platform `trash` crate, Vision cluster → mac-only stub. Gets a
   clean non-mac `cargo build`. Zero decode risk, ships regardless of what
   Phase 2 decides.
2. **Introduce the `ImageBackend` trait and cross-platform implementation.**
   JPEG/PNG/TIFF via `image`/`zune-jpeg` + `mozjpeg-rs`, RAW via `rawler`,
   no HEIC path on non-mac. This is the actual "runs on Linux/Windows"
   work and carries all the real risk in this memo.
3. **Defer Vision-cluster ONNX replacement** to a later pass; those
   features stay mac-only until then.

## Verification — what to measure before committing to Phase 2

1. **Native macOS baseline.** `loader.rs` already has the instrumentation
   (`report_decode` behind `timing_enabled()`). Run the release build
   against a representative JPEG/RAW folder, capture preview/full/thumb
   timings per format — this is the bar everything else is measured
   against.
2. **Build a standalone decode/encode benchmark**, following the existing
   `seg_probe`/`face_probe` pattern in `src/bin/` — a separate binary, no
   app state — that exercises the candidate crates (`rawler` RAW decode,
   `mozjpeg-rs` JPEG encode, marker-parse embedded-preview extraction) on
   the same folder.
3. **Compare throughput and visual fidelity against step 1.** RAW is the
   only format where this decides scope: if `rawler`'s coverage/fidelity
   on your actual camera formats lands close to ImageIO's output, Phase 2
   proceeds as scoped; if specific formats fail or look visibly wrong,
   scope those out of v1 rather than accepting silently broken frames.
   HEIC is not part of this measurement — it's already decided out of
   scope.

Independent of the above, spot-check the wgpu color-management risk
surfaced by RapidRAW (next section) on the current `wgpu = "29"` version
before or during the port, since it's unrelated to the decode-backend
question but affects the same renderer code path.

## Prior art: RapidRAW (validation, not a template)

[RapidRAW](https://github.com/CyberTimon/RapidRAW) (AGPL-3.0, local clone
checked at `../RapidRaw`) is a shipping Rust RAW editor targeting
Windows/macOS/Linux — close enough in problem shape to be a real data
point. It's Tauri + web frontend, not native egui/wgpu, so only its
decode/encode backend choices are a useful comparison, not its UI
architecture. Checked to validate/correct this memo's crate picks before
committing to them — not copied.

**Confirms:**
- **RAW via `rawler`** (their own fork, `RapidRAW-DngLab`) — covers ~40
  RAW formats in production. Validates this memo's RAW candidate directly.
- **Embedded-preview extraction is hand-rolled**, not free from a library:
  `rawler::analyze::extract_preview_pixels` plus their own
  `largest_tiff_jpeg_preview` TIFF/JPEG marker scan
  (`image_loader.rs:212,305` in their repo), with fallback to a full
  decode if extraction panics. Matches this memo's conclusion above.
- **`cfg(target_os)` dependency split**, same shape as recommended here —
  their mac-only `objc` crate is isolated under
  `[target.'cfg(target_os = "macos")'.dependencies]`.
- **Cross-platform trash via the `trash` crate** — same pick as above.

**Corrects/adds:**
- **JPEG encode: `mozjpeg-rs`, not `mozjpeg-sys`.** RapidRAW uses the
  pure-Rust reimplementation rather than C bindings, avoiding a C
  toolchain dependency in the build for the same quality target — this
  memo's recommendation above already reflects that correction.
- **wgpu color-management risk, previously unknown to us.** Their
  `Cargo.toml` pins `wgpu = "29.0"` with the comment "Downgraded to
  prevent P3 color shifts on Apple devices" — a real cross-platform wgpu
  color regression on a version LightPhotos is also currently on. Worth
  checking independently of the decode-backend work, since it's a
  renderer-level risk, not a codec one.
- **HEIC gap, now evidenced rather than assumed.** RapidRAW ships with
  zero HEIC/HEIF support on any platform, including macOS. LightPhotos'
  situation is easier — HEIC already works on mac via ImageIO and isn't
  being added, just not extended to non-mac. Decided above, not left open.

## Files referenced

- `plans/web-wasm-port-feasibility.md` — structural template, reused
  analysis for the parity-porting modules
- `Cargo.toml:12-22` (probe bin targets), `:34-63` (objc2 deps)
- `main.rs:26-50` (mod list), `:415-426` (`macos_delegate` wiring)
- `src/coregraphics.rs`, `src/image_decode.rs`, `src/image_encode.rs`,
  `src/thumbnail.rs` — doc comments state "no third-party codecs" today
- `src/loader.rs` — existing `report_decode`/`timing_enabled()`
  instrumentation, reusable for the verification benchmark
- `../RapidRaw/src-tauri/Cargo.toml`, `../RapidRaw/src-tauri/src/{formats.rs,image_loader.rs}`
- `README.md:5,8,15,73` / `CLAUDE.md:7,26,40,52` — will need updating once
  a phase actually ships; not part of this memo
