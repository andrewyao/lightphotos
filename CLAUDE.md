# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LightPhotos: a fast macOS Lightroom-lite photo culling & develop tool, written in Rust. Open a folder to browse thumbnails in a Grid; open a single image to jump straight into the Loupe. Decoding runs on background threads (Apple ImageIO) and images live as GPU textures, so zoom/pan only update a small transform uniform, never a re-decode. egui draws all the chrome (grid, filmstrip, filter bar, rating overlays); a hand-rolled wgpu renderer draws the loupe image.

## Commands

```sh
cargo build --release   # release binary at target/release/lightphotos (opt-level 3, thin LTO — matters for decode/render throughput)
cargo build              # debug build; works but noticeably slower at runtime
cargo test                # run all unit tests (tests live inline in each module, #[cfg(test)])
cargo test <name>         # run a single test by name substring, e.g. `cargo test burst::`
./scripts/bundle.sh       # build release + assemble LightPhotos.app + register with Launch Services (lsregister)
```

Run the binary directly against a path (no bundling needed for dev iteration):

```sh
./target/release/lightphotos /path/to/a/photo.jpg     # opens in Loupe
./target/release/lightphotos /path/to/a/folder        # opens in Grid
```

Requires macOS 11+ and Rust stable ≥ 1.92 (pinned via `rust-toolchain.toml`; egui 0.34 needs it for wgpu 29 compatibility). No lint config (clippy.toml/rustfmt.toml) beyond cargo defaults.

## Architecture

**Entry point / event loop**: `src/main.rs` is the crate root. It owns the winit event loop and `main()`, translating raw window events into calls on `App`. It does not hold app state itself — everything about *what* to show and *how* the loupe is transformed lives in `src/app.rs`.

**`App` (`src/app.rs`, ~2.9k lines)**: the coordinator that ties together the GPU renderer, the background loader, the ratings/edits catalog, and the egui chrome, plus keyboard bindings. This is the biggest and most central module — read it first when tracing how a keypress or click turns into a state change.

**Rendering split**: two renderers coexist deliberately.
- `src/renderer.rs` + `src/shader.wgsl`: hand-rolled wgpu renderer, draws only the loupe image, confined to a viewport rect. Zoom/pan/rotate update a small uniform, never trigger a re-decode.
- `src/ui.rs`: egui chrome (grid, filmstrip, filter bar, rating overlays) built against egui 0.34's `Panel`/`show_inside` API. The Loupe's central region is frameless/transparent so the wgpu image shows through underneath. `ui.rs` reports back the central image rect and any user actions (clicks, slider, filter changes) for `main.rs`/`app.rs` to apply — it doesn't mutate `App` state directly.

**Background decode/loader (`src/loader.rs`)**: a worker thread pool decodes off the UI thread, with two cache tiers sharing one work queue and one results channel — a full-image LRU for the loupe (`request`/`get`/`poll`) and a larger thumbnail LRU for the grid/filmstrip (`request_thumb`/`get_thumb`/`poll_thumbs`, backed by `src/thumbnail.rs`'s on-disk `ThumbCache`). Jobs are prioritized (full-image over thumbnail over capture-time metadata reads) with a dedicated worker reservation so a freshly opened image isn't stuck behind a thumbnail flood — see the priority-queue and reservation comments in `loader.rs` before touching scheduling.

**Decode/encode via ImageIO (no third-party codecs)**: `src/image_decode.rs` and `src/image_encode.rs` are counterpart pipelines built on Apple's ImageIO + CoreGraphics (CFURL → CGImageSource/CGImageDestination, drawn through a CGBitmapContext). `src/coregraphics.rs` holds the classic (non-block) CoreGraphics symbol declarations and CFURL/bitmap-context setup shared by both, since `objc2-core-graphics` 0.3 doesn't surface them. `src/thumbnail.rs` mirrors `image_decode.rs` but asks ImageIO for a decode-at-size preview instead of a full decode.

**Develop / edits model (`src/develop.rs`)**: `Adjustments` is the single source of truth for non-destructive edits (persisted via serde in the catalog); `GpuAdjust` is its packed `#[repr(C)]` mirror uploaded as a uniform to the shader; `apply_linear` is a CPU mirror of the same tone pipeline used for the live histogram, so the WGSL shader and the histogram never disagree. Tone sliders are −100..=100 (0 = identity), exposure is −5..=5 stops (0 = identity); `Adjustments::default()` is the identity edit.

**Pure pixel ops (`src/image_ops.rs`)**: crop/tone/rotate math with no `App` dependency, shared by export (`src/export.rs`), thumbnail baking, and the histogram — this is what guarantees an exported JPEG matches the on-screen edited thumbnail.

**Export (`src/export.rs`)**: exporting bakes edits into a full-resolution decode and writes JPEG via ImageIO — expensive, so it runs on a small worker pool (mirroring `loader.rs`) rather than the UI thread. Workers are handed a fully self-contained `ExportJob` and never touch `App` state, so there's no shared-state coupling to reason about.

**Catalog / persistence (`src/catalog.rs`)**: ratings + develop edits live in per-photo sidecar files, `<photo's directory>/.lightphotos/<photo filename>.xmp` (the `.xmp` extension is cosmetic — the body is our own JSON, not real Adobe XMP/RDF), so they travel with a folder when it's moved, copied, or shared instead of being orphaned by a single global catalog. `.lightphotos` is created lazily on first write. `Catalog` is scoped to one active directory at a time — `open_dir` (called from `App::seed_mirrors` on every folder/file open, including sidebar subfolder navigation) reloads its in-memory read cache from that directory's sidecars; individual reads/writes still resolve directly from a photo's own path, independent of the active directory. Writes are one-file-per-photo, atomic (temp file + rename). Originals in photo folders are never touched. The legacy single global SQLite catalog (and, before that, JSON) is auto-migrated once via `migrate_legacy_catalog`, fanning each row out to its target directory's sidecar; a row whose directory is missing/unwritable is skipped and retried on the next launch, and the legacy file is only retired once a full pass has zero skips.

**Best-of-burst (`src/burst.rs` + `src/sharpness.rs`)**: `sharpness.rs` scores focus via variance-of-Laplacian on a downscaled grayscale image (comparable across differing resolutions); `burst.rs` is pure/total logic that, given a burst grouping and scores, decides the best frame per burst and its siblings — deliberately free of UI/filesystem/decode state so it's unit-testable in isolation.

**macOS integration**: `src/macos_delegate.rs` hooks Finder "open document" events (double-click / "Open With" / `open -a`) by adding an `application:openURLs:` method to winit's own `NSApplicationDelegate` class at runtime via the Objective-C runtime, since winit both requires owning the app delegate and doesn't implement that method itself. It forwards the opened path into the winit event loop via an `EventLoopProxy`. `src/trash.rs` moves files to the macOS Trash via `NSFileManager`.

**Navigation (`src/navigation.rs`)**: given an opened image, lists sibling images in the same directory (sorted) for prev/next stepping.

**Hashing (`src/hash.rs`)**: a minimal dependency-free FNV-1a 64-bit hasher, used because it's stable/deterministic across process runs (unlike `DefaultHasher`) — needed for the on-disk thumbnail cache key and the develop edit signature.

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
