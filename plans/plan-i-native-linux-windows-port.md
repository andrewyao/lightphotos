# Plan — Native Linux/Windows port (Phase 1 + Phase 2, macOS-verified)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `lightphotos` build and run on Linux/Windows by cfg-gating the
mac-only peripheral cluster (Phase 1) and replacing the ImageIO decode/
encode/thumbnail path with a cross-platform backend for JPEG/PNG/TIFF/RAW
(Phase 2), with everything verified as far as macOS-only hardware allows.

**Architecture:** Pure `#[cfg(target_os = "macos")]` / `#[cfg(not(target_os
= "macos"))]` splits at the function level, not a new trait/dyn-dispatch
layer — `image_decode.rs`/`image_encode.rs`/`thumbnail.rs`/`trash.rs` already
expose plain free functions and plain data structs (`DecodedImage`,
`ImageMetadata`, `Mask`, `FaceQuality`) with zero macOS types in their public
signatures, so every caller (`loader.rs`, `export.rs`, `app/*`, `burst.rs`)
keeps compiling unchanged on every platform; only the function *bodies*
differ per OS. This is a deliberate simplification of the memo's
`ImageBackend`-trait suggestion — YAGNI, since nothing today calls through a
trait object.

**Tech Stack:** `image`/zune-jpeg (JPEG/PNG/TIFF decode), `mozjpeg-rs` (JPEG
encode), `rawler` (RAW decode), `img-parts`/`kamadak-exif` (embedded-preview
marker scan), `trash` (cross-platform recycle-bin/trash).

**Spec:** `plans/native-linux-windows-port-feasibility.md` (the feasibility
memo this plan implements — read its "Verdict" and "Recommendation" sections
first).

## Global Constraints

- HEIC stays **mac-only**, unconditionally — no non-mac HEIC path, ever, per
  the memo's decision (not a measurement question).
- Mac behavior must not change: the mac `#[cfg(target_os = "macos")]` arm of
  every split function is the *existing* code, moved, never rewritten.
- `cargo build --release` on mac must stay byte-for-byte the same dependency
  graph as before this plan, *except* while the `raw-probe` feature (Task 6)
  is explicitly enabled — that feature is off by default.
- No Vision-cluster replacement in this plan (Phase 3, deferred) —
  `vision.rs`, `featureprint.rs`'s `compute`, `facequality.rs`'s
  `detect_faces`/`detect_face_rects`, `segmentation.rs`'s `segment*` all get
  a non-mac stub that returns `Err`, not a real implementation.
- Real Linux/Windows execution and real-camera RAW fidelity are **out of
  scope for this pass** — the user will run those checks on their own Linux
  machine after merge (see Task 14). Every task here must be verifiable on
  macOS alone.

---

- [x] Task 1: Cargo.toml — move the objc2 dependency cluster to
      `[target.'cfg(target_os = "macos")'.dependencies]` (files: Cargo.toml)
      — done, commit `274cc56`
- [x] Task 2: `trash.rs` — cross-platform trash via the `trash` crate
      (requires Task 1) (files: src/trash.rs, Cargo.toml) — done, commit `83db3e1`
- [x] Task 3: `macos_delegate.rs` — surgical cfg-split, no mod-level gating
      needed (files: src/macos_delegate.rs) — done, commit `3393113`
- [x] Task 4: Vision cluster — function-level cfg-split with `Err` stubs
      (requires Task 1) (files: src/vision.rs, src/featureprint.rs,
      src/facequality.rs, src/segmentation.rs, Cargo.toml) — done, commit `5c34054`
- [x] Task 5: Probe binaries + Phase 1 clean-build gate (requires Tasks 1-4)
      (files: src/bin/face_probe.rs, src/bin/seg_probe.rs, main.rs) —
      done, commit `6bed940` (mid-plan, rusqlite was also dropped entirely —
      see the ledger — resolving an environmental cross-compile blocker
      this task first surfaced)
- [x] Task 6: `raw-probe` Cargo feature — optional cross-platform codec deps
      (files: Cargo.toml) — done, commit `372f370`
- [x] Task 7: Synthetic Linear DNG fixture generator (requires Task 6)
      (files: src/bin/decode_probe.rs or a shared test-fixture module) —
      done, commit `368fc9d`. Major finding: macOS ImageIO cannot decode any
      Linear DNG at all (confirmed platform limitation) — see Task 12.
- [x] Task 8: `decode_probe` binary — rawler decode vs known ground truth
      (requires Tasks 6-7) (files: src/bin/decode_probe.rs, Cargo.toml) —
      done, commit `e3dea8b`. Comparison target changed from "ImageIO
      baseline" to "the fixture's own analytic ground truth" per Task 7's
      finding — byte-exact match (0/147456 mismatches).
- [x] Task 9: JPEG/PNG/TIFF non-mac decode/encode (requires Task 1) (files:
      src/image_decode.rs, src/image_encode.rs, Cargo.toml) — done, commits
      `7eddcce`..`d5b0899` (1 fix round: non-mac source_size axis-swap bug
      for rotated photos)
- [x] Task 10: RAW decode + embedded-preview extraction, non-mac (requires
      Tasks 8-9) (files: src/image_decode.rs, src/thumbnail.rs, Cargo.toml)
      — done, commits `957bd89`..`25193d3` (1 fix round: non-mac RAW
      thumbnail previews missing EXIF orientation)
