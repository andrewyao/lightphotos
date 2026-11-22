# src/bin: probes and the MCP server

## Scope
Every line of these files was read:

| File | Lines |
|---|---|
| src/bin/face_probe.rs | 124 |
| src/bin/score_probe.rs | 238 |
| src/bin/seg_probe.rs | 208 |
| src/bin/sky_probe.rs | 151 |
| src/bin/lightphotos_mcp/main.rs | 61 |
| src/bin/lightphotos_mcp/rpc.rs | 165 |
| src/bin/lightphotos_mcp/tools.rs | 276 |
| src/bin/lightphotos_mcp/attached.rs | 407 |
| src/bin/lightphotos_mcp/headless.rs | 541 |
| Total | 2171 |

For cross-checks I also read `src/control_auth.rs`, the reply format in `src/shell/control/tcp.rs`, `sidecar::{write_record, probe, LockError}`, `export::export_file`, `FillSet::read`, `navigation::read_dir_paths`, and the integration test `tests/lightphotos_mcp.rs` (headings and the assertions near the JSON-RPC edge cases).

## Summary
This code is high quality and unusually defensive. Findings: 0 Critical, 0 High, 1 Medium, 5 Low, 2 Info.

- **Attribute rule:** all five bins carry the required `#![deny(clippy::...)]` block, and none has a non-test `unwrap`, `expect`, `panic` or `unsafe`.
- **JSON-RPC:** input lines are capped at 4 MiB and an oversized line is skipped without being buffered. Notifications and responses get no reply, and JSON-RPC batches are rejected. A panic inside a tool call is caught, so it fails the call and not the server. Stdout carries only protocol replies, and I found no `println!` in the lib code the MCP server reaches.
- **Attached mode:** the HMAC handshake is well designed. The token never crosses the socket, the server must prove it holds the token before the client sends its MAC, reads have a hard deadline, and replies are capped.
- **Headless path handling:** headless mode can only touch an image file directly inside the folder it was opened on. The check canonicalizes the file and refuses symlinks that point out of the folder. A path must be a bare file name or an absolute path in the root, with a recognised image extension. There is no way to read, write or delete an arbitrary path.
- **Biggest risk (Medium):** headless writes have no undo and no backup (BIN-01).
- **Remaining risks (Low):** an unlocked-write fallback, a latent `json!` panic on a non-UTF-8 folder path, `render` silently dropping a generative fill, and a few headless/app listing divergences.

## Findings

### [BIN-01] Headless commands overwrite or delete sidecars with no undo or backup, in bulk  — Severity: Medium — Confidence: Medium
- Location: `src/bin/lightphotos_mcp/headless.rs:86-163`. See also `sidecar::write_record` in `src/sidecar.rs:661`, which deletes the sidecar when the record is empty.
- Evidence: `run` applies one command to every named photo and then calls `sidecar::write_record(path, rec)` per photo. `photo.reset`, `photo.clear_masks` and similar commands drop edits, and a record that ends up empty deletes the sidecar file. In the app the same commands are one undo step (the tool text says so for `photo.develop`). Headless there is no history and no `.bak`.
- Impact / failure scenario: a mistaken or prompt-injected agent call, for example `photo.reset` with `photos` set to every file name from `get_state`, wipes the develop edits, masks, touch-ups and generative fills of the whole folder. The edits cannot be restored from the tool, and the user finds out only afterwards. `AGENT_NOTES` warns that file names are untrusted data, but nothing bounds the damage.
- Suggested fix: pick one of these.
  - Keep one pre-image per batch (for example `.lightphotos/mcp-undo/<ts>.json`) and add a headless `edit.undo` command.
  - Return the previous values in the reply so a client can restore them.
  - Cap the number of photos one destructive call may touch, or require an explicit `confirm` for `photo.reset`.

  At minimum, state in the tool description that headless edits cannot be undone.

### [BIN-02] A lock failure other than `Busy` falls back to writing without the folder lock  — Severity: Low — Confidence: Medium
- Location: `headless.rs:168-192`.
- Evidence: for `LockError::Unavailable`, `Refused` and `Unsupported`, `lock()` prints a log line and returns `Ok(None)`, and `run` then writes anyway. The `LockError::Unavailable` doc in `sidecar.rs:95` says "It can pass".
- Impact / failure scenario: the lock file is the only thing that keeps headless from racing the app, which is the sole "refused while the app has the folder open" guarantee advertised in `instructions()`. A transient failure, such as EMFILE or an I/O blip in this process, turns that guarantee off while the app still has the folder open. Writes are atomic, so nothing is corrupted. But the app's in-memory state and the headless edit can silently overwrite each other (lost update). Only `Unsupported` (a volume with no locks, such as some network shares) is a deliberate degrade.
- Suggested fix: fail on `Unavailable` with a "try again" message, as `Busy` does, or retry within `LOCK_WAIT`. Keep the unlocked write for `Unsupported` only, and say so in the reply `notes` so the agent knows the write was unprotected.

