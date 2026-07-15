# Best-of-Burst Culling — Design

**Date:** 2026-07-15
**Branch:** `phase1-best-of-burst`
**Status:** Approved, pending implementation plan

## Summary

Wire the existing (but currently uncalled) culling primitives into the UI as a
non-destructive **best-of-burst** aid. A `Bursts` toggle groups the current
folder into bursts by capture-time proximity, scores each frame's sharpness, and
in the thumbnail grid **badges the sharpest frame** of each burst while **dimming
its siblings**. No ratings are changed and nothing is hidden — the badge is pure
visual guidance for manual culling.

The primitives already exist and are unit-tested:

- `navigation::group_by_time(times, gap) -> Vec<u32>` — group ids per entry.
- `sharpness::sharpness(rgba, w, h) -> f64` — variance-of-Laplacian focus score.
- `image_decode::capture_time(path) -> Option<SystemTime>` — EXIF/mtime shot time.

This design connects them; it does not modify their internals.

## Locked decisions

| Decision | Choice |
|----------|--------|
| Interaction model | **Badge only, non-destructive** — no rating writes, nothing hidden |
| Trigger | **Toggle (on-demand)**, off by default, per-session |
| Burst gap | **Fixed 2 seconds** (no UI knob — YAGNI) |
| Visual treatment | **Best badge + dim siblings** |
| Invocation | Toolbar `Bursts` selectable-label + keyboard `B` |
| Filter interaction | **Bursts available only when the filter is unset** (mutually exclusive) |

## Architecture

### New module: `src/burst.rs` (pure, unit-tested)

Keeps derivation logic out of the already-large `app.rs`.

```rust
pub const BURST_GAP: Duration = Duration::from_secs(2);

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BurstMark { Best, Sibling }

/// For each group of size >= 2, the member with the max known score is `Best`,
/// the rest `Sibling`; groups of size 1 -> `None`. If a burst has no scored
/// members yet, its first member is provisionally `Best`. Ties resolve to the
/// earliest member. Output is indexed 1:1 with the inputs.
pub fn compute_marks(
    group_ids: &[u32],
    scores: &[Option<f64>],
) -> Vec<Option<BurstMark>>;
```

`compute_marks` is pure and total: `group_ids.len() == scores.len()` is the
caller's contract; on mismatch it operates over the shorter length.

### `App` state additions (`app.rs`)

```rust
bursts_on: bool,                                  // toggle state
capture_times: HashMap<PathBuf, Option<SystemTime>>, // cached EXIF reads
sharpness: HashMap<PathBuf, f64>,                 // cached focus scores
burst_marks: Vec<Option<BurstMark>>,              // derived, indexed like entries()
```

`capture_times` and `sharpness` are caches that survive toggling off/on within a
session (so re-enabling is instant). They are **not** persisted to the SQLite
catalog — recomputed per session. `burst_marks` is rebuilt (cheap) whenever the
toggle flips or new times/scores arrive.

## Compute flow (reuses existing background pools)

1. **Capture times** — add a `Job::Meta(PathBuf)` variant to the `Loader`
   (`loader.rs:28`) that runs `image_decode::capture_time` off-thread. When
   `Bursts` turns on, enqueue `Meta` jobs for every entry whose time is not yet
   cached. `Loader::poll_all` (`loader.rs:279`) gains a third result bucket for
   `(PathBuf, Option<SystemTime>)`, drained alongside thumbs/full in
   `main.rs:248`.

2. **Grouping** — when new capture times arrive, rebuild group ids by mapping
   the cached times over `playlist.entries()` in order and calling
   `navigation::group_by_time(&times, BURST_GAP)`. The folder is filename-sorted
   (`navigation::sort_by_name`); for camera output filename order matches shot
   order, so contiguous bursts land in contiguous group ids. **Assumption:**
   filename order ≈ capture order. A missing time inherits the current group per
   `group_by_time`'s contract, so a single unreadable EXIF won't fragment a burst.

3. **Sharpness** — piggybacks on `App::sync_thumb_textures` (`app.rs:2021`),
   where the decoded thumbnail `Arc<DecodedImage>` is already in hand. When
   `bursts_on`, compute `sharpness::sharpness(&img.rgba, img.width, img.height)`
   and cache by path if not already present. Thumbnails are premultiplied RGBA8;
   photos are opaque (alpha = 255) so premultiplied == straight for the luma
   computation.

