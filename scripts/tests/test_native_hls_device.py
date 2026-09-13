"""Real HTTP checks for the temporary native-device evidence proxy."""
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import importlib.util
import json
from pathlib import Path
import tempfile
import threading
import unittest
from unittest.mock import patch


SPEC = importlib.util.spec_from_file_location(
    "native_hls_device", Path(__file__).resolve().parents[1] / "native-hls-device.py")
DEVICE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(DEVICE)
PLAYLIST = b"#EXTM3U\n#EXT-X-TARGETDURATION:12\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:12.0,\nsegment.mp4\n#EXT-X-ENDLIST\n"
SOURCE = bytes(range(256)) * 8


class Upstream(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.server.observed_headers = dict(self.headers)
        if self.path == "/playlist.m3u8":
            data, status = PLAYLIST, 200
        elif self.path == "/truncated":
            data, status = b"abc", 200
        else:
            data, status = SOURCE[37:116], 206
        self.send_response(status)
        self.send_header("Content-Length", str(10 if self.path == "/truncated" else len(data)))
        self.send_header("ETag", '"test-source"')
        self.send_header("Content-Security-Policy", "default-src 'self'; frame-ancestors 'none'; object-src 'none'")
        self.send_header("X-Frame-Options", "DENY")
        if status == 206:
            self.send_header("Content-Range", f"bytes 37-115/{len(SOURCE)}")
        self.end_headers()
        if self.command != "HEAD":
            self.wfile.write(data)

    do_HEAD = do_GET


class ObservedEvidence(DEVICE.Evidence):
    def __init__(self, output):
        super().__init__(output)
        self.recorded = threading.Event()

    def append(self, name, record):
        result = super().append(name, record)
        if name == "requests.jsonl":
            self.recorded.set()
        return result


class NativeDeviceProxyTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="rustydlna-native-proxy-")
        self.addCleanup(self.directory.cleanup)
        self.evidence = ObservedEvidence(Path(self.directory.name))
        self.backend = ThreadingHTTPServer(("127.0.0.1", 0), Upstream)
        self.assets = {"/web/player.js": (b"original snapshot", "text/javascript")}
        self.proxy = DEVICE.create_server(("127.0.0.1", 0), self.backend.server_port,
                                          self.evidence, self.assets, b"test page")
        for server in [self.backend, self.proxy]:
            thread = threading.Thread(target=server.serve_forever, kwargs={"poll_interval": 0.01})
            thread.start()
            self.addCleanup(self.stop_server, server, thread)

    def stop_server(self, server, thread):
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        self.assertFalse(thread.is_alive())

    def request(self, method, path, body=None, headers=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.proxy.server_port, timeout=3)
        try:
            connection.request(method, path, body, headers or {})
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), response.read()
        finally:
            connection.close()

    def last_request(self):
        self.assertTrue(self.evidence.recorded.wait(timeout=3), "proxy did not finish recording the response")
        return json.loads((self.evidence.output / "requests.jsonl").read_text().splitlines()[-1])

    def test_ranges_head_and_asset_snapshot(self):
        status, headers, body = self.request("GET", "/media", headers={
            "Range": "bytes=37-115", "Cookie": "private-cookie", "Authorization": "private-auth"})
        self.assertEqual((status, body), (206, SOURCE[37:116]))
        self.assertEqual(headers["Content-Range"], f"bytes 37-115/{len(SOURCE)}")
        self.assertEqual(headers["Connection"], "close")
        self.assertEqual(self.backend.observed_headers["Range"], "bytes=37-115")
        self.assertNotIn("Cookie", self.backend.observed_headers)
        self.assertNotIn("Authorization", self.backend.observed_headers)
        self.assertEqual(self.last_request()["response_headers"]["ETag"], '"test-source"')
        status, headers, body = self.request("HEAD", "/media")
        self.assertEqual((status, body), (206, b""))
        self.assertEqual(headers["Content-Length"], "79")
        self.assets["/web/player.js"] = (b"later edit", "text/javascript")
        self.assertEqual(self.request("GET", "/web/player.js")[2], b"original snapshot")
        _, headers, _ = self.request("GET", "/")
        self.assertEqual(headers["X-Frame-Options"], "SAMEORIGIN")
        self.assertEqual(headers["Content-Security-Policy"], "default-src 'self'; frame-ancestors 'self'; object-src 'none'")

    def test_playlist_identity_and_truncated_upstream_evidence(self):
        self.assertEqual(self.request("GET", "/playlist.m3u8")[2], PLAYLIST)
        recorded = self.last_request()["playlist"]
        self.assertEqual(recorded["sha256"], hashlib.sha256(PLAYLIST).hexdigest())
        self.assertEqual((recorded["segments"], recorded["target"], recorded["sequence"]), (1, ["12"], ["0"]))
        self.assertTrue(recorded["complete_capture"])
        self.evidence.recorded.clear()
        with self.assertRaises(http.client.IncompleteRead) as failure:
            self.request("GET", "/truncated")
        self.assertEqual(failure.exception.partial, b"abc")
        record = self.last_request()
        self.assertEqual(record["body_bytes"], 3)
        self.assertIn("expected 10, received 3", record["error"])

    def test_capture_origin_paths_and_body_limits(self):
        body = json.dumps({"client": "ipad-1", "records": [{"event": "seeked"}]}).encode()
        self.assertEqual(self.request("POST", "/__native/record", body)[0], 403)
        origin = {"Origin": f"http://127.0.0.1:{self.proxy.server_port}"}
        self.assertEqual(self.request("POST", "/__native/record", body, origin)[0], 200)
        saved = json.loads((self.evidence.output / "client-ipad-1.jsonl").read_text())
        self.assertEqual(saved["records"], [{"event": "seeked"}])
        self.assertEqual(self.request("POST", "/__native/record", b'{"client":"../escape"}', origin)[0], 400)
        self.assertEqual(self.request("POST", "/unrelated-mutation", b"", origin)[0], 405)
        self.assertEqual(self.request("POST", "/__native/record", b"", {
            **origin, "Content-Length": str(DEVICE.MAX_RECORD_BYTES + 1)})[0], 413)
        with patch.object(DEVICE, "MAX_TOTAL_BYTES", self.evidence.bytes):
            self.assertEqual(self.request("POST", "/__native/record", body, origin)[0], 507)


if __name__ == "__main__":
    unittest.main()
