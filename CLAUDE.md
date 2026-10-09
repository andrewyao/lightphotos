# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

LightPhotos: a fast macOS Lightroom-lite photo culling & develop tool, written in Rust. The codebase builds on macOS, Linux, and Windows. Open a folder to browse thumbnails in a Grid; open a single image to jump straight into the Loupe. Decoding runs on background threads (Apple ImageIO on macOS; `image`/`rawler`/`mozjpeg-rs`/`kamadak-exif` crates on non-macOS platforms) and images live as GPU textures, so zoom/pan only update a small transform uniform, never a re-decode. egui draws all the chrome (grid, filmstrip, filter bar, rating overlays); a hand-rolled wgpu renderer draws the loupe image. HEIC and Vision-backed features (face/blink scoring, subject-selection overlay) are macOS-only.

## Commands

Per-clone setup, before any `cargo` command below: `./scripts/setup.sh` (`./scripts/setup-web.sh` for the browser build, which runs it too). It calls `./scripts/setup-vendor-rawler.sh` (safe to re-run — no-op once the tree is present; `--force` regenerates it; `--check` exits non-zero when the tree is missing or stale, which `build.sh` and `deploy-web.sh` use). `[patch.crates-io]` in `Cargo.toml` redirects the `rawler` crate (camera RAW decode) to a local patched copy at `vendor/rawler-0.7.2` for every target, not just wasm32 (Cargo has no way to scope a patch to one target) — see that entry's own comment for why. The script fetches plain rawler 0.7.2 from crates.io and applies `patches/rawler-web-time.patch` and `patches/rawler-ljpeg-restart.patch`, and regenerates a tree set up with a different patch list. `vendor/` isn't committed (see `.gitignore`), so a cold clone needs network to crates.io before its first build; `release.sh` and `make-dmg.sh` run it for you.

```sh
./scripts/build.sh release   # = cargo build --release; release binary at target/release/lightphotos (opt-level 3, thin LTO — matters for decode/render throughput)
cargo build              # debug build; works but noticeably slower at runtime
cargo test                # run all unit tests (tests live inline in each module, #[cfg(test)])
cargo test <name>         # run a single test by name substring, e.g. `cargo test quality::`
cargo build --bins        # also builds the face_probe/seg_probe/score_probe harnesses; see below
./scripts/make-dmg.sh aarch64-apple-darwin out.dmg   # build release, assemble + ad-hoc sign LightPhotos.app, wrap in a .dmg (release.yml runs it per target)
./scripts/release.sh      # test, tag origin/main as the next patch (or pass v1.2.3), push the tag → release.yml builds and publishes
```

Verify a change with `cargo fmt --check && cargo test && cargo build --release && cargo build --bins`.
The package has a lib target (`src/lib.rs`) holding the decode, encode, develop and
scoring layer, and an app binary (`src/main.rs`) holding the rest. The probes in
`src/bin/` link the lib. The app imports each lib module at its
crate root, so `crate::develop::..` resolves in both. A lib item the app or a probe
uses has to be `pub`, not `pub(crate)`, and a lib module can't reach into the app.
Their `#[cfg(test)]` blocks run as `cargo test --lib`, which `cargo test` includes.

rawler (LGPL-2.1) is a non-mac dependency only, and the macOS build, which ships to the
App Store, must never contain it. So the non-mac decode path and its tests compile only
off macOS, and a Mac cannot run them. Every rawler call lives in `src/decode/rawler/`, which
`decode/mod.rs` builds only off macOS. `decode_probe`, the harness for that path, is in
`src/decode/rawler/probe/`. Keep new rawler code there.

`.github/workflows/ci.yml` runs on every push to `main` and every pull request. On macOS
it runs `cargo build --bins`, `cargo test` and clippy, and fails if rawler enters the
macOS dependency graph. On Linux it runs `cargo build --bins` and `cargo test`, the only
place the non-mac decode tests and `decode_probe`'s fixture suite run. It also builds the
wasm target, and builds `--bin lightphotos` on Windows. Before a tag, these are the only
things that typecheck the non-mac branches. `cargo fmt --check` runs there too and
blocks, so run `cargo fmt` before committing.

The probes compile on every platform, but to a stub that exits where they cannot run:
the Vision probes off macOS, `decode_probe` on macOS. The optimized native build stays
out of CI, since `release.yml` covers it when a tag is pushed.

The wasm32 (browser) build goes through `trunk`, on a dated nightly, with its environment in `scripts/web-env.sh`. The decode workers run as wasm threads over one shared memory, which needs std rebuilt with atomics (`-Z build-std`), hence the nightly; native stays on stable. The same file sets the unstable-apis cfg for the wgpu/WebGPU and File System Access bindings, which `Trunk.toml`'s `rustflags` key does *not* reach cargo with in trunk 0.21.14, and the shared-memory link arguments. The page must be cross-origin isolated (COOP/COEP) for `SharedArrayBuffer`: `trunk serve` sends the headers from `Trunk.toml`, and `deploy-web.sh` writes them into the site's `public/_headers`. Move the pinned nightly on purpose and rerun `tools/web-bench` after.

```sh
./scripts/setup-web.sh    # once: setup.sh + pinned nightly with rust-src and wasm target
source scripts/web-env.sh && trunk build --release --config Trunk.toml   # build into dist/ only
source scripts/web-env.sh && trunk serve --release --config Trunk.toml
./scripts/deploy-web.sh   # check setup, trunk build --release, sync into the lightphotos.app site repo
```

`index.html` copies the full Noto Sans SC (8 MB, not committed) into the build, so `deploy-web.sh` runs `fetch-cjk-font.sh` before it builds (a no-op once the file is there). Run `./scripts/fetch-cjk-font.sh` yourself once before a bare `trunk build`/`trunk serve`. The app fetches that file only when a listed file or folder name has Chinese characters the bundled UI subset can't draw.

Always `--release` for wasm: debug wasm is 10-30x slower at RAW decode/demosaic and the `bg.wasm` is ~10x larger.

Run the binary directly against a path (no bundling needed for dev iteration):

```sh
./target/release/lightphotos /path/to/a/photo.jpg     # opens in Loupe
./target/release/lightphotos /path/to/a/folder        # opens in Grid
./target/release/lightphotos --drive scripts/drive-examples/rate.txt --drive-out /tmp/d /path/to/a/folder
```

`--drive <script>` runs the real app against a hidden window with no OS input, feeds the script's steps (`key`, `click-cell`, `scroll`, `idle`, `shot`, `state`, and the rest listed at the top of `src/shell/drive.rs`) into the same input handling a window's events take, writes each `shot` as a PNG under `--drive-out`, prints one JSON line per `state`, and exits non-zero on a bad script line or a 30 s `idle`. With no path it starts on the home page as a first launch, tour up. `scripts/drive-examples/` holds one script per checked feature and `make-fixture.sh`, which writes a numbered JPEG folder to drive against.

## Profiling

`hotpath` timings (`--profile`, its env vars) and the lightwatch live view are in [README.md](README.md#profiling-optional).

Requires macOS 11+ and Rust stable ≥ 1.92 (pinned via `rust-toolchain.toml`; egui 0.34 needs it for wgpu 29 compatibility). No lint config (clippy.toml/rustfmt.toml) beyond cargo defaults.

## Architecture

See [docs/SYSTEM_DIAGRAM.md](docs/SYSTEM_DIAGRAM.md) for the Mermaid component and sequence diagrams, and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for when each decode/render pipeline runs on each platform.

## UI

Forms, buttons, spacing and page switches follow [docs/UI.md](docs/UI.md). Read it before adding or changing any UI.

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
