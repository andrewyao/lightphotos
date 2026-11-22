# Export, geotag and sidecar (src/export, src/geotag, src/sidecar.rs, src/sidecar/)

## Scope

All lines read. Line counts:

| File | Lines |
|---|---|
| `src/export/mod.rs` | 1637 |
| `src/export/metadata.rs` | 743 |
| `src/export/immich.rs` | 578 |
| `src/geotag/mod.rs` | 445 |
| `src/geotag/gpx.rs` | 164 |
| `src/geotag/zone.rs` | 111 |
| `src/geotag/tests.rs` | 558 |
| `src/sidecar.rs` | 1487 |
| `src/sidecar/lock.rs` | 614 |
| Total | 6337 |

Cross-references checked outside the unit:
- `paths.rs`: `write_atomic*`, `export_target`, `read_capped`.
- `persist/folder_store/writeback.rs`: patch apply and `replace_unreadable`.
- `app/export.rs`: `start_export` and `export_blocker`.
- `decode/image_encode.rs`: `Embedded`, `MAX_JPEG_EXIF`.
- `develop/image_ops.rs`: `fit_long_edge`.
- `kamadak-exif` 0.6.1: the reader and `experimental::Writer`.

## Summary

The code is careful and defensive overall.
- The non-test code has no panicking constructs and uses `get()` and checked or saturating math on input-derived numbers.
- Atomic writes, FIFO and symlink guards, and size caps are present and tested.
- The GPX parse and clock-model code is bounded against hostile offsets.
- The XMP-style record keeps unknown fields and unknown mask kinds through a rewrite.

The biggest risks are data-integrity edges in the sidecar record:
- `write_record` never checks the size it produces against `MAX_SIDECAR_BYTES`, so it can write a file it will refuse to read back.
- Fills this build can't parse are dropped on load, so the next write destroys them.
- Export and metadata have only small robustness gaps: a name-length miscalculation, a Latin-1 copyright becoming U+FFFD, and no final fallback for an oversize EXIF block.
- An unbounded `cameraClock` count in a hostile GPX can hang geotagging.

No Critical or High findings.

| Severity | Count |
|---|---|
| Medium | 2 |
| Low | 11 |
| Info | 6 |

## Findings

### [EXP-01] `write_record` can write a sidecar larger than the limit `read` enforces — Severity: Medium — Confidence: Medium
- Location: `src/sidecar.rs:661-668` (write), `:541-546` (read limit); `UnknownFields` at `:149`, `MaskList` foreign entries at `:296`.
- Evidence: `write_record` does `serde_json::to_vec_pretty(rec)` and hands the bytes to `write_atomic_in_dir` with no `bytes.len() <= MAX_SIDECAR_BYTES` check. `parse_record` refuses anything over `1 << 20`. The test at `:1102` protects only the brush-point case.
- Impact: A sidecar can arrive at roughly 1 MiB: foreign masks, unknown top-level fields from a later build, or a hostile folder. `RawValue` entries are copied verbatim, but this build's own fields are pretty-printed and so grow. Any rating or flag change then writes a file over the limit. The next read returns `ReadError::Unreadable` and the photo shows as unrated. The following write moves that file aside as `.unreadable-N`, which orphans the foreign data. The user sees their edit succeed, then vanish after a reload.
- Suggested fix: Check `bytes.len() > MAX_SIDECAR_BYTES` in `write_record`. If it is over, return an error naming the cause. Or write compact JSON (`to_vec`) when the pretty form is over the limit. Preserved unknown data could also have its own total cap.

### [EXP-02] Fills this build can't parse are dropped on load and lost on the next write — Severity: Medium — Confidence: Medium
- Location: `src/sidecar.rs:268-279` (`readable_fills`); `"fills"` is in `UnknownFields::KNOWN` at `:160`.
- Evidence: Unparseable entries go through `filter_map(... .ok())`. A `fills` that isn't a list becomes an empty `Vec`. `UnknownFields::of` skips `fills` as a known key, so nothing is kept the way masks and `location` are.
- Impact: This contradicts the module's forward-compat design, which keeps unknown masks and an unreadable location. A later build adds a field to `Fill`, a variant ID format, or a larger variants count. This build then reads zero or fewer fills. Any rating change rewrites the sidecar and the later build's generative fills are gone, along with the reference to their pixel files. Entries past `MAX_FILLS` are lost the same way. The `hostile fills` test (`:867`) confirms dropping is intended for hostile data, but nothing distinguishes it from a newer-version fill.
- Suggested fix: Keep unparseable fill entries as raw values, as `MaskList` does with `foreign`. Or keep the whole original `fills` array in `unknown` when anything was dropped, and write it back until fills are edited.

