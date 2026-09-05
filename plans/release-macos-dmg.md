# Release: macOS .dmg + add Windows x86_64 back

## Context

The `release` workflow builds macOS (Apple Silicon) and Linux (x86_64) on tag
push. Two asks:

- macOS should ship a `.dmg` instead of a `.app` tarball.
- Add the Windows x86_64 target back (the repo builds on Windows; it was
  dropped from the release matrix, not broken).

macOS-side problems in the **Package (macOS)** step of
`.github/workflows/release.yml`:

1. **No `.dmg`.** The step only `tar`s `LightPhotos.app` into
   `lightphotos-macos-arm64.tar.gz`. No `hdiutil` / `create-dmg`. A `.dmg` is
   the expected macOS delivery format (drag-to-Applications).

2. **`target/` folder leaked into the archive.** The old line was
   `tar -czf "$name.tar.gz" "$APP" "target/${{ matrix.target }}/release/lightphotos"`
   — no `-C`, so the standalone CLI binary got stored at its literal nested
   path. Mooted by dropping the tarball entirely (below).

Desired outcome: macOS release ships **only** a `.dmg`. Users who want the CLI
run the binary already inside the bundle at
`LightPhotos.app/Contents/MacOS/lightphotos`. Linux is unchanged (bare-binary
tarball).

## Changes — `.github/workflows/release.yml`

### Replace the `Package (macOS)` step

```yaml
      - name: Package (macOS)
        if: runner.os == 'macOS'
        run: |
          APP="LightPhotos.app"
          mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
          cp Info.plist "$APP/Contents/Info.plist"
          cp "target/${{ matrix.target }}/release/lightphotos" "$APP/Contents/MacOS/lightphotos"
          chmod +x "$APP/Contents/MacOS/lightphotos"

          # .dmg: stage the app + an /Applications symlink for drag-install.
          STAGE="$(mktemp -d)"
          cp -R "$APP" "$STAGE/"
          ln -s /Applications "$STAGE/Applications"
          hdiutil create -volname LightPhotos -srcfolder "$STAGE" \
            -fs HFS+ -format UDZO -ov "${{ matrix.name }}.dmg"
```

Notes:
- `hdiutil` is built into every macOS runner — no extra `uses:` / brew install.
- `UDZO` = zlib-compressed read-only image, the standard distributable format.

### Add Windows x86_64 to the build matrix

```yaml
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            name: lightphotos-windows-x86_64
```

`setup-vendor-rawler.sh` runs under `shell: bash` (git-bash on the runner) —
`curl` / `tar` / `patch` / `mktemp` are all present. `cargo check` for this
target passes locally (no cfg-gate breakage on the non-macOS decode path).

### Add a Windows package step

```yaml
      - name: Package (Windows)
        if: runner.os == 'Windows'
        shell: pwsh
        run: Compress-Archive -Path "target/${{ matrix.target }}/release/lightphotos.exe" -DestinationPath "${{ matrix.name }}.zip"
```

`Compress-Archive` ships with the runner; git-bash has no `zip`.

### Widen the upload path

Each entry emits one of `.dmg` / `.tar.gz` / `.zip`:

```yaml
      - uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.name }}
          path: |
            ${{ matrix.name }}.dmg
            ${{ matrix.name }}.tar.gz
            ${{ matrix.name }}.zip
```

Per matrix entry, one line matches and the rest match nothing;
`actions/upload-artifact@v4` defaults to `if-no-files-found: warn` and succeeds
as long as at least one line matches.

The `release` job needs no change — it already globs `files: dist/*` after
`download-artifact` with `merge-multiple: true`.

### Header comment

Top-of-file comment now says macOS ships a `.dmg`; the CLI binary rides inside
the bundle at `Contents/MacOS/lightphotos`.

## Out of scope / caveats

- **Unsigned / unnotarized.** The `.app` and `.dmg` have no Developer ID
  signature, so Gatekeeper quarantines on first open (right-click → Open, or
  `xattr -dr com.apple.quarantine`). Already true of today's tarball; signing
  needs an Apple Developer cert in secrets — separate task.
- No custom volume icon / background image for the DMG window — plain `hdiutil`.
  `create-dmg` could style it later.
- `scripts/bundle.sh` (local dev bundling) unchanged — never built a dmg,
  not on the release path.
- **"Source code (zip/tar.gz)" on the Release page cannot be removed.** GitHub
  auto-generates those from the tag for every release; there is no API field or
  `softprops/action-gh-release` option to hide them. Left as-is.
- Windows `.exe` is unsigned — SmartScreen will warn on first run.

## Verification

1. **Eyeball the YAML** (small diff; `actionlint` if installed).
2. **Dry-run locally** (this box is Apple Silicon):
   ```sh
   cargo build --release --bin lightphotos --target aarch64-apple-darwin
   # run the step body with matrix.target=aarch64-apple-darwin,
   # matrix.name=lightphotos-macos-arm64
   ```
   Expect `lightphotos-macos-arm64.dmg`.
3. **Inspect the dmg**:
   ```sh
   hdiutil attach lightphotos-macos-arm64.dmg -nobrowse -mountpoint /tmp/lpmnt
   ls /tmp/lpmnt        # LightPhotos.app + Applications symlink
   hdiutil detach /tmp/lpmnt
   ```
4. **Windows**: `cargo check --target x86_64-pc-windows-msvc` locally (done —
   passes). Full link + `Compress-Archive` only runs in CI.
5. **Full CI run**: throwaway tag (e.g. `v0.1.4-rc1`) or `workflow_dispatch`;
   confirm the Release carries `lightphotos-macos-arm64.dmg`,
   `lightphotos-linux-x86_64.tar.gz`, `lightphotos-windows-x86_64.zip`, and the
   `.dmg` mounts on a clean machine.

## Status

Implemented. Verified locally: dmg mounts (app + `/Applications` symlink);
Windows target `cargo check` passes. Not committed. Full Windows build + all
packaging steps not yet CI-verified.
