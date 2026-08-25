# Fix Loupe zoom-to-fit on tier swap; reinstate web speed→quality→full staging

## Context

Reported symptom: open a RAW photo in the web Loupe → thumbnail shows correctly zoomed → after ~1s a higher-res image appears zoomed into the top-left corner (not re-fit) → after a few more seconds another (noisier) image appears, also mis-positioned.

Two Explore passes nailed the root cause precisely:

**Root cause** — `src/app/thumbs.rs`'s `upload_shown()` (the single choke point every tier's texture upload goes through — Thumb, Preview, Full alike):
```rust
if same_photo {
    self.push_transform();   // reuses old zoom/pan, unconditionally
} else {
    self.fit_to_window();
}
```
This never checks `self.fitted` (the "still auto-fit, user hasn't manually zoomed" flag that gates *every other* re-fit site in the codebase: window resize, EXIF/`source_size` landing, rotate, compare-toggle, the web preview handler). `image_size()` (`app/thumbs.rs`) falls back to the just-uploaded texture's own raw pixel dimensions whenever real source dimensions haven't landed yet — so a same-photo tier swap before that happens reapplies the *old* tier's zoom/pan against the *new* tier's (different) pixel dimensions: shrunk scale, near-zero offset → stuck zoomed into the top-left corner.

This is a known, previously-worked-around bug, not a new one: `web_worker_pool.rs` and `app/web.rs`'s own doc comments record that a "Quick" fast-preview tier was already tried on the web Loupe and **reverted** because of this exact failure ("a wrongly zoomed flash on open... proved fragile") — the workaround was to remove the extra tier rather than fix `upload_shown`. Current web code is a single pass straight to the quality decode (plus an independent `Thumb` job that's explicitly blocked from painting into the Loupe's texture for RAW files, `skip_thumb_placeholder` in `try_show()`, added in `fed9af4` as a different workaround for the same underlying fragility).

**The fix below addresses the actual root cause**, which makes it safe to reinstate the staged flow you want:

> thumbnail (instant) → speed/fast decode at screen resolution (re-fit correctly) → [stop] → only on zoom past 1:1, load the full/quality decode, fixed at the current zoom (no re-fit).

Plus a throwaway diagnostic: tint the Loupe's background by which tier is currently shown (white/18% gray/black), to make the staging visually unambiguous while verifying the fix. Remove once confirmed.

## Fix 1 — root cause (small, do regardless of the rest)

**`src/app/thumbs.rs::upload_shown`**: gate the `same_photo` branch on `self.fitted`, matching the pattern used everywhere else in the codebase:
```rust
if same_photo {
    if self.fitted {
        self.fit_to_window();
    } else {
        self.push_transform();
    }
} else {
    self.fit_to_window();
}
```
`fitted == false` means the user has manually zoomed (`zoom_at`/`reset_100` clear it) — that's the scenario the existing comment ("a sharper tier of the same picture must not disturb the view") is actually about, and it's preserved correctly. `fitted == true` means the view is still tracking the window/image — any tier swap in that state must re-fit, which this now does.

This alone fixes the reported bug for the *current* single-pass web pipeline and for native's existing Quick→Preview→Full staging. Fix 2/3 below are the actual feature work (reinstating the staged web flow), made safe by this fix.

## Fix 2 — reinstate a speed-then-quality staged decode on the web Loupe

Currently `JobKind` (`src/web/web_worker_pool.rs`) has only `Thumb`/`Preview`, with `quality` derived purely from `kind` at `submit()` (`quality = kind == JobKind::Preview`) — so there's no way to request two different-quality decodes at the same target size, and `already_have`/inflight/failed tracking in `app/web.rs::request_web_preview` is keyed by `(path, target)` alone, which would collide if two different-quality requests used the same key.

Add a third kind, `JobKind::Speed` (quality=false, decoded via the same `decode()` fast branch `Thumb` already uses in `wasm_worker.rs`, just requested at `preview_px` instead of `thumb_px`), with its own independent tracking (mirror `Thumb`'s/`Preview`'s existing per-kind `inflight`/`failed` sets and result handling in `web_worker_pool.rs` and `app/web.rs`) so a `Speed` request and the real `Preview` (quality=true) request for the same `(path, preview_px)` can both be in flight/cached without colliding.