### [EXP-03] Strict enums and fields make the whole record unreadable on a newer value — Severity: Low — Confidence: Medium
- Location: `src/sidecar.rs:43-59` (`Flag`, `ColorLabel`), `:85` (`adjustments`), `:245` (`StoredScore`), `:547` (`from_slice`).
- Evidence: Masks, fills, detections and location tolerate bad input. `flag: "maybe"`, `label: "orange"` or a `score` of a newer shape fail the whole `from_slice`.
- Impact: A folder shared with a later build that adds a flag or label variant produces `Unreadable` for those photos. `replace_unreadable` moves the file aside rather than overwriting, so nothing is destroyed. The photo still loses its rating, edits and unknown fields from this build's point of view until it is edited, at which point they are orphaned in `.unreadable-N`.
- Suggested fix: Make `flag`, `label` and `score` tolerant like `detections`. An unknown value reads as `None` and is preserved through `unknown`.

### [EXP-04] A near-limit stem makes the atomic temp file name exceed 255 bytes — Severity: Low — Confidence: High
- Location: `src/export/mod.rs:184-186` (`MAX_STEM_BYTES = 240`); `src/paths.rs` `write_atomic_then` appends `.<pid>-<n>.tmp`.
- Evidence: The doc says 240 leaves room for a collision suffix and an extension. It doesn't count the temp name. 240 + `-NNNN` (5) + `.jpg` (4) is 249, and the temp suffix adds about 12 to 16 bytes (pid up to 7 digits).
- Impact: A template or source name that produces a stem of about 236 to 240 bytes makes `OpenOptions::open(tmp)` fail with `ENAMETOOLONG`, so that photo fails to export. Sidecars have the same shape: a photo named 252 to 255 bytes gives a sidecar name, or a sidecar temp name, over the limit.
- Suggested fix: Lower `MAX_STEM_BYTES` to about 200. Or have `write_atomic` build the temp name from a short hash or counter and not from `dest`'s full name.

### [EXP-05] Unbounded `cameraClock` entries cause quadratic work and a long hang — Severity: Low — Confidence: Medium
- Location: `src/geotag/mod.rs:269-290` (`clock_periods`), `:223-256` (`resolve`); `src/geotag/gpx.rs:79-83`.
- Evidence: Each in-noise sample runs `samples.sort_unstable()` on a growing `Vec`. A hostile or corrupt GPX (up to 64 MiB, about 700k `<lr:cameraClock>` elements) can sit all within 5 s of one value, which makes O(n²) work. Or it can alternate more than 5 s apart, which makes P periods. `resolve` then does O(P) work per photo, each step with a `covers` binary search.
- Impact: The GPX comes from a user-picked file and the parse is bounded, but one bad file can hang a geotag run for minutes or tens of minutes. If it runs on the UI thread, that is a visible freeze.
- Suggested fix: Cap `camera_clock` entries at parse time (for example 10k, as an error). Compute the median without re-sorting each push, for example `select_nth_unstable` once per period.

### [EXP-06] `probe()` opens the lock file before checking it is a regular file — Severity: Low — Confidence: Medium
- Location: `src/sidecar/lock.rs:354-364`.
- Evidence: `std::fs::File::open(&path)` (read-only) runs before `refuse_unless_same_file`. `acquire` opens read/write, which doesn't block on a FIFO on Linux or macOS, and then checks. `probe` has no such order. The only caller is the headless MCP (`src/bin/lightphotos_mcp/headless.rs:217`).
- Impact: A folder with a FIFO planted at `.lightphotos/.lock` hangs `get_state` forever, because `open(O_RDONLY)` on a FIFO blocks until a writer appears. The rest of the code (`open_regular`, `read_capped`) guards against exactly this.
- Suggested fix: `symlink_metadata` and `is_file()` before opening, as `open_regular` does. Or open with `O_NONBLOCK`.

