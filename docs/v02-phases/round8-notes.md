# Fix round 8 — notes (2026-11-23)

Round 7 (app v0.2.0 + extension 0.2.6) was live-tested on
2026-11-23 against novahd.cc, nime.pro and atlantic.bz (all The
Odyssey, 2026). The nime DASH download completed end to end
(2:56:47, 1080p, 2.47 GB), but the atlantic download failed the
same way twice — a segment timeout from totallyacdn — and the
quality popup never appeared in any live test. Round 8 fixes the
atlantic root cause (Fix A) and makes the popup appear on every
video download click (Fix B, a standing user goal that supersedes
the round-6 popup design). Extension version 0.2.7.

Baseline before round 8: `cargo test --workspace` 175 passed,
0 failed; extension harnesses 29 passed. After round 8: 188
passed, 0 failed; harnesses 47 passed (see §8).

## 1. Fix A — evidence: totallyacdn serves HLS as byte ranges

**Evidence (live, 2026-11-23).** The atlantic stream's CDN
(totallyacdn) serves HLS as byte ranges of ONE giant MP4 file:
HEAD on its `seg_00001.jpg` returned Content-Length 12681890561
(12.68 GB) with first bytes `ftypisom`, and the manifest lists
1730 segments. The browser player sends Range requests per
`#EXT-X-BYTERANGE` and plays fine. The same URL with
`Range: bytes=0-1048575` returned 206 + exactly 1 MB in 1.8 s;
without Range it pulled 91.9 MB in 30 s and was still going.

**Root cause.** The engine's HLS parser NEVER parsed
`#EXT-X-BYTERANGE` (`crates/engine/src/media.rs` built every HLS
Segment with `range: None`; the range field existed only for DASH
SegmentList `@mediaRange`). So the engine fetched each segment
URL with no Range header, the server streamed the 12.68 GB file
from byte 0 at ~3 MB/s, and the read timed out ("error decoding
response body: operation timed out"). The atlantic sequential
fallback failed the same way — concurrency was never the cause.

**Fix.** `parse_hls_segments` now returns `HlsMediaPlaylist`
(`init: Option<HlsSegment>`, `segments: Vec<HlsSegment>`) where
`HlsSegment` is `{ url, range: Option<(u64, u64)> }` (inclusive
range). Parsing rules, per RFC 8216:

- `#EXT-X-BYTERANGE:n[@offset]`; an omitted offset continues
  from the previous segment's end (the first ranged segment
  starts at 0). Only ranged segments advance the chain — a
  whole-file segment between ranged ones does not break it.
- A malformed byterange fails the parse loudly. Silently
  ignoring it would fetch a whole multi-GB file instead of one
  slice — the exact atlantic failure.
- `#EXT-X-MAP:URI="..."` with optional `BYTERANGE="n[@o]"`
  (omitted offset → 0) is parsed as the init segment and
  prepended as piece 0 (DASH precedent: `download_dash_with`
  inserts its init at index 0). fMP4 byterange playlists need
  the init or the assembled file has no ftyp/moov and will not
  play. Only the first MAP is kept — a mid-playlist map change
  cannot be represented in one assembled file.

**Reuse, not rebuild.** The existing `fetch_segment` Range
machinery (built for DASH) and the meta sidecar's `range=`
persistence already handle fetch + resume, so no new fetch code
was needed. The meta sidecar gained a `first_range=s-e` line
(validated on read; old metas have no such line and old segments
had no ranges, so it is backward compatible). The stale comment
in `download_segments` blaming atlantic throttling was corrected
to point at this finding.

**Exact totals.** `exact_total_bytes(segments)` sums range
lengths when EVERY segment has one, and is wired into
`download_segments_parallel`'s report closure and
`download_segments_sequential`'s progress as `known_total`
(falling back to `estimate_total_bytes`). A byterange download
now reports its exact total from the first progress report, and
DASH SegmentList downloads get exact totals too.

**Known limitation.** `fetch_segment`'s 200-fallback (a server
that ignores Range) reads the whole body and slices it. Against
a server that ignores Range on a giant file, that fails with a
read timeout rather than OOM (the reqwest read timeout bounds
it) — but it still fails. totallyacdn honours Range (206
proven live), so this is a fallback-path note, not the atlantic
path. A server that returns 200 for a ranged request on a
multi-GB file is not a case the engine can download safely.

## 2. Fix B — the popup, always (extension)

**Old behaviour.** The quality popup appeared ONLY when 2+
formats existed (master-playlist variants, DASH reps, or bridge
`POST /media/formats`); otherwise the click sent silently, and
a formats failure fell back to a silent one-click send. The
popup never appeared in any live test so far.

**New behaviour (user order, 2026-11-23).** EVERY download click
on any MP4 or running video shows the popup — no silent
one-click sends. Decision table (`onOverlayClick`):

