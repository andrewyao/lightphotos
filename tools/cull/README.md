# cull

Three ways to cull a folder of photos through `lightphotos-mcp` in headless
mode. Each gives every photo a 0 to 100 score and turns it into a reject flag
or a 2-star rating with the same table:

| Score | Result |
|---|---|
| below 40 | rejected |
| 40 to 59 | 2 stars |
| 60 and up | left as it is |

| | Who scores | Leaves the machine | Runs on | 85 RAW files |
|---|---|---|---|---|
| App score | LightPhotos (`photo.score`) | nothing | macOS | about 17 s |
| Local model | a vision model in Ollama | nothing | Linux, Windows (macOS with `--use-model`) | tens of minutes |
| Claude Code | Claude | every render, to Anthropic | anywhere | depends on the session |

Frames already rejected are left alone. The photo files are never touched;
the ratings and flags land in the folder's sidecars, as if you had set them in
the app. Close the folder in LightPhotos first: headless writes are refused
while the app has it open.

All three need a build of the server:

```sh
cargo build --release --bin lightphotos-mcp
```

## App score (macOS)

`cull.py` needs Python 3 and nothing else. It scores every photo that has no
current score, the score the app's Score All gives (focus, clipping,
darkness, noise and closed eyes on top of Apple's on-device aesthetics model,
about 0.2 s a RAW file), then applies the table.

```sh
python3 tools/cull/cull.py \
  --mcp target/release/lightphotos-mcp \
  --folder /path/to/your/shoot
```

## Local model (Linux and Windows)

The app score needs Apple's Vision framework, so on Linux and Windows
`cull.py` asks a vision model for the score instead. The model runs in
Ollama on 127.0.0.1, so nothing leaves the machine; it is much slower.

1. Install Ollama from <https://ollama.com/download> (on macOS, also
   `brew install ollama`).
2. Download a vision model. `qwen3-vl:32b-instruct` (about 21 GB, 32 GB of
   memory or more) is the default. `qwen3-vl:30b-a3b-instruct` is several
   times faster but scores almost every photo the same.

   ```sh
   ollama pull qwen3-vl:32b-instruct
   ```

3. Start an Ollama server with its cloud features off, on its own port. Its
   log says `Ollama cloud disabled: true`.

   ```sh
   OLLAMA_HOST=127.0.0.1:11435 OLLAMA_NO_CLOUD=1 ollama serve
   ```

4. Run it:

   ```sh
   python3 tools/cull/cull.py \
     --mcp target/release/lightphotos-mcp \
     --folder /path/to/your/shoot \
     --ollama http://127.0.0.1:11435 \
     --shoot "portrait shoot" \
     --log cull-log.jsonl
   ```

The model sees four consecutive frames at 1024 px per request and scores
each, comparing them with each other. `--shoot` says what the folder is, for
the prompt; `--review-size` sets the render size; `--log` keeps every prompt
and reply. Add `--use-model` to do this on macOS instead of the app score.

## Claude Code

See [claude.md](claude.md): Claude renders and scores the photos itself
through the MCP tools. Every render is uploaded to Anthropic.