### [EXP-07] A non-UTF-8 source copyright becomes U+FFFD and is written into EXIF and XMP — Severity: Low — Confidence: Medium
- Location: `src/export/metadata.rs:244-246`, `:353-358` (`copyright_text`), `:398-419` (`xmp`).
- Evidence: `String::from_utf8_lossy(v.first()?)` decodes the source `Copyright`. EXIF ASCII from Windows tools or older cameras is often Latin-1 or cp1252, for example `0xA9` for ©. Lossy decoding turns that into `\u{FFFD}`, and `copyright_line` then writes it out as UTF-8. A copyright set on export (`ExportSettings::copyright`) is unaffected.
- Impact: Exports from sources with a Latin-1 `©` carry a copyright with replacement characters in both the EXIF and the XMP `dc:rights`. The user's own attribution is damaged without notice.
- Suggested fix: When the bytes aren't valid UTF-8, decode as Windows-1252. Or pass the original bytes through unchanged and keep XMP out of that case.

### [EXP-08] An oversized EXIF block has no final fallback; the export fails — Severity: Low — Confidence: Medium
- Location: `src/export/mod.rs:379-385`; `src/decode/image_encode.rs:336, 520`.
- Evidence: When `block` is over `MAX_JPEG_EXIF`, `essential(&fields)` is used. `essential` keeps every `ALWAYS_TAGS` field, which includes unbounded ASCII tags (`Make`, `Model`, `Software`, `LensModel`, `Copyright`, and others). If those alone exceed 64 KB, the JPEG or AVIF encoder returns "EXIF block too large".
- Impact: A malformed or hostile source with a 70 KB `Software` or `Model` string cannot be exported as JPEG or AVIF at all. The test at `:1514` covers only an oversize `UserComment`, which `essential` drops.
- Suggested fix: As a last step, drop the largest ASCII fields, or cap each ASCII value written (say 256 bytes), or retry with date and copyright only. Export without EXIF is better than not exporting.

### [EXP-09] `SourceMeta::read_path` reads an entire TIFF-based RAW into memory, twice for Immich — Severity: Low — Confidence: High
- Location: `src/export/metadata.rs:68-81`; `src/export/mod.rs:743` (second read in `do_export`).
- Evidence: kamadak-exif's `read_from_container` calls `reader.read_to_end(&mut buf)` when the first 4 KB looks like TIFF (`reader.rs:142-143`). That covers ARW, NEF, CR2, DNG, ORF, RW2, TIFF and others. `export_file` reads this once; the Immich branch calls `SourceMeta::read_path(&job.src)` again just for a date.
- Impact: Each export worker transiently holds the full file (50 to 200 MB for a high-end RAW or a big TIFF) in addition to the decode. For Immich batches it is read twice. This is extra I/O and memory pressure against the decode budget that the TIFF16 gate exists to protect.
- Suggested fix: Cap the read (read the first N MB, then fall back to the platform stamp). Return the `SourceMeta` from `export_file` so the Immich branch reuses it.

### [EXP-10] The 16-bit TIFF gate is held across the Immich upload — Severity: Low — Confidence: High
- Location: `src/export/mod.rs:708-709`, `:734-760`.
- Evidence: `_one_at_a_time` lives to the end of `do_export`. The comment explains that the gate guards the decode, bake and encode. The folder write and the upload happen under it too.
- Impact: Immich uploads of TIFF16 are serialized by network time, which can be minutes each. Other TIFF16 exports can't start decoding in parallel.
- Suggested fix: Drop the guard after `export_file` returns. The encoded bytes held afterward are smaller than the working set the gate protects.

### [EXP-11] `parse_utc_offset` accepts up to `24:59` against its doc — Severity: Low — Confidence: High
- Location: `src/geotag/mod.rs:414-435`.
- Evidence: The doc says "within a day either way", but `h <= 24 && m < 60` admits +24:59, which is 25 h. The test rejects `+25:00` only.
- Impact: Negligible in practice, since the value comes from a user field or EXIF and every real offset is within ±14 h. A mistyped offset like `24:30` is accepted and shifts every photo by a day.
- Suggested fix: Accept `h < 24` or `h * 3600 + m * 60 <= 24 * 3600`. A tighter bound of ±14:00 is more useful.

### [EXP-12] `ExportSize::LongEdge(0)` from prefs exports a 2-px image — Severity: Low — Confidence: Medium
- Location: `src/export/mod.rs:796-805` (`decode_px`), `src/develop/image_ops.rs:548` (`fit_long_edge`).
- Evidence: `decode_px(0, _)` returns `ceil(0 / f) + 2 = 2`, so the source is decoded at 2 px on its long edge. `fit_long_edge` with `max_px == 0` returns its input unchanged. The result is a 2-px "full-size" export.
- Impact: Only reachable from a hand-edited or corrupt prefs file, since the UI offers fixed choices. It silently produces a useless export for the whole batch.
- Suggested fix: Treat `LongEdge(0)` as `Full` in `max_px()`. Or reject it when settings load.

