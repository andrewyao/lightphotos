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
`src/bin/` and `src/raw/probe.rs` link the lib. The app imports each lib module at its
crate root, so `crate::develop::..` resolves in both. A lib item the app or a probe
uses has to be `pub`, not `pub(crate)`, and a lib module can't reach into the app.
Their `#[cfg(test)]` blocks run as `cargo test --lib`, which `cargo test` includes.

rawler (LGPL-2.1) is a non-mac dependency only, and the macOS build, which ships to the
App Store, must never contain it. So the non-mac decode path and its tests compile only
off macOS, and a Mac cannot run them.

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

`--drive <script>` runs the real app against a hidden window with no OS input, feeds the script's steps (`key`, `click-cell`, `scroll`, `idle`, `shot`, `state`, and the rest listed at the top of `src/drive.rs`) into the same input handling a window's events take, writes each `shot` as a PNG under `--drive-out`, prints one JSON line per `state`, and exits non-zero on a bad script line or a 30 s `idle`. `scripts/drive-examples/` holds one script per checked feature and `make-fixture.sh`, which writes a numbered JPEG folder to drive against.

## Profiling

`hotpath` instruments the paths a culling session waits on. Listing a folder gates the rest and always runs. Behind it sit the grid's screenful of thumbnails, the filmstrip's sliding window of them, the first pixels of a photo and the preview escalation that sharpens them, the full-resolution decode a zoom needs, an Auto Tone batch, a JPEG export batch, the Loupe's subject selection, and the Vision signals. It is off unless a feature turns it on, and with its own features off its macros hand the function body back unchanged, so a default build carries no instrumentation.

```sh
cargo run --release --features hotpath -- --profile /path/to/a/folder        # timings
cargo run --release --features hotpath-alloc -- --profile /path/to/a/folder  # timings + bytes allocated per function
```

`--profile` drives the paths headlessly through the real `navigation`, `catalog`, `Loader`, `thumbnail` and `export` code, prints a per-function report, and exits without opening a window. `src/profile.rs` explains why the measurement does not go through the window.

`LIGHTPHOTOS_PROFILE_PHASES` narrows the run to a comma-separated list of phase keys, from `grid`, `strip`, `scroll`, `frame`, `open`, `full`, `auto_tone`, `export`, `select_subject` and `vision`. Unset means all of them. An unrecognized key prints a warning naming the valid keys, and the run continues. `LIGHTPHOTOS_PROFILE_COLD=1` deletes the folder's cached thumbnails first, so the grid phase measures a first visit. `LIGHTPHOTOS_PROFILE_THUMBS`, `_OPENS`, `_PREVIEW_PX`, `_FULLS`, `_EXPORTS` and `_VISION` size the phases. The `vision` phase also times `score::score_photo` per photo, and `LIGHTPHOTOS_PROFILE_SCORE_DURING=1` runs a scoring job through the `scroll` phase so its fill time can be compared with and without one. `scroll` flicks a simulated grid viewport (`_SCROLL_ROWS` by `_SCROLL_COLS`, default 5 by 6) down the first `_THUMBS` photos one row per `_STEP_MS` (default 16) without waiting, and `strip` holds the arrow key the same way, so both report how long the last viewport takes to fill after the input stops; the report's `get_or_make` count is how many thumbnails the pool decoded on the way. The queue keeps up at 16 ms, so use `_STEP_MS=4` or `1` to see a backlog. The export phase writes every JPEG into a scratch directory under the system temp directory and removes it afterwards, so profiling a folder never leaves files in it. `frame` measures what a grid frame pays on the UI thread: baking edits into a viewport of thumbnails inline versus through the decode workers, and the signal cache's periodic write, which it runs against a scratch copy of the folder's file names. `LIGHTPHOTOS_CACHE_THUMBS`, `_PREVIEWS` and `_FULLS` override the loader's cache sizes (`src/cache_limits.rs`, one default per platform) for the app and the profiler alike.

hotpath's own `HOTPATH_*` variables still apply, so `HOTPATH_OUTPUT_FORMAT=json HOTPATH_OUTPUT_PATH=run.json` writes a report that a later run can be diffed against. The report prints the top 15 functions by total time. A full run instruments over 40, so raise `HOTPATH_FUNCTIONS_LIMIT` to see the cheap phases.

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

See [docs/PROJECT_LAYOUT.md](docs/PROJECT_LAYOUT.md), and [docs/SYSTEM_DIAGRAM.md](docs/SYSTEM_DIAGRAM.md) for the same in Mermaid diagrams.

## Forms

Every form is built from `src/ui/form.rs`. That covers the modals, the right-panel Export form, and Develop's Crop and Masks tabs. `Form::new`, `section` and `row` lay out label and value rows; a side panel uses `Form::stacked`, which sets each label over its value. `form::title` heads a dialog, and a side panel page is `form::page_heading` over `form::page`. `form::segmented` is the control for a fixed single choice where one click should do it, such as Theme or crop Aspect; its segments wrap onto a grid when the row is too narrow, and it reports a click on the current segment too, so filter it when a repeat means nothing. Its segments are as tall as a footer button, so a side-by-side form puts it in `button_row` rather than `row`, which lowers the label to match. A choice that will grow, such as Language, is a list of radios. A long or open-ended list, such as Export Size or the Immich albums, is a ComboBox. A setting that is hidden for now sits behind a `SHOW_*` const in `src/app/mod.rs`, like `SHOW_AUTOTONE_CENTERING`, rather than being deleted. `form::hint` and `form::error` set helper and error text under a value. `form::dialog` opens a modal at `form::DIALOG_WIDTH` with the shared margin.

Buttons go through `form::footer`, never laid out by hand. A footer is a table of `form::Button { label, role, enabled }`, each with a `Role` of `Cancel`, `Primary` or `Danger`, and `footer` returns the role clicked. It sits against the right edge and orders the buttons for the platform: Cancel then the primary on macOS, the web and Linux, the primary then Cancel on Windows. Its buttons share a minimum width. A destructive confirm uses `Danger`. An action that belongs to one row, such as Immich's Connect, uses `form::button` with the same roles.

Spacing comes from the constants in `form.rs` through `font_size::px`, never from literal `add_space` numbers, so Alt+= and Alt+- scale it. The filled button colors are `primary_fill`, `primary_text`, `danger_fill` and `danger_text` in `theme::Palette`, and a test holds every theme to their contrast. A new fill needs a matching test.

A real page switch, such as Develop's Sliders, Crop and Masks, is a column of painted icons on the panel's right edge (`develop_panel::develop_rail`), with the page's name as hover text. It is not for a choice inside a form. New strings go in both the English and the Chinese table in `src/i18n.rs`.

## Commit messages

Do not add a `Co-Authored-By` (or similar co-author) trailer to commit messages or PR descriptions in this repo.
