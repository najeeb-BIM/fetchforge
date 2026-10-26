#!/usr/bin/env bash
# P02 (F07): a pre-planted symlink .part must NEVER let a download write
# through it to a file outside the download folder. The symlink itself
# is removed, the download starts fresh into a regular file, and the
# outside file stays byte-for-byte unchanged. Run from repo root.
set -u
ROOT=/tmp/ff-test-symlink
rm -rf "$ROOT"
mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads/Other"
python3 -c "
with open('$ROOT/server/sym.bin','wb') as f:
    f.write(bytes((i*13)%256 for i in range(65536)) * 4)
"
python3 tests/server.py "$ROOT/server" 8766 > "$ROOT/server.log" 2>&1 &
SERVER_PID=$!
trap 'kill $SERVER_PID 2>/dev/null; kill $BRIDGE_PID 2>/dev/null' EXIT
sleep 1
printf 'OUTSIDE-ORIGINAL\n' > "$ROOT/outside.txt"
ln -s "$ROOT/outside.txt" "$ROOT/downloads/Other/sym.bin.part"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18447 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
curl -s -X POST http://127.0.0.1:18447/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url":"http://127.0.0.1:8766/sym.bin","filename":"sym.bin"}' > /dev/null
for i in $(seq 1 30); do
  STATE=$(curl -s http://127.0.0.1:18447/downloads -H "X-FetchForge-Token: $TOKEN")
  echo "$STATE" | grep -q '"status":"completed"' && break
  sleep 1
done
echo "outside size after: $(wc -c < "$ROOT/outside.txt") bytes"
[ "$(wc -c < "$ROOT/outside.txt")" -eq 17 ] && grep -q "OUTSIDE-ORIGINAL" "$ROOT/outside.txt" || { echo "FAIL: outside file was written through the symlink"; ls -l "$ROOT/downloads/Other/" "$ROOT/outside.txt"; exit 1; }
[ -L "$ROOT/downloads/Other/sym.bin.part" ] && { echo "FAIL: symlink .part still present"; exit 1; }
[ -f "$ROOT/downloads/Other/sym.bin" ] || { echo "FAIL: downloaded file missing"; ls -l "$ROOT/downloads/Other/"; exit 1; }
sha256sum "$ROOT/server/sym.bin" "$ROOT/downloads/Other/sym.bin"
[ "$(sha256sum "$ROOT/server/sym.bin" | cut -d' ' -f1)" = "$(sha256sum "$ROOT/downloads/Other/sym.bin" | cut -d' ' -f1)" ] || { echo "FAIL: downloaded file hash mismatch"; exit 1; }
echo "SYMLINK_PART PASS"
