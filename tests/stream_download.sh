#!/usr/bin/env bash
# FetchForge HLS/DASH stream proof (v0.2.0, Phase 5). Run from repo root:
#   bash tests/stream_download.sh
# Exits non-zero on any failed check. Leaves artefacts in /tmp/ff-test-stream.
#
# NO NETWORK NEEDED: all streams are served by a local python
# http.server from generated fixtures, so this also runs on the
# sandbox VM where reqwest cannot reach public HTTPS (MITM proxy).
# ffmpeg is required for the merge checks (separate-audio HLS and
# DASH video+audio); those checks are skipped when it is missing.
set -u
ROOT=/tmp/ff-test-stream
rm -rf "$ROOT"
mkdir -p "$ROOT/data" "$ROOT/downloads" "$ROOT/fx"
BRIDGE_PID=""
SERVER_PID=""
cleanup() {
  [ -n "$BRIDGE_PID" ] && kill $BRIDGE_PID 2>/dev/null
  [ -n "$SERVER_PID" ] && kill $SERVER_PID 2>/dev/null
}
trap cleanup EXIT
cat > "$ROOT/data/settings.json" <<EOF
{"download_dir": "$ROOT/downloads", "max_simultaneous": 3, "max_segments": 4, "max_retries": 3, "bandwidth_kbps": null, "organise_by_category": true}
EOF
HAVE_FFMPEG=0
command -v ffmpeg >/dev/null 2>&1 && HAVE_FFMPEG=1
[ "$HAVE_FFMPEG" -eq 1 ] || echo "NOTE: ffmpeg not found; merge checks will be skipped"

echo "== generate fixtures =="
cd "$ROOT/fx" || exit 1
if [ "$HAVE_FFMPEG" -eq 1 ]; then
  ffmpeg -y -v error -f lavfi -i "color=c=black:s=64x64:d=2" -c:v libx264 -pix_fmt yuv420p low_full.ts
  ffmpeg -y -v error -f lavfi -i "color=c=red:s=64x64:d=2" -c:v libx264 -pix_fmt yuv420p mid_full.ts
  ffmpeg -y -v error -f lavfi -i "color=c=blue:s=64x64:d=2" -c:v libx264 -pix_fmt yuv420p high_full.ts
  ffmpeg -y -v error -f lavfi -i "sine=frequency=440:d=2" -c:a aac audio_full.ts
  ffmpeg -y -v error -f lavfi -i "color=c=green:s=64x64:d=2" -c:v libx264 -pix_fmt yuv420p v360_full.mp4
  ffmpeg -y -v error -f lavfi -i "color=c=yellow:s=64x64:d=2" -c:v libx264 -pix_fmt yuv420p v720_full.mp4
  ffmpeg -y -v error -f lavfi -i "sine=frequency=880:d=2" -c:a aac a64k_full.m4a
else
  printf 'LOW-VIDEO-BYTES-0123456789' > low_full.ts
  printf 'MID-VIDEO-BYTES-0123456789' > mid_full.ts
  printf 'HIGH-VIDEO-BYTES-0123456789' > high_full.ts
  printf 'AUDIO-BYTES-0123456789' > audio_full.ts
  printf 'V360-BYTES-0123456789' > v360_full.mp4
  printf 'V720-BYTES-0123456789' > v720_full.mp4
  printf 'A64K-BYTES-0123456789' > a64k_full.m4a
fi
# Split each media file into 2 halves served as 2 segments.
split_half() { # split_half <file> <prefix>
  local size half
  size=$(stat -c%s "$1")
  half=$((size / 2))
  dd if="$1" of="${2}_1" bs=1 count=$half status=none
  dd if="$1" of="${2}_2" bs=1 skip=$half status=none
}
split_half low_full.ts low
split_half mid_full.ts mid
split_half high_full.ts high
split_half audio_full.ts aud
mv low_1 low_1.ts; mv low_2 low_2.ts
mv mid_1 mid_1.ts; mv mid_2 mid_2.ts
mv high_1 high_1.ts; mv high_2 high_2.ts
mv aud_1 aud_1.ts; mv aud_2 aud_2.ts

# HLS: master with 3 variants; 1080p carries a separate audio group.
cat > master.m3u8 <<'EOF'
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360
low.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720
mid.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,AUDIO="aac"
high.m3u8
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aac",NAME="English",DEFAULT=YES,URI="audio.m3u8"
EOF
mk_media() { # mk_media <playlist> <seg1> <seg2>
  cat > "$1" <<EOF
#EXTM3U
#EXT-X-TARGETDURATION=2
#EXT-X-MEDIA-SEQUENCE:0
#EXTINF:1.0,
$2
#EXTINF:1.0,
$3
#EXT-X-ENDLIST
EOF
}
mk_media low.m3u8 low_1.ts low_2.ts
mk_media mid.m3u8 mid_1.ts mid_2.ts
mk_media high.m3u8 high_1.ts high_2.ts
mk_media audio.m3u8 aud_1.ts aud_2.ts