### [EXP-13] Foreign mask order drifts when garbage entries precede them — Severity: Low — Confidence: Medium
- Location: `src/sidecar.rs:392-413` (positions recorded as `at` in the input list), `:361-389` (re-merge uses `out.len()`).
- Evidence: A foreign entry records its index in the file as read. Serialization compares that index with the length of the output as built. Dropped entries (garbage, or masks removed by `sanitized_within` or the cap) mean the output indices have shifted. For `[garbage, foreign, known]` the foreign entry (at=1) is emitted after the known mask (output `[known, foreign]`).
- Impact: The comment promises in-place. Mask order may matter to the later build that understands the foreign entry, since masks stack. The effect is rare and affects only foreign data.
- Suggested fix: Record the foreign entry's index among surviving entries, or the count of known masks before it, and not the raw file index.

### [EXP-14] `FolderLock::acquire` creates the target of a dangling symlink — Severity: Low — Confidence: Medium
- Location: `src/sidecar/lock.rs:302-318`.
- Evidence: The lock file is opened with `create(true)` before `refuse_unless_same_file`. If `.lightphotos/.lock` is a dangling symlink, `open` creates its target and only then does the check refuse it. The test `a_planted_link_at_the_lock_file_...` uses an existing target, which is only checked for unchanged contents.
- Impact: A hostile folder can make the app create an empty file at a path of its choosing, with the user's permissions. No data is overwritten (`truncate(false)`).
- Suggested fix: Check `symlink_metadata` before opening. Or open with `O_NOFOLLOW` on Unix.

### [EXP-15] A sidecar `rating` is not clamped to `0..=MAX_RATING` on load — Severity: Low — Confidence: Low
- Location: `src/sidecar.rs:79-80`; `src/persist/folder_store.rs:636`.
- Evidence: `rating: Option<u8>` is read as is. Setters clamp (`folder_store.rs:646`), but a hand-edited or hostile sidecar can hold 200, and `rating()` returns it raw. `Some(0)` also makes a non-empty record, although the setters never write it.
- Impact: Filters, sort and any UI that draws stars from the number may show out-of-range values. I did not trace the UI. The Immich path guards with `(1..=5)`.
- Suggested fix: Clamp or drop in `parse_record`, like `rotation` does with `% 4`.

### [EXP-16] Fills reach past `MAX_SIDECAR_BYTES`; the unknown-field cap isn't enforced — Severity: Info — Confidence: Low
- Location: `src/sidecar.rs:253-260` and `:538-551`.
- Evidence: A sidecar whose single mask or unknown value is 1 MiB parses. Combined with EXP-01, it makes the written file cross the limit. Noted separately because the remedy (a cap on `unknown` and foreign bytes) is independent of EXP-01's write-side check.

### [EXP-17] The measured clock model assumes a clock change happened at the measurement time — Severity: Info — Confidence: High
- Location: `src/geotag/mod.rs:223-256`, `:269-290`.
- Evidence: A period starts at `measuredAt`. A photo whose wall time falls between the real change (for example a DST switch) and the next measurement uses the old offset. It can still land on the track and be located, an hour off, with no warning.
- Impact: Inherent to the model, and it matches `geotag.py` per the module doc. The `Located` result gives no sign that the photo sits near a period boundary.
- Suggested fix: Optional: report `Ambiguous` or add a flag for photos within a tolerance of a period boundary.

### [EXP-18] GPX points at exactly (0, 0) are accepted — Severity: Info — Confidence: Low
- Location: `src/geotag/gpx.rs:149-164`.
- Evidence: A logger with no fix often writes `lat="0" lon="0"`. Such points are valid coordinates, so photos can be tagged near "null island".
- Suggested fix: Skip points at (0, 0), or at least with `hdop` or `sat` absent, if the logs commonly contain them.

### [EXP-19] Immich `iso8601` clamps pre-1970 dates to 1970-01-01 — Severity: Info — Confidence: Medium
- Location: `src/export/immich.rs:388-390`.
- Evidence: `duration_since(UNIX_EPOCH).unwrap_or_default()` turns any earlier `SystemTime` into the epoch. `fileCreatedAt` and `fileModifiedAt` are the fallback for photos without EXIF. The exported file has its own EXIF date, so the effect shows only on photos with none.

