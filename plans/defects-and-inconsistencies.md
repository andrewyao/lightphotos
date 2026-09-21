# Plan — Defects and inconsistencies

Work through these top to bottom (or by worktree assignment, if parallel).
Check a box only after verification passes.

Standalone plan, not part of the `00-overview.md` roadmap sequence. Found during
a read-only audit of the Vision/ML surface on 2026-09-20 at commit `6e50544`.
Every claim below carries a `file:line` citation that was checked against the
tree, except the two marked **UNVERIFIED**, which state what would confirm them.

## A. Runtime bugs

- [ ] Task 1: **UNVERIFIED (static reading, needs a browser run).** On a target where no worker thread spawns, both Vision pools silently swallow every job and spin the event loop forever. `DistancePool::new` and `FacePool::new` build `job_rx` as an `Arc<Mutex<Receiver>>`, clone it into each worker closure, and never store it in `Self` (`src/featureprint.rs:142`, `src/facequality.rs:~318`). When every `thread::Builder::spawn` fails, those closures drop, the receiver drops at the end of the constructor, and `submit`'s `let _ = self.job_tx.send(..)` becomes a no-op. But `request_feature_prints` inserts into `feature_pending` *before* submitting (`src/app/thumbs.rs:444-447`), so the pending set fills and never drains, the function returns `true` on every frame, and `src/main.rs:479-487` calls `request_redraw()` with no `wasm32` cfg gate. `request_face_quality` has the same shape. Reachable today on web by pressing `B` or `D` (`src/app/keys.rs:185-186`, ungated); `SHOW_GROUPING_TOOLS` hides the buttons but not the keys. `request_selection_mask` handles its own spawn failure correctly (`src/app/loupe.rs:418-422`) and is the fix model: either store `job_rx` in the struct, or count successful spawns and have `submit` refuse (and `request_*` return `false`) when the count is zero. Confirm with `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`, open the page, press `D`, and watch for a pegged frame loop. Add a unit test that a zero-worker pool reports itself unusable rather than accepting jobs (files: src/featureprint.rs, src/facequality.rs, src/app/thumbs.rs)

- [ ] Task 2: **UNVERIFIED (needs an Apple availability check, not readable from this tree).** `Info.plist` declares `LSMinimumSystemVersion 11.0` and the README repeats macOS 11+, but `VNGeneratePersonSegmentationRequest` (`src/segmentation.rs:158`) and `VNGenerateForegroundInstanceMaskRequest` (`src/segmentation.rs:176`) both postdate macOS 11. There is no runtime availability check anywhere: `grep -rn "respondsToSelector\|operatingSystemVersion\|isOperatingSystemAtLeast" src/` returns nothing. On an OS that lacks the class, the `objc2` call likely traps rather than degrading. Confirm each request's real minimum from Apple's documentation, then either raise `LSMinimumSystemVersion` and the README together, or add a class-availability check that makes `segment` return the ordinary `Err` path (files: Info.plist, README.md, src/segmentation.rs)

- [ ] Task 3: The Loupe's "Show Selection" button renders on every platform with no cfg gate (`src/ui/loupe.rs:389-408`, zero `cfg(` lines in the file). Off macOS, `segmentation::segment` returns `Err("subject segmentation is unsupported on this platform")` immediately, and `poll_selection_mask` collapses every `Err(_)` into "no subject found" by design (`src/app/loupe.rs:436-437`). So on Linux, Windows and web the button permanently reads "No subject", which a user cannot distinguish from "Vision looked and found nothing here". Either cfg out the button off macOS, or carry the unsupported case as a distinct state with its own i18n string. Verify by building for a non-macOS target and confirming the control is absent or honestly labelled (files: src/ui/loupe.rs, src/app/loupe.rs, src/i18n.rs)

## B. Unvalidated constants blocking the roadmap

- [ ] Task 4: `CLOSED_EYE_RATIO = 0.15` decides every blink verdict and its own comment says "Not yet checked against real photos; use `src/bin/face_probe.rs` for that" (`src/facequality.rs:149`). Run `cargo run --bin face_probe -- <photos>` over a real burst containing a genuine blink plus open-eyed siblings, read the per-eye openness values, and set the constant from the observed separation. `plans/00-overview.md` Task 5 is BLOCKED on exactly this (files: src/facequality.rs)

