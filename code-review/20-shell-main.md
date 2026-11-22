# src/shell, main.rs, control_auth.rs, update.rs, lib.rs

## Scope

Every line of these files was read.

| File | Lines |
|---|---|
| src/main.rs | 810 |
| src/lib.rs | 33 |
| src/control_auth.rs | 181 |
| src/update.rs | 409 |
| src/shell/mod.rs | 24 |
| src/shell/control/mod.rs | 195 |
| src/shell/control/tcp.rs | 1039 |
| src/shell/drive.rs | 1179 |
| src/shell/profile.rs | 920 |
| src/shell/menu.rs | 623 |
| src/shell/dialog.rs | 190 |
| src/shell/macos_delegate.rs | 101 |
| src/shell/display.rs | 77 |
| src/shell/window_rect.rs | 180 |
| src/shell/trash.rs | 39 |

Cross-references checked: `src/app/menu.rs`, `src/app/keys.rs`, `src/renderer/mod.rs` (`resize`), `src/develop/image_ops.rs` (`Retouch`), `Cargo.toml` (ureq, ring) and `.github/workflows/ci.yml`.

## Summary

This is careful, well-tested code. The control channel's design is strong. A mutual HMAC-SHA256 handshake keeps the token off the wire. Verification goes through `ring::hmac::verify`, which compares in constant time. Binding is loopback only. Request lines are capped at 4 KiB before sign-in and 1 MiB after. There is an absolute sign-in deadline, a write timeout and a 4-connection cap, and each of these has a test. I found no panic path, no unsafe-without-SAFETY, and no non-atomic file write in non-test code. The update check only reads one JSON tag over TLS and only ever links to a fixed URL. It downloads and executes nothing.

The real problems are mostly at the edges:
- A GPU or window failure at startup is invisible to a user who didn't launch from a terminal.
- `--features hotpath` appears not to compile.
- Window-rect restore can place the window nearly or fully off-screen.
- The TCP server can be kept busy by any local process or browser page.
- A few input-handling gaps: the Space-key latch, auto-repeat, and Finder multi-open.

Findings: 0 Critical, 0 High, 3 Medium, 11 Low, 3 Info.

## Findings

### [SHELL-01] Startup failure is invisible to the user and exits with status 0 — Severity: Medium — Confidence: High
- Location: `src/main.rs:109-115` (`startup_failed`), callers at `:136`, `:148`, `:161`. `main` at `:776-779`.
- Evidence: `startup_failed` does `eprintln!("[lightphotos] can't start: {msg}")` (console `error_1` on wasm) and `event_loop.exit()`. `run_app` then returns `Ok(())`, so `main` falls off the end. Nothing in `src/` uses `rfd::MessageDialog` (grep).
- Impact: A missing or unsupported GPU/adapter, or a failed window creation, is the case CLAUDE.md names ("a missing GPU ... must give an error the user can act on"). Release Windows builds use `windows_subsystem = "windows"`, so stderr goes nowhere unless the app was started from a console. A Finder or Dock launch on macOS also hides stderr. The user double-clicks and nothing happens: no window, no dialog, exit code 0. In the browser, a failed `Renderer::new` goes only to the JS console and leaves a blank canvas. Scripts and supervisors also read this as success.
- Suggested fix:
  - On native, show a blocking `rfd::MessageDialog` (the dialog can be on the main thread here, since the loop is about to exit). Say what failed and what to try, for example update the GPU driver or run with a software adapter.
  - Record the failure in a field of `App`, and have `main` return `ExitCode::FAILURE` after `run_app`.
  - On wasm, write the message into a DOM element.

