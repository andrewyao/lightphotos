# Research: LightPhotos as Rust + WASM in the browser

**Status: open question, not a decision. Not executable — no checklist here.**
Written 2026-08-12.

Read the *Verification* section first and the *Recommendation* second. The
recommendation is a prior, not a conclusion — the decode harness described at the
bottom is what actually settles this, and it has not been run.

---

## Context

LightPhotos today is a macOS-native culling tool whose speed comes from three
specific bets: Apple ImageIO does decode-at-size and embedded-preview extraction
so thumbnails never cost a full decode; a worker pool keeps decode off the UI
thread; and images live as GPU textures so zoom/pan/develop only rewrite a small
uniform. The question is whether a Rust+WASM web port — with the user granting
File System Access to a local photo folder — holds the same speed.

Scope assumptions, as answered when this was written:

- **Browsers: Chrome/Edge only.** They are the only engines with
  `showDirectoryPicker`, so local-folder browsing genuinely works. This also
  removes the WebGPU fallback question.
- **Library: JPEG + iPhone HEIC + camera RAW** (CR2/NEF/ARW/DNG).
- **Deliverable: feasibility memo.** No code.

---

## Verdict

**Interaction speed ports at parity. Sustained decode throughput is the open
risk, and with HEIC and RAW in the library that is the part that decides it.**

Note on framing: every format here is *decodable* in a browser — Photopea decodes
RAW today, and libheif compiles to WASM. Feasibility is not the question. The
question is whether decode sustains the throughput culling demands, where the Mac
app is leaning on hardware and embedded previews that the browser will not hand
you for free.

The thing users *feel* as "fast" in a culling tool splits in two, and the halves
have different answers:

| | Web parity? |
|---|---|
| Zoom, pan, rotate, develop sliders, grid scroll, filmstrip | **Yes — essentially 1:1** |
| Grid thumbnails, all formats (via embedded previews) | **Close — if you extract previews yourself** |
| Time from arrow-key press to a sharp full image | **Unproven; HEIC is the weak point** |

Everything downstream of "pixels are already in a GPU texture" is a near-verbatim
port. Everything upstream of it — getting bytes off disk and turned into pixels —
is where macOS is doing work the browser will not do for you.

---

## What ports at parity

**The renderer.** `renderer.rs`, `shader.wgsl`, `mipgen.wgsl` compile to WebGPU
through wgpu essentially unchanged — that is wgpu's entire reason for existing.
`write_texture`, mip chains, the viewport-confined draw, the transform uniform:
all present in WebGPU. Zoom/pan stays a uniform write, never a re-decode. **This
is the strongest part of the story and it is the part users perceive as
responsiveness.**

**The egui chrome.** egui/egui-wgpu run on web as a first-class target. Grid,
filmstrip, filter bar, develop panel, rating overlays port with minimal change.

**All the pure logic.** `develop.rs`, `image_ops.rs`, `burst.rs`, `sharpness.rs`,
`phash.rs`, `duplicates.rs`, `hash.rs`, `navigation.rs` have no platform
dependency. `develop.rs`'s `apply_linear` CPU mirror and the WGSL tone pipeline
both survive, so the histogram/shader agreement invariant holds. Note
`sharpness.rs` (variance-of-Laplacian) and `phash.rs` are CPU-bound scalar loops
that will run ~1.5–2× slower under WASM SIMD than native — real, but they run on
downscaled images and are not the bottleneck.

**Threading.** Rust threads work in WASM via `SharedArrayBuffer` + Web Workers,
which needs COOP/COEP cross-origin isolation headers on the server. The existing
architecture is a good fit: `loader.rs` is already message-passing (shared
priority queue, `mpsc` results, `poll()` from the UI thread) rather than blocking
joins, which matters because the browser main thread cannot `Atomics.wait`. The
`available_parallelism() - 2` worker sizing translates to
`navigator.hardwareConcurrency`. Worker spawn is ~1ms each versus ~50µs for a
native thread, so spawn once at startup and keep them — which is what
`Loader::new` already does.

---

## The decode wall

The crux. Three formats, three different answers.

### JPEG — recoverable, with work

`createImageBitmap` and WebCodecs `ImageDecoder` hit Chrome's native JPEG
decoder, which is fast and off-main-thread. But you lose the thing that makes the
grid feel instant today: `CGImageSourceCreateThumbnailAtIndex` with
`kCGImageSourceCreateThumbnailFromImageIfAbsent` (see `thumbnail.rs`) pulls the
*embedded ~1600px preview* out of a 45MP file instead of decoding it. The browser
will decode all 45 megapixels and then downscale. On a large JPEG that is a
10–30× difference per thumbnail — exactly the flood `loader.rs` is architected
around.

The fix is real work but well-understood: parse JFIF/EXIF markers in Rust to
locate the embedded preview, then hand *that* small JPEG to `ImageDecoder`.
Byte-scanning, no codec needed. With it, JPEG thumbnailing lands within roughly
1.5–2× of native. Without it, the grid is visibly worse than the Mac app.

