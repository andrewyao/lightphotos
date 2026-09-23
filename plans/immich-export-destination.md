# Plan — Inline export form, with local and Immich destinations

Work through these top to bottom. Check a box only after verification passes.
The gate for every task is `cargo test && cargo build --release && cargo build --bins`,
plus what the task names. `--bins` matters here because `src/bin/face_probe.rs`
and `src/bin/seg_probe.rs` pull modules in through `#[path]`, so a moved helper
can break them while `cargo build` and `cargo test` stay green.

- [x] Task 1: `ExportSettings` and output size in the pipeline. `ExportJob` gains `size: ExportSize`; after `bake_edited`, an area-average `downscale_rgba` shrinks the baked pixels to the requested long edge (never upscales). Headless only, no UI. Tests: a 4000×3000 bake at `LongEdge(2048)` comes out 2048×1536, a crop still yields the requested long edge, `Full` is byte-identical to today (files: src/export.rs, src/image_ops.rs, src/app/export.rs)
- [x] Task 2: `ExportDest` / `ExportLanding`, `Folder` the only variant in use. `dest: PathBuf` becomes `dest: ExportDest`, `ExportOutcome.result` becomes `Result<ExportLanding, String>`. Pure refactor; one local export by hand to confirm nothing moved (files: src/export.rs, src/app/export.rs, src/app/web.rs)
- [x] Task 3: The inline export form, local destination only. `BulkKind::Export` and its confirm string go away; the toolbar button and `X` set `App.export_form = Some(..)`, which draws a right-hand panel in place of Develop (Loupe) or beside the grid (Grid). Folder row: `<folder>/Exports` by default, a Choose… button on native, fixed to `Exports/` on web. Size row. Export / Cancel. Last-used settings round-trip through `prefs`. Verified by driving the app: open, change size, export, check the files' pixel dimensions with `sips -g pixelWidth -g pixelHeight` (files: src/ui/export_panel.rs (new), src/ui/mod.rs, src/ui/toolbar.rs, src/app/export.rs, src/app/keys.rs, src/app/catalog.rs, src/app/mod.rs, src/i18n.rs)
- [x] Task 4: `src/immich.rs`, the client, native only. Discovery through `/.well-known/immich`, credential check through `GET /api/users/me`, upload, rating, albums. Unit tests with no network: multipart body against a fixed boundary, parsing of `created` and `duplicate`, discovery fallback to `/api`. Exercised against a real server through a throwaway `[[bin]]` probe; the Linux and Windows CI jobs build `--bin lightphotos`, so the probe must not be the only thing that typechecks the client (files: src/immich.rs (new), Cargo.toml, src/main.rs)
- [x] Task 5: Credentials and the Immich section of the form. Server URL in `prefs`, API key in the macOS Keychain (`0600` file elsewhere), Connect runs `users/me` off-thread and shows who the key belongs to. Verified by a key surviving an app restart and `security find-generic-password -s app.lightphotos.immich` showing the item (requires Task 3, Task 4) (files: src/immich_auth.rs (new), src/ui/export_panel.rs, src/app/export.rs, src/main.rs, src/i18n.rs, Cargo.toml)
- [x] Task 6: The `Immich` variant, one photo. Bake to a staging file, upload, delete the staging file on every path. A failed upload leaves no staging file, covered by a unit test. End to end: one photo lands in the Immich web UI at the chosen size (requires Task 2, Task 5) (files: src/export.rs, src/app/export.rs)
- [ ] Task 7: Rating and album. *Rating shipped with the MVP (set right after upload, skipped for unrated because Immich v3 rejects 0); the album picker and batch add remain.* Per-asset `set_rating` in the worker; one `add_to_album` per batch on a one-shot thread whose channel `main.rs` drains and feeds into the `WaitUntil` computation. The toast names the upload phase (files: src/export.rs, src/app/export.rs, src/main.rs, src/i18n.rs)
- [ ] Task 8: Share the backoff. Move `retry_backoff` out of `src/app/web.rs` into `src/backoff.rs` with a native jitter source from `SystemTime` nanos, migrate the wasm callers, delete the private copy, wire upload retries onto it (files: src/backoff.rs (new), src/app/web.rs, src/immich.rs, src/main.rs)
- [ ] Task 9: **Human acceptance.** Against a local Immich from its Docker compose file, and against Gumnut at `https://immich.gumnut.ai` with a Gumnut API key:
  - Export three rated photos at 2048 px into a new album. Assets exist, long edge is 2048, star ratings match, the album holds exactly those three.
  - Re-run the same export. The toast reports duplicates, no second copy appears, the album is unchanged.
  - Wrong key, unreachable host, and a host killed mid-batch each surface in the form or toast and leave no staging files.
  - A local export to a custom folder at Full size matches today's output byte for byte.
  - `RUSTFLAGS="--cfg=web_sys_unstable_apis" trunk build --release --config Trunk.toml` succeeds, and the web form offers Local only.