| Download | Popup |
|---|---|
| HLS master (even 1 variant) | HLS popup: variant ladder + Audio only |
| Page URL / DASH (viaPage, .mpd) | Formats popup: bridge ladder + Audio only (DASH degraded only) |
| Single-variant HLS / direct video file | Simple popup: 1 quality + Audio only |
| Image / 3D / PDF / document | One-click send (no quality or audio choice exists) |
| Short clip on a long-form page | Refusal, never the popup (round-7 clip guard kept) |

**The ladder.** Video highest-first: 1080 or higher, then 720,
then 480, then lower. `parseHlsMaster` now sorts by height,
then bandwidth (a fat 720p never outranks 1080p); the formats
popup sorts bridge formats with `sortFormatsForPopup` (video by
height then bitrate, audio-only rows last — defence in depth,
the server already sorts best-first). The filename field is
kept; there is NO location choice (the app always saves to
Downloads/\<category\> — a picker is a future feature).

**Degraded popup.** Loading state + 15 s timeout, as before.
When the formats cannot be determined but the bridge IS
reachable, the popup still shows with the single known quality
("Video · default quality") plus a note ("Quality info
unavailable") — and an Audio only row for DASH only (a page
URL's audio needs the quality list; yt-dlp has no usable audio
path without a format id). A late formats reply never
overwrites the degraded popup (settled guard).

