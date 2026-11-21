#!/usr/bin/env bash
# Phase 10 (§13 matrix): a server without Range support must still
# download correctly — single GET, no segments, and the record must
# say resumable=false (never claim resumability it does not have).
# Exits non-zero on failure. Leaves artefacts in /tmp/ff-test-norange.
set -u
ROOT=/tmp/ff-test-norange
rm -rf "$ROOT"; mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads"
python3 -c "
data = bytes((i * 11) % 256 for i in range(3 * 1024 * 1024))
open('$ROOT/server/plain.bin', 'wb').write(data)
"
python3 - 8773 <<'PY' > "$ROOT/server.log" 2>&1 &
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def do_GET(self):
        # Deliberately ignores Range headers: always 200, no
        # Accept-Ranges header, whole file every time.
        with open("/tmp/ff-test-norange/server/plain.bin", "rb") as f:
            data = f.read()
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)
    def do_HEAD(self):
        self.send_response(200)
        self.send_header("Content-Type", "application/octet-stream")
        self.send_header("Content-Length", str(3 * 1024 * 1024))
        self.end_headers()
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
SRV=$!
cd "$(dirname "$0")/.." || exit 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18782 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
cleanup() { kill $BPID $SRV 2>/dev/null; }
trap cleanup EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
ADD=$(curl -s -X POST http://127.0.0.1:18782/downloads -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d '{"url":"http://127.0.0.1:8773/plain.bin"}')
echo "$ADD"
ID=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$ADD")
RESUMABLE=$(python3 -c "import sys,json; print(json.load(sys.stdin).get('resumable'))" <<< "$ADD")
[ "$RESUMABLE" = "False" ] || { echo "FAIL: record claims resumable=$RESUMABLE for a no-Range server"; exit 1; }
for _ in $(seq 1 30); do
  STATE=$(curl -s http://127.0.0.1:18782/downloads -H "X-FetchForge-Token: $TOKEN")
  ST=$(python3 -c "import sys,json; r=[d for d in json.load(sys.stdin) if d['id']=='$ID'][0]; print(r['status'])" <<< "$STATE")
  [ "$ST" = "completed" ] && break
  [ "$ST" = "failed" ] && { echo "FAIL: download failed: $STATE"; exit 1; }
  sleep 1
done
[ "$ST" = "completed" ] || { echo "FAIL: timed out, last state: $ST"; exit 1; }
sha256sum "$ROOT/downloads/Other/plain.bin" "$ROOT/server/plain.bin"
[ "$(sha256sum "$ROOT/downloads/Other/plain.bin" | cut -d' ' -f1)" = "$(sha256sum "$ROOT/server/plain.bin" | cut -d' ' -f1)" ] || { echo "FAIL: bytes differ"; exit 1; }
echo "NO_RANGE_SERVER PASS"
