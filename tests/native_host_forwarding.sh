#!/usr/bin/env bash
# FetchForge native-host forwarding proof (v0.2.0). Run from repo root:
#   bash tests/native_host_forwarding.sh
# Exits non-zero on any failed check. Leaves artefacts in /tmp/ff-test-native.
#
# Proves: a download sent to `fetchforge-bridge native-host` is forwarded
# to the long-running bridge over loopback HTTP (so it survives the host
# process exiting), and a detached bridge is spawned when none is running.
set -u
ROOT=/tmp/ff-test-native
rm -rf "$ROOT"
mkdir -p "$ROOT/server" "$ROOT/data" "$ROOT/downloads"
python3 -c "
with open('$ROOT/server/small.dat','wb') as f:
    f.write(bytes((i*13)%256 for i in range(65536)))
with open('$ROOT/server/small2.dat','wb') as f:
    f.write(bytes((i*29)%256 for i in range(32768)))
"
python3 tests/server.py "$ROOT/server" 8766 > "$ROOT/server.log" 2>&1 &
SERVER_PID=$!
BRIDGE_PID=""
cleanup() {
  kill $SERVER_PID 2>/dev/null
  [ -n "$BRIDGE_PID" ] && kill $BRIDGE_PID 2>/dev/null
  # Any bridge the native host spawned detached (same data dir).
  pkill -f "fetchforge-bridge serve --data-dir $ROOT/data" 2>/dev/null
}
trap cleanup EXIT
sleep 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF

# Send one length-prefixed native message to a native-host process and
# print its reply (mirrors Chrome's sendNativeMessage framing).
native_send() {
  python3 - "$1" "$2" <<'PYEOF'
import json, struct, subprocess, sys
msg = json.loads(sys.argv[1])
p = subprocess.run(
    ["./target/debug/fetchforge-bridge", "native-host", "--data-dir", sys.argv[2]],
    input=struct.pack("<I", len(json.dumps(msg))) + json.dumps(msg).encode(),
    capture_output=True, timeout=60,
)
out = p.stdout
if len(out) < 4:
    print("NO-REPLY stderr=" + p.stderr.decode(errors="replace")[:500]); sys.exit(1)
n = struct.unpack("<I", out[:4])[0]
print(out[4:4+n].decode())
PYEOF
}

wait_completed() {
  # $1 = filename to wait for (per-record check, not a global grep:
  # an earlier completed record must not satisfy this wait).
  local token
  token=$(cat "$ROOT/data/bridge-token.txt")
  for _ in $(seq 1 30); do
    if curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $token" | python3 -c "
import json, sys
for r in json.load(sys.stdin):
    if r['filename'] == '$1' and r['status'] == 'completed':
        sys.exit(0)
sys.exit(1)"; then
      return 0
    fi
    sleep 1
  done
  echo "TIMEOUT waiting for $1 to complete"
  curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $token"; echo
  return 1
}

echo "== start bridge =="
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18443 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
curl -s http://127.0.0.1:18443/health; echo

echo "== native ping =="
REPLY=$(native_send '{"type":"ping"}' "$ROOT/data")
echo "$REPLY"
echo "$REPLY" | grep -q '"type":"pong"' || { echo "FAIL: ping did not pong"; exit 1; }

echo "== native addDownload with bridge running (must forward) =="
REPLY=$(native_send '{"type":"addDownload","url":"http://127.0.0.1:8766/small.dat"}' "$ROOT/data")
echo "$REPLY"
echo "$REPLY" | grep -q '"type":"added"' || { echo "FAIL: addDownload was not added"; exit 1; }
echo "$REPLY" | grep -q '"via":"bridge"' || { echo "FAIL: reply did not come via the bridge"; exit 1; }
wait_completed "small.dat" || exit 1
find "$ROOT/downloads" -name "small.dat" -exec sha256sum {} \;
sha256sum "$ROOT/server/small.dat"

echo "== stop the bridge; native host must spawn a detached one =="
kill $BRIDGE_PID 2>/dev/null; BRIDGE_PID=""; sleep 1
curl -s --max-time 2 http://127.0.0.1:18443/health && { echo "FAIL: bridge still up"; exit 1; } || true
REPLY=$(native_send '{"type":"addDownload","url":"http://127.0.0.1:8766/small2.dat"}' "$ROOT/data")
echo "$REPLY"
echo "$REPLY" | grep -q '"type":"added"' || { echo "FAIL: addDownload was not added (spawn path)"; exit 1; }
echo "$REPLY" | grep -q '"bridge_started":true' || { echo "FAIL: reply did not report bridge_started"; exit 1; }
sleep 1
curl -s http://127.0.0.1:18443/health; echo
wait_completed "small2.dat" || exit 1
find "$ROOT/downloads" -name "small2.dat" -exec sha256sum {} \;
sha256sum "$ROOT/server/small2.dat"

echo "NATIVE HOST FORWARDING: ALL CHECKS PASSED"
