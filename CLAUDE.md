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

Requires macOS 11+ and Rust stable ≥ 1.92 (pinned via `rust-toolchain.toml`; egui 0.34 needs it for wgpu 29 compatibility). No lint config (clippy.toml/rustfmt.toml) beyond cargo defaults.

## Architecture

See [docs/PROJECT_LAYOUT.md](docs/PROJECT_LAYOUT.md).

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