4. **Stable winners** — a burst's "best" must not flip as the user scrolls (i.e.
   it must be "best of the whole burst", not "best of what scrolled past"). When
   `bursts_on`, proactively `request_thumb` for members of bursts (size >= 2)
   that are not yet scored. This bounds the extra decode work to burst members
   only, never the whole folder, and singletons are never scored.

5. **Derivation** — `burst_marks` recomputed via `burst::compute_marks(group_ids,
   scores)` when groups or scores change; sets a redraw.

## UI

### Invocation

- **Toolbar** (`ui.rs` `global_toolbar`, ~`:165`): a `Bursts` selectable-label
  next to the `Filter:` group, same style as `All`/`Unrated`. Emits a new
  `UiAction::ToggleBursts`.
- **Keyboard** `B` (currently unbound — taken letters are `G`/`E`/`C`/`X`; digits
  are ratings/filters; `Tab` cycles regions; `[`/`]` rotate). Added to the key
  match (`app.rs:~2514`) and the `?` help overlay list (`ui.rs:441`).

Both flip `bursts_on` through `App::toggle_bursts()`. Turning off clears the
visible badges/dimming immediately but keeps the caches.

**Mutually exclusive with the star filter.** Burst is only meaningful over the
whole folder (a filter would hide burst members and make the badge/dim visual
misleading), so the two are never active at once:

- While a filter is active (`filter.is_some()`), the `Bursts` toolbar label is
  **disabled/greyed** with a tooltip ("Clear the filter to use Bursts"), and the
  `B` key is a **no-op**. Pressing `B` under a filter does *not* clear the filter.
- While `bursts_on`, applying any filter (`SetFilter(Some(..))`) **auto-turns-off
  Bursts** — the filter wins. Caches are retained, so clearing the filter and
  pressing `B` again re-shows badges instantly.
- Net invariant: `bursts_on` implies `filter.is_none()`. Because burst always
  runs over the unfiltered folder, every burst is whole and the winner is the
  globally sharpest member.

### Grid rendering

- New accessor `App::burst_mark_at(pos) -> Option<BurstMark>` (mirrors
  `rating_at(pos)`), taking a visible-position index.
- In `thumbnail_cell` (`ui.rs:641`, shared by grid and Loupe filmstrip):
  - `Some(Sibling)` → paint a translucent dim rect over the image rect (after the
    image is drawn, ~`:666`).
  - `Some(Best)` → paint a badge marker in the **top-left** corner (rating stars
    live bottom-left at ~`:677`, so no clash).
  - `None` → unchanged.
- Badge glyph: a crown if the bundled egui font renders it, else an
  accent-colored `★` — decided at implementation time against the actual font
  (the app already renders `★` for ratings, so that fallback is guaranteed).

## Edge cases

- **Missing EXIF:** `capture_time` already falls back to file mtime; a fully
  unknown time inherits the current group.
- **Single-image folder / no bursts:** every group has size 1 → no badges, no
  dimming; toggle is a no-op visually.
- **Toggle off mid-scan:** background jobs already queued may still complete;
  their results land in the caches but nothing is painted while `bursts_on` is
  false.
- **Filter interaction:** enforced mutually exclusive (see UI → Invocation).
  `bursts_on` implies `filter.is_none()`, so burst marks are always computed and
  displayed over the full folder — no partial/fragmented bursts in the view.

## Testing

- **Unit (`burst.rs`):**
  - singleton group → `None`;
  - 2-member group picks the higher score as `Best`, other `Sibling`;
  - all-unscored burst → first member provisional `Best`;
  - tie → earliest member `Best`;
  - mixed groups in one input mark correctly and independently.
- **Manual verification:** open a folder with a known burst, press `B`, confirm
  the sharpest frame is badged and siblings dimmed; scroll to confirm the winner
  is stable; press `B` again to confirm the grid returns to plain and re-toggling
  is instant.

## Out of scope (YAGNI)

- No rating or reject writes (non-destructive by decision).
- No collapse/hide of siblings.
- No user-adjustable gap slider.
- No persistence of capture times or scores to the SQLite catalog.
- No re-sorting the grid by capture time.