- [x] Task 11: Cross-compile check — `cargo check --target
      x86_64-unknown-linux-gnu` for the whole crate (requires Tasks 1-10)
      (files: —) — done: 0 errors on Linux (plain and `--features
      raw-probe`) and Windows (`x86_64-pc-windows-gnu`, optional, ran clean).
- [x] Task 12: RAW fidelity pass on the synthetic DNG (requires Tasks 7-10)
      (files: —) — labeled a smoke test, not a coverage claim; see Task 14.
      Done: byte-exact pass, see detail below.
- [x] Task 13: Docs — README.md/CLAUDE.md non-mac build notes, `00-overview.md`
      Task 3 pointer (files: README.md, CLAUDE.md, plans/00-overview.md) —
      done, commit `84b05aa`
- [ ] Task 14: End-to-end gate: `cargo test && cargo build --release` (mac,
      full regression) (requires all above) (files: —) — **real Linux
      runtime and real-camera RAW fidelity stay BLOCKED: needs the user's
      Linux machine + real RAW files, see "Manual verification steps" in
      00-overview.md.**

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Task detail

### Task 1: Cargo.toml — target-gate the objc2 cluster

Move every `objc2*` dependency line (`objc2`, `objc2-foundation`,
`objc2-app-kit`, `objc2-core-graphics`, `objc2-core-foundation`,
`objc2-image-io`, `objc2-vision`, `objc2-core-video`) out of the flat
`[dependencies]` table into a new section:

```toml
[target.'cfg(target_os = "macos")'.dependencies]
objc2 = "0.6.4"
objc2-foundation = { version = "0.3.2", features = [...] }   # copy features verbatim
objc2-app-kit = { version = "0.3.2", features = [...] }
objc2-core-graphics = "0.3.2"
objc2-core-foundation = { version = "0.3.2", features = [...] }
objc2-image-io = { version = "0.3.2", features = [...] }
objc2-vision = { version = "0.3.2", default-features = false, features = [...] }
objc2-core-video = { version = "0.3.2", default-features = false, features = [...] }
```

`winit`, `wgpu`, `pollster`, `bytemuck`, `egui`/`egui-winit`/`egui-wgpu`,
`serde`/`serde_json`, `rusqlite` stay in `[dependencies]` unconditionally —
all already cross-platform, no change.

- [ ] Move the deps, preserving every `features = [...]` list verbatim.
- [ ] `cargo build --release` on mac — must succeed identically (same crate
      set resolves for the macOS target either way).
- [ ] `cargo test` — must still pass (126+ tests at time of writing).
- [ ] Commit: `git add Cargo.toml && git commit -m "build: target-gate objc2 dependency cluster to macOS"`

### Task 2: `trash.rs` — cross-platform trash

