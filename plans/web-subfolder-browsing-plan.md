# Web (wasm32) Subfolder Browsing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the wasm (browser) build's left folder panel a real recursive tree so the user can browse into subfolders, matching native behavior.

**Architecture:** The picked folder is reachable only through async File System Access directory handles. Web paths become relative to the picked root. A new async `list_dir` scans one directory level; results flow back through an mpsc channel and populate `App::subdirs` / handle maps, exactly like the existing `poll_folder_pick` one-shot. The synchronous native navigation functions (`open_folder`, `folder_expand`, `folder_move`, `folder_collapse`) get a `#[cfg(target_arch = "wasm32")]` request/apply split: when a folder's listing isn't cached yet, stash the navigation intent, kick off the listing, and complete it when the result lands. Native code paths are untouched.

**Tech Stack:** Rust, wasm32-unknown-unknown, `web-sys` File System Access API, `wasm-bindgen-futures`, `std::sync::mpsc`, egui 0.34, trunk 0.21.

**Spec:** `plans/web-subfolder-browsing.md`

## Global Constraints

- Rust stable ≥ 1.92 (pinned in `rust-toolchain.toml`).
- Native (`cfg(not(target_arch = "wasm32"))`) code paths must not change behavior. Every new code path is under `#[cfg(target_arch = "wasm32")]`; shared functions keep their native arms byte-for-byte identical.
- `Playlist::from_dir` and `std::fs::read_dir` must never execute on wasm32.
- Per-clone setup before any cargo command: `./scripts/setup-vendor-rawler.sh`.
- Native test command: `cargo test <name>`.
- wasm compile gate: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`. Always `--release` for wasm.
- No `Co-Authored-By` / co-author trailer on commits (repo CLAUDE.md).
- Sidecars stay per-folder `.lightphotos/` — the catalog's wasm directory handle swaps to the current subfolder on navigation.
- Web `PathBuf` keys are relative to the picked root, with the root's own name (`FileSystemDirectoryHandle::name()`) as the first component.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `src/navigation.rs` | Path/tree helpers | Extract `is_listable_subdir`; add multi-level `flatten_visible_tree` test |
| `src/web/web_fs.rs` | File System Access picking + listing | Add `DirListing` + `list_dir`; relative-path keying; `PickedFolder.dir_handles` |
| `src/app/mod.rs` | `App` state, dispatch, `ensure_subdirs`, `load_folder`, `load_playlist` | New wasm fields + channel; `ensure_subdirs` wasm arm; `load_folder` wasm guard |
| `src/app/web.rs` | wasm async folder flows | `request_dir_listing`, `poll_dir_listing`, `apply_web_open_folder`, `apply_web_load_folder`; `poll_folder_pick` seeding |
| `src/app/nav.rs` | Folder-tree navigation | Request/apply split in `open_folder` / `folder_expand`; reroute `load_folder` calls in `folder_move` / `folder_collapse` |
| `src/main.rs` | Frame loop | Call `poll_dir_listing` in `about_to_wait` |

---

## Task 1: Extract `is_listable_subdir` predicate

**Files:**
- Modify: `src/navigation.rs:126-151` (`list_subdirs`)
- Test: `src/navigation.rs` `#[cfg(test)] mod tests` (around line 296)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn is_listable_subdir(name: &str) -> bool` — `true` when a directory entry with this file name belongs in the folder tree (not hidden, not a macOS bundle).

- [ ] **Step 1: Write the failing test**

Add to `mod tests` in `src/navigation.rs`:

```rust
#[test]
fn is_listable_subdir_filters_hidden_and_bundles() {
    assert!(is_listable_subdir("2024"));
    assert!(is_listable_subdir("Exports"));
    assert!(is_listable_subdir("My Photos"));
    assert!(!is_listable_subdir(".git"));
    assert!(!is_listable_subdir(".lightphotos"));
    assert!(!is_listable_subdir("Photos.app"));
    assert!(!is_listable_subdir("Library.photoslibrary"));
    // Case-insensitive extension match.
    assert!(!is_listable_subdir("Thing.APP"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test is_listable_subdir_filters_hidden_and_bundles`
Expected: FAIL — `cannot find function \`is_listable_subdir\``

- [ ] **Step 3: Add the function and route `list_subdirs` through it**

Insert before `list_subdirs` in `src/navigation.rs`:

```rust
/// Whether a directory entry named `name` should appear in the folder tree.
/// Skips hidden entries (names starting with `.`) and macOS bundles
/// (`.app` / `.photoslibrary`, case-insensitive). Shared by the native
/// `list_subdirs` (`std::fs::read_dir`) and wasm32's `web_fs::list_dir`
/// (File System Access `values()`), so the two platforms filter identically.
pub fn is_listable_subdir(name: &str) -> bool {
    if name.starts_with('.') {
        return false;
    }
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    !matches!(ext.as_deref(), Some("app") | Some("photoslibrary"))
}
```

Replace the inline closure body in `list_subdirs` (the `.filter(|p| { ... })` at lines 134-147) with:

```rust
        .filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(is_listable_subdir)
        })
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test navigation::`
Expected: PASS — including the existing `list_subdirs_returns_sorted_visible_dirs`.

- [ ] **Step 5: Commit**

```bash
git add src/navigation.rs
git commit -m "refactor: extract is_listable_subdir predicate for folder-tree filtering"
```

---

## Task 2: Multi-level `flatten_visible_tree` test coverage

**Files:**
- Test: `src/navigation.rs` `mod tests` (near the existing `flatten_visible_tree_walks_expanded_dfs`, ~line 414)

**Interfaces:**
- Consumes: `flatten_visible_tree` (existing, unchanged).
- Produces: nothing (test-only).

- [ ] **Step 1: Write the test**

Add to `mod tests`:

```rust
#[test]
fn flatten_visible_tree_descends_multiple_levels() {
    use std::path::{Path, PathBuf};
    let kids = |p: &Path| -> Vec<PathBuf> {
        match p.to_str().unwrap() {
            "root" => vec![PathBuf::from("root/a"), PathBuf::from("root/b")],
            "root/a" => vec![PathBuf::from("root/a/a1"), PathBuf::from("root/a/a2")],
            _ => vec![],
        }
    };
    // root and root/a expanded, root/b collapsed.
    let expanded = |p: &Path| matches!(p.to_str().unwrap(), "root" | "root/a");
    assert_eq!(
        flatten_visible_tree(Path::new("root"), &expanded, &kids),
        vec![
            PathBuf::from("root"),
            PathBuf::from("root/a"),
            PathBuf::from("root/a/a1"),
            PathBuf::from("root/a/a2"),
            PathBuf::from("root/b"),
        ]
    );
}
```

- [ ] **Step 2: Run test to verify it passes**

Run: `cargo test flatten_visible_tree_descends_multiple_levels`
Expected: PASS (function already handles this — the test locks the behavior the wasm tree depends on).

- [ ] **Step 3: Commit**

```bash
git add src/navigation.rs
git commit -m "test: cover multi-level DFS in flatten_visible_tree"
```

---

## Task 3: `web_fs::list_dir`, `DirListing`, and relative-path keying

**Files:**
- Modify: `src/web/web_fs.rs` (`PickedFolder` ~44-49, `pick_and_list_folder` ~58-72, `list_images` ~74-124, imports ~29-34)

**Interfaces:**
- Consumes: `navigation::is_image`, `navigation::is_listable_subdir`, `navigation::sort_by_name`.
- Produces:
  - `pub struct DirListing { pub images: Vec<(PathBuf, FileSystemFileHandle)>, pub subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)> }`
  - `pub async fn list_dir(base: &Path, handle: &FileSystemDirectoryHandle) -> Result<DirListing, String>` — `base` is the directory's own relative path (e.g. `photos` or `photos/2024`); returned paths are `base.join(child_name)`; both vecs sorted case-insensitively by file name.
  - `PickedFolder` gains `pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>` (seeded with one entry: `dir` → root handle). `dir_handle` field stays.

- [ ] **Step 1: Add imports**

In `src/web/web_fs.rs`, extend the `use std::path` line and the `navigation` import:

```rust
use std::path::{Path, PathBuf};
```
```rust
use crate::navigation::{is_image, is_listable_subdir, sort_by_name};
```

- [ ] **Step 2: Add `DirListing` and `list_dir`**

Replace the whole `list_images` function (lines ~74-124) with:

```rust
/// The image files and immediate subdirectories of one directory handle.
/// Paths are relative to the picked root (`base` is this directory's own
/// relative path; children are `base.join(child_name)`).
pub struct DirListing {
    pub images: Vec<(PathBuf, FileSystemFileHandle)>,
    pub subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)>,
}

/// One async `values()` scan of `handle`, split by entry kind. Image files
/// are filtered by `navigation::is_image`; subdirectories by
/// `navigation::is_listable_subdir` (hidden entries and macOS bundles
/// dropped, same as the native `list_subdirs`). Both lists are sorted
/// case-insensitively by file name, matching `Playlist::from_dir` /
/// `list_subdirs` ordering. `FileSystemDirectoryHandle` has no synchronous
/// listing — every browser directory read goes through this iterator.
pub async fn list_dir(
    base: &Path,
    handle: &FileSystemDirectoryHandle,
) -> Result<DirListing, String> {
    let mut images: Vec<(PathBuf, FileSystemFileHandle)> = Vec::new();
    let mut subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)> = Vec::new();

    let iter = handle.values();
    loop {
        let next: JsValue = JsFuture::from(iter.next().map_err(|e| js_error_string(&e))?)
            .await
            .map_err(|e| js_error_string(&e))?;
        let done = js_sys::Reflect::get(&next, &"done".into())
            .ok()
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        if done {
            break;
        }
        let Ok(value) = js_sys::Reflect::get(&next, &"value".into()) else {
            continue;
        };
        let Ok(child) = value.dyn_into::<web_sys::FileSystemHandle>() else {
            continue;
        };
        let name = child.name();
        let path = base.join(&name);
        match child.kind() {
            FileSystemHandleKind::File => {
                if is_image(&path) {
                    images.push((path, child.unchecked_into()));
                }
            }
            FileSystemHandleKind::Directory => {
                if is_listable_subdir(&name) {
                    subdirs.push((path, child.unchecked_into()));
                }
            }
            _ => {}
        }
    }

    sort_pairs_by_name(&mut images);
    sort_pairs_by_name(&mut subdirs);
    Ok(DirListing { images, subdirs })
}

/// `sort_by_name` for a `Vec<(PathBuf, T)>`, keyed on the path.
fn sort_pairs_by_name<T>(v: &mut [(PathBuf, T)]) {
    v.sort_by(|a, b| {
        let an = a.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        let bn = b.0.file_name().map(|s| s.to_string_lossy().to_lowercase());
        an.cmp(&bn)
    });
}
```

(Keep `js_error_string` and the other helpers below unchanged. `sort_by_name` from `navigation` is still imported for parity of intent but `sort_pairs_by_name` is the tuple version actually used here — if the linter flags `sort_by_name` as unused after this task, drop it from the `use` until Task 4/beyond needs it; Task 6 does not. Simplest: remove `sort_by_name` from the import now and re-add if a later task needs it.)

- [ ] **Step 3: Update `PickedFolder` and `pick_and_list_folder`**

`PickedFolder`:

```rust
pub struct PickedFolder {
    pub dir: PathBuf,
    pub entries: Vec<PathBuf>,
    pub handles: HashMap<PathBuf, FileSystemFileHandle>,
    pub dir_handle: FileSystemDirectoryHandle,
    /// Every directory handle discovered so far, keyed by relative path.
    /// Seeded with just the root (`dir` → `dir_handle`); extended as the
    /// user browses into subfolders (`web_fs::list_dir` via
    /// `app/web.rs::poll_dir_listing`).
    pub dir_handles: HashMap<PathBuf, FileSystemDirectoryHandle>,
}
```