### [BIN-03] `json!` with a `PathBuf` panics for a non-UTF-8 folder path  — Severity: Low — Confidence: Medium
- Location: `headless.rs:211` (`"folder": self.root`). The same shape appears wherever a `Path` or `PathBuf` goes into `json!`.
- Evidence: `serde_json::json!` expands to `to_value(&expr).unwrap()`. `Path`'s `Serialize` returns an error for non-UTF-8 paths. Clippy's `unwrap_used` does not look inside an external macro expansion, so the deny attribute does not catch it.
- Impact / failure scenario: on Linux, starting `--folder` on a directory whose absolute path is not valid UTF-8 makes every `get_state` and `list_photos` call panic. `catch_unwind` in `rpc.rs:151` turns that into "get_state failed unexpectedly". The server survives, but the tool never works. This is practically unreachable on macOS (APFS requires UTF-8).
- Suggested fix: use `self.root.to_string_lossy()` (or `.display().to_string()`) in the JSON, as `file_name()` already does for names.

### [BIN-04] Headless `render` omits a missing or corrupt generative fill without telling the agent  — Severity: Low — Confidence: High
- Location: `headless.rs:233-249`; `develop/fill.rs:405-413` (`FillSet::read` only does `eprintln!` and returns `None` for that fill).
- Evidence: masks are strict (`rec.mask_set(&path)?` fails, and a test pins that). Fills are lenient, and the reply says "{file} with its saved edits...".
- Impact / failure scenario: if a fill's patch file is missing (for example the `.lightphotos` fills folder was not copied along with the photo), the agent is shown a picture without the fill and reports it as "the photo with its saved edits". The only trace is on the server's stderr. The app's own export behaves the same way by design, but an agent has no other view of the log.
- Suggested fix: compare `fills.len()` with `rec.fills.len()` after the read. Add a text note to the render reply such as "N of M generative fills are missing and are not shown", or fail like masks do.