**Unreachable error state.** Only a total failure to reach the
bridge shows an error state in the popup ("FetchForge app not
reachable", with what to do) — no rows, no send, never a silent
send. `getFormats` classifies: no-token / challenge-failed /
fetch TypeError → unreachable; `bridge-<status>` → reachable
(the bridge answered, the listing failed server-side). A
runtime-message failure (background unreachable) is also an
error state. A timeout is treated as a busy bridge (yt-dlp
extraction is slow), so it shows the degraded popup, not the
error state.

**Audio rows.** The HLS popup adds an "Audio only" row for
muxed masters (no audio groups — the engine extracts the best
variant's audio) and sends every audio row with `audioOnly:
true` (without it, an audio playlist would download as a video
file). The simple popup's audio row sends a paired audio
sibling alone when one exists (without the video's captured
manifest text), else the video URL with `audioOnly: true`.

**Bug found during the round.** The clip-guard probe disabled
the button ("Checking…") and the old code's one-click send
re-armed it; routing the probe's success path to the popup left
the button disabled on "Checking…" while the popup was open.
The probe now restores the button before opening the popup.

## 3. Fix B — audio-only delivery (engine + bridge)

**Engine.** `extract_audio` (`crates/engine/src/ffmpeg.rs`,
mirroring `merge_av`): stream copy (`-vn -c:a copy`) to `.m4a`
first; a single AAC re-encode only when the copy is impossible
(e.g. PCM in MKV has no mp4 tag). A failed extraction removes
any partial output and returns ffmpeg's error tail.
`download_hls_audio_only` (`media.rs`): resolves the variant's
audio group from the master and downloads THAT playlist alone;
for a muxed variant (or a plain media playlist) it downloads
the full variant and the caller extracts the audio. DASH and
yt-dlp audio-only already worked via format choice (an audio
rep / audio format id) and needed no engine change.

**Bridge.** `DownloadRecord.audio_only` (`#[serde(default)]`,
so old history files still load), threaded from the extension
through `AddRequest` → `add_download` → `add_media_download` /
`add_direct_record` → `try_media_download`, plus the native
host's JSON parse and `forward_add_to_bridge`. The workers:

- Direct (`run_one`), HLS (`run_stream_hls`), DASH
  (`run_stream_dash`): ffmpeg is resolved UPFRONT when
  `audio_only` is set (fail fast, like the merge path); after
  the download, `audio_only_post_step` turns the video file
  into `<stem>.m4a` (via `unique_path`) and deletes the video.
  A file that is already audio-only (DASH audio rep, HLS audio
  playlist named `.m4a`) passes through unchanged, no ffmpeg
  needed. On extraction failure the video file is KEPT and the
  record is marked Failed with the reason.
- HLS routing: a paired audio sibling downloads alone; a master
  variant resolves its audio group via `download_hls_audio_only`.
- yt-dlp (`run_media`): the flag is ignored — the format choice
  already governs there.

**Duplicates.** A video and its audio track are different
downloads of the same URL, so `find_duplicate` /
`find_duplicate_except` now match only records with the same
`audio_only` flag. Audio-only adds are checked under the video
filename (a queued audio request still carries it) AND the
`.m4a` filename (a completed audio download's record was
renamed by the post-step). A completed video never swallows an
audio request, and a completed audio download swallows a repeat
audio request. The record's filename/destination are updated to
the `.m4a` file at completion, so the app list points at the
file that exists.

**Cosmetic note.** The audio file lands in the directory chosen
from the video filename (e.g. Downloads/Video/), because the
category is decided at add time from the video name. Acceptable
for this round; a category re-derivation at extraction time is
a possible follow-up.

## 4. Version

Extension 0.2.6 → 0.2.7 (both manifests). Chromium and Firefox
copies of content.js, background.js, popup.js and rules/ads.json
are byte-identical (verified with diff). All 6 extension JS
files pass `node --check`.

## 5. Tests

`cargo test --workspace`: 188 passed, 0 failed (baseline 175;
+13). Extension harnesses: 47 passed (baseline 29; +18).
No clippy warnings were added (the three add functions gained
`#[allow(clippy::too_many_arguments)]` — they grew to 8 args
with the new flag; the remaining warnings are pre-existing).

New Rust tests:

- `media.rs` (9): byterange parsing with explicit offsets,
  omitted-offset continuation (first starts at 0, explicit
  offset resets the chain), whole-file segments not breaking
  continuation, MAP init parsing (with/without byterange,
  first-MAP-wins), malformed byterange rejection, end-to-end
  byterange download against a Range-aware test server (206
  slices; the server serves the WHOLE file without Range, so an
  engine that ignored byteranges would assemble 900 bytes
  instead of 300), MAP init prepended end to end (init + 2
  segments = 3 pieces, exact total reported),
  `download_hls_audio_only` downloading only the audio playlist
  (the video playlist is never fetched) and falling back to the
  muxed variant.
- `ffmpeg.rs` (1): `extract_audio` copies AAC losslessly
  (output is audio-only, verified by probe), re-encodes only
  when the copy is impossible (PCM in MKV → AAC), and fails
  cleanly on garbage input with no partial output.
- `state.rs` (1): `find_duplicate` treats audio-only as a
  different download from the video.
- `bridge` (2): the audio post-step skips audio files, extracts
  a muxed mp4 to `.m4a` and deletes the video, and keeps the
  video on extraction failure; an audio-only add does not match
  a completed video as a duplicate.

New harness tests (18):

- content (14): long file / probe failure open the popup (not a
  send); video files open the popup without probing; images/3D/
  PDFs still send one-click; single-variant master opens the HLS
  popup; media-playlist text opens the simple popup; single
  format still shows the popup; formats error shows the
  degraded popup (DASH: video + audio; page URL: video only);
  degraded DASH audio row sends audio-only; bridge unreachable
  shows the error state with no rows and no send; timeout shows
  the degraded popup and a late reply never overwrites it; HLS
  ladder is height-first (2160/1080/720/480 from scrambled
  input); audio groups become flagged audio-only rows;
  `sortFormatsForPopup` orders video height-first, audio last;
  muxed master gets an Audio only row for the best variant;
  master with audio groups shows the group row with no extra
  muxed row.
- background (4): `downloadMedia` audioOnly reaches the native
  host end to end; `sendViaBridge` carries `audio_only` in the
  POST body; `getFormats` marks no-token / challenge / network
  failures unreachable; `bridge-500` is reachable.

## 6. What was NOT verified live

Nothing in this round ran against a real site or a real
browser — that needs the user's laptop + Chrome, which this
build cannot reach. Specifically unverified:

- The atlantic byterange download against totallyacdn (the
  live manifest could not be inspected — its signed URLs expire
  in minutes — so the real manifest's MAP/byterange shape is
  unconfirmed; the end-to-end proof is the local Range-aware
  test server).
- The popup in a real browser, for any site. It has NEVER
  appeared in any live test so far; the live sweep must test it
  explicitly (single MP4, HLS master, DASH, YouTube as video
  and as audio-only).
- The degraded popup and the unreachable error state in a real
  browser (bridge down / bridge up-but-failing).
- Audio extraction on the user's machine (ffmpeg there is the
  bundled Windows build; the tests ran against the Linux ffmpeg
  8.1.2 on this VM).
- The 200-fallback path against a real server that ignores
  Range (see §1 limitation).

## 7. Not in this round

- The novahd hidden-stream research order (voice call
  2026-11-23 11:47 PKT: search Google/web/GitHub for how to
  expose novahd's hidden player stream; build a GENERAL
  hidden-stream detection capability with Nova as the first
  proving case; detection only, no DRM/auth/paywall bypass, no
  cookie theft — spec §10 stands) is pending/awaiting
  assignment, not built here.
- No History changes (the unfinished History point from the
  2026-11-22 voice call — the user will come back to it).
- No location picker (future feature, explicitly out of scope).
- No desktop Rust changes (the desktop crate cannot be
  type-checked on this VM; Windows CI is the compile gate).
