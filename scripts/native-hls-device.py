#!/usr/bin/env python3
"""Temporary Safari test page/proxy with bounded local playback evidence.

Uses a localhost rustyDLNA backend and the workspace's browser assets. It does
not replace the running daemon or write media files. Normal playback requests
can create backend transcode cache entries, just like the ordinary player.
"""
import argparse
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
from pathlib import Path
import re
import threading
import time
from urllib.parse import urlsplit

MAX_RECORD_BYTES = 2 * 1024 * 1024
MAX_TOTAL_BYTES = 64 * 1024 * 1024


class Evidence:
    def __init__(self, output):
        self.output = output
        self.lock = threading.Lock()
        self.bytes = 0

    def append(self, name, record):
        encoded = (json.dumps(record, separators=(",", ":")) + "\n").encode()
        with self.lock:
            if self.bytes + len(encoded) > MAX_TOTAL_BYTES:
                return False
            with (self.output / name).open("ab") as destination:
                destination.write(encoded)
            self.bytes += len(encoded)
        return True


def create_server(address, backend_port, evidence, assets, page):
    assets = dict(assets)

    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def respond(self, status, data=b"", content_type="text/plain; charset=utf-8"):
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(data)))
            self.send_header("Cache-Control", "no-store")
            self.send_header("Connection", "close")
            self.end_headers()
            self.close_connection = True
            if self.command != "HEAD":
                self.wfile.write(data)

        def do_GET(self):
            self.dispatch()

        def do_HEAD(self):
            self.dispatch()

        def do_POST(self):
            self.dispatch()

        def do_DELETE(self):
            self.dispatch()

        def dispatch(self):
            self.connection.settimeout(30)
            if len(self.path) > 8192 or self.headers.get("Transfer-Encoding"):
                self.respond(400)
                return
            path = urlsplit(self.path).path
            if self.command in ["POST", "DELETE"]:
                origin = self.headers.get("Origin")
                if origin != "http://" + self.headers.get("Host", ""):
                    self.respond(403, b"Use the test page on this server.")
                    return
            lengths = self.headers.get_all("Content-Length", [])
            if len(lengths) > 1 or (lengths and not lengths[0].isdigit()):
                self.respond(400)
                return
            length = int(lengths[0]) if lengths else 0
            if length > MAX_RECORD_BYTES:
                self.respond(413)
                return
            body = self.rfile.read(length)
            if len(body) != length:
                self.respond(400)
                return
            if path == "/__native/record" and self.command == "POST":
                try:
                    record = json.loads(body)
                    if not isinstance(record, dict) or not re.fullmatch(r"[a-zA-Z0-9-]{1,64}", record.get("client", "")):
                        raise ValueError("invalid client")
                except (ValueError, TypeError, RecursionError):
                    self.respond(400)
                    return
                accepted = evidence.append("client-" + record["client"] + ".jsonl", {**record, "received": time.time(), "peer": self.client_address[0]})
                self.respond(200 if accepted else 507, b"Saved" if accepted else b"Evidence storage is full")
                return
            if self.command in ["GET", "HEAD"] and path == "/__native/":
                self.respond(200, page, "text/html; charset=utf-8")
                return
            if self.command in ["GET", "HEAD"] and path in assets:
                data, content_type = assets[path]
                self.respond(200, data, content_type)
                return
            if self.command in ["POST", "DELETE"] and not re.fullmatch(r"/api/web/transcode/[0-9]+", path):
                self.respond(405)
                return
            upstream = http.client.HTTPConnection("127.0.0.1", backend_port, timeout=120)
            started = time.monotonic()
            sent = 0
            capture = bytearray()
            status = None
            error = None
            response_headers = {}
            try:
                headers = {key: value for key, value in self.headers.items() if key.lower() not in ["host", "connection", "accept-encoding", "content-length", "origin", "referer", "cookie", "authorization"]}
                headers["Host"] = f"127.0.0.1:{backend_port}"
                headers["Connection"] = "close"
                if self.command in ["POST", "DELETE"]:
                    headers["Origin"] = f"http://127.0.0.1:{backend_port}"
                upstream.request(self.command, self.path, body=body or None, headers=headers)
                response = upstream.getresponse()
                status = response.status
                response_headers = {name: response.getheader(name) for name in ["Content-Length", "Content-Range", "ETag", "Content-Type"]}
                self.send_response(status)
                for key, value in response.getheaders():
                    if path == "/" and key.lower() == "content-security-policy":
                        value = re.sub(r"frame-ancestors\s+[^;]+", "frame-ancestors 'self'", value)
                    if path == "/" and key.lower() == "x-frame-options":
                        value = "SAMEORIGIN"
                    if key.lower() not in ["connection", "transfer-encoding", "server", "date", "cache-control"]:
                        self.send_header(key, value)
                self.send_header("Connection", "close")
                self.send_header("Cache-Control", "no-store")
                self.end_headers()
                self.close_connection = True
                while data := response.read1(64 * 1024):
                    if path.endswith(".m3u8") and len(capture) + len(data) <= 32 * 1024 * 1024:
                        capture.extend(data)
                    if self.command != "HEAD":
                        self.wfile.write(data)
                    sent += len(data)
                promised = response_headers.get("Content-Length")
                if self.command != "HEAD" and promised and promised.isdigit() and sent != int(promised):
                    error = f"Truncated upstream body: expected {promised}, received {sent}"
            except (OSError, http.client.HTTPException) as failure:
                error = str(failure)
                self.close_connection = True
            finally:
                upstream.close()
                record = {"time": time.time(), "method": self.command, "url": self.path, "range": self.headers.get("Range"), "status": status, "body_bytes": sent, "response_headers": response_headers, "seconds": time.monotonic() - started, "error": error}
                if capture:
                    text = capture.decode("utf-8", errors="replace")
                    record["playlist"] = {"sha256": hashlib.sha256(capture).hexdigest(), "captured_bytes": len(capture), "complete_capture": len(capture) == sent, "segments": text.count("#EXTINF:"), "target": re.findall(r"#EXT-X-TARGETDURATION:(\d+)", text), "sequence": re.findall(r"#EXT-X-MEDIA-SEQUENCE:(\d+)", text), "ended": "#EXT-X-ENDLIST" in text}
                evidence.append("requests.jsonl", record)

    class Server(ThreadingHTTPServer):
        daemon_threads = True
        request_queue_size = 32

        def __init__(self, *values):
            self.slots = threading.BoundedSemaphore(32)
            super().__init__(*values)

        def process_request(self, request, address):
            if not self.slots.acquire(blocking=False):
                self.shutdown_request(request)
                return
            try:
                super().process_request(request, address)
            except BaseException:
                self.slots.release()
                raise

        def process_request_thread(self, request, address):
            try:
                super().process_request_thread(request, address)
            finally:
                self.slots.release()

    return Server(address, Handler)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--listen", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=18231)
    parser.add_argument("--backend-port", type=int, default=18230)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--seconds", type=int, default=7200)
    parser.add_argument("--assets-dir", type=Path, help="saved browser assets for a baseline capture")
    args = parser.parse_args()
    if args.port in [0, 1900, 8200] or not 1 <= args.port <= 65535 or not 1 <= args.backend_port <= 65535 or not 1 <= args.seconds <= 14400:
        parser.error("isolated listen port and 1..14400 second lifetime required")
    args.output.mkdir(parents=True, exist_ok=False)
    evidence = Evidence(args.output)
    web = args.assets_dir or Path(__file__).resolve().parents[1] / "crates/server/web"
    assets = {"/web/" + path.name: (path.read_bytes(), "text/javascript" if path.suffix == ".js" else "text/css") for path in web.iterdir() if path.suffix in [".js", ".css"] and not path.name.endswith(".test.js")}
    page = Path(__file__).with_name("native-hls-device.html").read_bytes()
    evidence.append("identity.jsonl", {"started": time.time(), "backend_port": args.backend_port, "assets": {route: hashlib.sha256(asset[0]).hexdigest() for route, asset in assets.items()}})

    with create_server((args.listen, args.port), args.backend_port, evidence, assets, page) as server:
        timer = threading.Timer(args.seconds, server.shutdown)
        timer.daemon = True
        timer.start()
        print(f"Native test page: http://{args.listen}:{args.port}/__native/", flush=True)
        try:
            server.serve_forever(poll_interval=0.2)
        except KeyboardInterrupt:
            pass
        finally:
            timer.cancel()


if __name__ == "__main__":
    main()
