#!/usr/bin/env bash
# F06: if the server file changes between pause and resume (same size,
# different bytes), the resumed download must NOT mix old and new data.
# It must restart from 0 and finish with the NEW file, never "completed"
# with a mixed file. Before the fix this test FAILS with a mixed file.
set -u
ROOT=/tmp/ff-changing; rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/server"
python3 -c "open('$ROOT/server/changing.bin','wb').write(bytes([0x41])*524288)"
cat > "$ROOT/server.py" <<'PY'
import os, sys, hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
ROOT = sys.argv[1]; PORT = int(sys.argv[2])
class H(BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def _file(self):
        p = os.path.join(ROOT, "changing.bin")
        data = open(p, "rb").read()
        etag = '"' + hashlib.sha256(data).hexdigest()[:16] + '"'
        lm = self.date_time_string(os.path.getmtime(p))
        return data, etag, lm
    def _send(self, head_only):
        data, etag, lm = self._file()
        rng = self.headers.get("Range", "")
        if rng.startswith("bytes="):
            s = int(rng[6:].split("-")[0])
            # If-Range: if ETag mismatches, ignore Range (return 200 full).
            ir = self.headers.get("If-Range", "")
            if ir and ir != etag:
                self.send_response(200); self.send_header("Accept-Ranges", "bytes")
                self.send_header("Content-Length", str(len(data))); self.send_header("ETag", etag); self.send_header("Last-Modified", lm); self.end_headers()
                if not head_only:
                    for i in range(0, len(data), 65536):
                        self.wfile.write(data[i:i+65536]); self.wfile.flush()
                return
            body = data[s:]
            self.send_response(206); self.send_header("Accept-Ranges", "bytes")
            self.send_header("Content-Range", f"bytes {s}-{len(data)-1}/{len(data)}")
            self.send_header("Content-Length", str(len(body))); self.send_header("ETag", etag); self.send_header("Last-Modified", lm); self.end_headers()
            if not head_only:
                # Slow drip so pause can land mid-download.
                import time
                for i in range(0, len(body), 65536):
                    self.wfile.write(body[i:i+65536]); self.wfile.flush(); time.sleep(0.2)
            return
        self.send_response(200); self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Length", str(len(data))); self.send_header("ETag", etag); self.send_header("Last-Modified", lm); self.end_headers()
        if not head_only:
            import time
            for i in range(0, len(data), 65536):
                self.wfile.write(data[i:i+65536]); self.wfile.flush(); time.sleep(0.2)
    def do_HEAD(self): self._send(True)
    def do_GET(self): self._send(False)
ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
PY
python3 "$ROOT/server.py" "$ROOT/server" 18831 > "$ROOT/server.log" 2>&1 & SPID=$!
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
cd "$(dirname "$0")/.." || exit 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18832 > "$ROOT/bridge.log" 2>&1 & BPID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
ID=$(curl -s -X POST http://127.0.0.1:18832/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18831/changing.bin","filename":"changing.bin"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
echo "ID=$ID"
# Wait until some progress, then pause.
for _ in $(seq 1 30); do
  DL=$(curl -s http://127.0.0.1:18832/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "import json,sys; d=json.load(sys.stdin); print([r['downloaded_bytes'] for r in d if r['id']=='$ID'][0])")
  [ "$DL" -gt 100000 ] && break; sleep 0.5
done
echo "PAUSED_AT=$DL"
curl -s -X POST http://127.0.0.1:18832/downloads/$ID/pause -H "X-FetchForge-Token: $TOKEN" >/dev/null
sleep 1
# Change the server file: same size, different bytes, new ETag/Last-Modified.
python3 -c "open('$ROOT/server/changing.bin','wb').write(bytes([0x42])*524288)"
cp "$ROOT/server/changing.bin" "$ROOT/new.bin"
curl -s -X POST http://127.0.0.1:18832/downloads/$ID/resume -H "X-FetchForge-Token: $TOKEN" >/dev/null
for _ in $(seq 1 40); do
  ST=$(curl -s http://127.0.0.1:18832/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "import json,sys; d=json.load(sys.stdin); print([r['status'] for r in d if r['id']=='$ID'][0])")
  [ "$ST" = "completed" ] && break; sleep 0.5
done
echo "FINAL_STATUS=$ST"
FINAL="$ROOT/downloads/changing.bin"
if [ ! -f "$FINAL" ]; then FINAL="$ROOT/downloads/Other/changing.bin"; fi
if cmp -s "$FINAL" "$ROOT/new.bin"; then echo "CHANGING_RESUME PASS (final = new file)"; RC=0;
else
  echo "CHANGING_RESUME FAIL (final != new file; mixed or original)"; python3 -c "
import sys
f=open('$FINAL','rb').read(); n=open('$ROOT/new.bin','rb').read()
print('final first byte', f[:1], 'new first byte', n[:1], 'final last byte', f[-1:], 'new last byte', n[-1:])
"; RC=1; fi
kill $BPID $SPID 2>/dev/null
exit $RC
