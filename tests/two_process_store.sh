#!/usr/bin/env bash
# F03: two processes (app + bridge, simulated here by two bridges)
# sharing one downloads.json must not lose each other's records.
# Before the fix, the second bridge's save overwrites the first
# bridge's record on disk (last writer wins). After the fix
# (file lock + reload-before-save), disk must contain BOTH records,
# and a restarted bridge must list both.
set -u
ROOT=/tmp/ff-two-process; rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
cd "$(dirname "$0")/.." || exit 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18821 > "$ROOT/b1.log" 2>&1 & B1=$!
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18822 > "$ROOT/b2.log" 2>&1 & B2=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
ID1=$(curl -s -X POST http://127.0.0.1:18821/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:9/one.zip"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
ID2=$(curl -s -X POST http://127.0.0.1:18822/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:9/two.zip"}' | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
echo "ID1=$ID1 ID2=$ID2"
sleep 1
echo "DISK_IDS=$(python3 -c "import json; print(sorted(json.load(open('$ROOT/data/downloads.json')).keys()))")"
DISK_HAS_BOTH=$(python3 -c "import json,sys; d=json.load(open('$ROOT/data/downloads.json')); print('yes' if '$ID1' in d and '$ID2' in d else 'no')")
echo "DISK_HAS_BOTH=$DISK_HAS_BOTH"
# Restart bridge 1: its in-memory list must now contain both records.
kill $B1 2>/dev/null; sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18821 > "$ROOT/b1r.log" 2>&1 & B1=$!
sleep 1
LIST=$(curl -s http://127.0.0.1:18821/downloads -H "X-FetchForge-Token: $TOKEN")
echo "RESTART_LIST_HAS_BOTH=$(python3 -c "import json,sys; d=json.load(sys.stdin); ids={r['id'] for r in d}; print('yes' if '$ID1' in ids and '$ID2' in ids else 'no')" <<<"$LIST")"
kill $B1 $B2 2>/dev/null
if [ "$DISK_HAS_BOTH" = "yes" ]; then echo "TWO_PROCESS_STORE PASS"; else echo "TWO_PROCESS_STORE FAIL"; exit 1; fi