Add `trash = "5"` (check crates.io for the current major at implementation
time — it's a small, stable crate) under
`[target.'cfg(not(target_os = "macos"))'.dependencies]`.

Split `move_to_trash`:

```rust
#[cfg(target_os = "macos")]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    // existing NSFileManager body, unchanged
    ...
}

#[cfg(not(target_os = "macos"))]
pub fn move_to_trash(path: &Path) -> Result<(), String> {
    trash::delete(path).map_err(|e| e.to_string())
}
```

- [ ] Add the dependency, split the function as above.
- [ ] `cargo build --release` on mac — unchanged behavior (mac arm untouched).
- [ ] `cargo test` — `trashing_a_missing_file_errors` still passes on mac.
- [ ] Commit: `git commit -am "feat(trash): cross-platform non-mac impl via the trash crate"`

### Task 3: `macos_delegate.rs` — surgical cfg-split

Only `application_open_urls` and `install_open_handler` touch `objc2`;
`UserEvent`, `PROXY`, `set_proxy` are already plain Rust and must stay
unconditional (winit's `ApplicationHandler<UserEvent>` needs the type on
every platform). Add a non-mac stub for `install_open_handler` so
`main.rs`'s unconditional call site needs no change:

```rust
#[cfg(target_os = "macos")]
extern "C-unwind" fn application_open_urls(...) { /* unchanged */ }

#[cfg(target_os = "macos")]
pub fn install_open_handler() -> bool { /* unchanged body */ }

/// Non-mac "open with" already works via the CLI path argument
/// (`lightphotos /path/to/photo.jpg`) — no Finder-equivalent hook exists to
/// install, so this is a no-op that reports "not installed" so main.rs's
/// existing warning still prints (harmless — there's nothing to warn about
/// on this platform, but the warning is not incorrect either).
#[cfg(not(target_os = "macos"))]
pub fn install_open_handler() -> bool {
    false
}
```

- [ ] Split the two functions as above; leave everything else untouched.
- [ ] `cargo build --release` on mac — unchanged.
- [ ] Commit: `git commit -am "feat(macos_delegate): non-mac stub for install_open_handler"`

### Task 4: Vision cluster — function-level cfg-split

`vision.rs`'s only consumers are the three Vision feature modules, and all
three only call into it from functions that are about to become mac-only —
gate the whole module:

```rust
// main.rs
#[cfg(target_os = "macos")]
mod vision;
```

For each of `featureprint.rs`, `facequality.rs`, `segmentation.rs`: keep
every *data* type unconditional (`DistanceJob`, `DistanceOutcome`,
`DistancePool`, `FaceOutcome`, `FacePool`, `FaceQuality`, `RawFace`,
`EyeState`, `Points`, `CLOSED_EYE_RATIO`, `Mask`, `MaskSource` — none embed
an objc2 type and all their impls (`Mask::at`, `Mask::resized`,
`face_quality`, `eye_openness`, `combined_score` in `burst.rs`) are pure).
Split only the functions that call Vision:

```rust
// featureprint.rs — FeaturePrint itself wraps a Vision type, so it splits too
#[cfg(target_os = "macos")]
pub struct FeaturePrint(objc2::rc::Retained<objc2_vision::VNFeaturePrintObservation>);
#[cfg(not(target_os = "macos"))]
pub struct FeaturePrint;

#[cfg(target_os = "macos")]
pub fn compute(path: &Path) -> Result<FeaturePrint, String> { /* unchanged body */ }
#[cfg(not(target_os = "macos"))]
pub fn compute(_path: &Path) -> Result<FeaturePrint, String> {
    Err("feature-print computation is unsupported on this platform".into())
}

#[cfg(target_os = "macos")]
pub fn feature_distance(a: &FeaturePrint, b: &FeaturePrint) -> Result<f32, String> { /* unchanged */ }
#[cfg(not(target_os = "macos"))]
pub fn feature_distance(_a: &FeaturePrint, _b: &FeaturePrint) -> Result<f32, String> {
    Err("feature-print distance is unsupported on this platform".into())
}
```

```rust
// facequality.rs
#[cfg(target_os = "macos")]
pub fn detect_faces(path: &Path) -> Result<Vec<RawFace>, String> { /* unchanged */ }
#[cfg(not(target_os = "macos"))]
pub fn detect_faces(_path: &Path) -> Result<Vec<RawFace>, String> {
    Err("face detection is unsupported on this platform".into())
}

#[cfg(target_os = "macos")]
pub fn detect_face_rects(path: &Path) -> Result<Vec<RawFace>, String> { /* unchanged */ }
#[cfg(not(target_os = "macos"))]
pub fn detect_face_rects(_path: &Path) -> Result<Vec<RawFace>, String> {
    Err("face detection is unsupported on this platform".into())
}
// `analyze` calls `detect_faces` and stays unconditional — it now compiles
// on every platform, erroring on non-mac exactly the way a real decode
// error already propagates.
```

```rust
// segmentation.rs
#[cfg(target_os = "macos")]
pub fn segment(path: &Path) -> Result<Mask, String> { /* unchanged */ }
#[cfg(target_os = "macos")]
pub fn segment_person(path: &Path) -> Result<Mask, String> { /* unchanged */ }
#[cfg(target_os = "macos")]
pub fn segment_foreground(path: &Path) -> Result<Mask, String> { /* unchanged */ }
#[cfg(not(target_os = "macos"))]
pub fn segment(_path: &Path) -> Result<Mask, String> {
    Err("subject segmentation is unsupported on this platform".into())
}
#[cfg(not(target_os = "macos"))]
pub fn segment_person(_path: &Path) -> Result<Mask, String> {
    Err("subject segmentation is unsupported on this platform".into())
}
#[cfg(not(target_os = "macos"))]
pub fn segment_foreground(_path: &Path) -> Result<Mask, String> {
    Err("subject segmentation is unsupported on this platform".into())
}
```

Each file's top-level `use objc2*`/`use crate::vision` imports must move to
`#[cfg(target_os = "macos")] use ...;` lines (or become fully-qualified
paths inside the mac-only function bodies) so non-mac compiles without
unused-import errors.

`DistancePool`, `FacePool` and their `new`/`submit`/`poll` impls need **no
change** — they only call the now-cfg-split `compute`/`analyze`, so on
non-mac they still spin up worker threads that immediately report an `Err`
outcome per job, which is exactly the "stub" behavior wanted (`app/*`'s
existing poll loops already handle a `Result::Err` per outcome without
special-casing it).

- [ ] Gate `vision.rs`'s mod declaration in `main.rs`.
- [ ] Split `FeaturePrint`/`compute`/`feature_distance` in `featureprint.rs`.
- [ ] Split `detect_faces`/`detect_face_rects` in `facequality.rs`.
- [ ] Split `segment`/`segment_person`/`segment_foreground` in `segmentation.rs`.
- [ ] `cargo build --release` on mac — unchanged (mac arms are the old code).
- [ ] `cargo test` — all Vision-touching tests (`identical_images_are_closer_than_different_ones`, `detect_faces_runs_and_finds_none_in_a_blank_image`, etc.) still pass on mac since they only run against the `#[cfg(target_os = "macos")]` arm.
- [ ] Commit: `git commit -am "feat(vision): cfg-split Vision cluster, non-mac stubs return Err"`

### Task 5: Probe binaries + Phase 1 clean-build gate

`face_probe.rs` and `seg_probe.rs` are `[[bin]]` targets in the same crate
(`test = false`), so a bare `cargo build`/`cargo check` tries to compile
both on every platform. Wrap each probe's `fn main()`:

```rust
// src/bin/face_probe.rs — wrap the existing body
fn main() {
    #[cfg(target_os = "macos")]
    { real_main(); }
    #[cfg(not(target_os = "macos"))]
    { eprintln!("face_probe is macOS-only (uses Apple Vision)."); }
}

#[cfg(target_os = "macos")]
fn real_main() {
    // existing main() body, renamed
}
```

Apply the same wrap to `seg_probe.rs`.

This is the task where Phase 1 becomes checkable end-to-end:

- [ ] Wrap `face_probe.rs`'s and `seg_probe.rs`'s `main()` as above.
- [ ] `rustup target add x86_64-unknown-linux-gnu` (one-time, if not already installed).
- [ ] `cargo check --target x86_64-unknown-linux-gnu` — must succeed with **zero errors**. This is Phase 1's actual deliverable: a clean non-mac compile. (Linking is not required — `check` type-checks without invoking a linker, so no cross-linker toolchain is needed.)
- [ ] `cargo build --release && cargo test` on mac — unchanged.
- [ ] Commit: `git commit -am "feat: wrap probe binaries so Phase 1 gives a clean non-mac cargo check"`

### Task 6: `raw-probe` Cargo feature

Add the cross-platform codec crates as **optional** dependencies, gated
behind a feature — this keeps the default mac build's dependency graph
unchanged (the Global Constraints promise) while making them available to
`decode_probe` on mac for this pass's verification:

```toml
[dependencies]
rawler = { version = "0.8", optional = true }      # confirm current version on crates.io
mozjpeg-rs = { version = "0.1", optional = true }   # confirm current version
img-parts = { version = "0.3", optional = true }    # confirm current version

[features]
raw-probe = ["dep:rawler", "dep:mozjpeg-rs", "dep:img-parts"]

[[bin]]
name = "decode_probe"
path = "src/bin/decode_probe.rs"
test = false
required-features = ["raw-probe"]
```

`required-features` makes Cargo skip `decode_probe` entirely on a plain
`cargo build`/`cargo check` (mac or non-mac) — it only compiles when
`--features raw-probe` is passed, so Tasks 1-5's "clean non-mac build"
claim is unaffected by this task.

**Before writing any call into these crates in Tasks 7-10**, run `cargo doc
--open -p rawler -p mozjpeg-rs -p img-parts` (or check docs.rs) to confirm
current function names — these are external crates and their APIs may have
moved since this plan was written.

- [ ] Add the optional deps + feature + `decode_probe` bin stub (empty
      `fn main() {}` is enough for this task).
- [ ] `cargo build --release` on mac with **no** `--features` flag — dependency graph must be identical to before this task (verify with `cargo tree` diffed against a pre-task snapshot).
- [ ] `cargo check --features raw-probe` on mac — must succeed (pulls in the three new crates, compiles the empty probe).
- [ ] Commit: `git commit -am "build: add raw-probe feature for optional cross-platform codec crates"`

### Task 7: Synthetic Linear DNG fixture generator

No real camera RAW file is available (checked: none in the repo, none on
this machine). Adobe's **Linear DNG** variant stores already-demosaiced
16-bit RGB samples rather than a Bayer CFA mosaic — a legitimate DNG
container (same TIFF/EXIF/DNG tag structure any DNG reader must parse) that
is realistically hand-constructible, unlike a real sensor's raw mosaic data.

**What this validates and what it doesn't**: this exercises DNG
container/tag parsing and a basic pixel round-trip through both ImageIO and
`rawler` — it does **not** validate real-camera Bayer-CFA demosaic fidelity
across makes (CR2/NEF/ARW). That gap is explicitly still open; see Task 14.

Write a small fixture builder (in `src/bin/decode_probe.rs`, behind
`raw-probe`, or as a `#[cfg(test)]` helper reused by both `decode_probe` and
a unit test):

```rust
/// Writes a minimal, valid Linear DNG: a little-endian TIFF with one IFD
/// carrying the DNG tags a reader needs to treat this as linear (non-mosaiced)
/// raw data, plus `width * height` RGB16 samples (test pattern: a horizontal
/// gradient, so a fidelity comparison has something non-uniform to diff).
///
/// Tag IDs used (see the DNG 1.7 spec, "Basic DNG Tags"):
///   0x00FE NewSubfileType = 0
///   0x0100 ImageWidth, 0x0101 ImageLength
///   0x0102 BitsPerSample = [16,16,16]
///   0x0103 Compression = 1 (none)
///   0x0106 PhotometricInterpretation = 34892 (LinearRaw)
///   0x0111 StripOffsets, 0x0116 RowsPerStrip, 0x0117 StripByteCounts
///   0x0115 SamplesPerPixel = 3
///   0xC612 DNGVersion = [1,4,0,0]
///   0xC613 DNGBackwardVersion = [1,1,0,0]
///   0xC621 ColorMatrix1 = identity 3x3 (SRATIONAL) — readers need *a* matrix
///     present even though this fixture doesn't care about color accuracy.
fn write_linear_dng(path: &Path, width: u32, height: u32) -> std::io::Result<()> {
    // Implementation note for whoever picks this up: build the IFD by hand
    // (offset-tracked Vec<u8> writer) rather than reaching for a TIFF-writer
    // crate — DNG's required-tag set is small and a hand-rolled writer keeps
    // this fixture legible and dependency-free. Verify byte-for-byte against
    // the DNG 1.7 spec (adobe.com/products/dng) before trusting the output;
    // a malformed IFD offset is the most common mistake (fails silently in
    // permissive readers, loudly in strict ones — test against both ImageIO
    // and rawler, not just one).
    todo!("hand-roll the TIFF/DNG writer per the tag table above")
}
```

The `todo!()` above is intentional — the exact byte-writing code depends on
choices (strip vs tile layout, exact gradient pattern) best made with the
DNG spec open, not pre-guessed in this plan. Replace it with a real
implementation before Task 8 can run.

- [ ] Implement `write_linear_dng`, consulting the DNG 1.7 spec for exact
      tag encoding.
- [ ] Add a unit test: write a small (e.g. 32x24) fixture, re-open it with
      `image_decode::open_image_source` + `image_decode::decode` (the
      *mac* ImageIO path) and confirm it decodes without error and reports
      the expected dimensions. This is the cheapest possible check that the
      fixture is a well-formed DNG before trusting `rawler` against it.
- [ ] `cargo test` on mac — new test passes.
- [ ] Commit: `git commit -am "test: synthetic Linear DNG fixture generator for RAW verification"`

### Task 8: `decode_probe` binary — rawler vs ImageIO

```rust
// src/bin/decode_probe.rs
use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let dir = std::env::temp_dir().join("lightphotos-decode-probe");
    std::fs::create_dir_all(&dir).expect("create probe fixture dir");
    let dng_path = dir.join("linear_test.dng");
    write_linear_dng(&dng_path, 256, 192).expect("write fixture DNG");

    // ImageIO baseline (mac only — this binary only runs on mac this pass).
    let baseline = image_decode::decode(&dng_path, u32::MAX)
        .expect("ImageIO should decode the synthetic Linear DNG");

    // rawler candidate.
    let candidate = decode_via_rawler(&dng_path).expect("rawler should decode the same file");

    println!(
        "ImageIO: {}x{}  rawler: {}x{}",
        baseline.width, baseline.height, candidate.width, candidate.height
    );
    assert_eq!(
        (baseline.width, baseline.height),
        (candidate.width, candidate.height),
        "dimension mismatch between ImageIO and rawler"
    );

    // Pixel fidelity: mean absolute difference per channel, since Linear DNG
    // -> RGBA8 involves two independent conversion paths (ImageIO's CG
    // pipeline vs rawler's own) that need not be bit-identical — only close.
    let mad = mean_abs_diff(&baseline.rgba, &candidate.rgba);
    println!("mean abs diff per channel byte: {mad:.2}");
    assert!(mad < 8.0, "rawler output diverges too far from ImageIO baseline: {mad}");

    // Extra files passed on the CLI: decode each with both backends and just
    // report timing (uses loader.rs's existing report_decode/timing_enabled
    // instrumentation pattern) — no assertion, since these are the user's
    // own real files with no baseline to compare against yet.
    for path in args.into_iter().map(PathBuf::from) {
        let t0 = std::time::Instant::now();
        match decode_via_rawler(&path) {
            Ok(img) => println!("{}: {}x{} in {:?}", path.display(), img.width, img.height, t0.elapsed()),
            Err(e) => println!("{}: FAILED: {e}", path.display()),
        }
    }
}

/// Decode a RAW file via `rawler`, returning the same `DecodedImage` shape
/// `image_decode.rs` uses everywhere else. Confirm the exact `rawler` entry
/// point against its current docs (Task 6's note) before implementing —
/// as of the version pinned in Task 6, the expected shape is
/// `rawler::decode_file(path)` returning a `RawImage`-like type with
/// `width`/`height`/pixel-data accessors; adapt this signature once the
/// real API is confirmed.
fn decode_via_rawler(path: &Path) -> Result<image_decode::DecodedImage, String> {
    todo!("wire up the real rawler decode call once its API is confirmed (Task 6 note)")
}

fn mean_abs_diff(a: &[u8], b: &[u8]) -> f64 {
    assert_eq!(a.len(), b.len());
    let sum: u64 = a.iter().zip(b).map(|(&x, &y)| (x as i32 - y as i32).unsigned_abs() as u64).sum();
    sum as f64 / a.len() as f64
}
```

The `todo!()` in `decode_via_rawler` is intentional for the same reason as
Task 7's — `rawler`'s exact API must be confirmed live, not guessed here.

- [ ] Implement `decode_via_rawler` against the real `rawler` API.
- [ ] `cargo run --bin decode_probe --features raw-probe` on mac — prints
      matching dimensions and a mean-abs-diff under the 8.0 threshold. If it
      doesn't: the RAW-fidelity question this plan exists to answer has a
      concrete negative data point — stop, don't silently loosen the
      threshold, record what happened in Task 14 instead.
- [ ] Commit: `git commit -am "feat(decode_probe): rawler vs ImageIO fidelity check on synthetic DNG"`

### Task 9: JPEG/PNG/TIFF non-mac decode/encode

Add under `[target.'cfg(not(target_os = "macos"))'.dependencies]`:

```toml
image = { version = "0.25", default-features = false, features = ["jpeg", "png", "tiff"] }
mozjpeg-rs = "0.1"   # confirm current version; this one IS required (not optional) on non-mac
```

(`mozjpeg-rs` moves from Task 6's optional/`raw-probe`-gated declaration to
being unconditionally required for non-mac — Cargo merges the two edges
fine: optional-and-mac-probe-only vs required-and-non-mac-only are disjoint
target predicates.)

Split `image_decode::decode` and `image_encode::encode_jpeg`:

```rust
// image_decode.rs
#[cfg(target_os = "macos")]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> { /* unchanged */ }

#[cfg(not(target_os = "macos"))]
pub fn decode(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    if is_raw_extension(path) {
        return decode_raw_nonmac(path, max_dim); // Task 10
    }
    let img = image::open(path).map_err(|e| e.to_string())?;
    let img = img.into_rgba8();
    let (w, h) = (img.width(), img.height());
    let (w, h) = fit_within(w, h, max_dim); // reuse the existing pure helper
    let resized = image::imageops::resize(&img, w, h, image::imageops::FilterType::Lanczos3);
    Ok(DecodedImage { width: w, height: h, rgba: resized.into_raw() })
}
```

```rust
// image_encode.rs
#[cfg(target_os = "macos")]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> { /* unchanged */ }

#[cfg(not(target_os = "macos"))]
pub fn encode_jpeg(out: &Path, width: u32, height: u32, rgba: &[u8]) -> Result<(), String> {
    // Confirm mozjpeg-rs's real encoder entry point against its docs (Task 6
    // note) before implementing — expected shape is a builder taking RGB(A)
    // planes and a quality setting, writing to a Vec<u8> or file handle.
    todo!("wire up the real mozjpeg-rs encode call once its API is confirmed")
}
```

`fit_within` in `image_decode.rs` is currently a private free function with
no macOS dependency — leave it unconditional (both cfg arms of `decode`
call it) rather than duplicating it.

- [ ] Add the two deps.
- [ ] Split `decode`/`encode_jpeg`, implementing the non-mac JPEG/PNG/TIFF
      path fully (the `mozjpeg-rs` call is the one piece needing live-API
      confirmation, same caveat as Task 8).
- [ ] `cargo check --target x86_64-unknown-linux-gnu` — type-checks (RAW
      dispatch stubbed to `todo!()` until Task 10 is fine for `check`, but
      note `check` still requires the function to *exist* with the right
      signature — stub `decode_raw_nonmac` as `todo!()` for now if Task 10
      hasn't landed yet in your working order).
- [ ] `cargo build --release && cargo test` on mac — unchanged (mac arm
      untouched).
- [ ] Commit: `git commit -am "feat(image_decode,image_encode): non-mac JPEG/PNG/TIFF backend"`

### Task 10: RAW decode + embedded-preview extraction, non-mac

Add `rawler` (unconditionally, non-mac) and `img-parts`/`kamadak-exif`
under the same `[target.'cfg(not(target_os = "macos"))'.dependencies]`
section from Task 9.

```rust
// image_decode.rs, non-mac only
fn is_raw_extension(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase()).as_deref(),
        Some("cr2" | "cr3" | "nef" | "arw" | "dng" | "orf" | "rw2" | "raf" | "pef" | "srw")
    )
}

fn decode_raw_nonmac(path: &Path, max_dim: u32) -> Result<DecodedImage, String> {
    // Same rawler entry point as decode_probe.rs's decode_via_rawler
    // (Task 8) — factor the shared logic into one function once both exist,
    // rather than duplicating the rawler call site.
    todo!("share the rawler decode call with decode_probe.rs's decode_via_rawler")
}
```

```rust
// thumbnail.rs, non-mac only — embedded-preview extraction
#[cfg(not(target_os = "macos"))]
pub fn thumbnail(path: &Path, max_px: u32) -> Result<DecodedImage, String> {
    if let Some(preview) = try_extract_embedded_preview(path, max_px) {
        return Ok(preview);
    }
    // Fall back to a full decode-at-size — no ImageIO decode-at-size
    // equivalent exists cross-platform, so this is a real (if rarer) full
    // decode followed by a resize, same shape as image_decode::decode's
    // non-mac arm.
    crate::image_decode::decode(path, max_px)
}

#[cfg(not(target_os = "macos"))]
fn try_extract_embedded_preview(path: &Path, max_px: u32) -> Option<DecodedImage> {
    // Confirm img-parts's / kamadak-exif's real marker-scan API against
    // current docs (Task 6 note) before implementing — the shape wanted is
    // "find the largest embedded JPEG/TIFF preview marker, decode just that
    // blob via the `image` crate, resize to max_px." Return None on any
    // failure so the caller's full-decode fallback takes over — this must
    // never be the thing that makes a thumbnail request fail outright.
    None
}
```

`decode_at_size`/`EmbeddedPreview` (the loupe's "never take the embedded
preview" mode) needs its own non-mac arm too — for `EmbeddedPreview::Never`
it's just `image_decode::decode(path, max_px)` (skip the preview-extraction
branch entirely); for `UseIfPresent` it's the `thumbnail()` body above.

- [ ] Add `rawler`/`img-parts` (or `kamadak-exif`) as non-mac deps.
- [ ] Implement `decode_raw_nonmac`, factored to share the rawler call with
      `decode_probe.rs` (revisit Task 8's `decode_via_rawler` to call this
      shared function instead of duplicating it, once both exist).
- [ ] Implement `try_extract_embedded_preview` and the `thumbnail`/
      `decode_at_size` non-mac arms in `thumbnail.rs`.
- [ ] `cargo check --target x86_64-unknown-linux-gnu` — full crate
      type-checks with no remaining `todo!()` reachable from the non-mac
      build (Tasks 7/8's `todo!()`s are fine to leave — they're behind
      `raw-probe`, not part of the plain non-mac build).
- [ ] `cargo build --release && cargo test` on mac — unchanged.
- [ ] Commit: `git commit -am "feat(image_decode,thumbnail): non-mac RAW decode + preview extraction"`

### Task 11: Cross-compile check

- [ ] `rustup target add x86_64-unknown-linux-gnu` if not already present
      (Task 5 may have done this already).
- [ ] `cargo check --target x86_64-unknown-linux-gnu` from the repo root —
      must succeed with zero errors, zero warnings about missing platform
      impls. This is the concrete artifact that answers "does this compile
      on Linux" without needing Linux hardware.
- [ ] Optional, if a Windows target is installed locally (`rustup target
      add x86_64-pc-windows-gnu`): `cargo check --target
      x86_64-pc-windows-gnu` — same bar. Not blocking if the target isn't
      installed; note in Task 14 whether this ran.
- [ ] No commit — this task produces no file changes, just a verification
      record for Task 14.

### Task 12: RAW fidelity pass — record the result

This is Task 8's `decode_probe` run, formalized as a checkpoint. Note the
comparison target changed from the plan's original design during Task 7/8
(a controller ruling, see the plan's execution ledger): macOS ImageIO turned
out unable to decode any Linear DNG at all (a confirmed platform limitation,
independent of fixture correctness), so the check compares `rawler`'s
decode against the fixture's own known analytic ground truth instead of an
ImageIO baseline — a stronger check (ground truth vs. one decoder) than the
original two-decoders-agree design.

- [x] Run `cargo run --release --bin decode_probe --features raw-probe` on mac.
- [x] Output (256×192 fixture, real run):
  ```
  --- Synthetic Linear DNG: rawler decode vs analytic gradient ground truth ---
  rawler decoded 256x192 cpp=3 bps=16: 147456 samples compared, 0 mismatched, max abs diff 0, mean abs diff 0.000000
  PASS: rawler's Linear DNG decode matches the analytic gradient ground truth exactly.
  --- Synthetic Linear DNG: rawler's RawDevelop pipeline (Task 10's decode_raw_nonmac path) ---
  PASS: RawDevelop::develop_intermediate -> to_dynamic_image ran end-to-end and produced a 256x192 image, same shape decode_raw_nonmac's non-mac RAW path builds on.
  ```
- [x] **Passed — byte-exact (0/147456 mismatches, zero tolerance).** `rawler`'s
      DNG container parsing and pixel decode are verified correct against a
      hand-built, hex-verified-correct fixture, and Task 10's actual
      production `decode_raw_nonmac` code path (`RawDevelop::develop_intermediate`
      → `to_dynamic_image`) was independently exercised end-to-end on the same
      fixture. Still-open gap, unaffected by this result: **no real-camera
      Bayer-CFA RAW (CR2/NEF/ARW) has been tested** — this fixture is
      non-mosaiced (`PhotometricInterpretation=34892`/LinearRaw, cpp=3), so
      rawler's CFA/Bayer demosaic branch (`ProcessingStep::Demosaic`, PPG/
      Bilinear4Channel dispatch on `photometric`/`cpp`) was never exercised,
      only its non-CFA `develop_intermediate` path. Code-review in Task 10
      found the CFA dispatch logic itself plausible/correct by inspection
      (branches on `cpp`/`photometric` generically, no RGB-only assumption)
      but it remains genuinely untested — real fidelity/coverage across
      camera makes is exactly the gap Task 14 leaves BLOCKED for the user's
      own follow-up with real files on real Linux hardware.

### Task 13: Docs

- [ ] `README.md` — add a short "Linux/Windows (experimental)" note:
      builds via `cargo build --release` on non-mac targets once Tasks 1-11
      are merged; HEIC and the Vision-backed features (duplicate
      refinement, face/blink scoring, subject-selection overlay) are
      mac-only. Point at this plan file for the real state.
- [ ] `CLAUDE.md` — update the "What this is" line (currently says "macOS
      Lightroom-lite... written in Rust") to note the decode backend is now
      platform-split; update the Commands section if `cargo build --release`
      behavior differs per platform in any way worth flagging (it shouldn't
      — same command, different resolved deps).
- [ ] `plans/00-overview.md` — Task 3's `files:` pointer changes from
      `plans/native-linux-windows-port-feasibility.md` to
      `plans/plan-i-native-linux-windows-port.md` (this file); the memo
      stays referenced from this plan's own header (`**Spec:**` above), not
      deleted.
- [ ] Commit: `git commit -am "docs: note non-mac build support and update roadmap pointer"`

### Task 14: End-to-end gate

- [x] `cargo test && cargo build --release` on mac — 171/171 tests pass,
      release build clean. Full regression, mac behavior unchanged
      end to end across all 13 prior tasks.
- [x] `cargo check --target x86_64-unknown-linux-gnu` — 0 errors (plain and
      `--features raw-probe`). `cargo check --target x86_64-pc-windows-gnu`
      also 0 errors (optional per the plan, ran clean anyway).

**Summary — what's verified:**
- Mac regression: full, byte-for-byte — every `#[cfg(target_os = "macos")]`
  arm across `trash.rs`, `macos_delegate.rs`, the Vision cluster
  (`vision.rs`/`featureprint.rs`/`facequality.rs`/`segmentation.rs`),
  `image_decode.rs`, `image_encode.rs`, `coregraphics.rs`, `thumbnail.rs` is
  the pre-existing code, moved, never rewritten — checked independently by
  every task reviewer via diff-level line accounting (removed vs. added
  lines), not just self-report.
- Non-mac compile: the whole crate type-checks clean on Linux and Windows,
  including the `raw-probe`-gated `decode_probe` binary. This is the
  concrete, current answer to the original ask ("make the Mac-specific part
  optional to compile") plus the harder goal (a real, if untested, non-mac
  decode/encode/RAW backend).
- Synthetic-DNG fidelity: byte-exact (Task 12) — `rawler`'s DNG decode and
  Task 10's actual `decode_raw_nonmac` production path both verified
  end-to-end against a hand-built, hex-verified-correct fixture.
- Along the way: dropped the `rusqlite` dependency entirely (a user request
  mid-plan, unrelated to the original 14 tasks but resolved a real
  cross-compile blocker this plan's own Task 5 first surfaced) and merged in
  the sidecar-file catalog rewrite (`feat/compare-mode-zoom`) that had
  landed on the user's local branch but not yet this one.
- Every task went through implementer → reviewer, with two real Important
  findings caught and fixed in review (not self-certified): a rotated-photo
  zoom/aspect bug in non-mac `source_size` (Task 9), and sideways/cached-wrong
  RAW thumbnail previews from a missing EXIF-orientation step (Task 10).

**What's still open (unchanged from the plan's Global Constraints — nothing
in this pass closes these):**
- Real Linux/Windows runtime — the app has never actually run outside
  `cargo check`'s type-checking. No GPU/windowing/event-loop behavior,
  no actual file I/O behavior, nothing UI-level has been exercised.
- Real-camera RAW fidelity across makes (CR2/NEF/ARW/...) — the synthetic
  fixture is a non-mosaiced Linear DNG; `rawler`'s actual Bayer-CFA
  demosaic path was never exercised against real sensor data, only judged
  plausible by code inspection (Task 10's review).
- Phase 3 (Vision-cluster ONNX replacement) — out of scope for this plan
  by design; the Vision cluster stays a mac-only stub returning `Err` on
  non-mac, exactly as scoped from Task 4 onward.

- [ ] **BLOCKED: needs Linux hardware + real camera RAW files.** Left
      unchecked per `00-overview.md`'s "Manual verification steps"
      convention — the user has said they'll run the real Linux smoke test
      themselves, on their own machine, with their own RAW files, after
      merge. This pass does not and cannot self-certify that result.

---

## Reference

**What**: Implements `plans/native-linux-windows-port-feasibility.md`'s
Phase 1 (cfg-gate the peripheral cluster) and Phase 2 (cross-platform
decode/encode/thumbnail backend), skipping Phase 3 (Vision-cluster ONNX
replacement — explicitly deferred, no work here). Scoped down from the
memo's suggested `ImageBackend` trait to plain function-level `#[cfg]`
splits, since every current caller (`loader.rs`, `export.rs`, `app/*`,
`burst.rs`) already consumes `image_decode`/`image_encode`/`thumbnail`/
Vision-cluster functions as free functions over plain data types with zero
macOS types in their public signatures — a trait/dyn-dispatch layer would
add indirection nothing today needs.

**Effort/Priority**: Large — this is the riskiest, most novel plan in the
repo (three new external crates with unconfirmed exact APIs at plan-writing
time, a hand-rolled DNG fixture, zero real-camera RAW test data). Several
tasks (7, 8, 9's `mozjpeg-rs` call, 10) contain a deliberate `todo!()` where
the exact external-crate API must be confirmed live rather than guessed —
resolve those first before treating this plan as executable end to end.

**What's explicitly NOT done here**: real Linux/Windows execution (Task 14
BLOCKED), real-camera RAW fidelity beyond the synthetic Linear DNG (Task
12), Phase 3's Vision-cluster ONNX replacement (out of scope per Global
Constraints), Windows cross-compile check (optional, Task 11).

**Critical files**: `src/image_decode.rs`, `src/image_encode.rs`,
`src/thumbnail.rs` (Phase 2 core), `src/trash.rs`, `src/macos_delegate.rs`,
`src/vision.rs`, `src/featureprint.rs`, `src/facequality.rs`,
`src/segmentation.rs` (Phase 1), `Cargo.toml` (both phases), `src/bin/
decode_probe.rs` (new, verification-only).
