"""Local browser editor for optional Series X hardware test observations."""
import base64
import binascii
import json
import os
from pathlib import Path
import secrets
import struct
import tempfile
import time
import webbrowser
import zipfile
from http.server import BaseHTTPRequestHandler, HTTPServer

SIDES = ("main", "candidate")
NOTES = tuple(side + "-observation.txt" for side in SIDES)
PICTURES = tuple(side + "-screenshot.png" for side in SIDES)
MAX_IMAGE = 4 * 1024 * 1024
MAX_BODY = 12 * 1024 * 1024
PAGE = Path(__file__).with_name("observations.html")


def read_draft(archive):
    draft = {}
    for side, note, image in zip(SIDES, NOTES, PICTURES):
        draft[side] = {
            "note": archive.read(note).decode() if note in archive.namelist() else "",
            "image": base64.b64encode(archive.read(image)).decode() if image in archive.namelist() else "",
        }
    return draft


def screenshot(encoded):
    """Keep PNG pixels, palette and transparency. Drop optional metadata chunks."""
    if len(encoded) > (MAX_IMAGE + 2) // 3 * 4:
        raise ValueError("Use a smaller screenshot (4 MiB per image).")
    data = base64.b64decode(encoded, validate=True)
    signature = b"\x89PNG\r\n\x1a\n"
    if not data.startswith(signature) or len(data) > MAX_IMAGE:
        raise ValueError("Use a PNG screenshot up to 4 MiB.")
    pixels = bytearray(signature)
    pos = len(signature)
    has_pixels = False
    while pos + 12 <= len(data):
        size, kind = struct.unpack_from(">I4s", data, pos)
        end = pos + size + 12
        if end > len(data) or binascii.crc32(data[pos + 4:end - 4]) != int.from_bytes(data[end - 4:end], "big"):
            break
        if pos == len(signature):
            if kind != b"IHDR" or size != 13:
                break
            width, height = struct.unpack_from(">II", data, pos + 8)
            if not (0 < width <= 4096 and 0 < height <= 4096):
                raise ValueError("Screenshot dimensions exceed the review limit.")
        if kind == b"IDAT" and size:
            has_pixels = True
        if kind in (b"IHDR", b"PLTE", b"IDAT", b"tRNS", b"IEND"):
            pixels.extend(data[pos:end])
        if kind == b"IEND":
            if size == 0 and end == len(data) and has_pixels:
                return bytes(pixels)
            break
        pos = end
    raise ValueError("Incomplete screenshot. Paste the image again.")


def save_draft(filtered, draft):
    files = {}
    for side, note, image in zip(SIDES, NOTES, PICTURES):
        value = draft[side]["note"].strip()
        if len(value) > 4000 or any(not ch.isprintable() and ch not in "\n\t" for ch in value):
            raise ValueError("Use up to 4,000 plain-text characters per build.")
        if value:
            files[note] = value.encode()
        if draft[side]["image"]:
            files[image] = screenshot(draft[side]["image"])
    with tempfile.TemporaryDirectory(dir=filtered.parent) as temp:
        reviewed = Path(temp) / "reviewed.zip"
        with zipfile.ZipFile(filtered) as original, zipfile.ZipFile(reviewed, "x") as out:
            for name in original.namelist():
                if name not in NOTES + PICTURES:
                    out.writestr(original.getinfo(name), original.read(name))
            for name, data in files.items():
                out.writestr(zipfile.ZipInfo(name), data, compress_type=zipfile.ZIP_DEFLATED)
        os.replace(reviewed, filtered)


def edit(filtered):
    # The unguessable route and origin check confine writes to this local form.
    route = "/" + secrets.token_urlsafe(32)
    done = False

    class Handler(BaseHTTPRequestHandler):
        timeout = 5

        def log_message(self, *_):
            pass

        def reply(self, code, content, media="text/plain"):
            self.send_response(code)
            self.send_header("Content-Type", media + "; charset=utf-8")
            self.send_header("Content-Length", str(len(content)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Referrer-Policy", "no-referrer")
            self.send_header("Content-Security-Policy", "default-src 'none'; script-src 'unsafe-inline'; "
                             "style-src 'unsafe-inline'; img-src data: blob:; connect-src 'self'; "
                             "frame-ancestors 'none'; form-action 'none'; base-uri 'none'")
            self.end_headers()
            self.wfile.write(content)

        def do_GET(self):
            if self.headers.get("Host") != host:
                self.reply(403, b"Invalid host.")
            elif self.path == route:
                self.reply(200, PAGE.read_bytes(), "text/html")
            elif self.path == route + "/draft":
                with zipfile.ZipFile(filtered) as archive:
                    self.reply(200, json.dumps(read_draft(archive)).encode(), "application/json")
            else:
                self.reply(404, b"Missing.")

        def do_POST(self):
            nonlocal done
            if (self.headers.get("Host") != host or self.headers.get("Origin") != origin
                    or self.path not in (route + "/save", route + "/cancel")):
                self.reply(403, b"Invalid local review session.")
                return
            try:
                size = int(self.headers.get("Content-Length", "-1"))
                if not 0 <= size <= MAX_BODY:
                    raise ValueError("Screenshot data exceeds the review limit.")
                if self.path.endswith("/save"):
                    save_draft(filtered, json.loads(self.rfile.read(size)))
            except (ValueError, KeyError, AttributeError, TypeError, OSError) as error:
                self.reply(400, str(error).encode())
                return
            done = True
            self.reply(200, b"Return to the terminal to choose what to share.")

    with HTTPServer(("127.0.0.1", 0), Handler) as server:
        host = f"127.0.0.1:{server.server_port}"
        origin = "http://" + host
        url = origin + route
        print(f"Local observations editor: {url}\nSave or cancel in the browser. Ctrl+C returns to results.", flush=True)
        webbrowser.open(url)
        server.timeout = 0.5
        deadline = time.monotonic() + 1800
        try:
            while not done and time.monotonic() < deadline:
                server.handle_request()
        except KeyboardInterrupt:
            pass