---

## Reference

### What changes for the user

Today, Export (toolbar button in the Grid, `X` in either view) pops a modal that
says "Export N photos?" and writes full-resolution JPEGs to `<folder>/Exports/`.
There is nothing to choose.

After this plan, Export opens a form on the right-hand side of the window. The
photos stay visible, so the user sees what they are exporting while they choose
where it goes and how big it is. The form has two destinations:

- **Local.** A folder (default `<current folder>/Exports`, or any folder picked
  with Choose…) and an output size.
- **Immich.** A server URL, an API key, an optional album, and the same output
  size. Any server that speaks the Immich API works, including Gumnut.

The fast path stays fast. The form remembers its last settings, so `X` then
Enter exports the way the previous batch did.

### Behaviour of the form

- **Open.** The toolbar Export button or `X`. In the Loupe it takes the Develop
  panel's slot; closing it brings Develop back. In the Grid it is a new right
  panel. It exports the selection as it stands when Export is pressed, and its
  heading shows the count ("Export 12 photos"), so changing the selection while
  the form is open does what the user sees.
- **Run.** The Export button, or Enter. The form closes and the existing
  progress toast takes over, as it does today.
- **Close.** Cancel, Esc, or `X` again. No side effects.
- **Guards.** The three that `start_export` already has (nothing selected, a
  batch already running, catalog still loading) disable the Export button and
  say why under it, instead of surfacing as a toast after the click.
- **Keys.** While the form has a focused text field, `handle_key` must not
  treat typed letters as shortcuts. The presets name prompt already needed this
  guard (plan-a-presets Task 6); reuse it.

### Output size

```rust
pub enum ExportSize {
    Full,
    /// Longest side of the baked (cropped, rotated) image, in pixels.
    LongEdge(NonZeroU32),
}
```

The form offers Full, 4096, 2048 and 1024. A Custom field is left for later. The limit applies
to the *output*, after crop and rotation, which is what a user means by
"2048 px". So the worker decodes at full resolution as today, bakes, then
downsamples.

Downsampling uses a new area-average (box) filter in `image_ops.rs`. The
existing `resample_bilinear_u8` aliases badly at the 3-6× ratios a 24 MP photo
sees going to 2048, and `downsample_linear` is a strided nearest-pixel sampler
built for histograms. One CPU function keeps local and Immich output identical
on every platform, and the web build gets the feature for free because
`bake_jpeg` is the same code. Decoding smaller up front via `decode(max_px)`
would be faster but the decode bound applies before the crop, so a cropped photo
would come out under the requested size.

### Settings shape

```rust
pub struct ExportSettings {
    pub target: ExportTarget,
    pub size: ExportSize,
}

pub enum ExportTarget {
    Folder(FolderChoice),
    #[cfg(not(target_arch = "wasm32"))]
    Immich { album: AlbumChoice },
}

pub enum FolderChoice {
    /// `<current folder>/Exports`, resolved at export time.
    ExportsSubfolder,
    Custom(PathBuf),
}

pub enum AlbumChoice {
    None,
    Existing { id: String, name: String },
    New(String),
}
```

`FolderChoice::ExportsSubfolder` stays relative on purpose. A user who exports
from three folders in a row expects each batch in that folder's own `Exports/`,
not all three in the first one. The Immich server URL and key are not part of
`ExportSettings`; they belong to the install, not to a batch (see Credentials).