# DASH: 2 video reps + 1 audio rep, 1 s segments, 2 s duration.
# Each rep file is split into init (first third) + 2 segments.
mkdir -p v360 v720 a64k
split3() { # split3 <file> <dir> <id>
  local size third
  size=$(stat -c%s "$1")
  third=$((size / 3))
  dd if="$1" of="$2/${3}_0.m4v" bs=1 count=$third status=none
  dd if="$1" of="$2/${3}_1.m4v" bs=1 skip=$third count=$third status=none
  dd if="$1" of="$2/${3}_2.m4v" bs=1 skip=$((third * 2)) status=none
}
split3 v360_full.mp4 v360 v360
split3 v720_full.mp4 v720 v720
# Audio segments use .m4a names per the template below.
size=$(stat -c%s a64k_full.m4a); third=$((size / 3))
dd if=a64k_full.m4a of=a64k/a64k_0.m4a bs=1 count=$third status=none
dd if=a64k_full.m4a of=a64k/a64k_1.m4a bs=1 skip=$third count=$third status=none
dd if=a64k_full.m4a of=a64k/a64k_2.m4a bs=1 skip=$((third * 2)) status=none
cat > bbb.mpd <<'EOF'
<MPD type="static" mediaPresentationDuration="PT2S" xmlns="urn:mpeg:dash:schema:mpd:2011">
 <Period>
  <AdaptationSet mimeType="video/mp4" contentType="video">
   <SegmentTemplate duration="30" timescale="30" media="$RepresentationID$/$RepresentationID$_$Number$.m4v" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4v"/>
   <Representation id="v360" codecs="avc1.64001e" bandwidth="1000000" width="640" height="360" frameRate="30"/>
   <Representation id="v720" codecs="avc1.64001f" bandwidth="4000000" width="1280" height="720" frameRate="30"/>
  </AdaptationSet>
  <AdaptationSet mimeType="audio/mp4" contentType="audio">
   <SegmentTemplate duration="48000" timescale="48000" media="$RepresentationID$/$RepresentationID$_$Number$.m4a" startNumber="1" initialization="$RepresentationID$/$RepresentationID$_0.m4a"/>
   <Representation id="a64k" codecs="mp4a.40.5" bandwidth="67071"/>
  </AdaptationSet>
 </Period>
</MPD>
EOF
sed 's|<AdaptationSet mimeType="video/mp4" contentType="video">|<AdaptationSet mimeType="video/mp4" contentType="video"><ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/>|' bbb.mpd > drm.mpd
cd - >/dev/null || exit 1

echo "== start fixture server + bridge =="
(cd "$ROOT/fx" && exec python3 -m http.server 18777 >/dev/null 2>&1) &
SERVER_PID=$!
sleep 1
./target/debug/fetchforge-bridge serve --data-dir "$ROOT/data" --port 18443 > "$ROOT/bridge.log" 2>&1 &
BRIDGE_PID=$!
sleep 1
TOKEN=$(cat "$ROOT/data/bridge-token.txt")
FX="http://127.0.0.1:18777"

wait_for() { # wait_for <format_id-or-url-substring> <want-status> <label>
  local needle want label
  needle="$1"; want="$2"; label="$3"
  for _ in $(seq 1 45); do
    if curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" | python3 -c "
import json, sys
for r in json.load(sys.stdin):
    if (r.get('format_id') == '''$needle''' or '''$needle''' in r.get('url','')) and r['status'] == '''$want''':
        sys.exit(0)
    if (r.get('format_id') == '''$needle''' or '''$needle''' in r.get('url','')) and r['status'] == 'failed':
        print('FAILED:', r.get('error')); sys.exit(2)
sys.exit(1)"; then return 0; fi
    [ $? -eq 2 ] && { echo "FAIL: $label failed (see above)"; exit 1; }
    sleep 2
  done
  echo "FAIL: $label did not reach $want in time"
  exit 1
}

