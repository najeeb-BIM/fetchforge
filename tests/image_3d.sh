#!/usr/bin/env bash
# FetchForge Phase 7 e2e: images + 3D assets, against local fixtures only.
# Run from repo root:   bash tests/image_3d.sh
# Exits non-zero on any failed check. Leaves artefacts in /tmp/ff-test-image3d.
set -u
ROOT=/tmp/ff-test-image3d
rm -rf "$ROOT"
mkdir -p "$ROOT/server/textures" "$ROOT/data" "$ROOT/downloads"
# Deterministic fixtures: a real 1x1 PNG, a JPEG (bytes only — the engine
# never decodes images), a GLB, and a glTF with a .bin buffer plus two
# texture files (one nested), one embedded data: image (must be skipped),
# one broken glTF, and one glTF with an unreachable dependency.
python3 - <<'PY'
import base64, json, os
root = "/tmp/ff-test-image3d/server"
png = base64.b64decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
open(root + "/photo.png", "wb").write(png)
jpg = bytes((i * 13) % 256 for i in range(4096))
open(root + "/photo.jpg", "wb").write(jpg)
open(root + "/photo", "wb").write(jpg)  # extension-less image, image/jpeg by content type
open(root + "/robot.glb", "wb").write(bytes((i * 29) % 256 for i in range(2048)))
open(root + "/model.bin", "wb").write(bytes((i * 3) % 256 for i in range(512)))
open(root + "/textures/diffuse.png", "wb").write(png)
open(root + "/logo.png", "wb").write(png)
gltf = {
    "asset": {"version": "2.0"},
    "buffers": [{"uri": "model.bin", "byteLength": 512}],
    "images": [
        {"uri": "textures/diffuse.png"},
        {"uri": "logo.png"},
        {"uri": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg=="},
    ],
}
open(root + "/model.gltf", "w").write(json.dumps(gltf))
open(root + "/broken.gltf", "w").write("{ this is not json")
missing = {"asset": {"version": "2.0"}, "buffers": [{"uri": "missing.bin", "byteLength": 1}]}
open(root + "/missing-dep.gltf", "w").write(json.dumps(missing))
PY
python3 tests/server.py "$ROOT/server" 8766 > "$ROOT/server.log" 2>&1 &
SERVER_PID=$!
cleanup() { kill $SERVER_PID 2>/dev/null; kill $BRIDGE_PID 2>/dev/null; }
trap cleanup EXIT
sleep 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18444 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
BASE=http://127.0.0.1:18444

fail() { echo "FAIL: $1"; exit 1; }

add() { # add <url> -> prints record id
  local resp id
  resp=$(curl -s -X POST "$BASE/downloads" -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d "{\"url\":\"$1\"}")
  echo "$resp" >&2
  id=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$resp") || fail "bad add response for $1: $resp"
  echo "$id"
}

# wait_for <id> <want-status> — polls GET /downloads up to 30 s.
wait_for() {
  local id=$1 want=$2 state
  for _ in $(seq 1 30); do
    state=$(curl -s "$BASE/downloads" -H "X-FetchForge-Token: $TOKEN")
    echo "$state" | python3 -c "
import sys, json
recs = {r['id']: r for r in json.load(sys.stdin)}
r = recs.get('$id')
sys.exit(0 if r and r['status'] == '$want' else 1)" && return 0
    sleep 1
  done
  echo "timed out waiting for $id to become $want; last state:" >&2
  curl -s "$BASE/downloads" -H "X-FetchForge-Token: $TOKEN" >&2
  return 1
}

record_field() { # record_field <id> <field> -> prints the value
  curl -s "$BASE/downloads" -H "X-FetchForge-Token: $TOKEN" | python3 -c "
import sys, json
recs = {r['id']: r for r in json.load(sys.stdin)}
print(recs['$1'].get('$2', ''))"
}

echo "== 1. direct image (photo.png) =="
ID1=$(add "http://127.0.0.1:8766/photo.png")
wait_for "$ID1" completed || fail "photo.png did not complete"
[ -f "$ROOT/downloads/Images/photo.png" ] || fail "photo.png not in Images/"
cmp -s "$ROOT/server/photo.png" "$ROOT/downloads/Images/photo.png" || fail "photo.png bytes differ"
[ "$(record_field "$ID1" category)" = "Images" ] || fail "photo.png category is not Images"

echo "== 2. extension-less image (photo -> photo.jpg via content type) =="
ID2=$(add "http://127.0.0.1:8766/photo")
wait_for "$ID2" completed || fail "extension-less photo did not complete"
[ -f "$ROOT/downloads/Images/photo.jpg" ] || fail "extension-less photo was not saved as photo.jpg"
cmp -s "$ROOT/server/photo.jpg" "$ROOT/downloads/Images/photo.jpg" || fail "photo.jpg bytes differ"
[ "$(record_field "$ID2" filename)" = "photo.jpg" ] || fail "extension-less photo filename is not photo.jpg"

echo "== 3. single-file 3D model (robot.glb) =="
ID3=$(add "http://127.0.0.1:8766/robot.glb")
wait_for "$ID3" completed || fail "robot.glb did not complete"
[ -f "$ROOT/downloads/3D/robot.glb" ] || fail "robot.glb not in 3D/"
cmp -s "$ROOT/server/robot.glb" "$ROOT/downloads/3D/robot.glb" || fail "robot.glb bytes differ"
[ "$(record_field "$ID3" category)" = "3D" ] || fail "robot.glb category is not 3D"

echo "== 4. glTF bundle (model.gltf + .bin + textures, folder preserved) =="
ID4=$(add "http://127.0.0.1:8766/model.gltf")
wait_for "$ID4" completed || fail "model.gltf bundle did not complete"
[ "$(record_field "$ID4" category)" = "3D" ] || fail "model.gltf category is not 3D"
BUNDLE=$(echo "$ROOT"/downloads/3D/model-*)
[ -d "$BUNDLE" ] || fail "no bundle folder under Downloads/3D/"
for f in model.gltf model.bin textures/diffuse.png logo.png; do
  [ -f "$BUNDLE/$f" ] || fail "bundle is missing $f"
done
cmp -s "$ROOT/server/model.gltf" "$BUNDLE/model.gltf" || fail "bundle model.gltf bytes differ"
cmp -s "$ROOT/server/model.bin" "$BUNDLE/model.bin" || fail "bundle model.bin bytes differ"
cmp -s "$ROOT/server/textures/diffuse.png" "$BUNDLE/textures/diffuse.png" || fail "bundle diffuse.png bytes differ"
cmp -s "$ROOT/server/logo.png" "$BUNDLE/logo.png" || fail "bundle logo.png bytes differ"
# The embedded data: image must not have produced a file.
[ "$(find "$BUNDLE" -type f | wc -l)" = "4" ] || fail "bundle has extra files (data: image must not be downloaded)"

echo "== 5. broken glTF must fail honestly =="
ID5=$(add "http://127.0.0.1:8766/broken.gltf")
wait_for "$ID5" failed || fail "broken.gltf was not marked failed"
ERR5=$(record_field "$ID5" error)
echo "broken.gltf error: $ERR5"
echo "$ERR5" | grep -qi "not a valid glTF" || fail "broken.gltf error does not explain the problem"

echo "== 6. unreachable dependency must fail honestly, naming the file =="
ID6=$(add "http://127.0.0.1:8766/missing-dep.gltf")
wait_for "$ID6" failed || fail "missing-dep.gltf was not marked failed"
ERR6=$(record_field "$ID6" error)
echo "missing-dep.gltf error: $ERR6"
echo "$ERR6" | grep -q "missing.bin" || fail "missing-dep error does not name missing.bin"
[ ! -e "$ROOT/downloads/3D/missing-dep-"* ] || fail "a failed bundle must not leave a folder behind"

echo "IMAGE_3D E2E DONE"
