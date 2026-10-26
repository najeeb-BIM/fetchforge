#!/usr/bin/env bash
# Audit Phase 2A: hostile servers against the real bridge (Linux only).
# Prints one line per case: AUDIT-RESULT <case> <status> <detail>.
# Does NOT modify product code. Uses fresh /tmp dirs only.
set -u
ROOT=/tmp/ff-audit-hostile
rm -rf "$ROOT"; mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/server"
python3 tests/audit/server_audit.py "$ROOT/server" 18771 18772 > "$ROOT/audit-server.log" 2>&1 &
ASRV=$!
python3 tests/audit/server_audit.py "$ROOT/server" 18772 18772 > "$ROOT/secret-server.log" 2>&1 &
SSRV=$!
# secret server serves /secret on 18772; audit server redirects there.
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18773 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
cleanup() { kill $BPID $ASRV $SSRV 2>/dev/null; }
trap cleanup EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
wait_status() { # $1 url -> prints status after up to 30s
  local url="$1" st=""
  for _ in $(seq 1 60); do
    st=$(curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18773/downloads | python3 -c "import json,sys; recs=json.load(sys.stdin); print(next((r['status'] for r in recs if r['url']=='$url'), 'missing'))" 2>/dev/null)
    case "$st" in completed|failed|paused|cancelled) echo "$st"; return;; esac
    sleep 0.5
  done
  echo "timeout($st)"
}
get_rec() { # $1 url -> full json record
  curl -s -H "X-FetchForge-Token: $TOKEN" http://127.0.0.1:18773/downloads | python3 -c "import json,sys; recs=json.load(sys.stdin); print(json.dumps(next((r for r in recs if r['url']=='$1'), {})))"
}
run_case() { # $1 label, $2 url, $3 filename-or-empty
  local label="$1" url="$2" fn="$3"
  if [ -n "$fn" ]; then curl -s -o /dev/null -X POST http://127.0.0.1:18773/downloads -H "X-FetchForge-Token: $TOKEN" -d "{\"url\":\"$url\",\"filename\":\"$fn\"}"; else curl -s -o /dev/null -X POST http://127.0.0.1:18773/downloads -H "X-FetchForge-Token: $TOKEN" -d "{\"url\":\"$url\"}"; fi
  local st; st=$(wait_status "$url")
  local rec; rec=$(get_rec "$url")
  echo "AUDIT-RESULT $label status=$st rec=$rec"
}
run_case lie-length "http://127.0.0.1:18771/lie-length" "lie.bin"
run_case ignore-range-2mb "http://127.0.0.1:18771/ignore-range" "ignore.bin"
run_case mismatch-range "http://127.0.0.1:18771/mismatch-range" "mismatch.bin"
run_case short-segment "http://127.0.0.1:18771/short-segment" "shortseg.bin"
run_case redirect-loop "http://127.0.0.1:18771/redirect-loop" "loop.bin"
run_case redirect-file "http://127.0.0.1:18771/redirect-file" "redfile.bin"
run_case redirect-ftp "http://127.0.0.1:18771/redirect-ftp" "redftp.bin"
run_case redirect-private "http://127.0.0.1:18771/redirect-private" "redpriv.bin"
run_case gzip-broken "http://127.0.0.1:18771/gzip-broken" "gzip.bin"
run_case chunked "http://127.0.0.1:18771/chunked" "chunked.bin"
run_case reset-mid "http://127.0.0.1:18771/reset-mid" "reset.bin"
run_case zero "http://127.0.0.1:18771/zero" "zero.bin"
run_case disp-traversal "http://127.0.0.1:18771/disp-traversal" ""
run_case disp-con "http://127.0.0.1:18771/disp-con" ""
run_case huge-length "http://127.0.0.1:18771/huge-length" "huge.bin"
echo "--- files in downloads tree ---"
find "$ROOT/downloads" -type f -o -type l | head -50
echo "--- check mismatch/shortseg content (first bytes + zero count) ---"
for f in "$ROOT"/downloads/*/mismatch.bin "$ROOT"/downloads/*/shortseg.bin; do
  if [ -f "$f" ]; then echo "$f size=$(stat -c%s "$f")"; python3 -c "import sys; d=open(sys.argv[1],'rb').read(); print('  unique bytes:', sorted(set(d))[:10], 'zero bytes:', d.count(0))" "$f"; fi
done
echo "--- check for traversal escape ---"
ls -l "$ROOT/evil.txt" 2>&1 | head -2; ls "$ROOT/downloads"/*/evil.txt 2>&1 | head -2
echo "--- check redirect-private content ---"
for f in "$ROOT"/downloads/*/redpriv.bin; do if [ -f "$f" ]; then echo "$f:"; cat "$f"; echo; fi; done
echo "--- huge file disk usage (ls vs du) ---"
for f in "$ROOT"/downloads/*/huge.bin*; do if [ -e "$f" ]; then echo "$f ls=$(stat -c%s "$f") du=$(du -B1 "$f" | cut -f1)"; fi; done
echo "AUDIT-HOSTILE DONE"
