# CLAUDE.md

LightPhotos: a fast Lightroom-lite photo culling and develop tool in Rust, for macOS, Linux, Windows and the browser (wasm32). egui draws the chrome; a hand-rolled wgpu renderer draws the Loupe image. Decode uses Apple ImageIO on macOS and the `image`/`rawler`/`mozjpeg-rs`/`kamadak-exif` crates elsewhere. HEIC and the Vision features are macOS-only.

## Commands

Run `./scripts/setup.sh` once per clone before any `cargo` command (`./scripts/setup-web.sh` for the browser build). It builds the patched rawler copy in `vendor/`, which isn't committed; `Cargo.toml`'s `[patch.crates-io]` comment says why it exists.

```sh
cargo fmt --check && cargo test && cargo build --release && cargo build --bins   # verify a change
cargo test <name>          # one test by name substring; tests live inline in each module
```

The package has a lib target (`src/lib.rs`: decode, encode, develop, scoring) and an app binary (`src/main.rs`: the rest). The probes in `src/bin/` link the lib. A lib item the app or a probe uses must be `pub`, not `pub(crate)`, and a lib module can't reach into the app.

### rawler stays out of the macOS build

rawler (LGPL-2.1) is a non-mac dependency only. The macOS build ships to the App Store and must never contain it. Every rawler call lives in `src/decode/rawler/`, which `decode/mod.rs` builds only off macOS. Keep new rawler code there. CI fails if rawler enters the macOS dependency graph.

### Checking the non-mac code from a Mac

The Linux and wasm branches typecheck locally:

```sh
CC_x86_64_unknown_linux_gnu=clang AR_x86_64_unknown_linux_gnu=ar \
CFLAGS_x86_64_unknown_linux_gnu="-isystem $(xcrun --show-sdk-path)/usr/include" \
cargo check --all-targets --target x86_64-unknown-linux-gnu
bash -c 'source scripts/web-env.sh && cargo check --target wasm32-unknown-unknown --bin lightphotos'
```

`check` doesn't link or run anything. The non-mac decode tests and `decode_probe`'s fixture suite run only on CI's Linux job, and Windows builds only on CI. `.github/workflows/ci.yml` runs on every push to `main` and every PR, and its `cargo fmt --check` blocks, so run `cargo fmt` before committing.

### Browser build

```sh
source scripts/web-env.sh && trunk serve --release --config Trunk.toml
./scripts/deploy-web.sh    # build and sync into the lightphotos.app site repo
```

Always `--release`: debug wasm is 10-30x slower. Run `./scripts/fetch-cjk-font.sh` once before a bare `trunk build`/`trunk serve`. `scripts/web-env.sh` explains the pinned nightly and the flags.

### Driving the app

```sh
./target/release/lightphotos /path/to/photo.jpg      # opens in Loupe
./target/release/lightphotos /path/to/folder         # opens in Grid
./target/release/lightphotos --drive scripts/drive-examples/rate.txt --drive-out /tmp/d /path/to/folder
```

`--drive` runs the real app in a hidden window and feeds it a script's steps (listed at the top of `src/shell/drive.rs`). It writes each `shot` as a PNG, prints a JSON line per `state`, and exits non-zero on a bad line or a 30 s `idle`. `scripts/drive-examples/make-fixture.sh` writes a test folder.

## Docs

- Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), diagrams in [docs/SYSTEM_DIAGRAM.md](docs/SYSTEM_DIAGRAM.md).
- UI: read [docs/UI.md](docs/UI.md) before adding or changing any UI.
- Releases and profiling: [README.md](README.md).

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
