# Native landing page with a folder picker

## Context

Today the native binary **requires** a path as its first CLI argument
(`src/main.rs:585-592`): the dev binary calls `print_usage_and_exit()` when it
is missing, and only a `.app` bundle is allowed to start with nothing (Finder
delivers a path later via AppleEvent). Launched empty, a bundle shows a dead
window — an empty folder sidebar and a "0 photos" grid — with **no way to open
a folder from inside the app**: there is no menu bar, no toolbar button, no
`Open` shortcut, and no native file dialog anywhere in the tree (`rfd` is not a
dependency; the only picker is wasm's `showDirectoryPicker`).

The wasm build already solves this with a **landing page**
(`draw_landing_page`, `src/ui/mod.rs:263-286`): when `self.playlist` is `None`
it draws a centered "LightPhotos" heading + one "Choose Folder" button that
fires `UiAction::PickFolder`; picking a folder runs the same
`App::load_playlist` tail the CLI path uses, and the next frame the grid
renders.

**Goal:** make the first CLI arg optional on native and reuse the wasm landing
page. No arg (or a bad path) → window opens on the landing page → "Choose
Folder" → native OS folder dialog → grid. A valid folder/photo arg still opens
straight into Grid/Loupe exactly as now.

**Also in this change (per review):**
- An in-app **"Open Folder…"** affordance — `Cmd/Ctrl+O` plus a toolbar button
  — that fires the same picker after a folder is already open.
- A **"back to landing page"** button (a home / close-folder action) that drops
  the current folder and returns to the landing screen without quitting.
- Landing page itself stays **folder-button-only** (no "Open Photo", no
  drag-and-drop — both deferred).

## Approach

Promote the landing page and the `PickFolder` action from wasm-only to shared,
and give native a real folder dialog via the `rfd` crate.

### 1. `src/main.rs` — make the arg optional

Replace the `match arg { ... }` at `src/main.rs:586-592` so the missing/invalid
cases produce `None` instead of exiting:

```rust
let arg = std::env::args().nth(1);
let initial = match arg.as_deref() {
    Some("-h" | "--help") => print_usage_and_exit(0),
    Some(s) => {
        let p = PathBuf::from(s);
        if p.exists() {
            Some(p)
        } else {
            eprintln!("[lightphotos] no such path: {s} — opening the folder picker");
            None
        }
    }
    None => None,
};
```

- `print_usage_and_exit` becomes `fn print_usage_and_exit(code: i32) -> !` and
  is only reached via `--help` now (exit 0 for that).
- `is_app_bundle()` (`src/main.rs:562-568`) is now unused — remove it and its
  doc comment (verify no other reference first; the explorer found only the one
  at line 588).
- A valid folder/photo arg still flows through `pending_initial` →
  `finish_window_setup` → `App::open(path)` during `resumed()`, before the
  first paint, so there is **no landing-page flash** when an arg is given.

### 2. Promote the landing page to all platforms — `src/ui/mod.rs`

- Drop the `#[cfg(target_arch = "wasm32")]` on the early-return block at
  `src/ui/mod.rs:215-219` so `if !app.has_playlist() { draw_landing_page(...);
  return out; }` runs on every target.
- Drop the `#[cfg(target_arch = "wasm32")]` on `fn draw_landing_page`
  (`src/ui/mod.rs:263`). Update its doc comment (no longer "wasm-only / early
  milestone").
- Inside it, replace `app.web_folder_pending()` with a new unconditional
  `app.folder_pick_pending()` (see §3) — on native the dialog is modal and
  synchronous, so this is always `false` and the button never shows "Opening…".
- Leave `site_nav(ui)` wasm-only (it is the marketing-site wordmark). The
  landing page has its own `ui.heading("LightPhotos")`, so native just gets the
  centered heading + button with no top bar. Optionally add one line of hint
  text under the heading (`ui.label("Choose a folder of photos to get
  started")`) since native users have no surrounding site context — minor.

### 3. `src/app/accessors.rs` — unconditional accessors

- Drop the `#[cfg(target_arch = "wasm32")]` on `has_playlist()`
  (`src/app/accessors.rs:20-23`); update its doc comment.
- Add:
  ```rust
  /// Whether a folder-picker dialog is currently in flight. Only ever true
  /// on wasm (the browser picker is async); native's OS dialog is modal and
  /// blocks, so it is always false there.
  pub(crate) fn folder_pick_pending(&self) -> bool {
      #[cfg(target_arch = "wasm32")]
      { self.web_folder_pending }
      #[cfg(not(target_arch = "wasm32"))]
      { false }
  }
  ```
  Keep the existing wasm `web_folder_pending()` accessor as-is if still
  referenced; otherwise fold its one use here.

### 4. Wire `UiAction::PickFolder` on native — `src/app/mod.rs:1442-1443`

Add a small shared method (native-only) so the toolbar button, the `Cmd+O`
keybinding (§7), and the `UiAction` arm all go through one place:

```rust
#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn pick_folder_interactive(&mut self) {
    if let Some(path) = crate::dialog::pick_folder() {
        self.open(path);
    }
}
```

Then the handler arm:

```rust
#[cfg(not(target_arch = "wasm32"))]
ui::UiAction::PickFolder => self.pick_folder_interactive(),
```

`App::open` (`src/app/mod.rs:1027`) already handles a directory (→ Grid via
`load_folder`/`load_playlist`) — the same convergence point wasm uses. Nothing
else downstream changes.

### 5. New module `src/dialog.rs` (native folder picker)

```rust
//! Native "choose a folder" dialog. Thin wrapper over `rfd` so the rest of
//! the app just gets an `Option<PathBuf>`. wasm has its own picker
//! (`web_fs::pick_and_list_folder`) and never compiles this.
use std::path::PathBuf;

pub fn pick_folder() -> Option<PathBuf> {
    rfd::FileDialog::new()
        .set_title("Choose a folder of photos")
        .pick_folder()
}
```

Declare in `src/main.rs` alongside the other `mod` lines:
`#[cfg(not(target_arch = "wasm32"))] mod dialog;`

`rfd::FileDialog::pick_folder()` is synchronous and must run on the main
thread — it does: `UiAction`s are applied in `App::redraw` on the winit event
loop (the main thread). The modal briefly blocks the loop, which is expected
dialog behaviour.

### 6. `Cargo.toml` — add `rfd`

Under `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` (next to
`egui-winit`):

```toml
rfd = "0.15"
```

`rfd` picks the **native** dialog per OS:

- **macOS:** Cocoa `NSOpenPanel` through `objc2` — the same stack this repo
  already uses. No new system libs.
- **Windows:** the Win32 `IFileOpenDialog`. No extra deps.
- **Linux:** rfd 0.15's default backend is the **XDG desktop portal** (D-Bus,
  pure Rust — `zbus`/`ashpd`), *not* GTK. No `libgtk-3-dev`, no system runtime
  lib. rfd's synchronous `pick_folder()` wraps the async portal call
  internally. The portal pulls a `zbus`/`async-io` transitive tree, but rfd
  gates it `target_os = "linux"`, so macOS/Windows builds never compile it
  (it only shows up as extra rows in the platform-agnostic `Cargo.lock`).
  Verified: `cargo check --target x86_64-unknown-linux-gnu` is clean.

### 7. In-app "Open Folder…" — `Cmd/Ctrl+O` + toolbar button

- **Shortcut:** `KeyCode::KeyO` is currently unbound (`src/app/keys.rs`). Add a
  binding (guard on the platform modifier, matching how other shortcuts read
  modifiers in `keys.rs`) → `self.pick_folder_interactive()`. Works from Grid
  and Loupe.
- **Toolbar button:** add an "Open" button to `grid_toolbar`
  (`src/ui/toolbar.rs:16`) — leading side, near the Grid/Loupe toggles — that
  pushes `UiAction::PickFolder`. `loupe_toolbar` optional (shortcut still
  works there).

### 8. "Back to landing page" — a close-folder action

- New `UiAction::CloseFolder` (unconditional variant, like `PickFolder`).
- Handler → `self.close_folder()`, a new `App` method that resets to the
  no-folder state: `playlist = None`, `sel = None`, `folder_root = None`,
  `folder_sel = None`, `expanded.clear()`, `subdirs.clear()`,
  `mode = ViewMode::Grid`, `reset_burst_state()`, `reset_dup_state()`,
  `recompute_visible()`, and drop the GPU image (`shown = Shown::Nothing`,
  release loupe texture the same way an empty playlist would). Audit the
  per-frame polls that run before the landing early-return
  (`request_working_thumbs`, `poll_catalog_load`, neighbour requests) — they
  key off the working set / `visible`, which are now empty, so they no-op, but
  confirm none `unwrap` the playlist.
- Next frame `has_playlist()` is `false` → `draw` takes the landing early
  return. Symmetric with wasm's "playlist is None" state.
- **Button:** a home / "×" button in `grid_toolbar` and `loupe_toolbar`
  (leading side) that pushes `UiAction::CloseFolder`. Wording: "Home" or a
  house glyph — match the toolbar's existing button style
  (`src/ui/toolbar.rs`).

## Out of scope / follow-ups

- **Drag-and-drop a folder onto the window** (winit `DroppedFile`). No D&D
  anywhere today; separate task.
- **Picking a single photo** from the landing page (wasm only does folders
  too). `App::open` already handles a file path; a second button could be added
  later.
- Landing-page visual polish (icon, styling) — keeping it deliberately minimal,
  matching the wasm version.

## Verification

Build: `cargo build --release`.

1. `./target/release/lightphotos` (no arg) → window opens on the landing page
   (centered "LightPhotos" + "Choose Folder"). Click → macOS folder dialog →
   pick a photo folder → grid populates with thumbnails. Previously this
   printed usage and exited 1.
2. `./target/release/lightphotos /path/to/folder` → straight into Grid, no
   landing flash (regression check).
3. `./target/release/lightphotos /path/to/photo.jpg` → straight into Loupe
   (regression check).
4. `./target/release/lightphotos /does/not/exist` → stderr warning, then the
   landing page (not an exit).
5. `./target/release/lightphotos --help` → usage text, exit 0.
6. **In-app open:** with a folder open, press `Cmd+O` (and click the toolbar
   "Open" button) → folder dialog → picking a new folder swaps the grid.
7. **Back to landing:** with a folder open, click the "Home" button → landing
   page returns, no crash; the folder sidebar, grid, and any loupe image are
   cleared. From there "Choose Folder" opens a folder again cleanly.
8. `cargo test` — navigation/app/burst tests unaffected.
9. wasm still compiles and behaves unchanged:
   `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
   — landing page path is now shared code; confirm the `folder_pick_pending`
   rename compiles, the "Opening…" state still shows on web, and
   `UiAction::CloseFolder` routes on wasm too (either wire it to the same
   `close_folder` reset or make the wasm arm a no-op if handle cleanup needs
   more care — decide during implementation).
10. `cargo check --target x86_64-pc-windows-msvc` — `rfd` compiles for Windows.
    (Full Linux/Windows dialog behaviour is only verifiable on those OSes; dev
    is macOS.)

## Status — implemented

- `Cargo.toml`: `rfd = "0.15"` under the non-wasm target deps.
- New `src/dialog.rs` (`pick_folder()`), declared
  `#[cfg(not(target_arch = "wasm32"))] mod dialog;` in `src/main.rs`.
- `src/main.rs`: arg is optional; `is_app_bundle()` removed;
  `print_usage_and_exit(code)` now only for `--help` (exit 0).
- `src/ui/mod.rs`: landing-page branch + `draw_landing_page` are unconditional;
  button state reads `folder_pick_pending()`; added a hint line. New
  `UiAction::CloseFolder`; `PickFolder` doc updated.
- `src/app/accessors.rs`: `has_playlist()` unconditional; `web_folder_pending()`
  replaced by unconditional `folder_pick_pending()`.
- `src/app/mod.rs`: `open_folder_picker()` + `close_folder()`; both `UiAction`
  arms unconditional.
- `src/app/keys.rs`: `Cmd/Ctrl+O` → `open_folder_picker()`.
- `src/ui/toolbar.rs`: "Home" + "Open…" buttons in both toolbars (mouse-only,
  outside the F6 focus cycle — `TOOLBAR_CONTROLS` counts unchanged).
- `.github/workflows/release.yml`: comment note only (no GTK dep needed).

Verified: `cargo build --release` clean, `cargo test` 193 pass, `cargo check`
clean for wasm32 / windows-msvc / linux-gnu, `--help` exits 0. **Live GUI
behaviour (landing page render, dialog, Home button) not yet exercised — dev
does not drive the running app.**