### [BIN-05] Headless `get_state` has its own folder listing, which has diverged from the app's  — Severity: Low — Confidence: High
- Location: `headless.rs:194-224`, versus `navigation.rs:278-302` and `navigation.rs:337-346` (the app's listing).
- Evidence:
  - `state()` keeps `p.is_file() && is_image_extension(p)`, which follows symlinks, and sorts by raw `PathBuf` order (case-sensitive). The app sorts by lowercased name.
  - `photo()` refuses a symlink that resolves outside the folder (`headless.rs:285`, tested at line 518), but `state()` still lists it.
  - `MAX_LISTED` is 2000 here and 5000 in attached `list_photos`.
- Impact / failure scenario: the agent is shown a photo name and every command and `render` on it then answers "no photo ... in <folder>". Photo order differs from the Grid, which matters if the agent says "the 3rd photo". Beyond 2000 files the tail cannot be listed at all, although those files could still be addressed by name.
- Suggested fix: reuse one pub listing function from the lib (sorted the same as the app) and run `photo()`'s containment check on each entry. Align the caps, or add paging (`offset`) to `get_state`.

### [BIN-06] The attached reply cap gives a misleading error and triples memory use  — Severity: Low — Confidence: Medium
- Location: `attached.rs:22`, `attached.rs:245-259`, and `attached.rs:51-56` (`.to_string()` of the parsed result).
- Evidence: `MAX_REPLY` is 64 MiB. A reply line longer than that is cut by `take(MAX_REPLY)` and reported as "sent a reply that was cut off". The line is then held as bytes, parsed to a `Value`, re-serialized to a `String`, and cloned again for the `image`/`text` content item.
- Impact / failure scenario: a reply over 64 MiB (a screenshot of a very large display, or a huge `list`) is blamed on the app's connection, not on size. Peak memory is about 3-4 times the reply. Because `serve` is single-threaded, nothing else is affected.
- Suggested fix: when `line.len() == MAX_REPLY` with no newline, return "the app's reply was larger than 64 MiB". For screenshots, stream the `png_base64` string out of the parsed value without a second copy.

### [BIN-07] Loose JSON-RPC conformance at the edges  — Severity: Info — Confidence: Medium
- Location: `rpc.rs:93-110`, `rpc.rs:23-40`.
- Evidence:
  - `"id": null` is treated as a request and answered with `id: null` (JSON-RPC says null ids should not be used).
  - Object, array and bool ids are echoed back as is.
  - `tools/call` works before `initialize`.
  - The read loop is single-threaded, so `notifications/cancelled` is never seen while a 35-45 s attached call or a long render runs, and a client that closes stdin mid-call is noticed only after the call returns.
- Impact / failure scenario: none that can be exploited. A client that wants to cancel a long call just waits.
- Suggested fix: optional. Reject non-string, non-number ids with -32600. Consider a reader thread feeding a channel if cancellation is wanted.

### [BIN-08] The probes swallow errors and duplicate the extension list  — Severity: Info — Confidence: High
- Location: `score_probe.rs:78` (`read_record(path).unwrap_or_default()`), `score_probe.rs:153` (`vision_signals(...).unwrap_or_default()`), `score_probe.rs:42-45` (`PHOTO_EXTENSIONS`); `seg_probe.rs:163`, `sky_probe.rs:108` (output names); `face_probe.rs:76`.
- Evidence and impact:
  - A corrupt sidecar becomes "no rating, no edits", so that photo drops out of the Spearman set without a warning. On Linux, `vision_signals` returns `Err`, which becomes the default, and the report then says "unavailable (needs macOS 15)", which is misleading.
  - `PHOTO_EXTENSIONS` lacks `gif`, `bmp`, `pef` and `srw`, so it has already diverged from `image_decode::is_image_extension`.
  - `seg_probe` writes `<stem>.mask.jpg` into the current directory, and `sky_probe` writes `<stem>.sky.jpg`. Two inputs with the same stem (`a/IMG.jpg` and `b/IMG.jpg`, or `x.jpg` and `x.png`) silently overwrite each other's outputs. These writes go through `encode_jpeg` and are not atomic (the CLAUDE.md rule does not strictly apply to dev tools).
  - `face_probe` computes `w as f32 / h as f32`, which gives infinity when `pixel_size` reports `h == 0`.
  - `seg_probe` prints errors on stdout.
- Suggested fix: print a warning when the sidecar read fails. Use `is_image_extension`. Add a numeric suffix on a stem collision. These are all low priority.

## Test coverage assessment
- The inline tests are meaningful:
  - `attached.rs` has a slow-drip deadline test, a handshake test asserting the token never crosses the wire, a squatter test showing no MAC is sent, and an old-app test.
  - `headless.rs` has a symlink-escape test, a missing-coverage test, and a detection test that skips cleanly when Vision cannot run.
  - `tools.rs` checks that `--token` is rejected as a flag.
- `tests/lightphotos_mcp.rs` (643 lines) drives the real binary over stdio: parse error, unknown method, bad params, an oversized line followed by a normal request, `../` rejection, all-or-nothing validation, `render` pixel checks, score, a fake attached app, a missing token, and lock waiting.
- Not covered:
  - Non-UTF-8 paths (BIN-03).
  - A missing fill in `render` (BIN-04).
  - The `Unavailable` / `Refused` unlocked fallback (BIN-02).
  - `get_state` against a symlink that points out of the folder (BIN-05).
  - Duplicate or case-variant names in `params.photos`.
  - A partial write failure (the "saved n of m" path).
  - A reply of exactly 64 MiB.
  - On Windows, the symlink test is `cfg(unix)` only.
- The probes have no tests. That is acceptable for manual validation tools. `spearman` and `ranks` in `score_probe.rs` are pure and could be unit tested (ties, constant input).

## Things done well / verified-safe areas
- `photo()` (`headless.rs:263-293`) holds up:
  - The relative form allows exactly one `Normal` component, which rejects `..`, `./x`, and multi-part paths.
  - The absolute form requires its canonicalized parent to equal the canonical root. It uses `file_name()`, which is `None` for `..`.
  - The result is joined with the root, canonicalized again and checked with `starts_with(root)`.
  - The extension must be an image extension, so nothing outside the photos' sidecars can be targeted.
- The scoring path is correct: scoring on non-macOS returns before taking the lock, because it writes nothing. On macOS, scoring runs under the lock, and the sidecar is written only after every record has been edited.
- The control sign-in verifies `proof` against the client's own nonce before sending anything token-dependent. Nonces are length- and hex-checked, and `control_auth::verify` uses a constant-time `ring::hmac::verify`.
- The deadline-wrapped reader bounds a slow-drip peer.
- `release` has no `panic = "abort"`, so the `catch_unwind` in `rpc.rs` works as intended.
- `render` passes `Keep::NONE`, so no GPS data or EXIF reaches the agent even when the sidecar holds a location.
- Hostile input sizes are bounded: `max_size` is clamped to 16-4096 px. `serde_json`'s recursion limit (128) bounds nesting.
- The token is never accepted as a flag, and it is never echoed in errors.

## Open questions for maintainers
1. Is no undo for headless writes intentional (BIN-01)? The tool text implies "use attach if you want the app's safety".
2. Should `Unavailable` (transient lock failure) really fall through to an unlocked write? The comment says "a volume without locks", but `Unavailable` and `Refused` are not that case.
3. Does the listing order or size in `get_state` need to match the app's Grid, so an agent can say "the Nth photo"? If not, a note in the tool text would do.
