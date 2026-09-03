# Web (wasm32) subfolder browsing

## Context

On native, the left folder panel is a lazy recursive tree: `ensure_subdirs`
calls `navigation::list_subdirs` (`std::fs::read_dir`), the result populates
`App::subdirs`, and `folder_node` (`src/ui/grid.rs`) renders disclosure
triangles and recurses into expanded folders. Arrow-key navigation
(`folder_move`/`folder_expand`/`folder_collapse`/`open_folder` in
`src/app/nav.rs`) walks that tree and calls `load_folder` →
`Playlist::from_dir` to swap the grid's image list.

On the wasm build none of this works. `std::fs::read_dir` returns nothing in
the browser, so `list_subdirs` is always empty, the tree shows only the
picked root with no children, and the whole navigation layer is effectively
dead code. A picked folder is reachable only through its root
`FileSystemDirectoryHandle` (File System Access API); every directory read is
an async `values()` iteration, and `Playlist::from_dir` must never run.

Today the wasm folder pick (`src/web/web_fs.rs::pick_and_list_folder` →
`list_images`) lists exactly one level of image files, keyed by bare
filename, and hands back `PickedFolder { dir, entries, handles, dir_handle }`.
`app/web.rs::poll_folder_pick` stores `handles` in `App::web_file_handles`,
points the catalog at the root via `Catalog::set_wasm_dir_handle`, builds a
`Playlist::from_entries`, and calls `load_playlist`.

This plan makes the wasm folder panel a real recursive tree, matching native
behavior, so the user can browse into subfolders (and, once the separate
wasm-export work lands, navigate into the `Exports/` folder to see results).

### Decisions already made

- **Navigation model:** mirror native — recursive, lazily-expanded tree, same
  keyboard/mouse behavior. Reuse the existing pure nav helpers.