echo "== HLS formats: 3 variants, default 720p =="
HLS_FORMATS=$(curl -s --max-time 60 -X POST http://127.0.0.1:18443/media/formats \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/master.m3u8\"}")
echo "$HLS_FORMATS" | python3 -c "import json,sys; d=json.load(sys.stdin); print('title:', d['title']); print('formats:', len(d['formats'])); print('default:', d['default_format_id'])"
echo "$HLS_FORMATS" | grep -q '"extractor":"HLS"' || { echo "FAIL: HLS extractor not reported"; exit 1; }
echo "$HLS_FORMATS" | python3 -c "import json,sys; d=json.load(sys.stdin); assert len(d['formats'])==3, d['formats']; assert d['default_format_id'].endswith('/mid.m3u8'), d['default_format_id']" || { echo "FAIL: HLS formats/default wrong"; exit 1; }

echo "== HLS muxed 360p download -> .ts with exact bytes =="
LOW_URL="$FX/low.m3u8"
ADD=$(curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/master.m3u8\",\"format_id\":\"$LOW_URL\"}")
echo "$ADD" | grep -q '"stream_kind":"hls"' || { echo "FAIL: record is not tagged hls"; exit 1; }
wait_for "$LOW_URL" completed "HLS 360p download"
LOW_TS=$(find "$ROOT/downloads" -type f -name "*.ts" | head -1)
[ -n "$LOW_TS" ] || { echo "FAIL: no .ts file downloaded"; exit 1; }
cmp -s "$LOW_TS" "$ROOT/fx/low_full.ts" || { echo "FAIL: .ts bytes differ from source (not a clean concat)"; exit 1; }
echo "HLS 360p .ts matches source byte for byte: $LOW_TS"

if [ "$HAVE_FFMPEG" -eq 1 ]; then
  echo "== HLS separate-audio 1080p download -> merged .mp4 =="
  HIGH_URL="$FX/high.m3u8"
  curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
    -H "X-FetchForge-Token: $TOKEN" \
    -d "{\"url\":\"$FX/master.m3u8\",\"format_id\":\"$HIGH_URL\"}" >/dev/null
  wait_for "$HIGH_URL" completed "HLS 1080p merge"
  HIGH_MP4=$(find "$ROOT/downloads" -type f -name "*.mp4" | head -1)
  [ -n "$HIGH_MP4" ] && [ -s "$HIGH_MP4" ] || { echo "FAIL: HLS merged mp4 missing/empty"; exit 1; }
  echo "HLS 1080p merged: $HIGH_MP4 ($(stat -c%s "$HIGH_MP4") bytes)"
fi

echo "== DASH formats: video + audio, default 720p =="
DASH_FORMATS=$(curl -s --max-time 60 -X POST http://127.0.0.1:18443/media/formats \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/bbb.mpd\"}")
echo "$DASH_FORMATS" | python3 -c "import json,sys; d=json.load(sys.stdin); print('title:', d['title']); print('formats:', [f['format_id'] for f in d['formats']]); print('default:', d['default_format_id'])"
echo "$DASH_FORMATS" | grep -q '"extractor":"DASH"' || { echo "FAIL: DASH extractor not reported"; exit 1; }
echo "$DASH_FORMATS" | python3 -c "import json,sys; d=json.load(sys.stdin); assert d['default_format_id']=='v720', d['default_format_id']; assert 'a64k' in [f['format_id'] for f in d['formats']]" || { echo "FAIL: DASH formats/default wrong"; exit 1; }

if [ "$HAVE_FFMPEG" -eq 1 ]; then
  echo "== DASH 720p video + audio merge -> .mp4 =="
  curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
    -H "X-FetchForge-Token: $TOKEN" \
    -d "{\"url\":\"$FX/bbb.mpd\",\"format_id\":\"v720\"}" >/dev/null
  wait_for "v720" completed "DASH 720p merge"
  DASH_MP4=$(find "$ROOT/downloads" -type f -name "bbb.mp4" | head -1)
  [ -n "$DASH_MP4" ] && [ -s "$DASH_MP4" ] || { echo "FAIL: DASH merged mp4 missing/empty"; exit 1; }
  echo "DASH 720p merged: $DASH_MP4 ($(stat -c%s "$DASH_MP4") bytes)"
fi

echo "== DASH audio-only -> .m4a =="
curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/bbb.mpd\",\"format_id\":\"a64k\"}" >/dev/null
wait_for "a64k" completed "DASH audio-only"
M4A=$(find "$ROOT/downloads" -type f -name "*.m4a" | head -1)
[ -n "$M4A" ] && [ -s "$M4A" ] || { echo "FAIL: DASH .m4a missing/empty"; exit 1; }
if [ "$HAVE_FFMPEG" -eq 1 ]; then
  cmp -s "$M4A" "$ROOT/fx/a64k_full.m4a" || { echo "FAIL: .m4a bytes differ from source"; exit 1; }
  echo "DASH audio .m4a matches source byte for byte"
fi

echo "== DASH auto-pick: POST /downloads with .mpd and no format_id =="
ADD=$(curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/bbb.mpd\"}")
echo "$ADD"
echo "$ADD" | grep -q '"format_id":"v720"' || { echo "FAIL: auto-pick did not record v720"; exit 1; }
echo "$ADD" | grep -q '"stream_kind":"dash"' || { echo "FAIL: record is not tagged dash"; exit 1; }
wait_for "bbb.mpd" completed "DASH auto-pick download"

echo "== DASH DRM manifest -> Failed record with DRM error =="
curl -s --max-time 60 -X POST http://127.0.0.1:18443/downloads \
  -H "X-FetchForge-Token: $TOKEN" \
  -d "{\"url\":\"$FX/drm.mpd\"}" >/dev/null
wait_for "drm.mpd" failed "DRM refusal"
curl -s http://127.0.0.1:18443/downloads -H "X-FetchForge-Token: $TOKEN" | grep -q "DRM" || { echo "FAIL: DRM error text missing from record"; exit 1; }
echo "DRM manifest refused with a DRM error"

echo "STREAM E2E DONE"
