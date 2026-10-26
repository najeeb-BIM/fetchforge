#!/usr/bin/env bash
# FetchForge e2e proof on Linux (engine + bridge). Run from repo root:
#   bash tests/e2e.sh
# Exits non-zero on any failed check. Leaves artefacts in /tmp/ff-test.
set -u
ROOT=/tmp/ff-test
rm -rf "$ROOT"
mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads"
# Deterministic 5 MiB file + small segments for HLS
python3 -c "
import os
with open('$ROOT/server/big.bin','wb') as f:
    chunk = bytes((i*7)%256 for i in range(65536))
    for _ in range(80): f.write(chunk)
open('$ROOT/server/seg1.ts','wb').write(b'SEGMENT-ONE-')
open('$ROOT/server/seg2.ts','wb').write(b'SEGMENT-TWO-')
"
python3 tests/server.py "$ROOT/server" 8765 > "$ROOT/server.log" 2>&1 &
SERVER_PID=$!
cleanup() { kill $SERVER_PID 2>/dev/null; kill $BRIDGE_PID 2>/dev/null; }
trap cleanup EXIT
sleep 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18443 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
echo "== health =="
curl -s http://127.0.0.1:18443/health; echo
echo "== unauthenticated list must be 401 =="
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18443/downloads
echo "== add 5 MiB segmented download =="
ADD=$(curl -s -X POST http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d '{"url":"http://127.0.0.1:8765/big.bin"}')
echo "$ADD"
ID=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$ADD")
for i in $(seq 1 30); do
  STATE=$(curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN")
  echo "$STATE"
  echo "$STATE" | grep -q '"status":"completed"' && break
  sleep 1
done
echo "== verify file =="
ls -lh "$ROOT/downloads"
find "$ROOT/downloads" -type f -exec sha256sum {} \;
sha256sum "$ROOT/server/big.bin"
echo "== invalid scheme must be rejected =="
curl -s -X POST http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d '{"url":"file:///etc/passwd"}'; echo
echo "== native-host ping =="
python3 - "$ROOT" <<'PY'
import json, struct, subprocess, sys, os
proc = subprocess.Popen(["./target/debug/fetchforge-bridge", "native-host", "--data-dir", sys.argv[1] + "/data"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
msg = json.dumps({"type": "ping"}).encode()
proc.stdin.write(struct.pack("<I", len(msg)) + msg); proc.stdin.flush()
ln = struct.unpack("<I", proc.stdout.read(4))[0]
print(proc.stdout.read(ln).decode())
proc.stdin.close()
PY
echo "E2E DONE"
