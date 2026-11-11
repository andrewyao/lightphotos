#!/usr/bin/env python3
"""A stand-in ComfyUI server for driving Generative Fill without a GPU.

It answers the six routes LightPhotos calls and returns a solid-colour PNG
for every queued prompt, a different colour each time, so three variants are
three different patches.

    fake-comfyui.py --port 8765                  serve until killed
    fake-comfyui.py --port 8765 --missing vae    leave one model off the lists
    fake-comfyui.py --port 8765 --delay 2        each prompt takes 2 s
    fake-comfyui.py --port 8765 --die-after 2    exit on the 2nd /history poll,
                                                 as a server killed mid-job
    fake-comfyui.py --make-models DIR            write empty files of the
                                                 right sizes into DIR and exit

The model list is read from src/comfy/features.rs, so it follows the app.
"""

import argparse
import json
import os
import re
import struct
import sys
import time
import zlib
from http.server import BaseHTTPRequestHandler, HTTPServer
from urllib.parse import parse_qs, urlparse

REPO = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
COLOURS = [(214, 64, 48), (52, 152, 96), (64, 96, 208), (220, 180, 40)]


def models():
    """(folder, file, bytes) for every model in the app's feature table."""
    text = open(os.path.join(REPO, "src", "comfy", "features.rs"), encoding="utf-8").read()
    rows = re.findall(
        r'folder:\s*"([^"]+)",\s*file:\s*"([^"]+)",.*?bytes:\s*([\d_]+)', text, re.S
    )
    return [(folder, name, int(size.replace("_", ""))) for folder, name, size in rows]


def make_models(root):
    for folder, name, size in models():
        os.makedirs(os.path.join(root, folder), exist_ok=True)
        with open(os.path.join(root, folder, name), "wb") as f:
            f.truncate(size)  # sparse: the size without the bytes


def png(width, height, rgb):
    def chunk(kind, data):
        body = kind + data
        return struct.pack(">I", len(data)) + body + struct.pack(">I", zlib.crc32(body))

    row = b"\x00" + bytes(rgb) * width
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
        + chunk(b"IDAT", zlib.compress(row * height))
        + chunk(b"IEND", b"")
    )


class State:
    def __init__(self, args):
        self.args = args
        self.queued = {}  # prompt id -> (time queued, colour)
        self.history_polls = 0
        self.listed = [name for _, name, _ in models() if args.missing not in name]


class Handler(BaseHTTPRequestHandler):
    state = None

    def log_message(self, fmt, *args):
        sys.stderr.write("[fake-comfyui] " + fmt % args + "\n")

    def reply(self, status, body, kind="application/json"):
        if not isinstance(body, bytes):
            body = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("content-type", kind)
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        return self.rfile.read(int(self.headers.get("content-length", 0)))

    def do_GET(self):
        url = urlparse(self.path)
        state = self.state
        if url.path == "/system_stats":
            return self.reply(200, {"system": {"comfyui_version": "fake"}})
        if url.path.startswith("/object_info/"):
            loader = url.path.rsplit("/", 1)[1]
            inputs = {
                name: [state.listed, {}]
                for name in ("unet_name", "clip_name1", "clip_name2", "vae_name", "ckpt_name", "clip_name")
            }
            return self.reply(200, {loader: {"input": {"required": inputs}}})
        if url.path.startswith("/history/"):
            state.history_polls += 1
            if state.args.die_after and state.history_polls >= state.args.die_after:
                sys.stderr.write("[fake-comfyui] dying mid-job\n")
                os._exit(0)
            prompt = url.path.rsplit("/", 1)[1]
            queued = state.queued.get(prompt)
            if not queued or time.time() - queued[0] < state.args.delay:
                return self.reply(200, {})
            image = {"filename": prompt + ".png", "subfolder": "", "type": "output"}
            return self.reply(
                200,
                {
                    prompt: {
                        "outputs": {"10": {"images": [image]}},
                        "status": {"status_str": "success", "completed": True},
                    }
                },
            )
        if url.path == "/view":
            prompt = parse_qs(url.query).get("filename", [""])[0].removesuffix(".png")
            queued = state.queued.get(prompt)
            if not queued:
                return self.reply(404, {})
            return self.reply(200, png(256, 256, queued[1]), "image/png")
        self.reply(404, {})

    def do_POST(self):
        url = urlparse(self.path)
        state = self.state
        body = self.body()
        if url.path == "/upload/image":
            return self.reply(200, {"name": "lightphotos_fill.png", "subfolder": "", "type": "input"})
        if url.path == "/prompt":
            n = len(state.queued)
            prompt = "fake-%d" % n
            state.queued[prompt] = (time.time(), COLOURS[n % len(COLOURS)])
            return self.reply(200, {"prompt_id": prompt, "number": n})
        if url.path in ("/queue", "/interrupt"):
            json.loads(body or b"{}")
            return self.reply(200, {})
        self.reply(404, {})


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawTextHelpFormatter)
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--missing", default="\0", help="leave models whose name contains this off the lists")
    parser.add_argument("--delay", type=float, default=0.5, help="seconds each prompt takes")
    parser.add_argument("--die-after", type=int, default=0, help="exit on this /history poll")
    parser.add_argument("--make-models", metavar="DIR", help="write right-sized empty model files and exit")
    args = parser.parse_args()
    if args.make_models:
        make_models(args.make_models)
        return
    Handler.state = State(args)
    HTTPServer(("127.0.0.1", args.port), Handler).serve_forever()


if __name__ == "__main__":
    main()