### [SHELL-02] `--features hotpath` build is broken: `Retouch` is not in scope in `profile.rs` — Severity: Medium — Confidence: High (not compiled, per the no-build rule)
- Location: `src/shell/profile.rs:437` (`Retouch::NONE`); `src/shell/mod.rs:20-21` gates the module on `feature = "hotpath"`.
- Evidence: The only occurrence of `Retouch` in the file is the use at line 437. The `use` list at lines 35-42 does not import it, and `crate::develop` does not re-export it. It lives at `crate::develop::image_ops::Retouch` (`image_ops.rs:288`). Commit d160ada added the use without an import. `.github/workflows/ci.yml` never builds `--features hotpath`, so nothing caught it.
- Impact: `cargo run --release --features hotpath -- --profile ...` and `scripts/lightwatch-up.sh` (which builds `--features hotpath,lightwatch`) fail with an unresolved-name error. The documented profiling workflow in the README is unusable until someone fixes it. Other signatures in this file may have drifted too (`request_bake`, `ExportJob`, `ScoreRequest` look consistent).
- Suggested fix: Add `use crate::develop::image_ops::Retouch;`. Add a CI step, `cargo clippy --features hotpath --bin lightphotos -- -D warnings` (or `cargo check`), so the profiler can't silently rot.

### [SHELL-03] Saved window rect is restored when it overlaps a monitor by one pixel, and its size is never clamped — Severity: Medium — Confidence: Medium
- Location: `src/shell/window_rect.rs:44-46` (`intersects`), `:71-83` (`apply`), `:29-38` (`parse`).
- Evidence: `saved` is accepted if `monitors.iter().any(|m| r.intersects(*m))`, and `intersects` is a strict overlap with no minimum area. `with_inner_size` and `with_position` are applied exactly as saved. `parse` only rejects non-finite or non-positive numbers.
- Impact:
  - After a monitor is unplugged or rearranged, a window whose edge overlaps a remaining monitor by a few pixels is restored almost entirely off-screen. Windows and Linux do not constrain the frame the way macOS does, so the title bar can be unreachable.
  - A window saved on a 5K display (say 5000 pt wide) is restored at full size on a laptop, larger than the screen. `default_size` clamping only applies to the fallback path.
  - A hand-edited or corrupt pref such as `"0 0 1e300 1e300"` is also accepted.
  - On mixed-DPI setups the saved rect is in window-scale logical points but monitors are converted with each monitor's own scale (`of_monitor`), so the overlap test compares different coordinate spaces. That is only wrong for secondary monitors with a different scale.
- Suggested fix: Require that the title-bar strip (for example the top 48 pt of the rect) overlaps a monitor by at least 100x40 pt. Clamp w and h to `MAX_SHARE` of the best-overlapping monitor. Cap w and h at a sane maximum. Compare in physical pixels, or convert with the window's scale. Pull the filter into a pure function that takes `&[Rect]` so it can be unit-tested; today `apply` needs an `ActiveEventLoop`, so none of the restore policy is tested.

### [SHELL-04] Control port can be tied up by any local process or web page; the server never rejects HTTP-looking input — Severity: Low — Confidence: Medium
- Location: `src/shell/control/tcp.rs:45` (`MAX_CONNECTIONS = 4`), `:53`, `:325-362` (`serve`), `:395-411`.
- Evidence: Before sign-in, a bad line gets an error reply and the connection stays open until the 5 s sign-in deadline. A browser `fetch('http://127.0.0.1:<port>/', {method:'POST', mode:'no-cors', ...})` is a line-oriented TCP peer from the server's point of view. It gets the request line and each header line answered with `bad_json` and holds a slot for 5 s. Nothing authenticates the caller before it takes a slot.
- Impact: Commands cannot be injected, because every op needs the MAC and the response is not valid HTTP. This is not a CSRF or DNS-rebinding hole. But a page or a local process that retries 4 connections every 5 s keeps the legitimate agent on `too_many_connections`. The port is predictable for users who pass a fixed `--control N`.
- Suggested fix:
  - Close at once on the first non-JSON line before sign-in. Better: close if the first bytes look like `GET `, `POST `, `OPTIONS ` or `CONNECT `, or are not `{`.
  - Shorten the pre-`hello` deadline (for example 1 s to the first line).
  - Optionally reserve one slot for the client that signs in fastest, or evict the oldest unauthenticated connection when full.

