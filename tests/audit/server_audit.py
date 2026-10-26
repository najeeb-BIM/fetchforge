#!/usr/bin/env python3
"""Hostile test server for the FetchForge audit round (loopback only).
Usage: python3 tests/audit/server_audit.py <root_dir> <port> [secret_port]
Endpoints (all deliberately hostile, none used by normal tests):
  /lie-length       Content-Length 10000, body 100 bytes then close
  /ignore-range     always 200 full body, ignores Range (2 MiB file)
  /mismatch-range   206 with WRONG Content-Range, body is 'X' * segment size
  /short-segment    206, correct Content-Range, but only half the bytes
  /redirect-loop    302 to itself
  /redirect-file    302 to file:///etc/passwd
  /redirect-ftp     302 to ftp://127.0.0.1/x
  /redirect-private 302 to http://127.0.0.1:<secret_port>/secret
  /gzip-broken      Content-Encoding gzip, body is plain text (not gzip)
  /chunked          chunked transfer, no Content-Length
  /reset-mid        half the body, then RST close
  /zero             0 bytes, Content-Length 0
  /changing.bin     file served from disk (auditor overwrites it mid-test)
  /disp-traversal   Content-Disposition filename="../../evil.txt"
  /disp-con         Content-Disposition filename="CON.txt"
  /huge-length      Content-Length 5_000_000_000, body 100 bytes then close
  /secret           (on secret_port server) body "INTERNAL-SECRET"
"""
import os, sys, re, socket, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ROOT = sys.argv[1]
PORT = int(sys.argv[2])
SECRET_PORT = int(sys.argv[3]) if len(sys.argv) > 3 else 0

def make_data(n, byte):
    return bytes([byte]) * n

class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass
    def _range(self, size):
        m = re.match(r"bytes=(\d+)-(\d*)", self.headers.get("Range", ""))
        if not m:
            return None
        s = int(m.group(1)); e = int(m.group(2)) if m.group(2) else size - 1
        return s, min(e, size - 1)
    def do_HEAD(self):
        # HEAD mirrors GET headers for the simple files, without a body.
        self._handle(head_only=True)
    def do_GET(self):
        self._handle(head_only=False)
    def _handle(self, head_only):
        p = self.path.split("?")[0]
        if p == "/secret":
            body = b"INTERNAL-SECRET"
            self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers()
            if not head_only: self.wfile.write(body)
            return
        if p == "/redirect-loop":
            self.send_response(302); self.send_header("Location", "/redirect-loop"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if p == "/redirect-file":
            self.send_response(302); self.send_header("Location", "file:///etc/passwd"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if p == "/redirect-ftp":
            self.send_response(302); self.send_header("Location", "ftp://127.0.0.1/x"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if p == "/redirect-private":
            self.send_response(302); self.send_header("Location", f"http://127.0.0.1:{SECRET_PORT}/secret"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if p == "/lie-length":
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", "10000"); self.end_headers()
            if not head_only: self.wfile.write(b"A" * 100)
            return
        if p == "/zero":
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", "0"); self.end_headers(); return
        if p == "/gzip-broken":
            body = b"this is not gzip at all, just plain text"
            self.send_response(200); self.send_header("Content-Encoding", "gzip"); self.send_header("Content-Length", str(len(body))); self.end_headers()
            if not head_only: self.wfile.write(body)
            return
        if p == "/chunked":
            body = b"chunked-content-ok"
            self.send_response(200); self.send_header("Transfer-Encoding", "chunked"); self.end_headers()
            if not head_only:
                self.wfile.write(b"%x\r\n%s\r\n0\r\n\r\n" % (len(body), body))
            return
        if p == "/ignore-range":
            size = 2 * 1024 * 1024
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(size)); self.end_headers()
            if not head_only: self.wfile.write(make_data(size, 0x41))
            return
        if p == "/mismatch-range":
            # Real file is ABCD: segment 0='A', 1='B', 2='C', 3='D' (524288 each).
            # Lying server: for ANY range request, returns segment-0 data ('A')
            # and claims Content-Range bytes 0-524287/2097152. A correct
            # client must reject this (Content-Range != requested range);
            # a client that trusts its own offset will write 'A' everywhere.
            size = 2 * 1024 * 1024
            r = self._range(size)
            if r is None:
                self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(size)); self.end_headers()
                if not head_only: self.wfile.write(make_data(524288, 0x41) + make_data(524288, 0x42) + make_data(524288, 0x43) + make_data(524288, 0x44))
                return
            s, e = r
            self.send_response(206); self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Range", "bytes 0-524287/2097152")
            self.send_header("Content-Length", str(e - s + 1)); self.end_headers()
            if not head_only: self.wfile.write(make_data(e - s + 1, 0x41))
            return
        if p == "/short-segment":
            size = 2 * 1024 * 1024
            r = self._range(size)
            if r is None:
                self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(size)); self.end_headers()
                if not head_only: self.wfile.write(make_data(size, 0x43))
                return
            s, e = r
            half = (e - s + 1) // 2
            self.send_response(206); self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Range", f"bytes {s}-{e}/{size}")
            self.send_header("Content-Length", str(half)); self.end_headers()
            if not head_only: self.wfile.write(make_data(half, 0x44))
            return
        if p == "/reset-mid":
            body = make_data(1024, 0x45)
            self.send_response(200); self.send_header("Content-Length", "2048"); self.end_headers()
            if not head_only:
                self.wfile.write(body); self.wfile.flush()
                # RST close
                self.connection.setsockopt(socket.SOL_SOCKET, socket.SO_LINGER, b"\x01\x00\x00\x00\x00\x00\x00\x00")
                self.connection.close()
            return
        if p == "/huge-length":
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", "5000000000"); self.end_headers()
            if not head_only: self.wfile.write(b"A" * 100)
            return
        if p == "/changing.bin":
            fp = os.path.join(ROOT, "changing.bin")
            if not os.path.isfile(fp):
                self.send_response(404); self.end_headers(); return
            data = open(fp, "rb").read(); size = len(data)
            r = self._range(size)
            if r:
                s, e = r; chunk = data[s:e+1]
                self.send_response(206); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Range", f"bytes {s}-{e}/{size}"); self.send_header("Content-Length", str(len(chunk))); self.end_headers()
                if not head_only: self.wfile.write(chunk)
                return
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(size)); self.end_headers()
            if not head_only: self.wfile.write(data)
            return
        if p == "/disp-traversal":
            body = b"traversal-test"
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(len(body))); self.send_header("Content-Disposition", 'attachment; filename="../../evil.txt"'); self.end_headers()
            if not head_only: self.wfile.write(body)
            return
        if p == "/disp-con":
            body = b"con-test"
            self.send_response(200); self.send_header("Accept-Ranges", "bytes"); self.send_header("Content-Length", str(len(body))); self.send_header("Content-Disposition", 'attachment; filename="CON.txt"'); self.end_headers()
            if not head_only: self.wfile.write(body)
            return
        self.send_response(404); self.end_headers()

if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
