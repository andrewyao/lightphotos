# Review action plan

From the architecture review of 2026-09-27. Items are ordered by risk, then payoff. Each one ends in a check that proves it.

## Phase 1. Security and correctness bugs

| # | Item | Status | Check |
|---|------|--------|-------|
| 1 | Temp-file writes follow a planted symlink. Sidecars, exports, non-mac thumbnails and the signal cache now go through `paths::write_atomic`, which opens the temp file with `create_new`. | Done | `paths::tests::write_atomic_never_writes_through_a_planted_symlink` fails when `create_new` is swapped for `create` + `truncate`. |
| 2 | The Immich API key follows redirects and `/.well-known/immich` to any host. Redirects are off, and an absolute endpoint must share the origin's scheme, host and port. | Done | `immich::tests::the_api_endpoint_comes_from_well_known_or_defaults_to_api` covers another host, HTTP, a lookalike suffix and another port. |
| 3 | The CPU RAW display curve in `develop.rs` drifted from the shader after `a01f39c`. The knot table now lives in `develop.rs`, and `nonmac_decode.rs` re-exports it. | Done | `develop::tests::raw_display_at_identity_follows_the_look_curve` and the shader knot parity test now run on macOS. Both fail on the old curve. |
| 4 | A panic in the face-quality or featureprint pool leaves its caller pending forever. Both workers now catch the panic and send an error outcome. | Done | `a_panicking_analysis_...` and `a_panicking_comparison_...` time out without the guard. |
| 5 | Catalog reads ignore the photo's folder. `Catalog::cache_key` now answers only for photos in the active directory. A write for another folder is refused and reported, and `remove` leaves the active folder's same-named record alone. | Done | `catalog::tests::a_photo_outside_the_active_directory_never_sees_its_records` fails when `cache_key` ignores the folder. |
| 6 | A sidecar with more than 64 touch-ups overflowed the 64-entry GPU buffer. `ImageRecord` now drops spots past `develop::MAX_TOUCHUPS` on load, which covers native and web. The renderer and the editing UI use the same constant. | Done | `catalog::tests::a_sidecar_loads_at_most_max_touchups` fails without the truncate. |
| 7 | Thumbnail cache entries can be forged. Hashing the photo's bytes does not help, because whoever plants the cache entry also supplies the photo and can compute the same hash. Real protection needs a per-user secret, which would stop caches being shared across machines and between the app and the browser. | Won't do | Accepted as low severity. Opening the photo in the Loupe always decodes the real file. |
| 8 | Decode size budget. `image_decode::MAX_DECODE_PIXELS` (500 MP) is checked where the macOS bitmap is allocated and before the non-mac RAW develop step. It keeps `w * h * 8` inside a u32, so the pixel index math can't wrap. ImageIO already refuses a tiny file claiming 16000x16000 or more on its own. | Done | `the_bitmap_draw_refuses_a_size_over_budget_before_allocating` and `the_decode_budget_admits_big_panoramas_and_refuses_bombs`. |

## Phase 2. Performance

| # | Item | Check |
|---|------|-------|
| 9 | Done. Background scoring redrew at vsync. The `request_*` calls now pick the `WaitUntil` interval: 16 ms while a thumbnail is pending, 100 ms while only Vision work is. Arrivals redraw when they land. The grouping tools are hidden behind `SHOW_GROUPING_TOOLS`, so today this mostly affects the grid's pending viewport. | With grouping flipped on, a D press on 80 ARWs drew 981 to 1626 frames in 40 s before, and 86 to 332 after. Total CPU stays near 35 s either way, because Vision's full-resolution decodes (item 13) dominate. |
| 10 | Four whole-folder scans run every frame (`app/thumbs.rs:260-563`). Keep the pending sets incremental. | `--profile` with a 10k folder. |
| 11 | Thumbnail LRU is really FIFO. Reads never refresh an entry (`loader.rs:940-955`), so a background pass evicts visible thumbnails. | `scroll` phase `get_or_make` count during a dupes pass. |
| 12 | Thumbnail re-bake on the UI thread every slider frame (`app/thumbs.rs:710`). Bake off-thread or at most once per frame. The bake does 12 `powf` per pixel, including a gamma round trip that cancels out (`image_ops.rs:83-155`). | `auto_tone` and `export` phases before and after. |
| 13 | Vision decodes full-resolution originals (`vision.rs:23`). Feed it the embedded preview. | `vision` phase. |
| 14 | Export tones at full resolution before downscaling, and on macOS round-trips JPEG bytes through a temp file (`export.rs:565`). | `export` phase. |
| 15 | UI-thread folder listing: a stat per entry before the extension filter, and a sort that allocates lowercase strings per comparison (`navigation.rs:102-132`). | `grid` phase with `LIGHTPHOTOS_PROFILE_COLD=1`. |

## Phase 3. CI, so drift bugs can't return

| # | Item |
|---|------|
| 16 | Run `cargo test` on Linux, not only `cargo build`. It would have caught item 3. |
| 17 | Build `--features raw-probe --bin decode_probe` and run its tests. |
| 18 | Make fmt blocking. `cargo fmt --check` passes today. Update the CLAUDE.md line that says it fails. |
| 19 | Clear the 23 clippy warnings, then run clippy with `-D warnings`. |
| 20 | Release workflow: scope `contents: write` to the publish job, and build with `--locked`. Verify the rawler download's sha256 in `setup-vendor-rawler.sh`. |

## Phase 4. Dead code and duplication

| # | Item |
|---|------|
| 21 | Delete `export::bake_jpeg` (tests only), the uncalled color-label write path, `Renderer::surface_format`, the unused `img-parts` dependency and the second `half` declaration. |
| 22 | Move the shader blocks duplicated between `shader.wgsl` and `raw_shader.wgsl` into `loupe_common.wgsl`. |
| 23 | One EXIF orientation routine instead of three (`image_decode.rs:692`, `image_ops.rs:371`, `raw/preview.rs:220`). |
| 24 | One generic worker pool for export, face quality and featureprint, with the panic guard built in. |
| 25 | Web: one `dir_entries` and `write_file` helper for the five folder-listing loops. One `DecodeTracker` for the four copies of retry bookkeeping in `app/web.rs`. |
| 26 | Point `raw/probe.rs` at the shipped `thumbnail.rs` functions instead of its drifted copies. |
| 27 | Fix doc drift. `ExportFs` no longer exists, and the module counts in `PROJECT_LAYOUT.md` are stale. |

## Phase 5. Architecture

| # | Item |
|---|------|
| 28 | Split the 167-field `App` into owned sub-structs (library, edits, analysis, export, view, input, web backend). Replace the six modal bools with `modal: Option<Modal>`, and the pending/failed set pairs with one `JobState` per photo. |
| 29 | Route keys through `UiAction` so there is one state reducer instead of two. |
| 30 | Add `src/lib.rs` so shared files stop being mounted by `#[path]` from seven places. |
| 31 | Typed errors in place of `Result<_, String>`, so retryable and permanent failures differ. |