### [SHELL-05] The server hands any local peer a keyed proof before sign-in, which allows offline guessing of a weak user-chosen token — Severity: Low — Confidence: Medium
- Location: `src/control_auth.rs:8-12` (design note); `src/shell/control/tcp.rs:174-182` (`hello` reply); `:118-125` (`check_token`).
- Evidence: `hello` returns `HMAC(token, "lightphotos-control/server:" + client_nonce + ":" + server_nonce)` to an unauthenticated peer. The client picks its own nonce, so this is a keyed oracle. `LIGHTPHOTOS_CONTROL_TOKEN` only has to be 16 characters long, with no entropy check. The generated token (128 bits) is safe.
- Impact: With a user-chosen token such as `passwordpassword`, a local attacker gets (nonce_c, nonce_s, proof) in one connection and brute-forces the token offline. Online guessing is limited to one try per connection, but offline guessing is not.
- Suggested fix: Document that a user-supplied token must be high entropy (the same as the generated one). Optionally require 32 or more hex characters, or reject tokens that are all one character class. Keep the server-proves-first design, which is a deliberate trade-off against port squatters.

### [SHELL-06] `space_down` can latch on, because release events are dropped when egui consumes them or focus changes — Severity: Low — Confidence: Medium
- Location: `src/main.rs:461-496` (`key_input`); no `WindowEvent::Focused` arm anywhere in `src/`. `space_down` is read at `:338`, `src/ui/loupe.rs:1605`.
- Evidence: A Space press sets `space_down`. The release clears it only if `egui_consumed` is false (`:462-477` returns early otherwise). If the release is consumed (a text field took focus in between) or never arrives (Alt-Tab, Cmd+Tab or a system shortcut with Space held; macOS sends no synthetic release), `space_down` stays true.
- Impact: In the Loupe, every plain drag then pans, even on an image that fits, until Space is tapped again. The `!self.space_down` guard also swallows the next Space press-action. It self-heals on the next clean Space tap, so it is cosmetic.
- Suggested fix: Handle `WindowEvent::Focused(false)` by clearing `space_down` and `dragging`. Apply the release of Space before the `egui_consumed` early-return, so releases are never dropped.

