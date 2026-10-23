# Cull with Claude Code

Claude Code drives `lightphotos-mcp` itself: it renders each photo, looks at
it, gives it a 0 to 100 score, and rejects or rates it with the same table
`cull.py` uses. It needs no script, no Ollama and no local model.

**Every render is uploaded to Anthropic.** Claude sees each photo at 1024 px.
Do not use this on photos of people who have not agreed to that; use
`cull.py`, which keeps everything on your machine.

## Set up

Build the server and add it to Claude Code, pointed at the folder:

```sh
cargo build --release --bin lightphotos-mcp
claude mcp add lightphotos -- "$PWD/target/release/lightphotos-mcp" --folder /path/to/your/shoot
```

Close the folder in LightPhotos first: headless writes are refused while the
app has it open.

## Run

Run the prompt in [claude-prompt.txt](claude-prompt.txt), allowing only the
LightPhotos tools:

```sh
claude -p "$(cat tools/cull/claude-prompt.txt)" --allowedTools mcp__lightphotos
```

Or start `claude` and paste the prompt into the chat.

The ratings and reject flags land in the folder's sidecars, as if you had
set them in the app. To undo, change them in the app.