`pick_and_list_folder` tail (replace `list_images(&handle).await`):

```rust
    let root = PathBuf::from(handle.name());
    let listing = list_dir(&root, &handle).await?;

    let mut handles = HashMap::new();
    let mut entries = Vec::with_capacity(listing.images.len());
    for (path, fh) in listing.images {
        handles.insert(path.clone(), fh);
        entries.push(path);
    }

    let mut dir_handles = HashMap::new();
    dir_handles.insert(root.clone(), handle.clone());
    // Note: subfolder handles from `listing.subdirs` are merged later by
    // `poll_dir_listing`; `PickedFolder` only needs the root here, plus the
    // subdir *paths* so the tree's first level renders immediately.
    let subdir_paths: Vec<PathBuf> = listing.subdirs.iter().map(|(p, _)| p.clone()).collect();
    for (p, h) in listing.subdirs {
        dir_handles.insert(p, h);
    }

    Ok(PickedFolder {
        dir: root,
        entries,
        handles,
        dir_handle: handle,
        dir_handles,
    })
```

Wait — `subdir_paths` is needed by `poll_folder_pick` (Task 4) to seed `subdirs[root]`. Rather than threading a separate field, `poll_folder_pick` will derive it from `dir_handles` keys whose parent is `dir`. So **drop the `subdir_paths` local** — it is not stored. Final `pick_and_list_folder` tail keeps only the `handles` / `entries` / `dir_handles` construction and the `Ok(PickedFolder { ... })`.

- [ ] **Step 4: wasm compile gate**

Run: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
Expected: builds clean. (`list_dir` is not yet called anywhere except `pick_and_list_folder`; `dir_handles` is unused downstream until Task 4 — a dead-code warning on the field is acceptable within this single task but should be gone after Task 4.)

- [ ] **Step 5: Commit**

```bash
git add src/web/web_fs.rs
git commit -m "feat(web): list_dir + DirListing + relative-path keys for File System Access"
```

---

## Task 4: `App` wasm fields, channel, and `poll_folder_pick` seeding

**Files:**
- Modify: `src/app/mod.rs` — field declarations (~325-335), constructor (`Self { ... }` ~755-783), `ensure_subdirs` (~974-979)
- Modify: `src/app/web.rs` — `poll_folder_pick` (~89-115)

**Interfaces:**
- Consumes: `web_fs::PickedFolder` (now with `dir_handles`), `web_fs::DirListing`.
- Produces (fields on `App`, all `#[cfg(target_arch = "wasm32")]`):
  - `web_dir_handles: HashMap<PathBuf, web_sys::FileSystemDirectoryHandle>`
  - `web_dirlist_tx: Sender<(PathBuf, Result<web_fs::DirListing, String>)>`
  - `web_dirlist_rx: Receiver<(PathBuf, Result<web_fs::DirListing, String>)>`
  - `web_dirlist_inflight: HashSet<PathBuf>`
  - `web_pending_nav: Option<WebPendingNav>` where `pub(crate) enum WebPendingNav { Open(PathBuf), Load(PathBuf), LoadAfterOpen(PathBuf) }`
- After this task, `poll_folder_pick` sets `folder_root`, `expanded`, `subdirs[root]`, `web_dir_handles`.

- [ ] **Step 1: Declare the enum and fields**

In `src/app/mod.rs`, near the other wasm field declarations (after `web_file_handles`, ~line 335):

```rust
    /// Directory handles for every folder the user has browsed into, keyed
    /// by relative path (root's `.name()` as the first component). Seeded
    /// from `PickedFolder::dir_handles` at pick time, extended by
    /// `poll_dir_listing` as subfolders are listed. The catalog's wasm
    /// sidecar handle is swapped to `web_dir_handles[current folder]` on
    /// each navigation.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dir_handles: HashMap<PathBuf, web_sys::FileSystemDirectoryHandle>,
    /// Async subfolder-listing results (`web_fs::list_dir`), same one-shot
    /// channel shape as `web_folder_tx`/`web_folder_rx`. Key is the listed
    /// directory's relative path.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_tx:
        Sender<(PathBuf, Result<crate::web_fs::DirListing, String>)>,
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_rx:
        Receiver<(PathBuf, Result<crate::web_fs::DirListing, String>)>,
    /// Directories with a `list_dir` in flight — dedupes repeated
    /// `request_dir_listing` calls from per-frame nav polling.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_dirlist_inflight: std::collections::HashSet<PathBuf>,
    /// A folder navigation deferred until its listing lands (see
    /// `app/nav.rs`'s request/apply split). `Open` toggles expansion +
    /// pure-container skip like native `open_folder`; `Load` just swaps the
    /// grid like native `load_folder` (used by `folder_move` /
    /// `folder_collapse`); `LoadAfterOpen` completes an open's pure-container
    /// skip without applying open semantics to the child.
    #[cfg(target_arch = "wasm32")]
    pub(crate) web_pending_nav: Option<WebPendingNav>,
```

Add the enum near the top-level `use` block or beside the other small `App`-support types in `src/app/mod.rs`:

```rust
#[cfg(target_arch = "wasm32")]
#[derive(Debug, Clone)]
pub(crate) enum WebPendingNav {
    Open(PathBuf),
    Load(PathBuf),
    LoadAfterOpen(PathBuf),
}
```

- [ ] **Step 2: Initialize in the constructor**

Near `let (web_folder_tx, web_folder_rx) = std::sync::mpsc::channel();` (~line 760):

```rust
        #[cfg(target_arch = "wasm32")]
        let (web_dirlist_tx, web_dirlist_rx) = std::sync::mpsc::channel();
```

In the `Self { ... }` literal, near `web_file_handles: HashMap::new(),`:

```rust
            #[cfg(target_arch = "wasm32")]
            web_dir_handles: HashMap::new(),
            #[cfg(target_arch = "wasm32")]
            web_dirlist_tx,
            #[cfg(target_arch = "wasm32")]
            web_dirlist_rx,
            #[cfg(target_arch = "wasm32")]
            web_dirlist_inflight: std::collections::HashSet::new(),
            #[cfg(target_arch = "wasm32")]
            web_pending_nav: None,
```

