#!/usr/bin/env python3
"""Local test server for FetchForge e2e tests. Loopback only.
Serves deterministic files with Range support, plus a tiny HLS playlist.
Usage: python3 tests/server.py <root_dir> <port>
"""
import os, sys, re, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[1]
PORT = int(sys.argv[2])

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def _serve(self, path, head_only=False):
        # Failure-case endpoints for tests: deterministic HTTP errors.
        status_map = {"/unauthorized": 401, "/forbidden": 403, "/rate-limited": 429, "/server-error": 500}
        if path in status_map:
            code = status_map[path]
            self.send_response(code)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return
        # HLS playlist
        if path == "/playlist.m3u8":
            body = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:1.0,\nseg1.ts\n#EXTINF:1.0,\nseg2.ts\n#EXT-X-ENDLIST\n"
            self.send_response(200)
            self.send_header("Content-Type", "application/vnd.apple.mpegurl")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if not head_only:
                self.wfile.write(body)
            return
        # Slow endpoint: same file as slow.bin, streamed in 16 KiB chunks
        # with a delay, so pause/resume can be exercised deterministically.
        slow = path == "/slow.bin"
        fs_path = os.path.join(ROOT, "slow.bin" if slow else path.lstrip("/"))
        if not os.path.isfile(fs_path):
            self.send_response(404); self.end_headers(); return
        size = os.path.getsize(fs_path)
        with open(fs_path, "rb") as f:
            data = f.read()
        # Range support
        range_h = self.headers.get("Range")
        if range_h:
            m = re.match(r"bytes=(\d+)-(\d*)", range_h)
            if m:
                start = int(m.group(1))
                end = int(m.group(2)) if m.group(2) else size - 1
                end = min(end, size - 1)
                chunk = data[start:end+1]
                self.send_response(206)
                self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
                self.send_header("Content-Length", str(len(chunk)))
                self.end_headers()
                if not head_only:
                    self._write_slow(chunk, slow)
                return
        self.send_response(200)
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(size))
        if path.endswith(".bin"):
            self.send_header("Content-Disposition", 'attachment; filename="big.bin"')
        self.end_headers()
        if not head_only:
            self._write_slow(data, slow)
    def _write_slow(self, data, slow):
        if not slow:
            self.wfile.write(data)
            return
        for i in range(0, len(data), 16384):
            self.wfile.write(data[i:i+16384])
            self.wfile.flush()
            time.sleep(0.15)
    def do_GET(self):
        self._serve(self.path)
    def do_HEAD(self):
        self._serve(self.path, head_only=True)

if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
