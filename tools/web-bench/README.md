# Web grid-scroll benchmark

Measures the browser build's main thread while a folder of edited photos
opens and the grid scrolls: frame gaps from `requestAnimationFrame` and
Long Tasks over 50 ms.

An automated browser cannot drive the native folder picker, so the page
copies photos into the Origin Private File System, writes an edit sidecar for
each, and hands the folder to the app through `window.__lpTestRoot`, which
`web_fs::pick_and_list_folder` checks before opening the picker.

```sh
source scripts/web-env.sh && trunk build --release --config Trunk.toml
ln -sfn /path/to/a/folder/of/jpegs dist/bench
python3 tools/web-bench/serve.py 8801 dist &
```

`grid-scroll.js` wants a folder of JPEGs. `raw-flick.js` wants Sony ARWs:
it opens 120 without edits, flicks the grid, and opens the Loupe mid-flick,
timing each until the screen stops changing. `export-check.js` exports three
ARWs and reports the JPEGs that land in `Exports/`.

Then run a script as a Playwright `run_code` against
`http://127.0.0.1:8801/`. To compare two builds, copy the first `dist/` aside
and serve each on its own port. Each port is its own origin, so each gets its
own OPFS copy. `serve.py` sends COOP/COEP, which the build's shared memory
needs.

`loupe-arrows.js` wants 20 Sony ARWs. It opens the first in the Loupe and
arrows through the strip four times at 60 ms a press, reporting the wasm
heap after each pass and any `unreachable` trap. Before the Loupe lanes were
pruned and capped, the heap reached the 4 GB ceiling within one pass.