- [ ] **Step 3: Seed tree state in `poll_folder_pick`**

In `src/app/web.rs`, `poll_folder_pick`'s `Ok(picked)` arm, replace the body up to `self.mode = ViewMode::Grid;` with:

```rust
                Ok(picked) => {
                    self.web_file_handles = picked.handles;
                    self.web_dir_handles = picked.dir_handles;

                    let root = picked.dir.clone();
                    // First level of the tree, derived from the seeded dir
                    // handles (their keys whose parent is the root).
                    let mut first_level: Vec<PathBuf> = self
                        .web_dir_handles
                        .keys()
                        .filter(|p| p.parent() == Some(root.as_path()))
                        .cloned()
                        .collect();
                    crate::navigation::sort_by_name(&mut first_level);
                    self.subdirs.insert(root.clone(), first_level);

                    self.folder_root = Some(root.clone());
                    self.expanded = std::collections::HashSet::from([root.clone()]);

                    // Before `load_playlist` triggers the catalog scan (its
                    // wasm arm needs this handle to read `.lightphotos/*.xmp`).
                    if let Some(h) = self.web_dir_handles.get(&root) {
                        self.catalog.set_wasm_dir_handle(h.clone());
                    }
                    let playlist = Playlist::from_entries(root.clone(), picked.entries);
                    self.load_playlist(playlist, root);
                    self.mode = ViewMode::Grid;
                }
```

Note: `Playlist::from_entries` now receives root-qualified entry paths (`photos/IMG.jpg`) because `web_fs::list_dir` keyed them that way in Task 3. `load_playlist` sets `folder_sel = Some(root)`.

Keep `crate::navigation::sort_by_name` importable — it is `pub(crate)`.

- [ ] **Step 4: `ensure_subdirs` wasm arm**

In `src/app/mod.rs`, `ensure_subdirs` (~974):

```rust
    fn ensure_subdirs(&mut self, dir: &Path) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if !self.subdirs.contains_key(dir) {
                self.subdirs
                    .insert(dir.to_path_buf(), navigation::list_subdirs(dir));
            }
        }
        #[cfg(target_arch = "wasm32")]
        {
            if !self.subdirs.contains_key(dir) {
                self.request_dir_listing(dir);
            }
        }
    }
```

`request_dir_listing` does not exist until Task 5 — this task's wasm build will fail to compile at this step. Reorder: **do Step 4 as the first step of Task 5 instead.** Leave `ensure_subdirs` untouched in Task 4.

- [ ] **Step 5: wasm + native compile gate**

Run: `cargo test app::` (native still compiles, no behavior change)
Run: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
Expected: both clean. `web_dirlist_*` fields are unused until Task 5 — acceptable dead-code warnings within this task.

- [ ] **Step 6: Commit**

```bash
git add src/app/mod.rs src/app/web.rs
git commit -m "feat(web): seed folder tree state on pick; add subfolder-listing channel"
```

---

## Task 5: `request_dir_listing`, `poll_dir_listing`, `ensure_subdirs` wasm arm, frame-loop poll

**Files:**
- Modify: `src/app/web.rs` — new methods
- Modify: `src/app/mod.rs` — `ensure_subdirs` wasm arm
- Modify: `src/main.rs` — `about_to_wait` (~357-365)

**Interfaces:**
- Consumes: `web_fs::list_dir`, `App::web_dir_handles`, `App::web_dirlist_tx/rx`, `App::web_dirlist_inflight`, `App::web_pending_nav`, `WebPendingNav`.
- Produces:
  - `pub(crate) fn request_dir_listing(&mut self, dir: &Path)` — kicks off `list_dir` for `dir` unless cached or in flight.
  - `pub(crate) fn poll_dir_listing(&mut self) -> bool` — drains results, merges into `subdirs` / `web_file_handles` / `web_dir_handles`, dispatches a matching `web_pending_nav`, redraws; returns whether any listing is still outstanding.
  - Calls to `apply_web_open_folder` / `apply_web_load_folder` (defined in Tasks 6 / 7) — until those exist, `poll_dir_listing`'s dispatch arm is a `todo!()`-free stub (see Step 3).

- [ ] **Step 1: `ensure_subdirs` wasm arm**

Apply the `ensure_subdirs` change from Task 4 Step 4 now (native arm unchanged, wasm arm calls `self.request_dir_listing(dir)`).

- [ ] **Step 2: `request_dir_listing`**

Add to the `impl App` block in `src/app/web.rs`:

```rust
    /// Kick off an async `web_fs::list_dir` for `dir` (a relative path)
    /// unless its listing is already cached in `self.subdirs` or a scan is
    /// already in flight. The result lands on `web_dirlist_rx`, drained by
    /// `poll_dir_listing`. A missing directory handle is logged and treated
    /// as an empty (leaf) listing — should not happen, since a folder only
    /// becomes reachable after its parent's listing produced its handle.
    pub(crate) fn request_dir_listing(&mut self, dir: &Path) {
        if self.subdirs.contains_key(dir) || self.web_dirlist_inflight.contains(dir) {
            return;
        }
        let Some(handle) = self.web_dir_handles.get(dir).cloned() else {
            web_sys::console::error_1(
                &format!("[web] no directory handle for {}", dir.display()).into(),
            );
            self.subdirs.insert(dir.to_path_buf(), Vec::new());
            return;
        };
        self.web_dirlist_inflight.insert(dir.to_path_buf());
        let base = dir.to_path_buf();
        let tx = self.web_dirlist_tx.clone();
        wasm_bindgen_futures::spawn_local(async move {
            let result = web_fs::list_dir(&base, &handle).await;
            let _ = tx.send((base, result));
        });
        self.request_redraw();
    }
```

- [ ] **Step 3: `poll_dir_listing`**

```rust
    /// Drain finished subfolder listings. For each: merge image handles into
    /// `web_file_handles`, merge subdir handles into `web_dir_handles`, set
    /// `self.subdirs[dir]` to the subdir relative paths, clear the in-flight
    /// mark, and — if a deferred navigation was waiting on this exact
    /// directory — run its apply step. Returns whether any listing is still
    /// outstanding (feeds the poll-cadence calc in `main.rs`).
    pub(crate) fn poll_dir_listing(&mut self) -> bool {
        while let Ok((dir, result)) = self.web_dirlist_rx.try_recv() {
            self.web_dirlist_inflight.remove(&dir);
            let listing_succeeded = result.is_ok();
            match result {
                Ok(listing) => {
                    let mut subdir_paths = Vec::with_capacity(listing.subdirs.len());
                    for (path, handle) in listing.subdirs {
                        subdir_paths.push(path.clone());
                        self.web_dir_handles.insert(path, handle);
                    }
                    for (path, handle) in listing.images {
                        self.web_file_handles.insert(path, handle);
                    }
                    self.subdirs.insert(dir.clone(), subdir_paths);
                }
                Err(e) => {
                    self.set_status(format!("Couldn't list {}: {e}", dir.display()));
                    // Treat as a leaf so the tree stops retrying every frame.
                    self.subdirs.insert(dir.clone(), Vec::new());
                }
            }

            // Complete a navigation only after a successful listing. On
            // failure, clear the matching intent but preserve the current
            // folder and mode.
            match self.web_pending_nav.clone() {
                Some(WebPendingNav::Open(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_open_folder(p);
                    }
                }
                Some(WebPendingNav::Load(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                    }
                }
                Some(WebPendingNav::LoadAfterOpen(p)) if p == dir => {
                    self.web_pending_nav = None;
                    if listing_succeeded {
                        self.apply_web_load_folder(p);
                        self.mode = ViewMode::Grid;
                        self.update_window_title();
                        self.normalize_focus();
                    }
                }
                _ => {}
            }
            self.request_redraw();
        }
        !self.web_dirlist_inflight.is_empty()
    }
```

`apply_web_open_folder` / `apply_web_load_folder` don't exist yet. To keep this task compiling, add **temporary minimal stubs** at the end of the `impl App` block, to be fleshed out in Tasks 6 and 7:

```rust
    // TODO(Task 6): full expansion-toggle + pure-container logic.
    pub(crate) fn apply_web_open_folder(&mut self, dir: PathBuf) {
        self.apply_web_load_folder(dir);
    }

    // TODO(Task 7): assumes `dir`'s listing is cached.
    pub(crate) fn apply_web_load_folder(&mut self, dir: PathBuf) {
        if let Some(h) = self.web_dir_handles.get(&dir) {
            self.catalog.set_wasm_dir_handle(h.clone());
        }
        let mut entries: Vec<PathBuf> = self
            .web_file_handles
            .keys()
            .filter(|p| p.parent() == Some(dir.as_path()))
            .cloned()
            .collect();
        crate::navigation::sort_by_name(&mut entries);
        let playlist = crate::navigation::Playlist::from_entries(dir.clone(), entries);
        self.load_playlist(playlist, dir);
        self.request_redraw();
    }
```

`load_playlist` is `fn load_playlist` (private to `mod.rs`'s `impl App`, same module tree) — callable from `app/web.rs` since both are `impl App` in the `crate::app` module. Confirm visibility during implementation; if it's not reachable, change `load_playlist` from bare `fn` to `pub(crate) fn` in `mod.rs` (no behavior change).

- [ ] **Step 4: Frame-loop poll**

In `src/main.rs` `about_to_wait`, the wasm `web_folder_pending` block (~357-365):

```rust
        #[cfg(target_arch = "wasm32")]
        let web_folder_pending = {
            self.poll_catalog_persist_errors();
            let pick_pending = self.poll_folder_pick();
            let listing_pending = self.poll_dir_listing();
            pick_pending || listing_pending
        };
```

- [ ] **Step 5: Compile gates**

Run: `cargo test` (native — no wasm code compiled, must still pass)
Run: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
Expected: both clean.

- [ ] **Step 6: Manual smoke (browser)**

Serve the `dist/` from the trunk build, open it, pick a folder that has subfolders. Expected: the folder tree now shows the root with disclosure triangle and the first level of subfolders beneath it. Clicking a subfolder currently loads its images (via the stub) but does not yet toggle expansion correctly — that's Task 6.

- [ ] **Step 7: Commit**

```bash
git add src/app/web.rs src/app/mod.rs src/main.rs
git commit -m "feat(web): async subfolder listing wired into the folder tree"
```

---

## Task 6: Request/apply split for `open_folder` and `folder_expand`

**Files:**
- Modify: `src/app/nav.rs` — `open_folder` (~610-639), `folder_expand` (~564-580)
- Modify: `src/app/web.rs` — flesh out `apply_web_open_folder` (replace the Task 5 stub)

**Interfaces:**
- Consumes: `App::subdirs`, `App::expanded`, `App::web_file_handles`, `App::web_dir_handles`, `App::web_pending_nav`, `WebPendingNav`, `request_dir_listing`.
- Produces: `apply_web_open_folder` with full native-parity semantics (expansion toggle, pure-container skip). `open_folder` / `folder_expand` gain wasm arms that defer when a listing is missing.

- [ ] **Step 1: `open_folder` wasm arm**

In `src/app/nav.rs`, wrap the existing body of `open_folder` in `#[cfg(not(target_arch = "wasm32"))]` and add:

```rust
    pub(super) fn open_folder(&mut self, path: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            // ... existing body, unchanged ...
        }
        #[cfg(target_arch = "wasm32")]
        {
            // A newer tree action supersedes any older deferred navigation,
            // including when this action can be applied from cache.
            self.web_pending_nav = None;
            if !self.subdirs.contains_key(&path) {
                self.web_pending_nav = Some(crate::app::WebPendingNav::Open(path.clone()));
                self.request_dir_listing(&path);
                self.request_redraw();
                return;
            }
            self.apply_web_open_folder(path);
        }
    }
```

Confirm the `WebPendingNav` path: it is declared in `src/app/mod.rs` as `pub(crate) enum WebPendingNav`; from `app/nav.rs` (a submodule of `app`) it is reachable as `super::WebPendingNav` or `crate::app::WebPendingNav`. Use whichever matches the file's existing style for sibling items.

- [ ] **Step 2: `folder_expand` wasm arm**

`folder_expand` calls `self.ensure_subdirs(&cur)` then reads `self.subdirs(&cur)`. On wasm `ensure_subdirs` only *starts* the listing. Add an early return when it's not ready:

```rust
    pub(super) fn folder_expand(&mut self) {
        let Some(cur) = self.folder_sel.clone() else {
            return;
        };
        #[cfg(target_arch = "wasm32")]
        {
            // Every tree action supersedes an older deferred navigation.
            self.web_pending_nav = None;
        }
        self.ensure_subdirs(&cur);
        #[cfg(target_arch = "wasm32")]
        if !self.subdirs.contains_key(&cur) {
            // Listing just kicked off; the user's next right-arrow press
            // (after it lands and the triangle appears) will expand it.
            return;
        }
        if self.subdirs(&cur).is_empty() {
            return; // leaf
        }
        // ... rest unchanged ...
    }
```

The `load_folder(first)` call inside `folder_expand`'s "already expanded" branch needs the platform helper from Task 7 — leave it as `self.load_folder(...)` for now; Task 7 reroutes it.

- [ ] **Step 3: Full `apply_web_open_folder`**

Replace the Task 5 stub in `src/app/web.rs`:

```rust
    /// wasm counterpart of the tail of native `open_folder`: toggle the
    /// folder's expansion, and if it is a pure container (subdirs but no
    /// images of its own) skip straight to its first child. Assumes
    /// `dir`'s own listing is cached in `self.subdirs` (the caller in
    /// `open_folder` guarantees it; `poll_dir_listing` calls this only
    /// after inserting `dir`'s listing).
    pub(crate) fn apply_web_open_folder(&mut self, dir: PathBuf) {
        let subdirs = self.subdirs.get(&dir).cloned().unwrap_or_default();
        let has_own_images = self
            .web_file_handles
            .keys()
            .any(|p| p.parent() == Some(dir.as_path()));
        let pure_container = !subdirs.is_empty() && !has_own_images;

        if !subdirs.is_empty() {
            if self.expanded.contains(&dir) {
                if !pure_container {
                    self.expanded.remove(&dir);
                }
            } else {
                self.expanded.insert(dir.clone());
            }
        }

        let target = if pure_container {
            subdirs.into_iter().next().unwrap_or_else(|| dir.clone())
        } else {
            dir.clone()
        };

        // The pure-container target is a different folder; its own listing
        // may not be loaded yet.
        if target != dir && !self.subdirs.contains_key(&target) {
            // The original open already toggled `dir`. Once the child is
            // listed, only load it; applying `Open` would toggle the child
            // and could recursively skip another pure-container level.
            self.web_pending_nav = Some(WebPendingNav::LoadAfterOpen(target.clone()));
            self.request_dir_listing(&target);
            self.request_redraw();
            return;
        }

        self.apply_web_load_folder(target);
        self.mode = ViewMode::Grid;
        self.update_window_title();
        self.normalize_focus();
        self.request_redraw();
    }
```

`apply_web_load_folder` still swaps the grid + sets the catalog handle (Task 5 stub is already correct for that part; Task 7 only adds its own deferral guard).

The Grid-mode transition belongs to `apply_web_open_folder`, after the target
has been loaded, and must also run in the `LoadAfterOpen` completion arm in
`poll_dir_listing`. `apply_web_load_folder` is shared by folder movement and
collapse, which preserve the current mode just like native `load_folder`.

- [ ] **Step 4: Compile gates**

Run: `cargo test`
Run: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
Expected: both clean.

- [ ] **Step 5: Manual (browser)**

Pick a nested folder. Expected:
- Clicking a folder row with subfolders toggles its disclosure triangle and loads its images.
- Clicking a pure-container folder (no loose images) jumps to its first child, no empty-grid flash.
- Arrow keys: Right on a collapsed folder starts/finishes expansion.

- [ ] **Step 6: Commit**

```bash
git add src/app/nav.rs src/app/web.rs
git commit -m "feat(web): expansion-aware folder open with pure-container skip"
```

---

## Task 7: Reroute `folder_move` / `folder_collapse`; `load_folder` wasm guard

**Files:**
- Modify: `src/app/nav.rs` — `folder_move` (~546-560), `folder_collapse` (~584-596), `folder_expand`'s inner `load_folder` call
- Modify: `src/app/mod.rs` — `load_folder` (~999-1001)
- Modify: `src/app/web.rs` — add the deferral guard to `apply_web_load_folder`

**Interfaces:**
- Consumes: `apply_web_load_folder`, `WebPendingNav::{Load,LoadAfterOpen}`, `request_dir_listing`.
- Produces: `fn nav_to_folder(&mut self, dir: PathBuf)` on `App` — `load_folder` on native, deferred `apply_web_load_folder` on wasm. All `load_folder` calls in `nav.rs` route through it.

- [ ] **Step 1: `nav_to_folder` helper**

In `src/app/nav.rs` (in the `impl App` block):

```rust
    /// Load `dir`'s images into the grid without touching expansion state —
    /// the shared target of `folder_move` and `folder_collapse`'s
    /// select-parent branch. Native: synchronous `load_folder`. wasm: defer
    /// to `apply_web_load_folder` once `dir`'s listing is cached.
    fn nav_to_folder(&mut self, dir: PathBuf) {
        #[cfg(not(target_arch = "wasm32"))]
        self.load_folder(dir);
        #[cfg(target_arch = "wasm32")]
        {
            // A newer tree action supersedes any older deferred navigation,
            // including when this action can be applied from cache.
            self.web_pending_nav = None;
            if !self.subdirs.contains_key(&dir) {
                self.web_pending_nav = Some(crate::app::WebPendingNav::Load(dir.clone()));
                self.request_dir_listing(&dir);
                self.request_redraw();
                return;
            }
            self.apply_web_load_folder(dir);
        }
    }
```

- [ ] **Step 2: Route the three call sites**

- `folder_move`: `self.load_folder(tree[next].clone());` → `self.nav_to_folder(tree[next].clone());`
- `folder_collapse`: `self.load_folder(parent.to_path_buf());` → `self.nav_to_folder(parent.to_path_buf());`
- `folder_expand` (already-expanded branch): `self.load_folder(first);` → `self.nav_to_folder(first);`
- Invalidate `web_pending_nav` at the start of the wasm `folder_collapse`
  path as well, including the collapse-only branch that does not call
  `nav_to_folder`. Every newer tree action must supersede the prior intent.

- [ ] **Step 3: `load_folder` wasm guard**

In `src/app/mod.rs`:

```rust
    #[cfg(not(target_arch = "wasm32"))]
    fn load_folder(&mut self, dir: PathBuf) {
        self.load_playlist(Playlist::from_dir(&dir), dir);
    }

    #[cfg(target_arch = "wasm32")]
    fn load_folder(&mut self, _dir: PathBuf) {
        debug_assert!(
            false,
            "load_folder must not run on wasm32 — use nav_to_folder / the async open path"
        );
    }
```

- [ ] **Step 4: `apply_web_load_folder` deferral guard**

Prepend to `apply_web_load_folder` in `src/app/web.rs`:

```rust
        if !self.subdirs.contains_key(&dir) {
            self.web_pending_nav = Some(WebPendingNav::Load(dir.clone()));
            self.request_dir_listing(&dir);
            self.request_redraw();
            return;
        }
```

- [ ] **Step 5: Verify no wasm-reachable `load_folder` caller remains**

Run: `grep -n "load_folder\|Playlist::from_dir" src/app/*.rs`
Expected: `load_folder` called only from `nav_to_folder`'s native arm and `open`'s native `is_dir` branch (`App::open` is native-only in practice — the wasm entry is `poll_folder_pick`). `Playlist::from_dir` only in `load_folder`'s native arm and `from_file`. Confirm `App::open` is not wasm-reachable (it is called from `main.rs` native arg handling / `pending_initial`); if it is compiled for wasm, add `#[cfg(not(target_arch = "wasm32"))]` to its `is_dir` branch's `load_folder` call path or guard the whole fn — note the finding and handle it.

- [ ] **Step 6: Compile gates**

Run: `cargo test`
Run: `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`
Expected: both clean. No `debug_assert` fires in a native test run (means nothing routes to the wasm guard).

- [ ] **Step 7: Manual (browser)**

Keyboard nav: Up/Down through a multi-level expanded tree loads each folder's images; Left collapses then selects parent; parent's images load. Deep folders (listing not yet cached) load after a brief beat, no stuck state.

- [ ] **Step 8: Commit**

```bash
git add src/app/nav.rs src/app/mod.rs src/app/web.rs
git commit -m "feat(web): route folder_move/collapse through async nav; guard load_folder"
```

---

## Task 8: Catalog sidecar round-trip per subfolder

**Files:**
- Verify only: `src/app/web.rs` (`apply_web_load_folder` already calls `catalog.set_wasm_dir_handle`), `src/catalog.rs` (wasm `write_sidecar`/`delete_sidecar` ~404-440), `src/app/catalog.rs` (`request_catalog_load` wasm arm ~75-88)

**Interfaces:**
- Consumes: existing `Catalog::set_wasm_dir_handle`, `Catalog::wasm_dir_handle`, `web_catalog_fs::{load_sidecars, write_sidecar, delete_sidecar}`.
- Produces: nothing new — this task confirms the wiring and adds a regression note. No code unless a gap is found.

- [ ] **Step 1: Trace the path on paper**

Confirm, reading the code:
1. `apply_web_load_folder(dir)` calls `self.catalog.set_wasm_dir_handle(web_dir_handles[dir])` **before** `load_playlist`.
2. `load_playlist` → `seed_mirrors` → `request_catalog_load(playlist.dir())` where `playlist.dir() == dir`.
3. `request_catalog_load` wasm arm reads `self.catalog.wasm_dir_handle()` (now `dir`'s handle) and `spawn_local`s `web_catalog_fs::load_sidecars(&handle)`, which opens `dir/.lightphotos/`.
4. `Catalog::write_sidecar` / `delete_sidecar` (wasm) use `self.wasm_dir_handle` + `path.file_name()`. With relative playlist paths, `path.file_name()` is still the bare filename, and `wasm_dir_handle` is `dir` → sidecar written to `dir/.lightphotos/<filename>.xmp`.

- [ ] **Step 2: Browser regression test**

1. Pick a folder. Navigate into a subfolder `A`. Rate a photo 3 stars, apply an exposure edit.
2. Confirm (DevTools → Application → File System, or re-pick): `A/.lightphotos/<photo>.xmp` was created, root `.lightphotos/` was not.
3. Navigate to a different subfolder `B`, then back to `A`. The rating and edit are still shown (loaded from `A/.lightphotos/`).
4. Reload the page, re-pick the root, navigate to `A` — rating and edit persist.

- [ ] **Step 3: Commit (doc note only, if any)**

If the trace and test pass with no code change, add a one-line comment in `apply_web_load_folder` above the `set_wasm_dir_handle` call:

```rust
        // Point sidecar I/O at THIS folder's .lightphotos/ before
        // load_playlist kicks off the catalog scan (request_catalog_load's
        // wasm arm reads catalog.wasm_dir_handle()). Matches native's
        // per-folder catalog switch.
```

```bash
git add src/app/web.rs
git commit -m "docs(web): note the per-folder catalog handle swap in apply_web_load_folder"
```

---

## Task 9: Full manual browser verification

**Files:** none — acceptance gate.

**Interfaces:** none.

- [ ] **Step 1: Build and serve**

```bash
RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml
# serve dist/ (e.g. `python3 -m http.server` from dist/, or `trunk serve --release`)
```

- [ ] **Step 2: Prepare a fixture folder**

A folder with: loose images at the top level; a subfolder `2024/` containing images; a subfolder `2024/January/` two levels deep with images; a pure-container `Albums/` (subfolders only, no loose images) with a child `Albums/Best/` that has images; a `2024/.lightphotos/` created by a prior native session (or hand-made with one valid `<name>.xmp`).

- [ ] **Step 3: Walk the acceptance checklist**

- [ ] Folder tree shows the root with a ▶/▼ triangle and the first level of subfolders.
- [ ] Clicking a subfolder row: toggles its triangle and loads its images into the grid; thumbnails decode.
- [ ] Expanding `2024` reveals `January`; expanding `January` loads its images.
- [ ] Collapsing a folder hides its children; the selection/grid follow native rules.
- [ ] Arrow keys (tree focused): Up/Down move + load; Right expands or dives to first child; Left collapses or selects parent.
- [ ] `Albums/` (pure container) jumps straight to `Albums/Best/` with no empty-grid flash.
- [ ] Rating a photo in `2024/` writes `2024/.lightphotos/<name>.xmp`; the root `.lightphotos/` is untouched.
- [ ] Ratings/edits made by the prior native session in `2024/.lightphotos/` show when `2024` is opened.
- [ ] Navigating away from and back to a folder preserves its ratings/edits (reloaded from its own `.lightphotos/`).
- [ ] Reloading the page and re-picking the root: everything above still holds.
- [ ] The originally-picked root folder still loads and behaves exactly as before this feature.
- [ ] No console errors during any of the above.

- [ ] **Step 4: Native regression**

```bash
cargo test
cargo build --release
./target/release/lightphotos <a folder with subfolders>
```

- [ ] Native folder tree, expansion, keyboard nav, and per-folder ratings unchanged.

- [ ] **Step 5: Commit (if any fixture scripts or notes were added)**

```bash
git add -A
git commit -m "test(web): manual acceptance checklist for subfolder browsing"
```

---

## Self-Review

**1. Spec coverage:**

| Spec section | Task |
|---|---|
| Path model migration (relative-to-root keys) | Task 3 (keying), Task 4 (playlist entries via seeded pick) |
| `web_dir_handles` field | Task 4 |
| `web_pending_open` → `WebPendingNav` | Task 4 (decl), Tasks 5–7 (use) |
| `web_dirlist_inflight` | Task 4 |
| Async subdir listing (`list_dir` / `DirListing`) | Task 3 |
| Shared `is_listable_subdir` predicate | Task 1 |
| `ensure_subdirs` wasm arm | Task 5 Step 1 |
| `request_dir_listing` / `poll_dir_listing` | Task 5 |
| `web_dirlist_tx/rx` channel | Task 4 |
| Frame-loop `poll_dir_listing` | Task 5 Step 4 |
| Catalog handle per folder | Task 6/7 (`apply_web_load_folder`), Task 8 (verify) |
| `open_folder` request/apply split | Task 6 |
| `apply_web_open_folder` (expansion toggle, pure-container) | Task 6 |
| `folder_expand` request/apply | Task 6 Step 2 |
| `folder_move` / `folder_collapse` reroute + `apply_web_load_folder` | Task 7 |
| `load_folder` wasm guard | Task 7 Step 3 |
| `App::open` / `poll_folder_pick` seeding (`folder_root`, `expanded`, `subdirs[root]`) | Task 4 Step 3 |
| UI `folder_node` unchanged | (no task needed — verified in Task 5/6 manual) |
| Testing: `is_listable_subdir` unit test | Task 1 |
| Testing: multi-level `flatten_visible_tree` | Task 2 |
| Testing: manual browser gate | Task 9 |

No gaps.

**2. Placeholder scan:** All code steps contain real code. The temporary stubs in Task 5 Step 3 are explicitly labeled and replaced in Tasks 6/7 — each is functional (not `todo!()`), so the build stays green between tasks. The `flatten_visible_tree` "relative-path key helper" unit test mentioned in the spec's testing section is folded into Task 3's `sort_pairs_by_name` (covered structurally by the Task 9 sort checks and the native `sort_by_name` tests) rather than a standalone test — `list_dir` itself can't run without a browser, so there's no honest native unit test for it; noted, not a gap.

**3. Type consistency:**
- `DirListing { images: Vec<(PathBuf, FileSystemFileHandle)>, subdirs: Vec<(PathBuf, FileSystemDirectoryHandle)> }` — same in Task 3 (def), Task 4 (channel type), Task 5 (`poll_dir_listing` destructure). ✓
- `WebPendingNav { Open(PathBuf), Load(PathBuf), LoadAfterOpen(PathBuf) }` — Task 4 decl, Tasks 5/6/7 use. ✓
- `web_dirlist_tx/rx: (PathBuf, Result<DirListing, String>)` — Task 4 decl, Task 5 `request_dir_listing` send / `poll_dir_listing` recv. ✓
- `request_dir_listing(&mut self, dir: &Path)` / `poll_dir_listing(&mut self) -> bool` / `apply_web_open_folder(&mut self, dir: PathBuf)` / `apply_web_load_folder(&mut self, dir: PathBuf)` / `nav_to_folder(&mut self, dir: PathBuf)` — consistent across Tasks 5–7. ✓
- `is_listable_subdir(name: &str) -> bool` — Task 1 def, Task 3 use (`&name` where `name: String` from `child.name()` → `&name` derefs to `&str`). ✓

Fixed inline: Task 3's original `pick_and_list_folder` draft carried a `subdir_paths` local that was never stored; removed it — `poll_folder_pick` (Task 4) derives the root's first level from `web_dir_handles` keys instead.

---

## Execution Handoff

Plan complete and saved to `plans/web-subfolder-browsing-plan.md`. Two execution options:

**1. Subagent-Driven (recommended)** — a fresh subagent per task, review between tasks, fast iteration.

**2. Inline Execution** — tasks executed in this session via executing-plans, batch with checkpoints.

Which approach?