- [ ] Task 5: `DEFAULT_MAX_FEATURE_DISTANCE = 0.5` decides every duplicate split and its own comment says "Vision documents no distance scale, so this value is a guess that has not been tuned on real photos" (`src/duplicates.rs:103`). Tune it against a real folder holding both true duplicates and merely-similar frames. `plans/00-overview.md` Task 6 is BLOCKED on the sibling check for segmentation (files: src/duplicates.rs)

- [ ] Task 6: Run `seg_probe` against real photographs with subjects and confirm the mask tracks them, which is the remaining blocked step of `plans/plan-d2-subject-segmentation-selection.md`. Note the recorded failure mode this cannot catch: person segmentation returns 13.4% *solid* coverage on Apple's `iMac Blue` wallpaper, which contains no person, and `src/segmentation.rs:117-118` states the coverage guard "can't catch a confident wrong mask" (files: src/segmentation.rs)

## C. Unreachable and dead code

- [ ] Task 7: The eyes-closed filter is unreachable in a shipped build. Its toolbar button is compiled out by `SHOW_GROUPING_TOOLS = false` (`src/app/mod.rs:40`, `src/ui/toolbar.rs:89`) and `grep -n "eyes\|Eyes" src/app/keys.rs` returns nothing, so no key binding exists either. Decide the feature's fate: give it a binding, ship the button, or delete the filter, `App::eyes_filter`, the `recompute_visible` clause (`src/app/nav.rs:63-71`) and the badge together. Leaving working code with no entry point is the worst of the three (files: src/app/keys.rs, src/ui/toolbar.rs, src/app/nav.rs)

- [ ] Task 8: Bursts and Duplicates ship behind undocumented keys. `SHOW_GROUPING_TOOLS = false` hides the toolbar buttons, and while `docs/KEYBOARD_SHORTCUTS.md` does list `B` and `D`, nothing in the running app does. Commit `d451c80` says the buttons stay out "until the feature earns its place there", which Tasks 4 and 5 are the precondition for. Resolve this after those land, and flip the constant or remove the dead branch rather than leaving it permanently false (files: src/app/mod.rs, src/ui/toolbar.rs)

- [ ] Task 9: `facequality::detect_face_rects` is finished, tested and unused, carrying `#[allow(dead_code)]` (`src/facequality.rs:110`, `:140`) with a note that it is the escape hatch if the landmark pass proves too slow or noisy. `segmentation`'s `Mask::at`, `Mask::resized` and `coverage` are likewise `#[allow(dead_code)]` (`src/segmentation.rs:57`, `:67`, `:105`), used only by tests and `seg_probe`. Either wire each into a real caller or delete it; per **Subtract Before You Add**, do this before building anything new on these modules (files: src/facequality.rs, src/segmentation.rs)

## D. Efficiency

- [ ] Task 10: No derived signal survives a quit. `ImageRecord` persists only `rating`, `label`, `adjustments`, `touchups` and `rotation` (`src/catalog.rs:27-40`), so `sharpness`, `phashes`, `capture_times`, `face_quality` and `feature_distances` are recomputed from scratch every session. Combined with Vision decoding each file itself at full resolution (`src/vision.rs:4`), that is the main reason the grouping features feel too heavy to run unasked (`src/ui/toolbar.rs:117-120`). Persist them. A scalar or a short tag list fits `ImageRecord`, which is safe to extend because every field is `#[serde(default, skip_serializing_if = ..)]`; anything larger should copy the `ThumbCache` pattern instead (same `.lightphotos/` directory, FNV-1a over mtime-ms and file length with its own version string, `src/thumbnail.rs:335-342`, temp-then-rename, a `sweep_orphans` sibling, and a suffix that is neither `.xmp` nor `.thumb.jpg`). Verify by opening a folder twice and confirming the second visit issues no Vision work (files: src/catalog.rs, src/thumbnail.rs, src/app/thumbs.rs)

- [ ] Task 11: Anchor feature prints are recomputed once per member. `VNFeaturePrintObservation` is not `Send`, so each worker computes both prints on one thread (`src/featureprint.rs:120-124`) and returns only the `f32`. A duplicate group of 10 therefore costs 18 feature-print computations instead of 10, each carrying its own full-resolution Vision decode. Fix by having one worker own a group and compute the anchor once, or by caching prints thread-locally per worker keyed by path. Verify with a counter or the hotpath instrumentation from Task 12 (files: src/featureprint.rs, src/app/thumbs.rs)