### HEIC — the weakest point

No Chromium build decodes HEIC. You would ship libheif/libde265 compiled to WASM
(~1–2MB), and it will not be close: ImageIO today routes HEVC decode through
Apple Silicon's dedicated media engine — fixed-function silicon. A scalar WASM
HEVC decoder against a hardware block is not a 2× gap, more like 5–20× on full
decodes.

Partial mitigation: iPhone HEICs carry an embedded thumbnail you can extract
without touching HEVC, which rescues the *grid*. But the Loupe — the
full-resolution decode you need the moment someone presses `→` — has no escape
hatch. **This is the most damaging finding, because culling is exactly "press →
400 times and look at each one full screen."**

### RAW — demonstrated in the browser; the question is throughput and fidelity

No browser decodes RAW natively, but Photopea is a working existence proof that
you can ship your own decoder. It added DNG in 2017 and CR2/NEF/ARW in [version
4.0 (Jan 2019)](https://blog.photopea.com/photopea-4-0-nef-cr2-arw-support.html),
in an app that was 68,161 lines *in total* at the time — hand-written JS, not a
libraw port. And it genuinely demosaics rather than cheating with the embedded
preview: [the RAW post](https://blog.photopea.com/raw-support-in-photopea.html)
describes a dialog that generates an image from sensor data, with [white balance
temp/tint, exposure, contrast and a live
histogram](https://www.photopea.com/tuts/open-raw-photos-online/).

Two implications, cutting in opposite directions.

*In favor:* if a hand-rolled JS decoder was tractable in 2019, a Rust→WASM one in
2026 should be both less work and meaningfully faster — so RAW decode speed is
probably better than a pessimistic reading suggests.

*Against:* the fidelity problems are structural, not implementation quality.
Photopea itself notes CR2 is undocumented and that it opens "**probably 95%** of
such files" — reverse-engineered, with a long tail that simply fails. A culling
tool pointed at a whole folder surfaces that tail as visibly broken frames, not
as a rounding error. And a self-rolled decoder produces *its own* render, not
Apple's, which breaks the guarantee in `image_ops.rs` that an export matches the
on-screen thumbnail.

**The load-bearing difference is the interaction model, not the codec.** Photopea
is File→Open one RAW, wait a beat, tune it in a modal, commit. Entirely
reasonable there, and nobody arrow-keys through 400 RAWs in Photopea. It proves
single-file feasibility — never the contested claim. It gives no evidence about
sustained decode throughput across a folder, the only thing that matters for
culling.

The pragmatic strategy remains what most RAW culling tools do: serve the grid
from embedded JPEG previews (fast, universal, no demosaic) and demosaic only for
the Loupe. That keeps browsing fast and confines both the speed and fidelity risk
to the single-image view.

### Filesystem access is not the problem

`showDirectoryPicker` + `FileSystemFileHandle` read at near-native throughput.
Two frictions, both one-time: enumerating a 5,000-file directory through an async
iterator is slower than `read_dir`, and permission is per-session unless you
persist handles in IndexedDB and re-prompt. Neither is a steady-state cost.

---

## The constraint nobody budgets for: WASM memory

wasm32 caps the address space at 4GB, browsers cap a tab lower in practice, and
WASM linear memory must be **contiguous** — so fragmentation bites long before
the nominal ceiling.

Run the current numbers: a 45MP image is 180MB as RGBA8. `FULL_CAPACITY = 3` plus
`PREVIEW_CAPACITY = 8` plus `THUMB_CAPACITY = 512` plus decode scratch is
comfortable in a native 64-bit address space and precarious in wasm32.
Survivable, but it constrains the design: keep decoded pixels in **GPU textures
and transferable `ArrayBuffer`s outside WASM linear memory**, and treat the WASM
heap as control state, not pixel buffers. memory64 exists but is still slower
today and not worth betting on.

---

## No web equivalent at all

The Vision-framework features are not a port, they are a rewrite with different
output quality:

- `segmentation.rs` — `VNGeneratePersonSegmentationRequest` /
  `VNGenerateForegroundInstanceMaskRequest` (the "Show Selection" overlay)
- `facequality.rs` — face landmarks and quality scoring
- `featureprint.rs` — `VNGenerateImageFeaturePrintRequest`, which
  `duplicates.rs` depends on

Replacements would be ONNX Runtime Web or TF.js models (U²-Net/MODNet class for
segmentation, BlazeFace for faces, some embedding model for feature prints) on
WebGPU. Feasible, but multi-MB model downloads, different mask quality, and
re-tuning every threshold in `burst.rs` and `duplicates.rs` that was calibrated
against Apple's scores.

Smaller platform holes:

- `trash.rs` — the File System Access API offers `remove()`, which is
  **permanent deletion, not the Trash.** A genuinely worse safety story for a
  tool whose whole point is rejecting photos. Move-to-a-subfolder is the honest
  workaround.
- `macos_delegate.rs` (Finder open) — becomes drag-drop or the File Handling API.
- `catalog.rs` — swap rusqlite for wa-sqlite over OPFS. Fine; single-row upserts
  are not a bottleneck.
- `image_encode.rs` — export via canvas `convertToBlob` loses EXIF/ICC control
  unless you splice metadata back yourself.

---

## Module inventory

Roughly 13.8k lines today.

**Ports cheaply (~7.5k lines):** `renderer.rs`, `shader.wgsl`, `mipgen.wgsl`,
`ui/*`, `app/*` state machine, `develop.rs`, `image_ops.rs`, `burst.rs`,
`sharpness.rs`, `phash.rs`, `duplicates.rs` (logic only), `hash.rs`,
`navigation.rs`, `paths.rs` (identity scheme needs rethinking — handles, not
paths).

**Rewrite (~4.5k lines):** `image_decode.rs`, `image_encode.rs`, `thumbnail.rs`,
`coregraphics.rs`, `loader.rs` (I/O and threading shape), `catalog.rs` (storage
backend), `segmentation.rs`, `facequality.rs`, `featureprint.rs`, `vision.rs`,
`trash.rs`, `macos_delegate.rs`.

The rewrite half is smaller in line count and much larger in risk — it is all
codec and ML work, the two areas where the browser gives you least.

---

## Recommendation

**The renderer and interaction model — the genuinely hard, distinctive parts of
LightPhotos — carry over almost free. The decode pipeline is the entire risk, and
its magnitude is currently unmeasured.**

Photopea shifts the prior: shipping your own decoder for undocumented formats is
clearly tractable, and Rust/WASM starts from a better position than the JS that
already works. What Photopea does *not* establish is sustained throughput, and
HEIC — where the Mac app uses fixed-function silicon — remains weaker than RAW.

Three options, in the order they should be considered:

1. **Grid-fast / Loupe-on-demand.** Serve every format's *grid* from embedded
   previews — fast and universal across JPEG, HEIC and RAW, since all three carry
   one — and do full decode only for the Loupe. This is what the Mac app already
   effectively does via `kCGImageSourceCreateThumbnailFromImageIfAbsent`, so it
   preserves the existing architecture rather than fighting it. The open question
   is only whether the Loupe's full decode is fast enough for arrow-key stepping.
   **Default recommendation, and cheap to falsify.**
2. **JPEG-first, other formats degraded.** If measurement kills option 1, ship
   the parts at parity and let HEIC/RAW be preview-only (no develop pipeline on
   them). Honest and shippable, but a materially smaller product.
3. **Don't port; go remote.** If cross-device access is the real motivation
   rather than the browser per se, decode server-side and stream previews. That
   sidesteps every codec problem here and trades it for a latency and
   infrastructure problem instead.

---

## Verification — the measurement that settles this

Do not plan a port before running this. Roughly a day of work, and it decides the
question.

1. **Establish the native baseline.** `loader.rs` already has the instrumentation
   — `report_decode` behind `timing_enabled()`. Run the release build against a
   representative folder (mixed JPEG/HEIC/RAW) and capture preview, full, and
   thumb timings per format. This is the bar.
2. **Build a standalone WASM decode harness** — no app, no UI. Ten files of each
   format, measured in Chrome:
   - (a) `ImageDecoder` on JPEG, at full size and at embedded-preview size
   - (b) libheif-WASM full decode on the HEICs
   - (c) **embedded-preview extraction on all three formats via marker parsing —
     this one decides option 1, so measure it first**
   - (d) a Rust RAW decoder compiled to WASM on one RAW, for the worst case

   Follow the pattern of the existing `seg_probe` / `face_probe` harnesses in
   `src/bin/` — separate binary, no app state.
3. **Compare against step 1 per tier.** Two independent decision rules:
   - *Grid:* if embedded-preview extraction lands within ~2× of native across all
     three formats, the grid is solved and option 1 is live.
   - *Loupe:* if HEIC full decode lands within ~2× of native, option 1 is a
     complete product; at 5×+ the Loupe feels broken under arrow-key culling and
     only option 2 or 3 survives.

   While there, eyeball one WASM-decoded RAW against the Mac app's render of the
   same file — the fidelity gap disqualifies as surely as the speed gap, and
   Photopea's own "95% of files" caveat says to expect one.
4. **Separately, sanity-check memory.** Allocate the equivalent of
   `FULL_CAPACITY=3` 45MP RGBA buffers in WASM linear memory alongside a
   512-entry thumb cache and confirm Chrome does not OOM. Cheap, and it dictates
   whether the pixel-buffer-outside-WASM design is mandatory or merely
   preferable.

Steps 1 and 2 are independent and can run in parallel. Neither requires touching
the existing app.
