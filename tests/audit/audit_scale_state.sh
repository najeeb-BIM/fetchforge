#!/usr/bin/env bash
# Audit Phase 2B/2C continued: bandwidth, memory, state-write frequency,
# disk limit, destination deleted, filename collision, token perms,
# rotation while running, settings fuzz. Fresh /tmp only.
set -u
ROOT=/tmp/ff-audit-scale
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/server"
python3 - <<'PY'
root="/tmp/ff-audit-scale/server"
open(root+"/slow.bin","wb").write(bytes(((i*13)%256) for i in range(2*1024*1024)))
open(root+"/small.bin","wb").write(bytes(((i*3)%256) for i in range(200*1024)))
open(root+"/other.bin","wb").write(b"OTHER-FILE-CONTENT")
PY
cd "$(dirname "$0")/../.."
python3 tests/server.py "$ROOT/server" 18841 > "$ROOT/server.log" 2>&1 &
SRV=$!
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18842 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
trap 'kill $BPID $SRV 2>/dev/null' EXIT
sleep 1; TOKEN=$(cat "$ROOT/data/bridge-token.txt")
echo "AUDIT token file perms: $(stat -c %a "$ROOT/data/bridge-token.txt") (expect 600)"
echo "=== bandwidth single-stream: 100 kbps, 200 KiB small.bin, expect ~16s ==="
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": 100, "organise_by_category": false}
EOF
kill $BPID; sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18842 > "$ROOT/bridge-bw.log" 2>&1 &
BPID=$!; sleep 1; TOKEN=$(cat "$ROOT/data/bridge-token.txt")
START=$(date +%s)
curl -s -X POST http://127.0.0.1:18842/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18841/small.bin","filename":"bw.bin"}' > /dev/null
for _ in $(seq 1 80); do ST=$(curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18842/downloads | python3 -c "import json,sys; print(json.load(sys.stdin)[0]['status'])"); case "$ST" in completed|failed) break;; esac; sleep 0.5; done
END=$(date +%s); echo "AUDIT-BW single-stream 100kbps 200KiB status=$ST elapsed=$((END-START))s expected_about_16s"
echo "=== state-write frequency during slow download ==="
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
kill $BPID; sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18842 > "$ROOT/bridge-state.log" 2>&1 &
BPID=$!; sleep 1; TOKEN=$(cat "$ROOT/data/bridge-token.txt")
curl -s -X POST http://127.0.0.1:18842/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18841/slow.bin","filename":"state.bin"}' > /dev/null
LAST=""; COUNT=0
for _ in $(seq 1 40); do M=$(stat -c %Y "$ROOT/data/downloads.json" 2>/dev/null); if [ "$M" != "$LAST" ]; then COUNT=$((COUNT+1)); LAST="$M"; fi; sleep 0.25; done
echo "AUDIT-STATE-WRITES distinct_mtimes_in_10s=$COUNT (each mtime change = at least one full state-file rewrite)"
echo "=== destination deleted mid-download ==="
sleep 1
ID=$(curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18842/downloads | python3 -c "import json,sys; print([r['id'] for r in json.load(sys.stdin) if r['filename']=='state.bin'][0])")
rm -rf "$ROOT/downloads"
sleep 8
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18842/downloads | python3 -c "import json,sys; print([(r['filename'],r['status'],r.get('error')) for r in json.load(sys.stdin) if r['filename']=='state.bin'])"
echo "=== filename collision: two different URLs, same filename ==="
curl -s -X POST http://127.0.0.1:18842/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18841/small.bin","filename":"same.bin"}' > /dev/null
curl -s -X POST http://127.0.0.1:18842/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18841/other.bin","filename":"same.bin"}' > /dev/null
sleep 4
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18842/downloads | python3 -c "import json,sys; print([(r['filename'],r['destination'],r['status']) for r in json.load(sys.stdin) if 'same' in r['filename']])"
find "$ROOT/downloads" -name "same*" -exec ls -l {} \;
echo "=== rotation while a download is running ==="
curl -s -X POST http://127.0.0.1:18842/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18841/slow.bin","filename":"rot.bin"}' > /dev/null
sleep 1
OLD="$TOKEN"
curl -s -X POST http://127.0.0.1:18842/unpair-all -H "X-FetchForge-Token: $OLD"; echo " <- unpair-all with old token"
NEW=$(cat "$ROOT/data/bridge-token.txt")
echo "AUDIT-ROTATION old_token_list=$(curl -s -o /dev/null -w '%{http_code}' -H "X-FetchForge-Token: $OLD" http://127.0.0.1:18842/downloads) new_token_list=$(curl -s -o /dev/null -w '%{http_code}' -H "X-FetchForge-Token: $NEW" http://127.0.0.1:18842/downloads)"
sleep 10
curl -s -H "X-FetchForge-Token: $NEW" http://127.0.0.1:18842/downloads | python3 -c "import json,sys; print([(r['filename'],r['status']) for r in json.load(sys.stdin) if r['filename']=='rot.bin'])"
echo "=== settings fuzz: one bad field discards all settings? ==="
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "/tmp/ff-audit-scale/custom-dir", "max_simultaneous": "not-a-number", "max_segments": 4, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
kill $BPID; sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18842 > "$ROOT/bridge-settings.log" 2>&1 &
BPID=$!; sleep 1; TOKEN=$(cat "$ROOT/data/bridge-token.txt")
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18842/settings
echo
echo "AUDIT-SCALE-STATE DONE"