## E. Observability

- [ ] Task 12: The entire Vision surface is invisible to the repo's own profiler. `grep -c "hotpath::measure"` returns 0 for `vision.rs`, `featureprint.rs`, `facequality.rs`, `segmentation.rs`, `duplicates.rs`, `phash.rs` and `sharpness.rs`, while 40 measure sites exist across decode, thumbnail, catalog, navigation and autotone. No wall-clock number for a Vision call exists anywhere in the repo, which is why every cost claim about it in the source comments is qualitative. `hotpath::measure` compiles to the untouched function body with its features off (`Cargo.toml:72-77`), so this is free in a default build. Add it to `featureprint::compute`, `feature_distance`, `facequality::analyze` and `segmentation::segment`, and add a fifth phase to `src/profile.rs` driving them headlessly the way the existing three are (files: src/featureprint.rs, src/facequality.rs, src/segmentation.rs, src/profile.rs)

## F. Hand-maintained invariants that will break silently

- [ ] Task 13: `ImageRecord::is_empty` is a hand-written five-way conjunction (`src/catalog.rs:73-81`). A future field added to the struct but not to this function makes a cleared photo keep writing a sidecar instead of deleting it. Encode the rule instead of restating it: derive the check, or add a test that constructs `ImageRecord::default()`, sets each field in turn by reflection over the serialized form, and asserts `is_empty()` is false. Per **Encode Lessons in Structure** (files: src/catalog.rs)

- [ ] Task 14: `TOOLBAR_CONTROLS` is a hand-maintained count of toolbar widgets used by the F6 focus cycle (`src/app/nav.rs:717`), already conditional on `SHOW_GROUPING_TOOLS`. Adding or removing a control without updating it silently breaks keyboard navigation. Derive the count from the widget list, or assert it in a test against the number of focusable controls the toolbar actually emits (files: src/app/nav.rs, src/ui/toolbar.rs)

- [ ] Task 15: The duplicate badge's geometry is written twice, once where it is drawn in `thumbnail_cell` and once in the click hit-test, with a comment saying the two must match (`src/ui/grid.rs:360-372`). Extract the badge centre into one function both call (files: src/ui/grid.rs)

- [ ] Task 16: `src/bin/face_probe.rs` and `src/bin/seg_probe.rs` pull their dependencies in through `#[path]` includes because the crate has no lib target, and `src/vision.rs:6-7` is forbidden from importing other crate modules solely to keep those lists short. Adding a `use crate::..` to `facequality.rs` or `segmentation.rs` breaks `cargo build --bins` while `cargo build` and `cargo test` stay green, and CI would not catch it (see Task 19). Add `cargo build --bins` to the verification gate, or give the crate a lib target and delete the `#[path]` lists (files: src/bin/face_probe.rs, src/bin/seg_probe.rs)

## G. Stale documentation

- [ ] Task 17: `docs/PROJECT_LAYOUT.md` describes `src/app.rs` and `src/ui.rs` as single files; both became directories in commit `6a917e3`. It also omits every module added since. Seven files go unmentioned by name (`vision.rs`, `featureprint.rs`, `facequality.rs`, `segmentation.rs`, `duplicates.rs`, `phash.rs`, `i18n.rs`), which is all six of the Vision surface plus localization, and the strings `src/web` and `src/raw` do not appear in the document at all, so both of those trees are undocumented too. `CLAUDE.md` points at this file as the architecture reference, so it is the first thing a new contributor reads (files: docs/PROJECT_LAYOUT.md)

- [ ] Task 18: `ARCHITECTURE.md` does not mention Vision, ML or the Neural Engine anywhere (`grep -cin "vision\|neural\|machine learning"` returns 0), and its per-file platform table omits the Vision modules entirely (files: ARCHITECTURE.md)

- [ ] Task 19: `plans/00-overview.md` contains two claims the tree contradicts. It states "lightphotos has zero ML today (only variance-of-Laplacian blur scoring in `sharpness.rs`)" and mandates "no bundled or trained models", but Vision feature prints, face landmarks and person segmentation are all trained models and all shipped. Separately, its Task 1 records Auto Tone as "**WON'T DO** (user decision, 2026-08-11) ... no plan file, no implementation", while commit `239af85` shipped it and `src/autotone.rs` plus `src/app/autotone.rs` are 1537 lines of it. Both statements steer future planning, so correct them (files: plans/00-overview.md)

