<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

<h1><img src="assets/icon/wordmark.svg" alt="LightPhotos" width="420"></h1>

A fast macOS Lightroom-lite photo culling & develop tool, written in Rust.

Open a folder to browse thumbnails in a Grid; open a single image to jump
straight into the Loupe. Decoding runs on background threads (Apple ImageIO
on macOS; `image` / `rawler` / `mozjpeg-rs` / `kamadak-exif` crates on other
platforms — including a Web Worker pool on wasm32) and images live as GPU
textures, so zoom and pan only update a small transform uniform — never a
re-decode. egui draws all the chrome (grid, filmstrip, filter bar, rating
overlays); a hand-rolled wgpu renderer draws the loupe image.

**Linux/Windows (experimental):** The codebase builds successfully via
`cargo build --release` on Linux and Windows targets (verified via `cargo check`),
after the one-time `rawler` vendor step (see [Build from scratch](#build-from-scratch)).
However, real runtime testing has been performed on Linux only, not on Windows yet.

HEIC support and Vision-backed features (duplicate refinement, face/blink
scoring, subject-selection overlay) are macOS-only.

See [`plans/plan-i-native-linux-windows-port.md`](plans/plan-i-native-linux-windows-port.md)
for full implementation status.

## Requirements

- **macOS 11.0 or later** (the app links AppKit / Core Graphics / ImageIO via `objc2`).
  Two Vision features need more than that and are checked at runtime, so an
  older system loses the feature rather than the app. Subject selection in the
  Loupe needs macOS 12.0 for person segmentation and macOS 14.0 for the
  general foreground fallback.
  Linux, Windows, and wasm32 builds are experimental — see the note above.
- **Rust stable ≥ 1.92.** The repo pins `channel = "stable"` in
  `rust-toolchain.toml`, so `rustup` selects a compatible toolchain automatically
  without touching your global default. The version floor comes from egui 0.34
  (the only egui line compatible with wgpu 29).

If you don't have Rust yet:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build from scratch

Clone, run the one-time vendor setup, then build the release binary:

```sh
git clone <repo-url> lightphotos
cd lightphotos
./scripts/setup-vendor-rawler.sh   # one-time per clone — see below
cargo build --release
```

### The `rawler` vendor step

`[patch.crates-io]` in `Cargo.toml` redirects the `rawler` crate (camera RAW
decode) to a locally patched copy at `vendor/rawler-0.7.2`. The patch
(`patches/rawler-web-time.patch`) swaps `std::time::Instant` for
`web_time::Instant` at four call sites that otherwise panic on
`wasm32-unknown-unknown` (no clock source); it is a real passthrough to
`std::time::Instant` — zero functional change — on macOS/Linux/Windows.

`vendor/` is **not** committed (it is git-ignored), so
`scripts/setup-vendor-rawler.sh` must be run once per fresh clone: it fetches
plain `rawler` 0.7.2 from crates.io and applies the patch. A cold clone
therefore needs network access to crates.io before its first build. The script
is a no-op on re-run (`--force` regenerates the tree), and
`scripts/bundle.sh` / `scripts/deploy-web.sh` run it for you.

Cargo applies `[patch.crates-io]` on every target, so this step is required
before **any** Cargo command — `cargo build`, `cargo test`, `cargo check` — not
just wasm builds.

**Windows:** run the script from Git Bash (bundled with
[Git for Windows](https://git-scm.com/download/win)), which provides the `sh`,
`patch`, and `mktemp` it needs; `curl` and `tar` are already part of
Windows 10+.

The binary lands at `target/release/lightphotos`. You can run it directly:

```sh
./target/release/lightphotos /path/to/a/photo.jpg     # opens in Loupe
./target/release/lightphotos /path/to/a/folder        # opens in Grid
```

A debug build (`cargo build`) works too, but release is strongly recommended —
the `[profile.release]` settings (`opt-level = 3`, thin LTO, single codegen
unit) matter a lot for decode/render throughput.

### Web build (experimental)

A wasm32 build runs in the browser via [trunk](https://trunkrs.dev) (after the
`rawler` vendor step above). Install `trunk` and the wasm target once per
machine:

```sh
brew install trunk                      # or: cargo install --locked trunk
rustup target add wasm32-unknown-unknown
```

Then build:

```sh
RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release
```

`scripts/deploy-web.sh` wraps this and syncs the output into the companion site
repo. The browser build reads folders through the File System Access API and
decodes on a hand-rolled Web Worker pool; it has none of the macOS-only
features (no HEIC, no Vision-backed scoring).

## Package as `LightPhotos.app`

To get a double-clickable macOS app bundle registered with Finder / "Open With":

```sh
./scripts/bundle.sh
```

This script:

1. Builds the release binary (`cargo build --release`).
2. Assembles `LightPhotos.app/` from `Info.plist` and the release binary.
3. Registers the bundle with Launch Services (`lsregister`) so Finder routes
   image files to it.

Both `target/` and `LightPhotos.app/` are git-ignored — they're build outputs.

After bundling:

```sh
open -a "$(pwd)/LightPhotos.app" /path/to/photo.jpg
# or in Finder: right-click an image → Open With → LightPhotos
```

The bundle registers as an *Alternate* viewer for common image types (JPEG,
PNG, TIFF, GIF, BMP, HEIC/HEIF, camera RAW), so it appears under "Open With"
without hijacking your default image handler.

## Keyboard shortcuts

See [`docs/KEYBOARD_SHORTCUTS.md`](docs/KEYBOARD_SHORTCUTS.md) for the full table. Press `?` in-app for the built-in overlay.

## Project layout

See [`docs/PROJECT_LAYOUT.md`](docs/PROJECT_LAYOUT.md) for the full directory listing.

## License

Licensed under the [GNU GPL v3.0 (or later)](LICENSE).
