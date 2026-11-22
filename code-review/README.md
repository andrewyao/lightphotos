# LightPhotos code review — index

Module-by-module read-through of everything under `src/`, done 2026-11-21. Each reviewer read every line of its files and checked the code against the CLAUDE.md rules (no panics, FFI safety, hostile numbers, atomic writes, rawler out of macOS). Source was not modified, and no tests, clippy or builds were run. Findings come from reading the code. Where a report says "confirmed", it did a scratch check (for example `rustc` or Swift) outside the repo.

Totals: **334 findings. 0 Critical, 2 High, 55 Medium, 200 Low, 77 Info.**

## Reports

| # | Report | Scope | C/H/M/L/I |
|---|---|---|---|
| 01 | [app-core](01-app-core.md) | app mod, state, accessors, session, history, menu, slideshow, tour, fonts | 0/0/1/8/4 |
| 02 | [app-input](02-app-input.md) | keys, commands, control | 0/0/3/4/2 |
| 03 | [app-nav](03-app-nav.md) | nav, sort, bursts, group compare | 0/0/2/8/3 |
| 04 | [app-views](04-app-views.md) | loupe, fullscreen, histogram, thumbs, faces, score | 0/0/1/6/3 |
| 05 | [app-develop](05-app-develop.md) | adjust, crop, masks, optics, autotone | 0/0/3/8/2 |
| 06 | [app-generative](06-app-generative.md) | generative, fill, comfy, agent, coverage | 0/0/2/7/6 |
| 07 | [app-library](07-app-library.md) | folder_store, folders, catalog, web_catalog, bulk_delete | 0/0/2/3/3 |
| 08 | [app-export-presets-web](08-app-export-presets-web.md) | export, presets, web, geotag | 0/0/1/10/4 |
| 09 | [comfy-web](09-comfy-web.md) | `src/comfy`, `src/web` | 0/0/1/11/4 |
| 10 | [decode](10-decode.md) | `src/decode` | 0/0/5/12/4 |
| 11 | [develop-a](11-develop-a.md) | develop: base, brush, chroma, color, copy, coverage, curve, effects, fill_request | 0/0/1/6/2 |
| 12 | [develop-b](12-develop-b.md) | develop: fill, grade, image_ops, local, mask, mod, parallel, sharpen, side_file | 0/0/3/6/1 |
| 13 | [export-geotag-sidecar](13-export-geotag-sidecar.md) | `src/export`, `src/geotag`, `src/sidecar*` | 0/0/2/13/6 |
| 14 | [i18n](14-i18n.md) | `src/i18n*` | 0/0/3/9/2 |
| 15 | [jobs-and-utils](15-jobs-and-utils.md) | `src/jobs`, worker_pool, paths, hash, base64 | 0/0/3/9/4 |
| 16 | [scoring](16-scoring.md) | `src/scoring` | 0/1/3/8/3 |
| 17 | [persist-a](17-persist-a.md) | catalog, groups, lr_preset, prefs | 0/0/2/8/4 |
| 18 | [persist-b](18-persist-b.md) | folder_store, preset_folders, presets, secret, signalcache | 0/0/1/12/3 |
| 19 | [renderer-shaders](19-renderer-shaders.md) | `src/renderer`, `src/shaders` | 0/1/3/6/4 |
| 20 | [shell-main](20-shell-main.md) | `src/shell`, main, control_auth, update, lib | 0/0/3/11/3 |
| 21 | [ui-a](21-ui-a.md) | develop_panel, export_panel, form, grade_wheel, grid, home, info_panel, loupe | 0/0/3/11/3 |
| 22 | [ui-b](22-ui-b.md) | mask_overlay, modals, photo_menu, presets, tabs, theme, toolbar, tour | 0/0/3/13/2 |
| 23 | [command-navigation-autotone](23-command-navigation-autotone.md) | top-level command, navigation, autotone | 0/0/3/6/3 |
| 24 | [bin](24-bin.md) | probes and the MCP server | 0/0/1/5/2 |