Sequence on Loupe open (`app/web.rs`):
1. `request_web_preview` (or a new sibling) submits the `Speed` request immediately.
2. When it lands, `upload_shown(path, img, Shown::Preview(...))` shows it — now safely re-fit by Fix 1.
3. Submit the real quality `Preview` request (unchanged from today) right after, or once `Speed` lands.
4. When quality lands, it replaces the Speed-tier texture through the same `upload_shown` path — same target size, so no visible jump, just a quality/noise-profile change (camera JPEG → real sensor demosaic, per the second Explore pass — that content difference is expected, only the *positioning* was ever the bug).

Also revisit `try_show`'s `skip_thumb_placeholder` (`app/thumbs.rs`, currently unconditionally `true` for RAW on wasm32): the grid thumbnail (already independently requested via `JobKind::Thumb` for the working set, which includes the current Loupe selection) can likely be un-blocked as the Loupe's initial placeholder now that Fix 1 makes the following tier swap safe — giving you the "thumbnail, correctly zoomed" first stage for free from the cache that's already being fetched anyway.

## Fix 3 — wasm-effective zoom-triggered full-res load

`ensure_full_for_zoom` (`app/loupe.rs`, called from `zoom_at`/`reset_100`, not cfg-gated) already fires on every zoom step past the point the current tier's resolution stops being enough (`zoom_outruns_preview`) — but it only calls `loader.request_full(path)`, which enqueues into the **native** worker queue that has zero live workers on wasm32 (per `ce7fe68`), so it's currently a silent no-op on web.

Add a wasm32 branch: a new `request_web_full()` in `app/web.rs`, mirroring `request_web_preview()` but requesting the real quality decode (`quality=true`) at `renderer.max_dim` (the same value native's `Loader::new(renderer.max_dim)` uses for `full_target` — "the GPU's max texture size, i.e. don't downscale") instead of `preview_px`. Own dedup tracking (own `(path, target)` key space, or reuse `Preview`'s if the target being different from `preview_px` already disambiguates it — confirm during implementation). Lands via the same `upload_shown` choke point as `Shown::Full`; since the user just zoomed (`fitted == false` by then), Fix 1's gate correctly does *not* re-fit — exactly "fixed at the same zoom."

## Throwaway diagnostic — tint background by shown tier

Single spot: `src/renderer.rs`'s image-pass clear color (currently hardcoded `wgpu::Color { r: 0.07, g: 0.07, b: 0.08, a: 1.0 }`, in `render()`'s "Pass 1: the image"). Make it a field on `Renderer`, settable via a small method called alongside `set_image` from `upload_shown` (which already knows the `tier: Shown` and, once Fix 2 lands, whether this was the Speed or Quality decode):

- `Shown::Thumb` → white
- Speed/fast Preview → 18% gray
- Quality Preview / `Shown::Full` → black

Mark clearly with a `// TEMPORARY DEBUG — remove once zoom-refit fix is verified` comment at both the field and the call sites. Strip it out (revert to the fixed `0.07/0.07/0.08`) once you've confirmed the staging looks right — do not leave this in as permanent UI, per your call.

## Files

- `src/app/thumbs.rs` — Fix 1 (`upload_shown`'s `fitted` gate), `skip_thumb_placeholder` revisit.
- `src/app/loupe.rs` — Fix 3 (`ensure_full_for_zoom`'s wasm32 branch).
- `src/app/web.rs` — Fix 2/3 (`request_web_preview` split into Speed+Quality, new `request_web_full`, `poll_web_preview`/`poll_web_thumbs`-equivalent handling for the new kind(s)).
- `src/web/web_worker_pool.rs` — `JobKind::Speed` addition, per-kind tracking mirroring `Thumb`/`Preview`.
- `src/web/wasm_worker.rs` — no change expected; `decode()`'s existing `quality: bool` parameter already covers Speed (false) vs Quality (true) at any target size.
- `src/renderer.rs` — throwaway debug clear color.

## Verification

- `cargo test` — confirm the existing suite (including `loupe.rs`'s `zoom_outruns_preview` tests) still passes; Fix 1 changes real control flow so this must be re-run, not assumed.
- Manual, in the actual web build (per project convention — this is UI/GPU behavior, not something to script/automate): open a RAW photo in the Loupe.
  - Confirm sequence: white (thumbnail, correctly fit) → 18% gray (speed, correctly re-fit, not top-left-stuck) → stays gray, no further automatic changes.
  - Zoom past 1:1: confirm black (full/quality) appears fixed at the zoom level you were already at, not re-fit and not jumping.
  - Repeat on a non-RAW (JPEG) file to confirm no regression there (single-pass path, Fix 1 should be a no-op improvement).
- Remove the throwaway tint (Renderer clear-color change) once the above is confirmed correct; leave Fixes 1-3 in place.
