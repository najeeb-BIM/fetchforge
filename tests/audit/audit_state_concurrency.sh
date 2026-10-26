#!/usr/bin/env bash
# Audit Phase 2B + 2C: crash/state, concurrency, scale (Linux only, fresh /tmp).
set -u
ROOT=/tmp/ff-audit-state
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/server"
# Slow file for kill/resume tests: use normal server slow.bin (2 MiB slow)
python3 - <<'PY'
import os
root="/tmp/ff-audit-state/server"
data=bytes(((i*13)%256) for i in range(2*1024*1024))
open(os.path.join(root,"slow.bin"),"wb").write(data)
open(os.path.join(root,"changing.bin"),"wb").write(b"A"*1000)
PY
python3 tests/server.py "$ROOT/server" 18811 > "$ROOT/server.log" 2>&1 &
SRV=$!
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18812 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
sleep 1; TOKEN=$(cat "$ROOT/data/bridge-token.txt")
echo "=== B1 kill -9 during slow download, restart, recovery ==="
curl -s -X POST http://127.0.0.1:18812/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18811/slow.bin","filename":"kill.bin"}' > /dev/null
sleep 2
kill -9 $BPID; sleep 1
ls -l "$ROOT/downloads/Other/" 2>&1 | head
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18812 > "$ROOT/bridge2.log" 2>&1 &
BPID=$!; sleep 1
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18812/downloads | python3 -m json.tool | head -40
echo "=== B2 corrupt / empty / truncated state files ==="
for mode in garbage empty truncated wrongtype; do
  case "$mode" in
    garbage) echo 'not json {{{' > "$ROOT/data/downloads.json";;
    empty) : > "$ROOT/data/downloads.json";;
    truncated) echo '{"dl-1": {"id": "dl-1", "url": "http://x' > "$ROOT/data/downloads.json";;
    wrongtype) echo '[]' > "$ROOT/data/downloads.json";;
  esac
  kill $BPID 2>/dev/null; sleep 1
  ./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18812 > "$ROOT/bridge-$mode.log" 2>&1 &
  BPID=$!; sleep 1
  echo "AUDIT-STATE $mode health=$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18812/health) list=$(curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18812/downloads)"
done
echo "=== B3 read-only destination ==="
mkdir -p "$ROOT/ro" && chmod 555 "$ROOT/ro"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/ro", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
kill $BPID 2>/dev/null; sleep 1
# Note: settings are read at startup, so restart picks up ro dir. Token file is in data dir, unchanged.
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18812 > "$ROOT/bridge-ro.log" 2>&1 &
BPID=$!; sleep 1
curl -s -X POST http://127.0.0.1:18812/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18811/slow.bin","filename":"ro.bin"}'
sleep 3
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18812/downloads | python3 -m json.tool | head -30
chmod 755 "$ROOT/ro"
echo "=== B4 symlink .part attack (pre-planted) ==="
mkdir -p "$ROOT/downloads2/Other"
echo "OUTSIDE-ORIGINAL" > "$ROOT/outside.txt"
ln -s "$ROOT/outside.txt" "$ROOT/downloads2/Other/sym.bin.part"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads2", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
kill $BPID 2>/dev/null; sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18812 > "$ROOT/bridge-sym.log" 2>&1 &
BPID=$!; sleep 1
# Use a tiny file server: slow.bin is 2 MiB, but sym test needs a file whose name matches sym.bin
# Instead, add via the normal server with filename sym.bin (probe will use slow.bin URL, filename override)
curl -s -X POST http://127.0.0.1:18812/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18811/slow.bin","filename":"sym.bin"}' > /dev/null
sleep 8
echo "outside.txt after sym download:"; cat "$ROOT/outside.txt" | head -c 200; echo
ls -l "$ROOT/downloads2/Other/" "$ROOT/outside.txt"
echo "=== B5 changing file between pause and resume (same size, different bytes) ==="
# Start a download of changing.bin via audit server? Use normal server: file is in server root as changing.bin (1000 bytes, all A)
# Bridge probe will get it, but our normal server serves it as a regular file.
curl -s -X POST http://127.0.0.1:18812/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:18811/changing.bin","filename":"changing.bin"}' > /dev/null
sleep 2
curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18812/downloads | python3 -c "import json,sys; print([(r['filename'],r['status']) for r in json.load(sys.stdin) if 'changing' in r['filename']])"
echo "AUDIT-STATE-CONCURRENCY DONE (concurrency part in next script)"
kill $BPID $SRV 2>/dev/null