Counts come from the `### [ID]` headings. A few reviewer chat summaries differ by one or two from the files (for example 19's own summary line). The files are authoritative.

## High severity

- **SCORE-01** ([16](16-scoring.md)): `segmentation.rs:176` rotates Vision's subject mask by EXIF orientation, but Vision already returns it upright. Every photo with orientation other than 1 gets a double-rotated overlay and a wrong persisted Subject coverage. The reviewer confirmed this with a Swift test.
- **REND-01** ([19](19-renderer-shaders.md)): no wgpu uncaptured-error or device-lost handler. wgpu's native default is to panic, so any GPU out-of-memory, validation error or device loss kills the UI thread. On wasm it leaves a blank canvas.

## Medium severity, by theme

**Data loss and integrity**
- PERSIST-A-01 and PERSIST-B-01: an unreadable presets file is treated as an empty library and then overwritten on the next save.
- EXP-01: `write_record` can write a sidecar larger than the limit the reader enforces, so the rating and edits are lost on reload.
- EXP-02, DEV-B-03 and DEV-B-02: generative fills are not forward-compatible. Fills this build can't parse are dropped, the orphan sweep deletes their patches, and a missing patch is silently skipped while the edit signature still counts it.
- JOBS-01: `write_atomic` never fsyncs, so a power loss can leave an empty file under the final name.
- BIN-01: headless MCP commands overwrite or delete sidecars in bulk with no undo or backup.
- APP-CORE-01: opening a file in another folder keeps the previous folder's undo history and group steps.
- DEV-B-01 and DEV-A-01: a hostile sidecar can turn the photo black (TouchUp has no sanitizer) or garbage (unclamped CA scale).

**Stale or stuck state**
- APP-VIEW-01, APP-NAV-01, APP-LIB-02, APP-OUT-01, APP-GEN-01, JOBS-02: results or caches that outlive the folder, photo or catalog they belong to.
- APP-LIB-01, APP-DEV-01, APP-GEN-02, COMFY-WEB-01: a dead worker thread or a lost poll leaves a feature wedged for the session.
- APP-DEV-02 and APP-DEV-03: Touch Up's undo stack and batch cancel drift from the main history.
- APP-INPUT-01, 02 and 03: Esc, `check()` and the GPS dialog each skip a guard that a sibling path applies.

**Robustness at boundaries**
- REND-02 and REND-03: GPU uploads trust caller dimensions, and surface Lost/Validation errors are mishandled.
- DECODE-04 and DECODE-05: browser RAW paths skip `decode_budget`, and CR3/RAF have no pre-allocation size check.
- SHELL-01 and SHELL-03: a startup failure is invisible and exits 0, and a saved window rect is restored without clamping.

**Correctness and performance**
- DECODE-01: the macOS capture-date fallback looks in the wrong dictionary.
- SCORE-02, 03 and 04: eye landmark normalization, a missing autorelease pool, and a blink metric that fires on any face.
- DECODE-02 and DECODE-03: whole-file reads for metadata on Linux and for TIFF-family files.
- CORE-02 and UI-A-02: about 200k `Develop` constructions per autotone, and per-frame path cloning for the whole selection.

**i18n, UI and accessibility**
- I18N-01 to 03: no OS-language detection on Windows, "Cmd" shown on other platforms, and 23 untranslated shortcut rows.
- UI-B-01 to 03 and UI-A-09: Purple label can't be set, toolbar and preset overflow, and custom widgets with no accessible names.

**Build and tests**
- SHELL-02: `--features hotpath` doesn't compile and CI never builds it.
- REND-04 and CORE-03: GPU parity tests cover little and run only on macOS. One malformed-request test passes for any input that parses.

## Positive notes repeated across reports

- No input-reachable panics in non-test code were found in nearly every module.
- Every `unsafe` block checked had a SAFETY comment matching its real invariant.
- The macOS build keeps rawler out.
- The TCP control server has a sound auth design: mutual HMAC handshake, constant-time compare, loopback bind, size and time limits.
- The update check downloads and executes nothing.
- The i18n catalogs are consistent across all 7 languages (596 keys, placeholders match).
- The Rust and WGSL struct layouts match field by field.

## Method and limits

- Each reviewer was told to verify a finding by tracing callers before reporting it. Confidence labels are in the reports. Treat Low-confidence items as leads, not facts.
- There was no second-pass adversarial verification. A good next step is to have an independent agent try to refute each Medium and High before anyone fixes it.
- Nothing was executed except small scratch programs by individual reviewers.
