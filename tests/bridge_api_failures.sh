#!/usr/bin/env bash
# Bridge API failure cases (run from repo root, bridge already built).
# Exits non-zero on any unexpected status. Uses a fresh data dir.
set -u
ROOT=/tmp/ff-test-api
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18445 > "$ROOT/bridge.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
check() { # $1 = label, $2 = expected, $3 = actual
  if [ "$2" != "$3" ]; then echo "FAIL: $1 expected $2 got $3"; exit 1; fi
  echo "PASS: $1 -> $3"
}
check "health" 200 "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18445/health)"
check "no token" 401 "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18445/downloads)"
check "wrong token" 401 "$(curl -s -o /dev/null -w '%{http_code}' -H 'X-FetchForge-Token: wrong' http://127.0.0.1:18445/downloads)"
check "bad JSON" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18445/downloads -H "X-FetchForge-Token: $TOKEN" -d '{not json')"
check "wrong type url" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18445/downloads -H "X-FetchForge-Token: $TOKEN" -d '{"url": 123}')"
check "missing url" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18445/downloads -H "X-FetchForge-Token: $TOKEN" -d '{}')"
check "bad origin" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H 'Origin: https://evil.example' -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18445/downloads)"
check "bad Host evil" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: evil.example' http://127.0.0.1:18445/health)"
check "bad Host suffix trick" 403 "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: 127.0.0.1.evil.example' http://127.0.0.1:18445/health)"
check "good Host localhost" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: localhost:18445' http://127.0.0.1:18445/health)"
check "good Host 127.0.0.1" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: 127.0.0.1:18445' http://127.0.0.1:18445/health)"
echo "--- bridge log must NOT contain the token ---"
if grep -q "$TOKEN" "$ROOT/bridge.log"; then echo "FAIL: token leaked into bridge log"; cat "$ROOT/bridge.log"; exit 1; fi
echo "PASS: token not in bridge log"
echo "API FAILURES DONE"
