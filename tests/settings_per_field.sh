#!/usr/bin/env bash
# P05 (F11): one bad field in settings.json must NOT discard all settings.
# Valid fields keep their values, bad fields fall back to defaults, and a
# visible warning names the bad fields. Run from repo root, bridge built.
set -u
ROOT=/tmp/ff-test-settings
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/custom-downloads"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/custom-downloads", "max_simultaneous": "not-a-number", "max_segments": 1, "max_retries": 0, "bandwidth_kbps": "fast", "organise_by_category": false}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18446 > "$ROOT/bridge.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
S=$(curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18446/settings)
echo "settings=$S"
echo "$S" | grep -q "\"download_dir\":\"$ROOT/custom-downloads\"" || { echo "FAIL: valid download_dir was discarded"; exit 1; }
echo "$S" | grep -q '"max_simultaneous":3' || { echo "FAIL: bad max_simultaneous did not fall back to 3"; exit 1; }
echo "$S" | grep -q '"max_segments":1' || { echo "FAIL: valid max_segments was discarded"; exit 1; }
echo "$S" | grep -q '"bandwidth_kbps":null' || { echo "FAIL: bad bandwidth_kbps did not fall back to null"; exit 1; }
echo "$S" | grep -q '"organise_by_category":false' || { echo "FAIL: valid organise_by_category was discarded"; exit 1; }
grep -q "WARNING" "$ROOT/bridge.log" && grep -q "max_simultaneous" "$ROOT/bridge.log" || { echo "FAIL: no warning naming max_simultaneous"; cat "$ROOT/bridge.log"; exit 1; }
echo "SETTINGS_PER_FIELD PASS"
