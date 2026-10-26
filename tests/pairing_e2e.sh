#!/usr/bin/env bash
# Pairing + challenge-response + rotation against the real bridge (Linux).
# Exits non-zero on any unexpected status or value. Uses a fresh data dir.
set -u
ROOT=/tmp/ff-test-pair
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads"
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": false}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18446 > "$ROOT/bridge.log" 2>&1 &
PID=$!
trap 'kill $PID 2>/dev/null' EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
check() { # $1 label, $2 expected, $3 actual
  if [ "$2" != "$3" ]; then echo "FAIL: $1 expected $2 got $3"; exit 1; fi
  echo "PASS: $1 -> $3"
}
hmac_py() { # $1 token, $2 nonce -> hex hmac-sha256
  python3 -c "import hmac,hashlib,sys; print(hmac.new(sys.argv[1].encode(), sys.argv[2].encode(), hashlib.sha256).hexdigest())" "$1" "$2"
}
# 1. Challenge: bridge HMAC must match the HMAC computed from the real token.
NONCE="testnonce123"
RESP=$(curl -s -X POST http://127.0.0.1:18446/challenge -d "{\"nonce\":\"$NONCE\"}")
GOT=$(python3 -c "import json,sys; print(json.load(sys.stdin)['hmac'])" <<<"$RESP")
EXPECT=$(hmac_py "$TOKEN" "$NONCE")
check "challenge HMAC matches" "$EXPECT" "$GOT"
check "challenge bad nonce" 400 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/challenge -d '{"nonce":""}')"
# 2. Pair generate + verify (wrong x4, then correct, then reuse fails).
CODE=$(curl -s -X POST http://127.0.0.1:18446/pair/generate -H "X-FetchForge-Token: $TOKEN" | python3 -c "import json,sys; print(json.load(sys.stdin)['code'])")
echo "PASS: pair generate returned a code (not printed)"
for i in 1 2 3 4; do
  check "pair wrong $i" 403 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/pair/verify -d '{"code":"WRONG123"}')"
done
PAIRED_TOKEN=$(curl -s -X POST http://127.0.0.1:18446/pair/verify -d "{\"code\":\"$CODE\"}" | python3 -c "import json,sys; print(json.load(sys.stdin)['token'])")
check "pair correct returns real token" "$TOKEN" "$PAIRED_TOKEN"
check "pair reuse fails" 403 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/pair/verify -d "{\"code\":\"$CODE\"}")"
# 3. Five wrong attempts invalidate the code.
CODE2=$(curl -s -X POST http://127.0.0.1:18446/pair/generate -H "X-FetchForge-Token: $TOKEN" | python3 -c "import json,sys; print(json.load(sys.stdin)['code'])")
for i in 1 2 3 4; do
  curl -s -o /dev/null -X POST http://127.0.0.1:18446/pair/verify -d '{"code":"WRONG123"}'
done
BODY5=$(curl -s -X POST http://127.0.0.1:18446/pair/verify -d '{"code":"WRONG123"}')
echo "$BODY5" | grep -q "invalidated" || { echo "FAIL: 5th wrong attempt should say invalidated, got $BODY5"; exit 1; }
echo "PASS: 5th wrong attempt invalidates"
check "correct code after invalidation fails" 403 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/pair/verify -d "{\"code\":\"$CODE2\"}")"
# 4. Display form (with hyphen) verifies.
CODE3=$(curl -s -X POST http://127.0.0.1:18446/pair/generate -H "X-FetchForge-Token: $TOKEN" | python3 -c "import json,sys; print(json.load(sys.stdin)['code'])")
check "pair display form verifies" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/pair/verify -d "{\"code\":\"$CODE3\"}")"
# 5. Rotation: old token dies, new token works, challenge follows the new token.
check "unpair-all" 200 "$(curl -s -o /dev/null -w '%{http_code}' -X POST http://127.0.0.1:18446/unpair-all -H "X-FetchForge-Token: $TOKEN")"
check "old token after rotation" 401 "$(curl -s -o /dev/null -w '%{http_code}' -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18446/downloads)"
NEW_TOKEN=$(cat "$ROOT/data/bridge-token.txt")
if [ "$NEW_TOKEN" = "$TOKEN" ]; then echo "FAIL: rotation did not change the token"; exit 1; fi
echo "PASS: rotation changed the token"
check "new token after rotation" 200 "$(curl -s -o /dev/null -w '%{http_code}' -H "X-FetchForge-Token: $NEW_TOKEN" http://127.0.0.1:18446/downloads)"
RESP2=$(curl -s -X POST http://127.0.0.1:18446/challenge -d "{\"nonce\":\"$NONCE\"}")
GOT2=$(python3 -c "import json,sys; print(json.load(sys.stdin)['hmac'])" <<<"$RESP2")
check "challenge follows new token" "$(hmac_py "$NEW_TOKEN" "$NONCE")" "$GOT2"
if [ "$GOT2" = "$(hmac_py "$TOKEN" "$NONCE")" ]; then echo "FAIL: challenge still matches old token"; exit 1; fi
echo "PASS: challenge no longer matches old token"
# 6. Port-bind failure: a second bridge on the same port must exit loudly, not pick another port.
SECOND_LOG="$ROOT/second.log"
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18446 > "$SECOND_LOG" 2>&1 &
PID2=$!
sleep 1
if kill -0 $PID2 2>/dev/null; then echo "FAIL: second bridge should have exited on bind failure"; kill $PID2 2>/dev/null; exit 1; fi
grep -q "could not bind" "$SECOND_LOG" || { echo "FAIL: second bridge log should say could not bind"; cat "$SECOND_LOG"; exit 1; }
echo "PASS: second bridge exits loudly on bind failure"
check "first bridge still alive" 200 "$(curl -s -o /dev/null -w '%{http_code}' http://127.0.0.1:18446/health)"
echo "--- bridge log must NOT contain either token ---"
if grep -q "$TOKEN" "$ROOT/bridge.log" || grep -q "$NEW_TOKEN" "$ROOT/bridge.log"; then echo "FAIL: token leaked into bridge log"; exit 1; fi
echo "PASS: token not in bridge log"
echo "PAIRING E2E DONE"
