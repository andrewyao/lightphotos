# CLAUDE.md

LightPhotos: a fast Lightroom-lite photo culling and develop tool in Rust, for macOS, Linux, Windows and the browser (wasm32). egui draws the chrome; a hand-rolled wgpu renderer draws the Loupe image. Decode uses Apple ImageIO on macOS and the `image`/`rawler`/`mozjpeg-rs`/`kamadak-exif` crates elsewhere. HEIC and the Vision features are macOS-only.

## Commands

Run `./scripts/setup.sh` once per clone before any `cargo` command (`./scripts/setup-web.sh` for the browser build). It builds the patched rawler copy in `vendor/`, which isn't committed; `Cargo.toml`'s `[patch.crates-io]` comment says why it exists.

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release && cargo build --bins   # verify a change
cargo test <name>          # one test by name substring; tests live inline in each module
```

The package has a lib target (`src/lib.rs`: decode, encode, develop, scoring) and an app binary (`src/main.rs`: the rest). The probes in `src/bin/` link the lib. A lib item the app or a probe uses must be `pub`, not `pub(crate)`, and a lib module can't reach into the app.

### rawler stays out of the macOS build

rawler (LGPL-2.1) is a non-mac dependency only. The macOS build ships to the App Store and must never contain it. Every rawler call lives in `src/decode/rawler/`, which `decode/mod.rs` builds only off macOS. Keep new rawler code there. CI fails if rawler enters the macOS dependency graph.

### Checking the non-mac code from a Mac

The Linux and wasm branches typecheck and lint locally:

```sh
CC_x86_64_unknown_linux_gnu=clang AR_x86_64_unknown_linux_gnu=ar \
CFLAGS_x86_64_unknown_linux_gnu="-isystem $(xcrun --show-sdk-path)/usr/include" \
cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings
bash -c 'source scripts/web-env.sh && cargo clippy --target wasm32-unknown-unknown --bin lightphotos -- -D warnings'
```

`clippy` doesn't link or run anything. The non-mac decode tests and `decode_probe`'s fixture suite run only on CI's Linux job, and Windows builds only on CI. `.github/workflows/ci.yml` runs on every push to `main` and every PR; its `cargo fmt --check` and `clippy -D warnings` (on all four targets) block, so run both before committing.

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

## Rust rules

People trust LightPhotos with their photo folders and edits. A malformed JPEG, raw, EXIF block, sidecar or preset, a bad `--drive` line, a missing GPU or a full disk must give an error the user can act on, never a panic. A panic on wasm32 kills the tab's app or a decode worker. Fix a crash before building on top of it.

- **Non-test code never panics.** No `unwrap()`, `expect()`, `panic!`, `unreachable!`, `todo!`, `unimplemented!`. Return the module's error through `Result` and `?`; use `ok_or(..)?`, `let … else`, `if let`, or a fallback (`unwrap_or…`) only where it can't silently corrupt a sidecar or an edit. UI code with no caller to return to logs and skips. Sole exception: a provably infallible literal, as `#[allow(clippy::expect_used)]` + `.expect("why it can't fail")`. Restructure a match rather than leave an impossible arm.
- **`unsafe` is for platform FFI only**: ImageIO, CoreGraphics, Vision and AppKit through objc2 (`decode/image_decode`, `decode/image_encode`, `decode/coregraphics`, `scoring/{vision,judge,facequality,segmentation}`, `jobs/thumbnail`, `shell/macos_delegate`), thread priority (`jobs/score`) and the Windows console (`main.rs`). Every block has a `// SAFETY:` comment naming the invariant it relies on. Wrap it in a safe function; callers never see raw pointers.
- **Input-derived numbers are hostile.** Offsets and lengths from files, EXIF, sidecars, presets or drive scripts use `get()`, not `[i]`/`[a..b]`, and checked or saturating math. No division by zero, NaN/inf or negative casts to `usize`; cap allocations sized by input; slice strings only at char boundaries. Pixel loops over buffers we sized ourselves may index.
- **Bound recursion** (folder walks, nested metadata) with a depth limit or a seen-set. **Don't cascade:** `lock().unwrap_or_else(PoisonError::into_inner)` or an error; treat thread joins as `Result`s.
- **Last-resort guard:** `jobs::loader`'s panic recovery and the `catch_unwind` around decode jobs turn an escaped panic into a failed job. It is a safety net, not a licence; keep native `panic = "unwind"`.
- **Prove it:** a crash fix lands with a small synthetic test (truncated or malformed input) that panicked before the fix.
- **Enforced by clippy.** `src/lib.rs`, `src/main.rs` and each `src/bin/*.rs` carry `#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable, clippy::undocumented_unsafe_blocks)]`; `clippy.toml` allows the panic lints in tests. A new binary starts with the attribute. Clippy only treats a plain `#[cfg(test)]` as test code, so gate a platform-specific test module with two attributes (`#[cfg(test)]` and `#[cfg(not(target_arch = "wasm32"))]`), not `cfg(all(test, …))`. Fix a lint rather than `#[allow]` it; an allow carries a one-line reason.

## Docs

- Architecture: [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md), diagrams in [docs/SYSTEM_DIAGRAM.md](docs/SYSTEM_DIAGRAM.md).
- UI: read [docs/UI.md](docs/UI.md) before adding or changing any UI.
- Releases and profiling: [README.md](README.md).

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