- **Path model:** web paths become **relative to the picked root** (root =
  the picked folder's name). Replaces today's bare-filename keys.
- **Sidecars:** per-folder `.lightphotos/`, matching native. The catalog's
  wasm directory handle swaps to the current subfolder on navigation, so
  ratings/edits in a subfolder read and write that subfolder's
  `.lightphotos/` — full interop with the native app.
- **Scope order:** this feature first, as its own spec/plan/implementation
  cycle; wasm export second.

### Constraints

- Native code paths must not change behavior. All new logic is under
  `#[cfg(target_arch = "wasm32")]`; the shared functions keep their native
  arms byte-for-byte.
- No File System Access read is synchronous. Anything that currently reads
  `subdirs` or builds a `Playlist` "right now" needs a request/apply split on
  wasm.
- No headless FSA test harness exists — browser verification is the
  integration gate.

## Path model migration

Web `PathBuf` keys become **relative to the picked root**, with the root's
own name as the first component:

- `folder_root` = `PathBuf::from("<picked folder name>")` (e.g. `photos`)
- a subfolder = `photos/2024`, `photos/2024/January`
- an image = `photos/2024/January/IMG_1234.jpg`

`Path::file_name()` still returns the bare filename for any entry, so the
catalog's `write_sidecar`/`delete_sidecar` (which key on `path.file_name()`)
need no change. `Path::parent()` / `Path::join` now behave for web paths the
way the native tree logic already assumes.

### `src/web/web_fs.rs`

- `list_images` (rename to `list_dir`, see next section) keys `entries` and
  `handles` by `root_name.join(child_name)` instead of `PathBuf::from(name)`.
- `PickedFolder` is explicitly shaped as:

  ```rust
  pub struct PickedFolder {
      pub dir: PathBuf,
      pub entries: Vec<DirEntry>,
      pub handles: HashMap<PathBuf, FileSystemFileHandle>,
      pub subdirs: Vec<PathBuf>,
      pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>,
      pub dir_handle: FileSystemDirectoryHandle,
  }
  ```

  `dir_handles` contains the picked root and every direct subdirectory found
  by the initial `list_dir` scan: `{ root_name → dir_handle }` plus each
  `(path, handle)` from that scan. (`dir_handle` stays as its own field too —
  `poll_folder_pick` still needs it directly for the catalog.)

### `src/app/mod.rs`

- `web_file_handles: HashMap<PathBuf, FileSystemFileHandle>` — unchanged type,
  keys are now relative paths.
- New field `web_dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>`,
  initialized empty, populated from `PickedFolder.dir_handles` in
  `poll_folder_pick` and extended as subfolders are listed.
- New field `web_pending_open: Option<WebPendingNav>` where
  `enum WebPendingNav { Open(PathBuf), Load(PathBuf) }` (see "Async folder-open").
- New field for in-flight listing dedupe: `web_dirlist_inflight: HashSet<PathBuf>`.

### Call sites keyed by the old bare-filename paths

`app/web.rs` has four `web_file_handles.get(&path)` / `.contains_key(&path)`
sites (lines ~160, ~332, ~556, ~703). Each receives a `path` that came from
the playlist, so once playlist entries are relative paths these lookups line
up automatically — no per-site change, but each must be re-read during
implementation to confirm the `path` in scope is the playlist key and not a
re-derived bare name.

## Async subdir listing

### `src/web/web_fs.rs::list_dir`

Replace `list_images` with:

```rust
pub struct DirListing {
    pub images: Vec<(PathBuf, FileSystemFileHandle)>,   // relative path → handle
    pub subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)>,
}

/// One `values()` scan of `handle`, split by kind. `base` is the directory's
/// own relative path; children are `base.join(child_name)`. Images filtered by
/// `navigation::is_image`; subdirs filtered by `navigation::is_listable_subdir`
/// (the shared predicate, see below). Both lists sorted case-insensitively by
/// name, matching `list_subdirs` / `from_dir`.
pub async fn list_dir(
    base: &Path,
    handle: &FileSystemDirectoryHandle,
) -> Result<DirListing, String>
```

`pick_and_list_folder` becomes: pick → `list_dir(root_name, &handle)` → wrap
in `PickedFolder` (root images become `entries`/`handles`, root subdirs seed
`subdirs`/`dir_handles`).

### Shared subdir-filter predicate

`src/navigation.rs::list_subdirs` currently inlines its filter (skip
`.`-prefixed names, skip `.app`/`.photoslibrary` bundles). Extract:

```rust
/// Whether a directory entry named `name` should appear in the folder tree.
pub fn is_listable_subdir(name: &str) -> bool
```

`list_subdirs` calls it; `web_fs::list_dir` calls it. One definition, one
test.

### `src/app/web.rs` — request / poll

```rust
/// Kick off an async `list_dir` for `dir` (a relative path) unless one is
/// already cached in `self.subdirs` or in flight. Needs `web_dir_handles[dir]`.
pub(crate) fn request_dir_listing(&mut self, dir: &Path)

/// Drain finished listings from `web_dirlist_rx`. For each:
///  - merge images into `web_file_handles`
///  - merge subdirs into `web_dir_handles`
///  - set `self.subdirs[dir]` to the subdir relative paths
///  - clear `web_dirlist_inflight` for `dir`
///  - if `web_pending_open` targets `dir`, take it and call the matching
///    apply step — `apply_web_open_folder` for `Open`, `apply_web_load_folder`
///    for `Load` (both below)
///  - `request_redraw`
/// Returns whether any listing is still outstanding.
pub(crate) fn poll_dir_listing(&mut self) -> bool
```

New channel `web_dirlist_tx` / `web_dirlist_rx` carrying
`(PathBuf /*dir*/, Result<DirListing, String>)`. A listing error is surfaced
through `set_status` (same as `poll_folder_pick`'s cancelled-pick path) and
recorded as an empty `subdirs[dir]` so the tree treats it as a leaf rather
than retrying every frame.

`src/main.rs` frame loop (`about_to_wait`, alongside `poll_folder_pick` /
`poll_catalog_load`) calls `poll_dir_listing`. Its return value is ORed into
the wasm `image_pending` calculation, so a completed `spawn_local` listing
keeps the event loop polling and gets drained even though the channel send
does not itself wake winit.

### `ensure_subdirs` wasm arm — `src/app/mod.rs`

```rust
fn ensure_subdirs(&mut self, dir: &Path) {
    #[cfg(not(target_arch = "wasm32"))]
    { /* unchanged: sync list_subdirs into self.subdirs */ }

    #[cfg(target_arch = "wasm32")]
    {
        if !self.subdirs.contains_key(dir) {
            self.request_dir_listing(dir);
        }
    }
}
```

Until the listing lands, `self.subdirs.get(dir)` is `None` and `subdirs()`
returns `&[]`. The folder renders without a disclosure triangle for one
frame, then the triangle appears when `poll_dir_listing` fills it in and
redraws.

## Catalog handle per folder

When navigation commits to folder `F` (the apply step below), before
`seed_mirrors` runs:

```rust
if let Some(h) = self.web_dir_handles.get(&F) {
    self.catalog.set_wasm_dir_handle(h.clone());
}
```

`seed_mirrors` → `request_catalog_load(F)` then runs its existing wasm arm
against that handle, scanning `F/.lightphotos/`. `Catalog::write_sidecar` /
`delete_sidecar` already read `wasm_dir_handle` + `path.file_name()`, so
writes land in the current folder's `.lightphotos/` with no change.

`request_catalog_load` already tolerates a `None` handle (leaves the cache
empty rather than hanging). A folder whose handle somehow isn't in
`web_dir_handles` degrades to "no persisted ratings" rather than breaking —
acceptable, and it shouldn't happen since the apply step only runs after the
parent's listing (which produced the handle) landed.

## Async folder-open / navigation restructure

The native nav functions in `src/app/nav.rs` are synchronous: they call
`ensure_subdirs`, immediately read `subdirs(...)`, and call `load_folder`
(→ `Playlist::from_dir`). On wasm each needs a **request → apply** split.

### `open_folder` (row click + Enter key)

```rust
pub(super) fn open_folder(&mut self, path: PathBuf) {
    #[cfg(not(target_arch = "wasm32"))]
    { /* unchanged */ }

    #[cfg(target_arch = "wasm32")]
    {
        // Need this folder's own listing (its images to show, its subdirs to
        // decide pure-container / expansion). If not loaded yet, remember the
        // intent and let poll_dir_listing finish the job.
        if !self.subdirs.contains_key(&path) {
            self.web_pending_open = Some(WebPendingNav::Open(path.clone()));
            self.request_dir_listing(&path);
            self.request_redraw();
            return;
        }
        self.apply_web_open_folder(path);
    }
}
```

### `apply_web_open_folder` (new, wasm-only)

Mirrors native `open_folder`'s tail with cached data instead of `std::fs`:

- `subdirs` = `self.subdirs[&path]` (already loaded)
- `pure_container` = `!subdirs.is_empty()` and this folder has no images of
  its own. "No images of its own" = no `web_file_handles` key whose parent is
  `path`. (Populated by the same `list_dir` that filled `subdirs`.)
- toggle `expanded` exactly as native does
- `target` = first subdir if `pure_container`, else `path`
- if `target != path` and `target`'s listing isn't loaded, set a continuation
  for the already-resolved child — `web_pending_open =
  Some(WebPendingNav::OpenResolved(target))` — then `request_dir_listing(target)`
  and return. This continuation must load the child directly; it must not
  re-run `open_folder` semantics (which would toggle the child or skip through
  another pure container).
- otherwise: set catalog handle for `target`, build
  `Playlist::from_entries(target, <entries under target, sorted>)`,
  `load_playlist(playlist, target)`, `mode = Grid`

`apply_web_open_resolved` performs that same direct load and Grid-mode
transition once the resolved child's listing arrives, without mutating
`expanded` or applying pure-container logic to the child.

Entries under `target` come from `web_file_handles` keys whose `parent()` is
`target`, sorted with the existing `navigation::sort_by_name`.

### `folder_expand`

Same split: if `subdirs(cur)` not loaded, `request_dir_listing(cur)` and
return (the triangle / first-child-load happens on the user's next press, or
we could stash a lighter "pending expand" — but keeping it to "press again
once loaded" avoids a second intent field; listings are fast). If loaded,
native logic runs unchanged on the cached `subdirs`.

### `folder_move` / `folder_collapse`

Native `folder_move`/`folder_collapse` call `load_folder(target)` — load that
folder's images into the grid, *without* toggling expansion or the
pure-container skip that `open_folder` does. The wasm equivalent is a
narrower apply than `apply_web_open_folder`:

```rust
/// wasm: load `dir`'s images into the grid. `dir`'s listing is assumed
/// already cached (folder_move only visits rows in `visible_tree()`, whose
/// expanded ancestors were all listed during expansion; folder_collapse's
/// parent was listed when the child was reached). If it somehow isn't,
/// stash `web_pending_open` and request it.
fn apply_web_load_folder(&mut self, dir: PathBuf)
```

Steps: if `!subdirs.contains_key(&dir)` → `web_pending_open =
Some(WebPendingNav::Load(dir.clone()))`, `request_dir_listing(&dir)`, return.
Else set catalog handle for `dir`, build `Playlist::from_entries` from
`web_file_handles` keys under `dir`, `load_playlist`. No `expanded` mutation,
no mode change (matches native `load_folder`).

- `folder_move`: replace `self.load_folder(tree[next].clone())` with a
  platform helper — `load_folder` on native, `apply_web_load_folder` on wasm.
- `folder_collapse`: same for its `load_folder(parent)` branch.

`poll_dir_listing`'s `web_pending_open` handler must therefore dispatch to
the right apply step. Make `web_pending_open` carry the intent —
`enum WebPendingNav { Open(PathBuf), OpenResolved(PathBuf), Load(PathBuf) }` —
so the poll knows whether to call `apply_web_open_folder`,
`apply_web_open_resolved`, or `apply_web_load_folder`.

### `load_folder` guard

`load_folder` / `Playlist::from_dir` must never execute on wasm. Add:

```rust
#[cfg(target_arch = "wasm32")]
fn load_folder(&mut self, _dir: PathBuf) {
    debug_assert!(false, "load_folder must not run on wasm32; use the async open path");
}
```

and confirm no remaining wasm-reachable caller (after the `folder_move` /
`folder_collapse` reroute).

### `App::open` (initial pick)

On a successful pick, `poll_folder_pick` must first set
`self.folder_root = Some(root.clone())` and initialize `self.expanded` with
the root. It then seeds `web_dir_handles` from `PickedFolder.dir_handles`,
sets `self.subdirs[root] = <root subdir relative paths>` from
`PickedFolder.subdirs`, builds the root playlist, and calls `load_playlist`
directly (bypassing `open`/`load_folder`). This explicit root initialization
is required because `folder_node` renders only when `folder_root` is `Some`.

## UI

`src/ui/grid.rs::folder_node` and `folder_content_width` are unchanged — they
already render from `app.subdirs(path)` / `app.is_expanded(path)` and recurse.
The disclosure triangle for a folder appears once its parent's listing lands.

An explicit "listing in flight" affordance (a spinner or `…` on the row) is
**not** in scope — listings are a single `values()` scan and the one-frame
pop-in is acceptable. Revisit only if it feels bad in the browser.

## Testing

### Unit (native `cargo test`)

- `navigation::is_listable_subdir` — hidden `.`-prefixed, `.app` /
  `.photoslibrary` bundles rejected; ordinary names accepted. Replaces the
  coverage currently implicit in `list_subdirs_returns_sorted_visible_dirs`
  (keep that test; it now also exercises the extracted predicate).
- `flatten_visible_tree` / `folder_move` math — extend the existing fixtures
  with a 3-level tree (root → A, B; A → A1, A2) and assert visible-row order
  and clamped movement across expand/collapse states.
- Relative-path key construction — a small pure test of the
  `base.join(child_name)` keying used by `list_dir` (extract the join into a
  testable free function if `list_dir` itself can't be called without a
  browser).

### Manual (trunk browser build — the integration gate)

`RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release`, serve, then:

1. Pick a folder containing both loose images and nested subfolders, at least
   one subfolder two levels deep, and a `.lightphotos/` inside one subfolder
   (created by a prior native session or hand-made).
2. Folder tree shows the first level of subfolders with triangles.
3. Expand / collapse via mouse and arrow keys; multi-level expansion works.
4. Entering each folder swaps the grid to that folder's images; thumbnails
   decode.
5. Rate a photo in a subfolder; reload the page, re-pick, navigate back —
   the rating persisted to *that subfolder's* `.lightphotos/`, not the root.
6. A pure-container folder (subdirs, no images) skips straight to its first
   child, no empty-grid flash.
7. The originally-picked root still behaves exactly as before.

## Files touched

| File | Change |
|---|---|
| `src/web/web_fs.rs` | `list_dir` + `DirListing`; relative-path keying; `PickedFolder.dir_handles` |
| `src/navigation.rs` | extract `is_listable_subdir`; keep/extend subdir test |
| `src/app/mod.rs` | `web_dir_handles`, `web_pending_open`, `web_dirlist_inflight` fields; `ensure_subdirs` wasm arm; `poll_folder_pick` seeding; `load_folder` wasm guard |
| `src/app/web.rs` | `request_dir_listing`, `poll_dir_listing`, `apply_web_open_folder`; channel wiring |
| `src/app/nav.rs` | request/apply split in `open_folder` / `folder_expand`; reroute `load_folder` calls in `folder_move` / `folder_collapse` through a platform helper |
| `src/app/catalog.rs` | set wasm dir handle for the current folder in the apply step (may live in `app/web.rs` instead — implementation detail) |
| `src/main.rs` | call `poll_dir_listing` in the frame loop |

## Out of scope

- wasm JPEG export (next cycle; this feature is its prerequisite for
  post-export visibility).
- In-flight listing spinner / row affordance.
- Watching the picked folder for external changes (native doesn't either).
- Any change to native folder-tree behavior.