- [ ] Task 20: `docs/KEYBOARD_SHORTCUTS.md` omits Auto Tone's `Cmd+U` and `Cmd+Shift+U` (`src/app/keys.rs:196-197`), the only bindings for the app's most prominent smart feature (files: docs/KEYBOARD_SHORTCUTS.md)

## H. CI gaps

- [ ] Task 21: `.github/workflows/release.yml` is the repository's only workflow, it is tag-triggered, and it runs `cargo build --release --bin lightphotos` and nothing else. No `cargo test`, no `cargo clippy`, no `cargo fmt --check`, no `trunk build`, and no `cargo build --bins`. So the 300 `#[test]` functions, the wasm target, and the probe binaries are all verified only by whoever remembers to run them locally. Add a push/PR workflow covering them. Note that `cargo test` invokes real Apple Vision on synthetic JPEGs (`src/featureprint.rs:199`, `src/facequality.rs:338`, `src/segmentation.rs:372`), so the test job needs a macOS runner and is not hermetic across OS versions (files: .github/workflows/)

<!--
Tips:
- Make each task independently verifiable (a test, a build, a specific output).
- Note "files:" per task if you plan to parallelize across worktrees —
  plan-runner uses this to avoid assigning conflicting tasks to different tracks.
- Keep tasks small; one failure shouldn't cascade into the next.
- Note dependencies explicitly ("requires Task 2") if order matters.
-->

---

## Reference

### Verification gate

Per `00-overview.md`, every task verifies with `cargo test && cargo build --release`.
Tasks 1 and 21 additionally need `cargo build --bins`, and Task 1 needs a real
browser run (`RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml`).

### Ordering

Tasks 4, 5 and 6 gate Tasks 7 and 8: the grouping UI cannot earn its place while
its two decision thresholds are admitted guesses. Task 12 should land before
Task 11, so the efficiency win is measured rather than asserted. Everything in
sections F, G and H is independent of everything else and parallelizes freely.

Task 10 is the highest-value item in this file. It is filed under efficiency, but
its real effect is on product viability: the grouping features are gated behind
`SHOW_GROUPING_TOOLS` partly because they are expensive, and they are expensive
mainly because nothing is remembered between sessions.

### Deliberate designs, recorded here so they are not "fixed" by mistake

These look like bugs and are not. Each is documented at its site with a test or
a comment pinning the behaviour.

- **Flat images all hash to 0** (`src/phash.rs:107-115`, with a test pinning it),
  so a folder of blown-out or fully black frames collapses into one duplicate
  group. Known limitation of dHash, not a defect.
- **Duplicate grouping is transitive**, so a~b and b~c groups a with c even when
  a and c are far apart (`src/duplicates.rs:25-28`, with a randomized test against
  a brute-force reference). `refine_by_feature_print` splits members from the
  group anchor only, so a chain can survive refinement.
- **Path-keyed derived caches are never evicted.** `reset_burst_state` and
  `reset_dup_state` clear only the index vectors, and both say so
  (`src/app/accessors.rs:224`, `:318`). The growth is real but deliberate, traded
  for fast folder revisits. Revisit only if Task 10 makes the tradeoff moot.
- **`Err(_)` from segmentation means "no subject".** `src/app/loupe.rs:436-437`
  collapses every error into the empty state on purpose, because "no subject
  found" is the common case. Task 3 narrows this, it does not reverse it.
- **`hotpath` is a non-optional dependency.** The macro must resolve in every
  build and hands the body back unchanged with its features off
  (`Cargo.toml:72-77`). Not dead weight.

### Audit method

Three parallel read-only explorers mapped the ML surface, the attachment seams,
and the platform/network/pacing constraints. Every claim was then re-checked
against the tree directly before being written here, which is why two items
carry an explicit **UNVERIFIED** marker and a stated confirmation step rather
than a confident assertion.

One finding from that audit is deliberately absent from this checklist, because
it is a product decision rather than a defect: the Vision-backed features are
hidden by choice, not missing, and what to do about that belongs in the roadmap.