`App.export_form: Option<ExportSettings>` is the whole UI state. `Some` means
the panel is showing and holds the in-progress edits; the draw function reads it
and pushes `UiAction::SetExportSettings` on change, the way the develop sliders
push `SetAdjustments`. `prefs` stores the last run's settings as JSON under
`export_settings`, so a stored `Custom` folder that no longer exists falls back
to `ExportsSubfolder` on load rather than failing at export time.

### Pipeline shape

`ExportJob.dest` is a `PathBuf` today (`src/export.rs:72-84`). That is the
assumption to remove, so a worker's result says what it produced rather than
always naming a file.

```rust
pub enum ExportDest {
    /// Collision-free path from `paths::jpg_export_target`.
    Folder(PathBuf),
    /// Bake to `staging`, upload, then delete `staging`.
    Immich { server: Arc<ImmichServer>, staging: PathBuf, filename: String, stars: Option<u8> },
}

pub enum ExportLanding {
    File(PathBuf),
    Asset { id: String, duplicate: bool },
}

pub struct ExportOutcome {
    pub src: PathBuf,
    pub result: Result<ExportLanding, String>,
}
```

Collision-free naming via `paths::jpg_export_target` already works for any
directory, so a custom folder needs no new naming logic. The one-batch-at-a-time
guard in `start_export` still protects it.

### Why a staging file rather than JPEG bytes in memory

On macOS the only compiled JPEG encoder writes to a path through
`CGImageDestination` (`src/image_encode.rs:24`). The in-memory
`encode_jpeg_to_vec` is mozjpeg, gated to non-mac builds plus `raw-probe`
(`src/image_encode.rs:70-73`). So: bake to a temp file with the existing encoder,
read it back, upload, delete. The alternative, new
`CGImageDestinationCreateWithData` FFI, buys nothing a user can see. An uploaded
JPEG stays byte-identical to the one a local export writes.

### What the Immich API offers

