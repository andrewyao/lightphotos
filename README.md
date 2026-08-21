<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# LightPhotos

A fast macOS Lightroom-lite photo culling & develop tool, written in Rust.

Open a folder to browse thumbnails in a Grid; open a single image to jump
straight into the Loupe. Decoding runs on background threads (Apple ImageIO
on macOS; `image`/`rawler` crates on Linux/Windows) and images live as GPU
textures, so zoom and pan only update a small transform uniform — never a
re-decode. egui draws all the chrome (grid, filmstrip, filter bar, rating
overlays); a hand-rolled wgpu renderer draws the loupe image.

**Linux/Windows (experimental):** The codebase builds successfully via
`cargo build --release` on Linux and Windows targets (verified via `cargo check`).
However, real runtime testing has not yet been performed on those platforms.
HEIC support and Vision-backed features (duplicate refinement, face/blink
scoring, subject-selection overlay) are macOS-only. See
[`plans/plan-i-native-linux-windows-port.md`](plans/plan-i-native-linux-windows-port.md)
for full implementation status.

## Requirements

- **macOS 11.0 or later** (the app links AppKit / Core Graphics / ImageIO via `objc2`).
- **Rust stable ≥ 1.92.** The repo pins `channel = "stable"` in
  `rust-toolchain.toml`, so `rustup` selects a compatible toolchain automatically
  without touching your global default. The version floor comes from egui 0.34
  (the only egui line compatible with wgpu 29).

If you don't have Rust yet:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

## Build from scratch

Clone, then build the release binary:

```sh
git clone <repo-url> lightphotos
cd lightphotos
cargo build --release
```

The binary lands at `target/release/lightphotos`. You can run it directly:

```sh
./target/release/lightphotos /path/to/a/photo.jpg     # opens in Loupe
./target/release/lightphotos /path/to/a/folder        # opens in Grid
```

A debug build (`cargo build`) works too, but release is strongly recommended —
the `[profile.release]` settings (`opt-level = 3`, thin LTO, single codegen
unit) matter a lot for decode/render throughput.

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

| Key | Action |
| --- | --- |
| `G` | Grid view |
| `E` / `Enter` | Loupe (open selected) |
| `Esc` | Back out (Loupe → Grid, Grid → quit) |
| Arrows | Move grid selection / step the loupe |
| `1`–`5` | Rate current image; `0` clears |
| `Shift`+`1`–`5` | Set a "≥ N stars" filter; `Shift`+`0` clears |
| `B` | Toggle Bursts view |
| `+` / `-` | Adjust thumbnail size (Grid) |
| Scroll | Zoom (Loupe) |
| `Space`+drag | Pan (Loupe) |
| `Cmd`+`[` / `Cmd`+`]` | Rotate (Loupe) |
| `Alt`+`0` | Reset to 100% |
| `C` | Crop mode (Loupe): drag edges, `Shift` keeps ratio, `C`/`Enter` commits, `Esc` cancels |
| `X` | Export selected image as `.jpg` in the same folder (edits baked in, never overwrites) |

## Project layout

- `src/main.rs` — crate root: owns the winit event loop and `main()`.
- `src/app.rs` — all viewer state and behavior.
- `src/renderer.rs`, `src/shader.wgsl` — the wgpu loupe renderer.
- `src/ui.rs` — egui chrome (grid, filmstrip, filter bar, overlays).
- `src/loader.rs`, `src/image_decode.rs`, `src/thumbnail.rs` — background decode & thumbnails.
- `src/develop.rs`, `src/image_ops.rs`, `src/export.rs` — edits, transforms, JPEG export.
- `src/burst.rs`, `src/sharpness.rs` — burst grouping & best-of-burst scoring.
- `src/catalog.rs` — SQLite catalog persistence (ratings, metadata).
- `src/coregraphics.rs`, `src/macos_delegate.rs` — macOS/AppKit integration.

## License

Licensed under the [GNU GPL v3.0 (or later)](LICENSE).
