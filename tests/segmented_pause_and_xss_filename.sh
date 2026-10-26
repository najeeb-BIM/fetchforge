#!/usr/bin/env bash
# Top-risk tests:
# 1. Segmented (4-way) slow download, paused mid-way, resumed: must NOT
#    corrupt; it restarts cleanly and the final hash must match.
# 2. A hostile filename must be sanitised by the engine before the UI
#    ever sees it (defence in depth with the textContent UI fix).
set -u
ROOT=/tmp/ff-test-seg
rm -rf "$ROOT"; mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads"
python3 -c "
with open('$ROOT/server/slow.bin','wb') as f:
    chunk = bytes((i*13)%256 for i in range(65536))
    for _ in range(32): f.write(chunk)  # 2 MiB
with open('$ROOT/server/tiny.bin','wb') as f: f.write(b'hello')
"
python3 tests/server.py "$ROOT/server" 8767 > "$ROOT/server.log" 2>&1 &
SPID=$!
cleanup() { kill $SPID 2>/dev/null; kill $BPID 2>/dev/null; }
trap cleanup EXIT
sleep 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18446 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
# --- segmented pause ---
ADD=$(curl -s -X POST http://127.0.0.1:18446/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:8767/slow.bin","filename":"slow.bin"}')
ID=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$ADD")
sleep 3
curl -s -X POST "http://127.0.0.1:18446/downloads/$ID/pause" -H "X-FetchForge-Token: $TOKEN" > /dev/null
sleep 2
echo "after pause:"; curl -s http://127.0.0.1:18446/downloads -H "X-FetchForge-Token: $TOKEN"; echo
echo "final file must NOT exist while paused:"; ls "$ROOT/downloads/Other/" 2>&1 || true
curl -s -X POST "http://127.0.0.1:18446/downloads/$ID/resume" -H "X-FetchForge-Token: $TOKEN" > /dev/null
for i in $(seq 1 60); do
  STATE=$(curl -s http://127.0.0.1:18446/downloads -H "X-FetchForge-Token: $TOKEN")
  echo "$STATE" | grep -q '"status":"completed"' && break
  sleep 1
done
echo "$STATE"
find "$ROOT/downloads" -type f -name "slow.bin" -exec sha256sum {} \;
sha256sum "$ROOT/server/slow.bin"
# --- hostile filename ---
ADD2=$(curl -s -X POST http://127.0.0.1:18446/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:8767/tiny.bin","filename":"<img src=x onerror=alert(1)>.txt"}')
echo "hostile add: $ADD2"
sleep 2
curl -s http://127.0.0.1:18446/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "import sys,json; [print(r['filename']) for r in json.load(sys.stdin) if 'img' in r['filename']]"
echo "SEG_PAUSE_XSS DONE"
