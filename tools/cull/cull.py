#!/usr/bin/env python3
"""Score a folder of photos through lightphotos-mcp, then reject or rate each from its score.

    python3 cull.py --mcp /path/to/lightphotos-mcp --folder ~/shoot

The score is 0 to 100. On macOS it is LightPhotos's own (photo.score: focus,
clipping, darkness, noise and blinks on top of Apple's on-device aesthetics
model), which takes seconds and needs nothing else. On Linux and Windows,
where that score does not exist, a vision model served by Ollama on
127.0.0.1 gives it instead, from renders; --use-model does that on macOS too.
Either way nothing leaves the machine. Then:
    below 40   rejected
    40 to 59   2 stars
    60 and up  left as it is

Frames the photographer already rejected are left alone. stdout carries
counts only; with --log, every prompt and reply goes there. Standard library only.
"""
import argparse
import json
import platform
import subprocess
import sys
import time
import urllib.request

# Below 40 is rejected, below 60 gets 2 stars, the rest is left as it is.
SCORE_STARS = ((40, "reject"), (60, 2))
SCORE_BATCH = 10
MODEL_GROUP = 4

MODEL_SCHEMA = {
    "type": "object",
    "properties": {"frames": {"type": "array", "items": {
        "type": "object",
        "properties": {
            "name": {"type": "string"},
            "score": {"type": "integer", "minimum": 0, "maximum": 100},
        },
        "required": ["name", "score"],
    }}},
    "required": ["frames"],
}

MODEL_PROMPT = (
    "You are helping a photographer cull a {shoot}. These {n} images are "
    "consecutive frames, in this order: {names}. Give each a score from 0 to "
    "100 for how likely the photographer is to keep it, using the names "
    "exactly as given. Be critical: missed focus on the subject's nearer eye, "
    "closed eyes, a blurred or awkward moment, and bad exposure score below "
    "40; a usable frame scores 40 to 59; a good one 60 or more. Compare the "
    "frames with each other."
)


class Mcp:
    """lightphotos-mcp over stdio, one JSON object per line."""

    def __init__(self, binary, folder):
        # The server's stderr goes to ours, so its own errors (a missing
        # folder, a bad flag) reach the terminal.
        self.proc = subprocess.Popen(
            [binary, "--folder", folder], stdin=subprocess.PIPE,
            stdout=subprocess.PIPE, text=True)
        self.next_id = 0
        self.calls = {}
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "cull", "version": "1"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        try:
            self.proc.stdin.write(json.dumps(message) + "\n")
            self.proc.stdin.flush()
        except BrokenPipeError:
            self.exited()

    def exited(self):
        code = self.proc.wait()
        sys.exit(f"lightphotos-mcp exited with code {code}; its error is above")

    def request(self, method, params):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        line = self.proc.stdout.readline()
        if not line:
            self.exited()
        reply = json.loads(line)
        if "error" in reply:
            raise RuntimeError(f"{method}: {reply['error']}")
        return reply["result"]

    def call(self, name, arguments):
        """(text, [base64 images]); raises on a tool error."""
        self.calls[name] = self.calls.get(name, 0) + 1
        result = self.request("tools/call", {"name": name, "arguments": arguments})
        texts = [c["text"] for c in result["content"] if c["type"] == "text"]
        if result.get("isError"):
            raise RuntimeError(f"{name}: {' '.join(texts)}")
        images = [c["data"] for c in result["content"] if c["type"] == "image"]
        return "\n".join(texts), images

    def command(self, id, params):
        return self.call("run_command", {"id": id, "params": params})

    def state(self):
        return {p["name"]: p for p in json.loads(self.call("get_state", {})[0])["photos"]}

    def close(self):
        self.proc.stdin.close()
        self.proc.wait()


class Ollama:
    def __init__(self, url, model, log):
        self.url, self.model, self.log = url.rstrip("/"), model, log
        self.requests = self.prompt_tokens = self.output_tokens = 0

    def form(self, prompt, schema, images):
        """One request, one filled-in form."""
        message = {"role": "user", "content": prompt, "images": images}
        body = {"model": self.model, "messages": [message], "stream": False, "format": schema,
                "options": {"num_ctx": 32768, "temperature": 0}}
        req = urllib.request.Request(f"{self.url}/api/chat", data=json.dumps(body).encode(),
                                     headers={"Content-Type": "application/json"})
        with urllib.request.urlopen(req, timeout=900) as r:
            reply = json.loads(r.read())
        self.requests += 1
        self.prompt_tokens += reply.get("prompt_eval_count", 0)
        self.output_tokens += reply.get("eval_count", 0)
        self.log({"prompt": prompt, "images": len(images), "reply": reply["message"]})
        return json.loads(reply["message"]["content"])