Checked against the published OpenAPI spec
([`open-api/immich-openapi-specs.json`](https://github.com/immich-app/immich/blob/main/open-api/immich-openapi-specs.json),
spec version 3.2.0).

- `GET /.well-known/immich` returns `{"api":{"endpoint":"/api"}}`. This is how
  Immich's own apps accept a bare server URL. The client fetches it once on
  Connect and falls back to `/api` if it 404s, so the user can paste either
  `https://immich.example.com` or `https://immich.example.com/api`.
- `POST /api/assets`, `multipart/form-data`. Required: `assetData`,
  `fileCreatedAt`, `fileModifiedAt`. Optional: `filename`, `isFavorite`,
  `visibility`, `sidecarData`. Auth via `x-api-key`. Response
  `{ id, status: "created" | "duplicate" }`.
- `PUT /api/assets/{id}` sets `rating`.
- `GET /api/albums`, `POST /api/albums`, `PUT /api/albums/{id}/assets` with
  `{ ids: [...] }`.
- `GET /api/users/me` is the credential check. It returns the key owner's name
  and email, which the form shows as "Connected as …".

Not usable: Immich has no generic blob store, `sidecarData` only understands
real XMP (LightPhotos' sidecars are JSON under a `.xmp` extension,
`src/catalog.rs:3-27`), and Immich generates its own thumbnails. So Immich is a
destination, never a store for LightPhotos' own data.

### Gumnut

[Gumnut](https://gumnut.ai) runs an Immich compatibility layer
([docs](https://docs.gumnut.ai/guides/immich/compatibility),
[source](https://github.com/gumnut-ai/immich-adapter)) that targets the Immich
v3.2.0 API, the same version as above. What matters for this plan:

- **The server URL is `https://immich.gumnut.ai`, not `gumnut.ai`.** The
  adapter serves `/.well-known/immich`, so discovery handles the `/api` suffix.
  The form's URL field should use `https://immich.gumnut.ai` as its placeholder
  example, because a user who types `gumnut.ai` reaches the marketing site.
- **Auth.** A Gumnut API key sent as `x-api-key`, the same header Immich uses.
- **Why `users/me` and not `server/about`.** The adapter's middleware forwards
  an API key without validating it, and its `server/about` handler never calls
  the backend (`routers/api/server.py`), so `server/about` answers 200 for any
  key. `users/me` has to resolve the user, so a wrong key fails at Connect
  instead of on the first upload.
- **Upload, albums, ratings, and exact-file dedupe on upload** are listed as
  supported.
- **Ratings and favorites share one scale on Gumnut.** A 5-star export shows as
  a favorite there. Nothing to do about it; worth knowing when checking Task 9.

### Idempotency

Immich (and Gumnut) dedupe on checksum and answer a repeat upload with
`status: "duplicate"` plus the existing asset id. An interrupted batch re-run
converges, and the album add still receives the right ids. So no client-side
checksum, no `x-immich-checksum`, no `bulk-upload-check`.

### `src/immich.rs`

Pure client, no `App` dependency.

```rust
pub struct ImmichServer { api_base: String, key: String }
pub struct Account { pub name: String, pub email: String }
pub struct UploadedAsset { pub id: String, pub duplicate: bool }
pub struct Album { pub id: String, pub name: String }

impl ImmichServer {
    /// Resolve `/.well-known/immich`, then check the key with `users/me`.
    pub fn connect(url: &str, key: String) -> Result<(ImmichServer, Account), String>;
    pub fn upload(&self, jpeg: &[u8], filename: &str,
                  created: SystemTime, modified: SystemTime) -> Result<UploadedAsset, String>;
    pub fn set_rating(&self, id: &str, stars: u8) -> Result<(), String>;
    pub fn albums(&self) -> Result<Vec<Album>, String>;
    pub fn create_album(&self, name: &str) -> Result<Album, String>;
    pub fn add_to_album(&self, album_id: &str, ids: &[String]) -> Result<(), String>;
}
```

`ureq = "3"` under `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]`.
Blocking suits the export pool, which is already blocking OS threads, and keeps
tokio out. The multipart body is hand-built (four fields, about thirty lines),
as small utilities are elsewhere in the repo (`src/hash.rs`).

### Credentials

Server URL in `prefs` under `immich_server`. API key in the macOS Keychain via
`security-framework`'s `passwords` module (three calls, where raw `objc2-security`
would mean hand-building CFDictionary queries), generic password, service `app.lightphotos.immich`,
account set to the server URL. Linux and Windows fall back to a `0600` file in
the config directory, and the form says so under the key field.

The Immich section of the form has two states. Disconnected shows URL and key
fields and a Connect button. Connected shows "Connected as <name> · <host>",
a Disconnect link, and the album picker. Connect runs off the UI thread; its
result returns on an `mpsc` drained in `main.rs` and feeds the `WaitUntil`
computation at `src/main.rs:398-415`, like every other in-flight result.

### Wiring

- Rating is per asset, so the worker calls `set_rating` right after `upload`.
- The album add is one call per batch. When `on_export_outcomes` drains the
  last outcome, a one-shot thread adds the collected ids (precedent:
  `src/app/catalog.rs:25`). An `AlbumChoice::New` is created at the start of
  the batch, not the end, so a name clash fails before any upload.
- The toast says "Uploading 3/12" rather than "Exporting 3/12" for an Immich
  batch. A network batch is slower, and a toast that doesn't mention the
  network reads as a hang.

### Web

The form ships on the web build with Local only. The folder is fixed to
`Exports/` because the web export writes through the folder handle the user
already granted (`web_export_fs::WebFs`); picking a second directory is a
separate permission flow and not needed to prove this. Output size works on
web because `bake_jpeg` is shared. Immich stays native-only: the Immich server
sends no CORS headers
([discussion #19175](https://github.com/immich-app/immich/discussions/19175)),
and `Trunk.toml`'s `Cross-Origin-Embedder-Policy: require-corp` adds a second
barrier.

### i18n

Every new string goes through `src/i18n.rs`. Each language is a struct, so a
missing string fails to compile, and a test scans UI source for English
literals passed to widget calls (`src/i18n.rs:957-1000`). `confirm_export`
goes away with the modal.

### Out of scope

- Immich as a photo *source*. It lands on the assumption that a photo is a
  filesystem path, which runs through `navigation.rs`, `loader.rs`,
  `thumbnail.rs` and `catalog.rs`.
- JPEG quality, format choice (HEIC, TIFF), metadata stripping, watermarks.
  The size row shows where they would go; none is asked for.
- A second remote target. `ExportTarget` makes one cheap to add later.
- A web directory picker for Local.
