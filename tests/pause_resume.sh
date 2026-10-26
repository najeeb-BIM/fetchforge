#!/usr/bin/env bash
# Pause/resume proof: single-stream (max_segments=1) slow download.
# Run from repo root after building the bridge: bash tests/pause_resume.sh
set -u
ROOT=/tmp/ff-test-pr
rm -rf "$ROOT"
mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads"
python3 -c "
with open('$ROOT/server/slow.bin','wb') as f:
    chunk = bytes((i*11)%256 for i in range(65536))
    for _ in range(8): f.write(chunk)
"
python3 tests/server.py "$ROOT/server" 8766 > "$ROOT/server.log" 2>&1 &
SERVER_PID=$!
cleanup() { kill $SERVER_PID 2>/dev/null; kill $BRIDGE_PID 2>/dev/null; }
trap cleanup EXIT
sleep 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18444 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
ADD=$(curl -s -X POST http://127.0.0.1:18444/downloads -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d '{"url":"http://127.0.0.1:8766/slow.bin","filename":"slow.bin"}')
echo "ADD: $ADD"
ID=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$ADD")
sleep 2
curl -s "http://127.0.0.1:18444/downloads" -H "X-FetchForge-Token: $TOKEN"; echo " <- before pause"
curl -s -X POST "http://127.0.0.1:18444/downloads/$ID/pause" -H "X-FetchForge-Token: $TOKEN"; echo " <- pause sent"
sleep 2
curl -s "http://127.0.0.1:18444/downloads" -H "X-FetchForge-Token: $TOKEN"; echo " <- after pause (expect paused)"
curl -s -X POST "http://127.0.0.1:18444/downloads/$ID/resume" -H "X-FetchForge-Token: $TOKEN"; echo " <- resume sent"
for i in $(seq 1 40); do
  STATE=$(curl -s "http://127.0.0.1:18444/downloads" -H "X-FetchForge-Token: $TOKEN")
  echo "$STATE"
  echo "$STATE" | grep -q '"status":"completed"' && break
  sleep 1
done
find "$ROOT/downloads" -type f -exec sha256sum {} \;
sha256sum "$ROOT/server/slow.bin"
echo "PAUSE_RESUME DONE"