def app_score(mcp, before):
    """{frame: score}, from LightPhotos, scoring every photo without a current score first."""
    todo = [n for n, p in before.items() if not p.get("score") or p["score"]["stale"]]
    for start in range(0, len(todo), SCORE_BATCH):
        batch = todo[start:start + SCORE_BATCH]
        reply = json.loads(mcp.command("photo.score", {"photos": batch})[0])
        for note in reply.get("notes", []):
            print(f"score: {note}", flush=True)
        print(f"score {start + len(batch)}/{len(todo)}", flush=True)
    return {n: p["score"]["value"] for n, p in mcp.state().items()
            if p.get("score") and not p["score"]["stale"]}


def model_score(mcp, llm, photos, shoot, size):
    """{frame: score}, from the model, four neighbouring frames per request."""
    scores = {}

    def ask(names, images):
        answer = llm.form(MODEL_PROMPT.format(shoot=shoot, n=len(names), names=", ".join(names)),
                          MODEL_SCHEMA, images)
        for f in answer["frames"]:
            if f["name"] in names and f["name"] not in scores:
                scores[f["name"]] = f["score"]

    for start in range(0, len(photos), MODEL_GROUP):
        group = photos[start:start + MODEL_GROUP]
        ask(group, [mcp.call("render", {"photo": n, "max_size": size})[1][0] for n in group])
        for name in group:
            if name not in scores:
                # A frame the model left out is scored alone.
                ask([name], mcp.call("render", {"photo": name, "max_size": size})[1])
        print(f"score {start + len(group)}/{len(photos)}", flush=True)
    return scores


def by_score(mcp, photos, scores):
    """{result: frames}. Rejects or rates each scored frame; a frame scoring 60
    or more, or without a score, is left as it is."""
    graded = {}
    for n in photos:
        if n in scores:
            result = next((r for limit, r in SCORE_STARS if scores[n] < limit), "unchanged")
            graded.setdefault(result, []).append(n)
    if graded.get("reject"):
        mcp.command("photo.flag", {"flag": "reject", "photos": graded["reject"]})
    if graded.get(2):
        mcp.command("photo.rate", {"stars": 2, "photos": graded[2]})
    return graded


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mcp", required=True)
    ap.add_argument("--folder", required=True)
    ap.add_argument("--use-model", action="store_true",
                    help="score with the model on macOS too, instead of the app score")
    ap.add_argument("--ollama", default="http://127.0.0.1:11434")
    ap.add_argument("--model", default="qwen3-vl:32b-instruct")
    ap.add_argument("--shoot", default="portrait shoot",
                    help="what the folder is, for the model's prompt")
    ap.add_argument("--review-size", type=int, default=1024,
                    help="long edge of each render the model sees, in pixels")
    ap.add_argument("--log", help="a JSON Lines file for every prompt, reply and score")
    args = ap.parse_args()
    use_model = args.use_model or platform.system() != "Darwin"

    log_file = open(args.log, "a") if args.log else None

    def log(entry):
        if log_file:
            log_file.write(json.dumps(entry) + "\n")
            log_file.flush()

    start = time.monotonic()
    mcp = Mcp(args.mcp, args.folder)
    before = mcp.state()
    photos = [n for n, p in before.items() if p["flag"] != "reject"]
    print(f"{len(before)} photos, {len(before) - len(photos)} already rejected, "
          f"scoring with {'the model' if use_model else 'LightPhotos'}", flush=True)
    llm = Ollama(args.ollama, args.model, log) if use_model else None
    if use_model:
        scores = model_score(mcp, llm, photos, args.shoot, args.review_size)
    else:
        scores = app_score(mcp, before)
    log({"scores": scores})
    graded = by_score(mcp, photos, scores)

    summary = {
        "photos": len(before), "already_rejected": len(before) - len(photos),
        "scored_by": args.model if use_model else "lightphotos",
        "rejected": len(graded.get("reject", [])),
        "two_stars": len(graded.get(2, [])),
        "unchanged": len(graded.get("unchanged", [])),
        "unscored": sum(1 for n in photos if n not in scores),
        "mcp_calls": mcp.calls,
        "total_seconds": round(time.monotonic() - start),
    }
    if llm:
        summary.update(model_requests=llm.requests, prompt_tokens=llm.prompt_tokens,
                       output_tokens=llm.output_tokens)
    log({"summary": summary})
    print(json.dumps(summary), flush=True)
    mcp.close()


if __name__ == "__main__":
    sys.exit(main())
