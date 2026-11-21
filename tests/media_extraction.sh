#!/usr/bin/env bash
# FetchForge media extraction proof (v0.2.0). Run from repo root:
#   bash tests/media_extraction.sh
# Exits non-zero on any failed check. Leaves artefacts in /tmp/ff-test-media.
#
# NEEDS NETWORK: it talks to real YouTube and Google Drive URLs, and the
# bridge downloads the pinned yt-dlp Linux binary (3 MB, SHA-256-verified)
# into the test data dir on first use. It does NOT need a Windows machine.
# Test sources (public, stable):
#   - Google Drive: SampleVideo_1280x720_1mb.mp4 (1 MB sample video)
#   - YouTube: Big Buck Bunny 60fps 4K (Blender Foundation, CC-BY)
set -u
ROOT=/tmp/ff-test-media
rm -rf "$ROOT"
mkdir -p "$ROOT/data" "$ROOT/downloads"
DRIVE_URL="https://drive.google.com/file/d/1FFVqpzcgA-T0db2l6Gz4XxxWBnDNYNg3/view"
YT_URL="https://www.youtube.com/watch?v=aqz-KE-bpKQ"
BRIDGE_PID=""
cleanup() { [ -n "$BRIDGE_PID" ] && kill $BRIDGE_PID 2>/dev/null; }
trap cleanup EXIT
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
echo "== start bridge (no yt-dlp anywhere yet: first-use download must happen) =="
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18443 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")

echo "== POST /media/formats for the Google Drive file =="
FORMATS=$(curl -s --max-time 240 -X POST http://127.0.0.1:18443/media/formats \
  -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" \
  -d "{\"url\":\"$DRIVE_URL\"}")
echo "$FORMATS" | python3 -m json.tool | head -40
echo "$FORMATS" | grep -q "SampleVideo_1280x720_1mb" || { echo "FAIL: Drive title missing"; exit 1; }
echo "$FORMATS" | grep -q '"format_id": "source"' || { echo "FAIL: Drive source format missing"; exit 1; }
echo "$FORMATS" | grep -q '"default_format_id": "source"' || { echo "FAIL: Drive default is not source"; exit 1; }
ls -la "$ROOT/data/tools/yt-dlp" || { echo "FAIL: yt-dlp was not downloaded into tools/"; exit 1; }

echo "== download the Drive file, format 22 (720p combined, no merge) =="
ADD=$(curl -s --max-time 240 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" \
  -d "{\"url\":\"$DRIVE_URL\",\"format_id\":\"22\"}")
echo "$ADD"
echo "$ADD" | grep -q '"format_id":"22"' || { echo "FAIL: record does not carry format_id 22"; exit 1; }
for _ in $(seq 1 60); do
  curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "
import json, sys
for r in json.load(sys.stdin):
    if r.get('format_id') == '22' and r['status'] == 'completed':
        sys.exit(0)
    if r.get('format_id') == '22' and r['status'] == 'failed':
        print('FAILED:', r.get('error')); sys.exit(2)
sys.exit(1)" && break
  [ $? -eq 2 ] && exit 1
  sleep 2
done
find "$ROOT/downloads" -type f -name "*.mp4" -exec ls -la {} \;
MP4=$(find "$ROOT/downloads" -type f -name "SampleVideo_1280x720_1mb.mp4" | head -1)
[ -n "$MP4" ] || { echo "FAIL: Drive mp4 not found"; exit 1; }
[ -s "$MP4" ] || { echo "FAIL: Drive mp4 is empty"; exit 1; }

echo "== POST /media/formats for YouTube (Big Buck Bunny) =="
YT_FORMATS=$(curl -s --max-time 240 -X POST http://127.0.0.1:18443/media/formats \
  -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" \
  -d "{\"url\":\"$YT_URL\"}")
echo "$YT_FORMATS" | python3 -c "import json,sys; d=json.load(sys.stdin); print('title:', d['title']); print('formats:', len(d['formats'])); print('default:', d['default_format_id'])"
echo "$YT_FORMATS" | grep -q "Big Buck Bunny" || { echo "FAIL: YouTube title missing"; exit 1; }

echo "== download YouTube 144p video-only (format 160) + best audio: merge path =="
ADD=$(curl -s --max-time 240 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" \
  -d "{\"url\":\"$YT_URL\",\"format_id\":\"160\"}")
echo "$ADD"
echo "$ADD" | grep -q '"needs_merge":true' || { echo "FAIL: record does not flag needs_merge"; exit 1; }
for _ in $(seq 1 90); do
  curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "
import json, sys
for r in json.load(sys.stdin):
    if r.get('format_id') == '160' and r['status'] == 'completed':
        sys.exit(0)
    if r.get('format_id') == '160' and r['status'] == 'failed':
        print('FAILED:', r.get('error')); sys.exit(2)
sys.exit(1)" && break
  [ $? -eq 2 ] && exit 1
  sleep 2
done
BBB=$(find "$ROOT/downloads" -type f -name "Big Buck Bunny*.mp4" | head -1)
[ -n "$BBB" ] || { echo "FAIL: merged BBB mp4 not found"; find "$ROOT/downloads" -type f; exit 1; }
ls -la "$BBB"
ffprobe -v error -show_entries stream=codec_type -of csv "$BBB" | sort | uniq -c

echo "== auto-pick: YouTube URL with no format_id records the default =="
ADD=$(curl -s --max-time 240 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" -H "Content-Type: application/json" \
  -d "{\"url\":\"$YT_URL\"}")
echo "$ADD"
echo "$ADD" | grep -q '"format_id":"' || { echo "FAIL: auto-pick did not record a format_id"; exit 1; }
# Cancel it immediately: the point is the recorded choice, not a 100 MB download.
ID=$(echo "$ADD" | python3 -c "import json,sys; print(json.load(sys.stdin)['id'])")
curl -s -X POST "http://127.0.0.1:18443/downloads/$ID/cancel" -H "X-FetchForge-Token: $TOKEN"; echo

echo "MEDIA EXTRACTION: ALL CHECKS PASSED"
