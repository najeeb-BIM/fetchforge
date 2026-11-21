#!/usr/bin/env bash
# Phase 10: a failed probe must not swallow yt-dlp's real error.
#
# Fixture server 403s the bridge probe only (its User-Agent is
# "FetchForge/0.2") and serves a plain HTML page with no media to
# everyone else — yt-dlp sends a Chrome UA, reads the page, and fails
# with its own error ("Unsupported URL" from the generic extractor).
# The bridge probe therefore fails with "HTTP error 403"; yt-dlp is
# installed, so extraction is tried and fails with its own error.
# The Failed record must carry the yt-dlp error, not the probe's
# 403: the 403 says nothing about why the page cannot be downloaded,
# the yt-dlp error does.
#
# Exits non-zero on failure. Leaves artefacts in /tmp/ff-test-probeerr.
set -u
ROOT=/tmp/ff-test-probeerr
rm -rf "$ROOT"; mkdir -p "$ROOT/data/tools" "$ROOT/downloads"
if [ -x /tmp/ff-test-media/data/tools/yt-dlp ]; then
  cp /tmp/ff-test-media/data/tools/yt-dlp "$ROOT/data/tools/yt-dlp"
elif command -v yt-dlp >/dev/null 2>&1; then
  cp "$(command -v yt-dlp)" "$ROOT/data/tools/yt-dlp"
else
  echo "SKIP: no yt-dlp binary available to stage"; exit 0
fi
python3 - 8769 <<'PY' > "$ROOT/server.log" 2>&1 &
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer
class H(BaseHTTPRequestHandler):
    def _handle(self):
        ua = self.headers.get("User-Agent", "")
        if ua.startswith("FetchForge/"):
            self.send_response(403)
            self.send_header("Content-Length", "0")
            self.end_headers()
        else:
            body = b"<html><head><title>plain page</title></head><body>no media here</body></html>"
            self.send_response(200)
            self.send_header("Content-Type", "text/html")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            if self.command == "GET":
                self.wfile.write(body)
    do_GET = _handle
    do_HEAD = _handle
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", int(sys.argv[1])), H).serve_forever()
PY
SRV=$!
cd "$(dirname "$0")/.." || exit 1
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 1, "max_retries": 0, "bandwidth_kbps": null, "organise_by_category": true}
EOF
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18781 > "$ROOT/bridge.log" 2>&1 &
BPID=$!
cleanup() { kill $BPID $SRV 2>/dev/null; }
trap cleanup EXIT
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
ADD=$(curl -s -X POST http://127.0.0.1:18781/downloads -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" -d '{"url":"http://127.0.0.1:8769/plain.html"}')
echo "$ADD"
ID=$(python3 -c "import sys,json; print(json.load(sys.stdin)['id'])" <<< "$ADD")
ERR=""
for _ in $(seq 1 40); do
  STATE=$(curl -s http://127.0.0.1:18781/downloads -H "X-FetchForge-Token: $TOKEN")
  ERR=$(python3 -c "import sys,json; r=[d for d in json.load(sys.stdin) if d['id']=='$ID'][0]; print((r.get('error') or ''))" <<< "$STATE")
  ST=$(python3 -c "import sys,json; r=[d for d in json.load(sys.stdin) if d['id']=='$ID'][0]; print(r['status'])" <<< "$STATE")
  [ "$ST" = "failed" ] && break
  sleep 1
done
echo "record error: $ERR"
case "$ERR" in
  *"HTTP error 403"*|*"Access denied by the server (401/403)"*) echo "FAIL: record carries the probe error, not yt-dlp's error"; exit 1;;
  *"not a supported video"*|*"needs sign-in or is private"*|*"No video formats"*|*"DRM-protected"*|*"was not found (404)"*|*"Could not reach the page"*|*"took too long"*|*"Access denied by the site (403)"*) echo "PROBE_ERROR_SURFACES PASS";;
  *) echo "FAIL: unexpected error text: $ERR"; exit 1;;
esac
