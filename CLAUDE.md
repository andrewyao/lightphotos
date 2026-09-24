# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LightPhotos: a fast macOS Lightroom-lite photo culling & develop tool, written in Rust. The codebase builds on macOS, Linux, and Windows. Open a folder to browse thumbnails in a Grid; open a single image to jump straight into the Loupe. Decoding runs on background threads (Apple ImageIO on macOS; `image`/`rawler`/`mozjpeg-rs`/`kamadak-exif` crates on non-macOS platforms) and images live as GPU textures, so zoom/pan only update a small transform uniform, never a re-decode. egui draws all the chrome (grid, filmstrip, filter bar, rating overlays); a hand-rolled wgpu renderer draws the loupe image. HEIC and Vision-backed features (duplicate refinement, face/blink scoring, subject-selection overlay) are macOS-only.

## Commands

Per-clone setup, before any `cargo` command below: `./scripts/setup-vendor-rawler.sh` (safe to re-run — no-op once the tree is present; `--force` regenerates it). `[patch.crates-io]` in `Cargo.toml` redirects the `rawler` crate (camera RAW decode) to a local patched copy at `vendor/rawler-0.7.2` for every target, not just wasm32 (Cargo has no way to scope a patch to one target) — see that entry's own comment for why. The script fetches plain rawler 0.7.2 from crates.io and applies `patches/rawler-web-time.patch`. `vendor/` isn't committed (see `.gitignore`), so a cold clone needs network to crates.io before its first build; `bundle.sh` and `deploy-web.sh` run the script for you.

```sh
cargo build --release   # release binary at target/release/lightphotos (opt-level 3, thin LTO — matters for decode/render throughput)
cargo build              # debug build; works but noticeably slower at runtime
cargo test                # run all unit tests (tests live inline in each module, #[cfg(test)])
cargo test <name>         # run a single test by name substring, e.g. `cargo test burst::`
./scripts/bundle.sh       # build release + assemble LightPhotos.app + register with Launch Services (lsregister)
./scripts/release.sh      # test, tag origin/main as the next patch (or pass v1.2.3), push the tag → release.yml builds and publishes
```

The wasm32 (browser) build goes through `trunk`, not bare `cargo` — plain `cargo build --target wasm32-unknown-unknown` misses the wgpu/WebGPU and File System Access bindings, which are gated behind an unstable-apis cfg that `Trunk.toml`'s `rustflags` key does *not* reach cargo with in trunk 0.21.14. Set it in the environment:

```sh
RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml
./scripts/deploy-web.sh   # the above + vendor setup + sync into the lightphotos.app site repo
```

Always `--release` for wasm: debug wasm is 10-30x slower at RAW decode/demosaic and the `bg.wasm` is ~10x larger.

Run the binary directly against a path (no bundling needed for dev iteration):

```sh
./target/release/lightphotos /path/to/a/photo.jpg     # opens in Loupe
./target/release/lightphotos /path/to/a/folder        # opens in Grid
```

## Profiling

`hotpath` instruments the three paths a culling session waits on: listing a folder, filling the grid with thumbnails, and opening one photo. It is off unless a feature turns it on, and with its own features off its macros hand the function body back unchanged, so a default build carries no instrumentation.

```sh
cargo run --release --features hotpath -- --profile /path/to/a/folder        # timings
cargo run --release --features hotpath-alloc -- --profile /path/to/a/folder  # timings + bytes allocated per function
```

`--profile` drives the paths headlessly through the real `navigation`, `catalog`, `Loader` and `thumbnail` code, prints a per-function report, and exits without opening a window. `src/profile.rs` explains why the measurement does not go through the window. `LIGHTPHOTOS_PROFILE_COLD=1` deletes the folder's cached thumbnails first, so the grid phase measures a first visit; `LIGHTPHOTOS_PROFILE_THUMBS`, `_OPENS` and `_PREVIEW_PX` size the run. hotpath's own `HOTPATH_*` variables still apply, so `HOTPATH_OUTPUT_FORMAT=json HOTPATH_OUTPUT_PATH=run.json` writes a report that a later run can be diffed against.

Turning `hotpath` on instruments the windowed app too. There the report prints when `main` returns, which Cmd+Q does not always reach, so set `HOTPATH_SHUTDOWN_MS=30000` to have it report on a timer instead.

Requires macOS 11+ and Rust stable ≥ 1.92 (pinned via `rust-toolchain.toml`; egui 0.34 needs it for wgpu 29 compatibility). No lint config (clippy.toml/rustfmt.toml) beyond cargo defaults.

## Live view (lightwatch)

`hotpath` reports after the fact. [lightwatch](https://github.com/andrewyao/lightwatch) shows the same run live, in a browser, while you cull. One script brings the whole session up and another takes it down:

```sh
./scripts/lightwatch-up.sh /path/to/a/folder   # daemon + bridge + instrumented app, opens http://127.0.0.1:7700
./scripts/lightwatch-down.sh                   # stops all three and clears the ingest socket
```

Three processes, because lightphotos emits only half the picture by itself. The daemon ingests and serves both the API and the UI; `lightwatch-hotpath` polls the app's hotpath server on `:6770` and re-emits function calls and timings; the app's own `lightwatch-probe` emits the live-object census that `#[lightwatch::track]` collects (`DecodedImage`, `Mask`). The daemon joins the two emitters into one session at `GET /api/sessions`. `LIGHTWATCH_REPO` points at the lightwatch checkout (default `../lightwatch`), `LIGHTWATCH_PORT` moves the UI, and pids and logs live under `$TMPDIR/lightphotos-lightwatch`.

## Architecture

See [docs/PROJECT_LAYOUT.md](docs/PROJECT_LAYOUT.md).

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