### [EXP-20] A few doc comments are misplaced — Severity: Info — Confidence: High
- Location: `src/sidecar/lock.rs:218-226` (two doc paragraphs merged: the `ENOTSUP` text belongs to `is_unsupported`, but sits on `lasting`); `src/export/immich.rs:259-263` (the `normalize_origin` doc is stacked onto `is_plain_http`).
- Impact: Misleading rustdoc only.

### [EXP-21] Other small observations — Severity: Info — Confidence: Medium
- Immich redirects are disabled on purpose to protect the key. A server that redirects http to https then surfaces as "HTTP 301" with no hint to use `https://` (`immich.rs:61-70, 238-241`).
- `NameTemplate::stem` trims `.` after whitespace, so `"a ."` yields `"a "`, a trailing space Windows rejects (`mod.rs:234`).
- An export into a planted `Exports` symlink writes new files wherever it points. Existing files are not overwritten, because `export_target` skips them and `write_atomic` renames.
- The exported GPS block has no `GPSMapDatum`. Readers assume WGS-84, which matches GPX (`metadata.rs:264`).
- A sidecar with `"location": null` is kept in `unknown` and keeps the record non-empty (`sidecar.rs:188-192`).
- A duplicate tag in a malformed source IFD can reach `exif::Writer`, whose docs say duplicates must not be appended (`metadata.rs:230-249`).

## Test coverage assessment

Strong:
- Sidecar record round trips, forward-compat for masks and location, hostile fills, touch-up, mask and brush-point caps, size-limit writing, a FIFO, a symlink loop, and unreadable files.
- Geotag contract fixture, antimeridian, clock model selection, GPX malformed-input cases including entity bombs, and merge ordering.
- Lock behavior, link planting, and key spellings.
- Export format magic, EXIF toggles read back by an independent reader, JPEG size-limit search including skewed guesses, name template, `decode_px` hostile crops, and a panicking runner.
- Immich pure-function tests.

Gaps:
- No test for `write_record` output over `MAX_SIDECAR_BYTES` (EXP-01).
- No test for a newer-build fill, flag or label surviving a rewrite (EXP-02, EXP-03).
- `every_persisted_field_on_its_own_makes_a_record_non_empty` (`sidecar.rs:788`) leaves `flag`, `edited` and `location` unset, so its "every field" claim is not literal. The `debug_assert` in `is_empty` backs it up in debug builds only.
- No test for `cameraClock` volume, long stems and temp names, a non-UTF-8 source copyright, or `parse_utc_offset("+24:59")`.
- `ClockModel::Measured` with several period transitions is covered only by the shared contract fixture (7 cases).
- No test for the `probe()` FIFO case. No export test that exercises `write_atomic` failure.

## Things done well / verified-safe areas

- No `unwrap`, `expect` or indexing on input-derived values in the non-test code. `get()`, `checked_sub` and `saturating_*` are used throughout. `wall_seconds` uses `get(..19)` at a char boundary. `NameTemplate::stem` slices only at `find('{')` positions and truncates at a char boundary.
- The sidecar reader rejects non-regular files before opening and caps reads at limit + 1. Unreadable files are moved aside, not overwritten.
- Lock design: the OS advisory lock with a separate holder file, identity checks against planted links, no lock-file deletion, the holder line cleared on drop only if still ours.
- Export overwrite safety: `export_target` plus the one-batch-at-a-time blocker (`app/export.rs:252-258`) means two batches cannot pick the same names. Writes go through `write_atomic`.
- The GPX parser checks namespaces, finite and in-range coordinates, a timezone-bearing time, and bounded offsets. `MAX_CLOCK_ERROR_S` keeps sums far from overflow. The Immich client never follows redirects with the key, and `api_endpoint` refuses an endpoint on another host.
- `within_limit` terminates by construction (each probe is strictly inside `(floor, over)`), and `u8` arithmetic cannot overflow.
- GPS rounding in `gps_fields` can't produce a minutes value of 60.

## Open questions for maintainers

1. Is it intended that fills a later build writes are lost to this build (EXP-02), or should they travel like masks do?
2. Should the app refuse a write that would produce a sidecar over `MAX_SIDECAR_BYTES`, or is `1 MiB` assumed unreachable?
3. Should exports use the camera's `OffsetTimeOriginal` when the source has it, to avoid relying on a user-chosen zone? `locate` takes only the wall time today. I did not check the caller in `app/geotag.rs`.
4. For macOS exports, is `ColorSpace = 1` always right, given that Display P3 or wide-gamut sources are baked to sRGB? I assumed the bake converts, because `Pixels` is documented as sRGB.