### [SHELL-07] Key auto-repeat is not distinguished from a fresh press — Severity: Low — Confidence: Medium
- Location: `src/main.rs:241-245`.
- Evidence: `self.key_input(code, event.state, consumed)` never reads `event.repeat`. `handle_key` runs for every repeat. Grep finds no `repeat` handling in `src/app` or `main.rs`.
- Impact: Holding a toggle chord flickers or cycles it. Backquote is `TogglePick`, `Z` cycles zoom, and Cmd+G groups. Holding Cmd+[ rotates continuously. Holding Delete is guarded by the confirm modal, so that one is safe. Navigation and Undo benefit from repeat, so the filter has to be selective.
- Suggested fix: Ignore `event.repeat` except for an allow-list of navigation, undo/redo and slider-nudge keys.

### [SHELL-08] `panel_scale` queries every display mode on each `Resized` event — Severity: Low — Confidence: Low
- Location: `src/main.rs:262-268`, `src/shell/display.rs:14-45`.
- Evidence: `Resized` fires every frame of a live resize drag. Each call runs `CGDisplayCopyDisplayMode` plus `CGDisplayCopyAllDisplayModes` (allocating the whole mode list), then filters it. The `Moved` handler (`:286-294`) avoids this with a monitor-change check, but `Resized` does not.
- Impact: Wasted CoreGraphics work while the user drags the window edge. I did not measure it; the cost is likely sub-millisecond to a few ms.
- Suggested fix: Cache by (`native_id`, current mode id or pixel width) and recompute only when the monitor or current mode changes, as `Moved` already does.

### [SHELL-09] Menu enabled-state is recomputed on every event-loop wake — Severity: Low — Confidence: Low
- Location: `src/main.rs:417-418` (`shell::menu::refresh(self)`), `src/shell/menu.rs:458-472`, `src/app/menu.rs:14-60`.
- Evidence: `refresh` loops over about 38 items on every `about_to_wait`, which runs at 16 ms intervals while thumbnails or decodes are pending. Several items call `self.check(&cmd)`, `selected_cells()` (allocates a Vec) and `group_at()` per selected cell.
- Impact: With Cmd+A on a large folder, `GroupSelected`, `Ungroup` and `DeleteGroup` each walk the selection on every wake, which adds avoidable main-thread work during the busiest moments. I could not confirm `check` is cheap.
- Suggested fix: Recompute only when a menu-relevant state changed (a dirty flag set by selection, mode, blocker or undo-stack changes), or throttle to about 10 Hz.

### [SHELL-10] Finder opening several files at once opens only the last one — Severity: Low — Confidence: Medium
- Location: `src/shell/macos_delegate.rs:67-71`, `src/main.rs:180-186`.
- Evidence: Each URL sends a separate `UserEvent::OpenFile`. With a renderer, each calls `open_from_user`, so the last one wins. Before the renderer exists, `pending_initial = Some(path)` is overwritten by each.
- Impact: Selecting 5 photos in Finder and choosing Open With LightPhotos shows only one of them, with no feedback about the rest.
- Suggested fix: Batch the URLs of one `openURLs:` call into a single event. Open their common parent folder, or the first photo plus a selection of all of them, or at least tell the user.

### [SHELL-11] `--drive` parser accepts unbounded `size`; script and scratch-dir rough edges — Severity: Low — Confidence: Medium
- Location: `src/shell/drive.rs:354-361`, `:515-516`, `:485-489`, `:404-408` (`scroll`).
- Evidence:
  - `size 4294967295 4294967295` is accepted. `Renderer::resize` clamps the surface to `max_dim` (`renderer/mod.rs:1264`), but `App::win_size` takes the unclamped value. A 16384x16384 offscreen target is about 1 GiB.
  - `config_dir = temp_dir()/lightphotos-drive-<pid>` is predictable and is not cleared at start. A killed earlier run with a recycled pid leaves stale prefs, which contradicts "settings start empty". On shared `/tmp` it is also a symlink target.
  - `type foo#bar` types `foo` silently, and leading or trailing spaces can't be typed.
  - A `run<TAB>{...}` line is not recognized as a JSON line, so any `#` in it truncates the JSON.
  - `scroll 0 1e38` is finite but can drive zoom to infinity.
- Impact: This is a developer tool, but a bad line is supposed to give an error, never a crash. These are bad-input cases without a clean error.
- Suggested fix: Cap `size` (for example 8192) in `parse_step`. Create the scratch dir with `tempfile`-style unique naming, or remove it first. Match the `run` prefix on any whitespace. Bound `scroll`.

### [SHELL-12] `profile.rs` busy-waits can hang, and its phases write into the user's folder — Severity: Low — Confidence: Medium
- Location: `src/shell/profile.rs:576-587`, `:625-634`, `:836-843`; `:306-319`; `:218-225`; `:200-206`.
- Evidence:
  - `while landed < wanted.len()`, `wait_for_thumbs` and the export loop spin forever if an item neither lands nor reports failure. That includes a thumbnail evicted under a small `LIGHTPHOTOS_CACHE_*` limit, or a lost export job.
  - `vision_signals` loads and writes the real folder's signal cache.
  - `LIGHTPHOTOS_PROFILE_COLD=1` deletes the folder's cached thumbnails and signal file.
  - `--profile` with no directory silently falls through and opens the GUI.
  - Scratch dirs use `temp_dir/...-<pid>`.
- Impact: Hangs with no timeout; it touches a user's photo folder. Only built with the `hotpath` feature.
- Suggested fix: Add a deadline to each wait loop that prints and moves on. Profile against a copy, or state the writes in `--help`. Report a missing `--profile` argument as an error.

### [SHELL-13] CLI typos are silently treated as a path — Severity: Low — Confidence: Medium
- Location: `src/main.rs:723-742`, `src/shell/control/tcp.rs:70-80`.
- Evidence: Only `args.first()` is inspected. `-h` and `--help` work only in first position, and `--control=4100` is not recognized (it becomes a path). Any unknown flag falls into `PathBuf::from(s)`, fails `exists()`, and prints "no such path ... opening the folder picker". A second positional argument is ignored.
- Impact: `lightphotos --contol 4100` (a typo) starts the GUI with no control server, and an agent waiting for the "listening" line hangs. A mis-typed `--help` opens a window.
- Suggested fix: Reject any argument starting with `-` that is not recognized (macOS's `-psn_`, `-NSDocumentRevisionsDebugMode` and the like can be allow-listed), accept `--control=PORT`, and error on extra positional arguments.

### [SHELL-14] Windows: "Move to Trash" may delete permanently on removable media — Severity: Low — Confidence: Low
- Location: `src/shell/trash.rs:22-25`.
- Evidence: Non-mac builds call `trash::delete(path)` and treat `Ok` as moved to the Recycle Bin. On Windows, FAT/exFAT camera cards and network shares have no Recycle Bin, so the shell deletes outright. I did not verify the `trash` crate's behavior with its no-UI flags, which is why confidence is low.
- Impact: Culling directly on a card is a core photographer workflow. If deletion is permanent there, the user's expectation, set by the "Move to Trash" label, is wrong and it is not undoable.
- Suggested fix: Verify on a real card. If the delete is permanent, check the drive type (`GetDriveTypeW`, `SHQueryRecycleBinW`) first and show a different confirmation, such as "this drive has no Trash; delete permanently?".

### [SHELL-15] `save_catalog` creates the target directory before the user confirms — Severity: Info — Confidence: High
- Location: `src/shell/dialog.rs:47-50`.
- Evidence: `let _ = std::fs::create_dir_all(&dir);` runs before `save_file()`, and its error is dropped. Cancelling the dialog still leaves the directory. If creation fails, the picker silently opens somewhere else.
- Impact: Cosmetic: an empty `~/Pictures/LightPhotos` appears from a dialog the user cancelled.
- Suggested fix: Create the folder only after a path is returned, or only set the directory if it already exists.

### [SHELL-16] Update check is on by default with no consent step; verified that nothing is downloaded or executed — Severity: Info — Confidence: High
- Location: `src/update.rs:142`, `:188-208`, `:99-117`.
- Evidence: With a stamped build and `auto_check` unset, the first launch contacts `api.github.com` (the pref loads as `is_none_or(|v| v != "off")`). The request goes through ureq 3 with its default rustls and webpki-roots TLS, a 10 s global timeout and a 1 MiB body cap. The tag must parse as strictly `N.N.N` (`Version::parse`, well tested), and the only effect is showing a link to a fixed `https://lightphotos.app/.../downloads`. A failed check does not write `LAST_CHECKED`, so it retries on the next launch only (not per frame). `LIGHTPHOTOS_UPDATE_LATEST` can only force the cue on and is never persisted.
- Impact: Security is fine. This is a privacy or policy question, not a bug.
- Suggested fix: None required. Consider a first-run notice.

### [SHELL-17] After sign-in the control channel has no per-message integrity or encryption — Severity: Info — Confidence: Medium
- Location: `src/control_auth.rs:3-16`, `src/shell/control/tcp.rs:3-26`.
- Evidence: The MAC covers only the two nonces. A process that can relay the handshake to the real app (for example when `--control 0` picks a random port and a hostile local process interposes between the client and the printed port) can then read screenshots and inject commands, since later lines are plain JSON.
- Impact: The stated goal, that a squatter cannot impersonate the app and cannot learn the token, holds. Active relaying needs the attacker to be in the path, which on loopback means already same-user or root. Low practical risk.
- Suggested fix: If relay resistance is wanted, derive a per-connection key from the handshake and MAC each request. Otherwise record the limit in the README.

## Test coverage assessment

- `tcp.rs` is the best-tested part:
  - Handshake: both MAC directions, one hello per connection, a nonce answering once, a wrong MAC closing the connection.
  - Hostile input: before sign-in nothing is parsed, the line caps (exactly at the cap and one over), a raw-token refusal, and a bad line followed by a good one.
  - Resource limits: the sign-in timeout, the idle timeout, write-timeout slot release against non-reading peers, and the connection cap.
  - A full loopback screenshot round trip.
- `control_auth.rs` includes an RFC 4231 vector and a documented openssl line, so third-party clients can match it.
- Missing control tests:
  - A signed-in client that trickles a line (each byte under the idle timeout) to keep a connection alive. The per-read idle timeout resets on every byte; it is bounded only by the 1 MiB cap and needs a token, but is untested.
  - HTTP-looking input.
  - A `hello` after sign-in.
  - Concurrent sign-ins.
  - `ask` expiry (the main thread answering after `Request::expires`).
- `drive.rs` parse tests are good (every form, line numbers, NaN and inf). Gaps:
  - Huge `size`.
  - A BOM or tab-separated `run`.
  - `type` with `#`.
  - The `Driver` itself (needs a GPU; only CI could cover it).
- `update.rs`: the pure logic is well tested. `fetch_latest`, the pref write path and `start_if_due` are skipped under `cfg!(test)`, so the real flow is untested by design.
- `window_rect.rs`: only the parse, intersect and default-size helpers are tested. `apply`'s filter is untestable as written (SHELL-03).
- `menu.rs`: good consistency tests (chord vs accelerator, chord in help overlay, unique IDs). Nothing checks that the menu and the `handle_key` bindings agree beyond the help text.
- `trash.rs`: only the missing-file error case; `dialog.rs`, `macos_delegate.rs` and `profile.rs`: nothing. The hotpath build is not in CI (SHELL-02).
- `main.rs`: `key_input` (the Space latch, SHELL-06) and `pump` have no direct tests. Space handling is only tested indirectly, if at all.

## Things done well / verified-safe areas

- `ring::hmac::verify` gives a constant-time compare. `from_hex` bounds length and rejects non-hex, odd and non-ASCII input. The test covers `é` and `zz`.
- `read_line` never buffers more than `max + 1` bytes (`take` plus `read_until`), and the cap-edge cases are tested. The sign-in deadline is absolute (`ConnReader`), so trickling before sign-in cannot extend it.
- `Slot` is a drop guard, so a failed thread spawn or a panic releases the slot. Reply writes have a timeout, so a client that never reads cannot pin a slot (tested).
- The server binds `Ipv4Addr::LOCALHOST` only. Generated tokens come from `getrandom` (128 bits) and are printed only when generated; a user-supplied token is never echoed.
- `drive.rs`: `shot` names are restricted to `[A-Za-z0-9_-]`, so there is no path traversal. Output goes through `write_atomic`. Coordinates reject NaN and inf. `stroke` is capped at 10,000 points, `wait` at 30 s, and the `idle` step times out with a non-zero exit.
- `unsafe` blocks in `display.rs` and `macos_delegate.rs` all carry SAFETY comments and are wrapped in safe functions. The null check on `urls` comes before the dereference. `dialog.rs` checks `MainThreadMarker` before using `NSOpenPanel`.
- Window-rect restore is guarded against NaN and inf, and it falls back cleanly when no monitors are listed.
- `main.rs` has no unwrap, expect or index on input. The one `unsafe` (the Windows `AttachConsole`) has a SAFETY comment.
- `update.rs`: `Version::parse` is strict (rejects a sign, a suffix, a fourth part or overflow), and `is_due` handles a clock set backwards.

## Open questions for maintainers

1. Is the missing-GPU or window-creation failure path meant to surface only on stderr? Should the app show a dialog and return a non-zero status?
2. Should `--features hotpath` be in CI (`cargo check` only)? It is already broken.
3. Is a long-lived control connection expected to survive an unauthenticated browser or other local peer holding the four slots? Should the server close immediately on non-JSON first input?
4. What is the intended minimum entropy of a user-supplied `LIGHTPHOTOS_CONTROL_TOKEN`? 16 characters of any kind is accepted, and the `hello` proof makes weak tokens guessable offline.
5. On Windows, has "Move to Trash" been tried on an SD card, a network share and a file larger than the Recycle Bin limit?
6. On macOS, does the `WinitApplicationDelegate` class name still match after the next winit bump? `install_open_handler` returns `false` with a warning if not, and Finder opens would silently stop working.
7. Is shortcut handling meant to follow the physical key on non-QWERTY layouts? `key_input` matches `PhysicalKey`, while the muda accelerators are character-based, so the two could disagree on Dvorak or AZERTY. I did not verify this.
