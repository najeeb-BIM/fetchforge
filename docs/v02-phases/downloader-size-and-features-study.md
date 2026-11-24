# Downloader size detection + missing features study

**Date:** 2026-11-24 · **Feeds:** FetchForge round 11 · **Status:** research only, uncommitted

**Question from the user:** other open-source downloaders show the EXACT size in
GB/MB, not an estimate — how do they do it? And what functions do they have
that FetchForge lacks?

**Method:** cloned 10 repos on the VM (`~/workspace/downloader-size-study/`),
read the size/popup/feature code, and ran yt-dlp + curl against PUBLIC test
streams only (Akamai DASH Big Buck Bunny, Shaka HLS Angel One, Unified
Streaming Tears of Steel). No laptop installs, no movie-site tests, no DRM /
auth / paywall work. Builds on the two earlier studies
(`downloader-study.md`, `github-downloader-access-study.md`) — not a redo.

**Licences:** XDM and cat-catch are GPL — techniques described here in our own
words, never code. yt-dlp is Unlicense, N_m3u8DL-RE / hls-downloader / Motrix
are MIT, res-downloader and AB Download Manager are Apache-2.0, streamlink is
BSD-2-Clause, aria2 is GPL.

---

## 0. The honest headline (read this first)

**There is NO mechanism that gives an exact total for a typical movie-site HLS
stream before downloading.** Every mature downloader either shows an estimate
with an uncertainty marker, or shows NO size at all for HLS/DASH:

- **yt-dlp shows no size at all for HLS/DASH manifest formats** — verified
  live: on the Akamai DASH BBB stream and the Shaka HLS stream, every format
  returned `filesize=None, filesize_approx=None`, even though bitrate and
  duration were known. The code deliberately suppresses the bitrate estimate
  for manifest formats (`YoutubeDL.py:2977-2979`, comment: "For fragmented
  formats, 'tbr' is often max bitrate and not average").
- **XDM marks its estimates** — exact plain, average-bandwidth `≈`, peak
  `~`, live `Live` (`StreamSize.kt`).
- **Motrix shows total 0 (unknown)** until every segment has a known size
  (`segment-downloader.ts:68`).
- **hls-downloader shows `~expectedBytes`** (`PlaylistView.tsx:226-227`).

The "exact size" the user sees in other downloaders is one of three things,
all of which FetchForge already handles or can adopt cheaply:

1. **Direct files** — `Content-Length` (exact). FetchForge already reads it on
   the probe path (`extract.rs:1272`).
2. **YouTube / Drive** — the site's API carries an exact length per format
   (YouTube `contentLength` → yt-dlp `filesize`, `_video.py:3461`). FetchForge
   already surfaces these through the yt-dlp formats popup.
3. **Byte-ranged streams** — the manifest declares every segment's byte range
   (Atlantic-style). FetchForge already sums them (`exact_total_bytes`,
   `media.rs:1385`).

What round 11 CAN add is the XDM-style estimate ladder (average-bandwidth
first, peak as fallback, marked `≈` / `~`), the Motrix-style learn-as-you-go
exact total during download, and honest labelling. Section 1.8 lists the
concrete changes.

---

## 1. EXACT SIZE — how the mechanisms work, ranked by reliability

### FetchForge baseline (verified in code, 2026-11-24)

| Code | Behaviour |
|---|---|
| `crates/engine/src/media.rs:1364` `estimate_total_bytes(downloaded, done, total)` | `downloaded × total / done` — running-average extrapolation, `None` until ≥1 segment completes. An estimate, never exact. |
| `crates/engine/src/media.rs:1385` `exact_total_bytes(segments)` | Sum of range lengths, `Some` only when EVERY segment carries a byte range (HLS `#EXT-X-BYTERANGE`, DASH SegmentList `@mediaRange`). Exact. |
| `crates/engine/src/media.rs:1719` / `:2060` | Progress uses `known_total` (exact) else the measured extrapolation, combined with `progress_base` for multi-phase downloads. |
| `crates/engine/src/media.rs:1034` / `:1057` `list_hls_formats` | `filesize: None` for every format (media playlist and each master variant). Duration from the default variant's playlist (`hls_duration_secs`, sum of `#EXTINF`). No bitrate-based size is computed. |
| `crates/engine/src/extract.rs:1838-1849` `plan_from_info` | yt-dlp path total = chosen filesize, else `vbr_kbps × 125 × duration_secs` (round-10 item-2 fix for the novahd 121 MB vs 3.37 GB bug); merge adds best audio per language or single best audio. |
| `crates/engine/src/extract.rs:1272` `probe_direct_file` | Reads total from `Content-Range` (206) or `Content-Length` (200). |
| `apps/desktop/src/index.html:598-599, 623-628` | Percent denominator = `total_bytes \|\| estimated_total_bytes`; progress text appends "N / M pieces" and ", ~X est." |

### 1.1 Direct file — `Content-Length` / `Content-Range` (EXACT)

**How it works:** one HEAD (or a 1-byte Range GET) returns the full file
length. Every tool does this:

- **res-downloader** (`core/downloader.go`): HEAD probe first,
  `TotalSize = resp.ContentLength`; multipart only if `Accept-Ranges: bytes`
  and size > MinPartSize, else single-stream; `TotalSize ≤ 0` → unknown (-1);
  file pre-truncated to `TotalSize` when known.
- **aria2** (`src/HttpResponse.cc:224` `getContentLength()`): total length
  flows through `DownloadContext`; unknown length uses
  `UnknownLengthPieceStorage` (`src/RequestGroup.cc:607`).
- **N_m3u8DL-RE** (`BinaryDownloadRunner.cs:38`): direct-response
  `Content.Headers.ContentLength` is the total for binary downloads;
  `DownloaderConfig.cs:24` `CheckContentLength` defaults true (per-segment
  validation, `SimpleDownloadManager.cs:509`).
- **FetchForge** already does this on the probe path (`extract.rs:1272`).

**Exact or estimate:** EXACT.
**Works when:** the server sends `Content-Length` (nearly always for direct
files) or answers a Range probe with `Content-Range: bytes 0-0/<total>`.
**Fails when:** chunked transfer with no length, or a server that refuses
HEAD and ignores Range (rare for files). Fallback: Range-probe the first byte.
**FetchForge needs:** nothing new for direct files — the mechanism exists.
The gap is only that the popup/UI does not always surface the probed total
before the download starts.

### 1.2 Byte-range sum — HLS `#EXT-X-BYTERANGE` / DASH `@mediaRange` (EXACT)

**How it works:** the manifest declares each segment's byte offset and
length inside one big file; the exact total is the sum of the lengths
(+ init segment).

- **XDM** (`StreamSize.kt`, kind EXACT): exact only when video AND separate
  audio (if any) are both exact; the code notes a variant's `BANDWIDTH`
  covers the whole combination including separate audio (RFC 8216 §4.3.4.2),
  so separate audio adds nothing to a bitrate estimate.
- **Motrix** (`segment-downloader.ts:285`): each job's known size starts
  from `job.part.byteRange?.length`.
- **FetchForge** (`media.rs:1385`): identical logic, already shipped. Proven
  on Atlantic (round 8): the 12.68 GB single-file HLS stream is fetched by
  byte ranges.

**Exact or estimate:** EXACT.
**Works when:** the manifest carries byte ranges (Atlantic-style HLS; DASH
SegmentList with `@mediaRange`; DASH SegmentBase `indexRange`).
**Fails when:** the manifest has no ranges — which is MOST HLS media
playlists (separate `.ts` files) and SegmentTemplate DASH (no ranges
declared). This is why it cannot be the general answer for movie sites.
**FetchForge needs:** nothing — already implemented. Note for the UI: when
`exact_total_bytes` returns `Some`, the size should be shown WITHOUT the `~`
marker (currently the UI cannot tell exact from estimated totals apart).

### 1.3 Extractor / API-provided length (EXACT)

**How it works:** the site's API or metadata carries a per-format byte
length; the extractor copies it into the format record.

- **yt-dlp / YouTube** (`extractor/youtube/_video.py:3461`):
  `'filesize': int_or_none(fmt_stream.get('contentLength'))` — YouTube's
  adaptiveFormats carry an exact `contentLength` per format. yt-dlp uses it
  for chunked-download planning (`_video.py:3276-3278`, 10 MB chunks over
  `f["filesize"]`). When no `contentLength` exists it falls back to
  `filesize_approx` from tbr (`_video.py:3482`).
- **yt-dlp / Google Drive** (`extractor/googledrive.py:161`):
  `'filesize': ('contentLength', {int_or_none})`.
- **yt-dlp / Acast** (`extractor/acast.py:22`): same pattern.

**Exact or estimate:** EXACT (as exact as the site's own metadata).
**Works when:** the site exposes lengths (YouTube, Drive, podcast feeds).
**Fails when:** the site doesn't (every HLS/DASH manifest site — the
manifest format has no length field for separate-file segments).
**FetchForge needs:** nothing — the yt-dlp path already carries `filesize`
through `plan_from_info` (`extract.rs:1838`). This is why YouTube formats in
the popup show real sizes (e.g. "2160p · 1.6 GB") while HLS formats show
none.

### 1.4 HEAD every segment, sum `Content-Length` (EXACT — with a heavy cost)

**How it works:** issue a HEAD (or 1-byte Range GET) for every segment URL
before downloading; sum the returned lengths. No studied tool does a full
pre-download HEAD sweep as its size mechanism, but the building blocks are
proven:

- **Live test (Akamai DASH BBB, 2026-11-24):** HEAD on the init segment
  returned `Content-Length: 706`; HEAD on segment 1 returned
  `Content-Length: 318111`; `Accept-Ranges: bytes` present. Exact sizes are
  obtainable segment by segment.
- **Live test (Shaka HLS / Google Cloud Storage, 2026-11-24):** HEAD returned
  `x-goog-stored-content-length` (3,836,611 / 525,011 / 417,943 for the first
  three segments) but NO plain `Content-Length` header — a plain
  Content-Length reader would see "unknown" on GCS. Any implementation must
  also accept the GCS header (and ideally fall back to a 1-byte Range GET,
  which returns `Content-Range` with the total on most servers).
- **Motrix's practical form** (`segment-downloader.ts:446`): instead of
  pre-HEADing, it learns each segment's size from aria2's `tellStatus`
  `totalLength` AS the segment starts, and the plan total becomes exact once
  every job has a known size (`segment-downloader.ts:68`: "Full plan size,
  or zero until every job has a known size"). The total converges to exact
  during the download rather than being known up front.

**Exact or estimate:** EXACT (when the server answers).
**Works when:** the CDN answers HEAD with a length (Akamai: yes; GCS: yes
via its own header; the nime CDN returned Content-Length on HEAD per the
2026-11-22 finding).
**Fails when:** the CDN refuses HEAD, omits the length, or rate-limits —
2,000+ extra requests for a movie is exactly the request-rate pattern that
429'd the nime audio phase (round 7: ~40–80 req/s → 429 after ~120
requests). Signed segment URLs can also expire between the HEAD sweep and
the download. And on a slow host (SolarMovie-class, ~47 KB/s) the sweep
itself takes minutes.
**FetchForge needs:** a bounded pre-flight in the HLS/DASH planner
(`media.rs` plan path, near `estimate_total_bytes`): HEAD a capped sample
(see 1.7) or, Motrix-style, record each segment's `Content-Length` from its
response headers as it starts and recompute the total as
`sum(known segment lengths) + estimate(remaining)`. The second option costs
zero extra requests and converges to exact; it touches the segment-fetch
loop and the progress-reporting path (`media.rs:1719` / `:2060`), plus a
new `total_bytes_is_exact` flag so the UI can drop the `~` when the total
becomes exact.

### 1.5 `AVERAGE-BANDWIDTH × duration` (ESTIMATE — usually within ~1%)

**How it works:** the HLS master playlist may declare `AVERAGE-BANDWIDTH`
per variant (the true mean bitrate of the whole variant). Total ≈
average-bandwidth / 8 × duration.

- **XDM** (`StreamSize.kt`, kind AVERAGE): second rung of its ladder, with
  the code comment that it is "usually within about 1%". Displayed with `≈`.
- **Live illustration (Shaka HLS master, 2026-11-24):** the 768x576 variant
  declares `BANDWIDTH=8062646, AVERAGE-BANDWIDTH=1824731` — peak is 4.4× the
  average. For the 60 s clip, peak math gives ~60 MB, average math ~13.7 MB.
  The real file is near the average figure. This is the single clearest
  proof that peak-bandwidth math is misleading and average-bandwidth math
  is the estimate to use when present.

**Exact or estimate:** ESTIMATE (good — ~1% per XDM's note).
**Works when:** the master declares `AVERAGE-BANDWIDTH` (Shaka does; many
movie-site manifests do not — the novahd/nime/atlantic manifests seen in the
sweep carried only `BANDWIDTH`).
**Fails when:** the attribute is absent (common on movie sites) or the
manifest is a single media playlist with no master at all.
**FetchForge needs:** parse `AVERAGE-BANDWIDTH` in the master-playlist
parser (alongside the existing `BANDWIDTH` parse used for ranking), compute
`filesize_approx = avg_bw / 8 × duration` in `list_hls_formats`
(`media.rs:1034` / `:1057`, which currently sets `filesize: None` for every
format), and surface it in the popup with the `≈` marker. Duration already
comes from the default variant's playlist. Separate-audio variants add
nothing (BANDWIDTH covers the combination — RFC 8216 §4.3.4.2, per XDM's
note), so no audio addition is needed for the estimate.

### 1.6 `BANDWIDTH (peak) × duration` (ROUGH ESTIMATE — often 2–4× high)

**How it works:** `BANDWIDTH / 8 × duration`, using the peak bitrate the
master declares. Always available when a master exists.

- **yt-dlp** (`utils/_utils.py:5668` `filesize_from_tbr(tbr, duration)` =
  `duration × tbr × 1000/8`) — but deliberately NOT applied to manifest
  formats (`YoutubeDL.py:2977-2979`: only when the format has no
  `manifest_url` OR the `manifest-filesize-approx` compat option is set;
  "tbr is often max bitrate and not average"). Format listing shows
  `filesize` plain, else `~filesize_approx` (`YoutubeDL.py:3967-3970`,
  `:4020`). **Verified live:** Akamai DASH + Shaka HLS formats all came
  back with `filesize=None, filesize_approx=None` despite known tbr and
  duration — yt-dlp prefers showing NO size over a misleading one.
- **hls-downloader** (`PlaylistModule.tsx:175`):
  `expectedBytes = (totalBitrate / 8) * duration`, totalBitrate = selected
  video bitrate + selected audio bitrate (default 128,000 when audio levels
  exist but carry no bitrate); UI shows `~expectedBytes`.
- **FetchForge** already uses this class of estimate on the yt-dlp path
  (`extract.rs:1838-1849`: `vbr_kbps × 125 × duration_secs`).

**Exact or estimate:** ESTIMATE — systematically high for VBR content;
XDM's code calls the peak rung "rough, can be tens of percent off"; the
Shaka live example shows 4.4× off.
**Works when:** always (any master with BANDWIDTH), as a rough upper bound.
**Fails when:** treated as exact — it will over-promise the size and the
progress bar will hit 100% early.
**FetchForge needs:** use as the LAST rung of the ladder (after exact and
average), marked `~`, in `list_hls_formats` and in the planner's
`estimated_total_bytes`. Never show it unmarked.

### 1.7 Sample-HEAD extrapolation — HEAD the first N segments (ESTIMATE)

**How it works:** HEAD a small sample of segments, average their lengths,
multiply by the segment count.

- **cat-catch** (`js/m3u8.js` `estimateFileInfo(fragments, duration)`):
  HEADs the first min(10, fragment count) segments, averages Content-Length,
  multiplies by fragment count; also derives bitrate = estimate × 8 /
  duration.

**Exact or estimate:** ESTIMATE.
**Works when:** segment sizes are roughly uniform; cheap (≤10 requests).
**Fails when:** the first segment is atypical — **verified live on Shaka:**
seg1 = 3,836,611 B, seg2 = 525,011 B, seg3 = 417,943 B (the first segment
carries init/keyframe data and is 7× the typical size). Sampling the first
10 overestimates. VBR scenes (action vs dialogue) also skew a small sample.
Sampling spread across the playlist (e.g. segments 1, N/4, N/2, 3N/4) would
be less biased than first-N.
**FetchForge needs:** only if the average-bandwidth rung is absent AND an
early estimate is wanted. A 4-point spread sample (4 HEADs) in the planner
is cheap and less biased than cat-catch's first-10. Do not use it when
`AVERAGE-BANDWIDTH` is available.

### 1.8 Ranking + what round 11 should implement

Ranked by reliability for movie-sized HLS/DASH streams:

| Rank | Mechanism | Exact? | Availability on movie sites | Cost |
|---|---|---|---|---|
| 1 | Byte-range sum (§1.2) | EXACT | Rare (Atlantic-style only) | Free (manifest already parsed) |
| 2 | HEAD every segment (§1.4) | EXACT | Usually (CDN-dependent) | 2,000+ requests; 429 risk — use Motrix learn-as-you-go instead |
| 3 | Extractor/API length (§1.3) | EXACT | YouTube / Drive only | Free (already in yt-dlp info) |
| 4 | `AVERAGE-BANDWIDTH × duration` (§1.5) | ~1% estimate | Sometimes (often absent) | Free (master already parsed) |
| 5 | `BANDWIDTH × duration` (§1.6) | Rough (up to 4× high) | Almost always | Free |
| 6 | Sample-HEAD extrapolation (§1.7) | Biased estimate | Usually | 4–10 requests |
| 7 | Running-average extrapolation (FetchForge today) | Converging estimate | Always, but only mid-download | Free |

**Recommended round-11 implementation (XDM ladder + Motrix convergence):**

1. **Estimate ladder in `list_hls_formats` / the planner** (touches
   `media.rs:1034` / `:1057`, the master parser, and `extract.rs:1838`):
   exact byte-range sum → `AVERAGE-BANDWIDTH × duration` → `BANDWIDTH ×
   duration` → running average. Carry a `size_kind` (exact / average /
   peak / measured) alongside the number.
2. **Markers in the popup and the downloads list** (XDM convention):
   exact plain, average `≈`, peak `~`, measured `~`. The UI must be able to
   tell exact from estimated — today it cannot.
3. **Learn-as-you-go exact total** (Motrix pattern): record each segment's
   `Content-Length` from its response headers as it starts; total =
   `sum(known) + estimate(remaining)`; flip `size_kind` to exact when every
   segment has started. Zero extra requests. Touches the segment-fetch loop
   and progress reporting (`media.rs:1719` / `:2060`).
4. **Do NOT pre-HEAD all segments** for movie-sized downloads — the request
   volume repeats the nime 429 failure mode. A 4-point spread sample is the
   most that should be added, and only when no better rung exists.
5. **Progress display** (user order): drop piece counts; show
   `downloaded / total` (e.g. "459 MB / 4.9 GB") + remaining. The total
   shown is whichever ladder rung applies, with its marker.

---

## 2. POPUP / BUTTON BEHAVIOUR — how the others present the choice

Behaviour observations only. FetchForge's standing popup goal (always popup;
audio + video; ladder 1080 → 720 → 480 → lower) is compared at the end.

### 2.1 XDM (GPL — techniques only)

- **No in-page button at all.** The extension manifest
  (`browser-extension/chrome-extension/manifest.json`, v3.6) declares NO
  content scripts; detection runs in the background via `webRequest` +
  `cookies` + `contextMenus` permissions with `*://*/*` host access.
- **Toolbar popup** (`popup.js`, `VideoPopup`): lists the videos detected in
  the active tab, newest first, each row with a format badge (`[mp4]`), size
  and first-keyframe thumbnail (MP4 only, never HLS); a preview action is
  offered. No quality choice happens here — it is a detection list.
- **Desktop dialog** (`StreamDownloadDialog.kt` + `StreamChoices.kt`): the
  quality choice happens AFTER the send, in the desktop app. Formats are
  labelled `resolution · bandwidth · codecs` (e.g. `1080p · 5.2 Mbps ·
  H.264`), HLS sorted by `BANDWIDTH` desc; audio modes CHOOSE / INCLUDED /
  NONE; audio-only variants appear as their own entries. Size estimates
  carry the uncertainty markers (exact plain, `≈` average, `~` peak, `Live`).
- **Direct files:** the browser monitor intercepts ordinary downloads
  (context menu / auto-capture); no in-page button is involved.
- **Note:** XDM explicitly refuses YouTube ("XDM doesn't download from
  YouTube", `bulk.js:711`).

### 2.2 AB Download Manager extension (Apache-2.0)

- **In-page floating panel** (`MediaSelectionPopup.ts`): injected above the
  LARGEST `<video>`/`<audio>` element (`getBiggestMediaPlayer`, sorted by
  `clientWidth × clientHeight`). Collapsed header shows the item count;
  expanding lists one row per item:
  `duration · size · resolution · bandwidth · extension`; click a row to
  download. Dismissible.
- **No desktop quality choice** — variant choice happens entirely in the
  extension popup.
- **Direct files:** the extension intercepts via `DownloadLinkInterceptor.ts`
  (`doWeAcceptThisFileSize`, `:131` — Content-Length used only as a
  minimum-size capture filter).

### 2.3 cat-catch (GPL — techniques only)

- **Toolbar popup → data table:** ALL detected media on the page (every
  type), with type filters, size sort, video preview, and batch
  download/export. The popup shows resolution where known (`popup.js:234`).
- **No per-resource quality choice.** Quality is not offered per item; HLS
  goes to a dedicated m3u8 downloader page (`m3u8.html` / `m3u8.js`) with a
  segment list and its own downloader. The only quality control is a global
  setting affecting only WeChat videos.
- **Direct files:** same table; click to download or export.

### 2.4 res-downloader (Apache-2.0)

- **Desktop data table, no browser popup.** Media is captured by a local
  proxy/sniffer and shown in a desktop table with type filters, size sort,
  and preview (video.js via a local proxy forwarding `Range`). Batch
  download/export. No quality choice per resource.

### 2.5 Motrix v2 (MIT)

- **No browser integration at all** — media tasks come from submitted
  manifest URLs. **No quality choice:** the master parser picks the
  highest-`BANDWIDTH` variant automatically (`hls-parser.ts`
  `parseHlsMaster`) plus the matching `EXT-X-MEDIA` audio rendition.

### 2.6 yt-dlp front-ends (TTsanchez / quinnuk VideoDownloader, MIT)

- **TTsanchez (`main.py`):** desktop preset dropdown (Auto, 2160p, 1440p,
  1080p, 720p, 480p) building `bv*[height<=H]+ba/b[height<=H]`, PLUS a
  format table (quality / type / codec / fps / size / note): row 1 "Best",
  one merged row per distinct height (best video at that height + best
  audio, size = sum of both, note "merge to MP4"), then every raw video-only
  format, then top 12 audio-only formats by filesize. Size estimation sums
  `filesize`/`filesize_approx` of the chosen pair. This is the most complete
  format table in the study — the model the earlier study recommended copying
  for FetchForge's popup body.
- **quinnuk (`ui.py`):** dropdown only — Best Available, 2160p/4K, 1440p,
  1080p, 720p, 480p, 360p, Audio Only (MP3). No format table; the probe does
  not even fetch formats. Audio Only disables container/subtitle controls
  and enables the bitrate menu.

### 2.7 How FetchForge compares

FetchForge's always-popup (in-page floating button → popup with ladder +
audio choice + editable filename) is closest to **ABDM's in-page panel**
combined with **XDM's desktop dialog**. It already exceeds every studied
tool on one point: no other tool guarantees a quality popup on EVERY video
click (XDM defers the choice to a desktop dialog, ABDM's panel lists items
without a forced choice, cat-catch offers no per-item quality at all,
Motrix offers no choice). The gaps against the best of them:

- No codec/fps/size columns in the popup (TTsanchez's table).
- No audio-mode choice (XDM's CHOOSE / INCLUDED / NONE) — only a single
  "Audio only" row.
- No per-language audio choice (see §3, item 5).
- The popup's size column is empty for HLS formats (§1.5 fixes that).

---

## 3. MISSING FUNCTIONS — what the others have that FetchForge does not

One line each: name + what it does + why a movie-downloading user would
care. ★ = matters most. Every item was checked against the FetchForge code
for absence (grep, 2026-11-24); "absent" means no code path, not merely no
UI control, except where noted.

1. ★ **Auto-start queued downloads** — when a running download finishes, the
   next queued one starts by itself; FetchForge's queued rows sit forever
   until resumed by hand (observed through the whole sweep; no
   auto-start/next-queued code exists in the bridge/engine).
2. ★ **Subtitle download + muxing** — fetch the subtitle rendition from the
   manifest and mux it into the file (N_m3u8DL-RE `--select-subtitle` /
   `--auto-subtitle-fix`; yt-dlp `--write-subs`; XDM lists subtitle
   renditions); FetchForge ignores subtitle tracks entirely (the only
   "subtitle" hits in the extension are the popup's subtitle TEXT element).
3. ★ **User-facing speed limit** — cap total download speed so a movie
   download doesn't starve the rest of the house (XDM
   `speedLimiterEnabled`/`speedLimit`, `AppConfig.kt:90-91`; ABDM
   `SpeedLimitMode` + speed-limit action group; aria2 `--max-download-limit`;
   Motrix via aria2). FetchForge's engine HAS the plumbing
   (`download.rs:270` `Limiter`, `opts.bandwidth_kbps` at `:544` and `:810`)
   but nothing exposes it — no UI control, no bridge field.
4. ★ **Real download speed display** — live KB/s or MB/s per row (every
   studied tool shows it; FetchForge's Speed column is hardcoded `"-"`,
   `index.html:632` — already a round-11 layout item).
5. ★ **Per-language audio choice in the popup** — pick which audio track(s)
   to keep when a stream carries several languages (XDM audio modes CHOOSE /
   INCLUDED / NONE; N_m3u8DL-RE `--select-audio` with lang tags and
   multi-audio mux; yt-dlp `--audio-multistreams`). FetchForge round 10
   added multi-audio on the yt-dlp path but the popup offers no
   per-language choice; the novahd stream carries 7 audio tracks and the
   user gets one.
6. **Batch download** — select several detected videos and download them all
   at once (cat-catch batch download/export; XDM bulk picker); FetchForge
   sends one video at a time.
7. **Clipboard monitoring** — a copied video/manifest link is offered as a
   download automatically (XDM clipboard batch, `AppConfig.kt:139-140`
   `batchAsOneFromClipboard`); FetchForge's paste-link is manual.
8. **Download scheduler** — queue a download to start later / at a set time
   (XDM `DownloadScheduler`, `DownloadScheduler.kt:24`, started in
   `AppMain.kt:110`; the new-download dialog offers "download later",
   `NewDownloadWindow.kt:404-413`); FetchForge has no scheduler.
9. **Shutdown / sleep when all downloads finish** — power off the machine
   after an overnight movie queue (XDM `shutdownAfterAllDone`,
   `AppConfig.kt:56,423`); FetchForge has nothing.
10. **Completion notification** — a system notification when a movie finishes
    (XDM, ABDM); FetchForge has no notification code at all — the user must
    look at the app window.
11. **Filename templates** — naming patterns with variables (title, year,
    quality) applied automatically (yt-dlp output templates; N_m3u8DL-RE
    `--save-name`); FetchForge has only a free-text editable field.
12. **Retry with quality downgrade** — when a download fails, offer "Try Best
    Available quality" / a lower variant instead of just failing (quinnuk's
    UI failure suggestions); FetchForge's failed rows just fail.
13. **Per-segment content validation** — verify each downloaded segment's
    length/content against the manifest or a checksum, re-fetching bad ones
    (N_m3u8DL-RE `CheckContentLength`, `SimpleDownloadManager.cs:509`;
    aria2 checksum verification); FetchForge retries failed segments but
    never validates a segment that arrived with the wrong bytes.
14. **Proxy support** — route downloads through a user-configured HTTP/SOCKS
    proxy (aria2, yt-dlp, XDM all support it); FetchForge's engine uses
    reqwest defaults with no proxy setting.
15. **Playlist / series download** — download every video in a playlist or
    series in one action (yt-dlp playlists); FetchForge handles one video
    per send.
16. **Metadata / chapter embedding** — write title, year and chapters into
    the finished file (yt-dlp `--embed-metadata` / `--embed-chapters`);
    FetchForge's only post-processing is the merge.
17. **Audio format choice** — extract audio as MP3 / Opus / M4A at a chosen
    bitrate (quinnuk offers MP3 with a bitrate menu; yt-dlp `--audio-format`
    with 320K/256K/192K/128K); FetchForge extracts AAC/M4A only.
18. **Export / import download list** — save the download list (or detected
    media) to a file for backup or sharing (cat-catch export); FetchForge
    keeps `downloads.json` on disk but offers no UI export.
19. **Live-stream recording** — record a live HLS/DASH stream while it plays
    (streamlink's core use case); FetchForge refuses live streams by design
    (deliberate — only add if the user asks for it).
20. **Browser-download interception** — take over ordinary file downloads
    from the browser, not just detected media (XDM browser monitor, ABDM
    link interceptor); FetchForge only handles media it detects plus manual
    sends.

**Deliberately NOT listed as gaps:** cookie/session forwarding (XDM and
ABDM forward the user's own browser cookies to the desktop app over
localhost; FetchForge's spec §10 forbids cookie use entirely), YouTube
bot-check circumvention (TTsanchez's PO-token server — flagged, do not
copy), and any DRM/auth/paywall bypass (forbidden by spec §10).

---

## 4. Live test log (public streams only, 2026-11-24)

| Test | Result |
|---|---|
| yt-dlp `-J` on Mux test stream (`test-streams.mux.com/x36xhzz/x36xhzz.m3u8`) | FAILED — "Remote end closed connection without response"; curl also empty. Stream unreachable from this VM. |
| yt-dlp `-J` on Akamai DASH BBB (`dash.akamaized.net/akamai/bbb_30fps/bbb_30fps.mpd`) | OK — 6+ formats, all `filesize=None, filesize_approx=None`, tbr 67–1254 kbps, duration None. |
| HEAD on Akamai DASH segments | OK — init `Content-Length: 706`, seg 1 `Content-Length: 318111`, `Accept-Ranges: bytes`. |
| Apple BipBop HLS (`devstreaming-cdn.apple.com/.../master.m3u8`) | DEAD — 302s to an HTML page on developer.apple.com; Apple moved/removed the example streams. |
| Bitmovin Sintel HLS (`bitdash-a.akamaihd.net/content/sintel/hls/playlist.m3u8`) | 403 Access Denied. |
| yt-dlp `-J` on Shaka HLS Angel One (`storage.googleapis.com/shaka-demo-assets/angel-one-hls/hls.m3u8`) | OK — duration 60 s, all formats `filesize=None, filesize_approx=None`; master declares both `BANDWIDTH` and `AVERAGE-BANDWIDTH` (768x576: peak 8,062,646 vs average 1,824,731 — 4.4×). |
| HEAD on Shaka HLS segments | OK — GCS returns `x-goog-stored-content-length`, NOT plain `Content-Length` (seg1 3,836,611; seg2 525,011; seg3 417,943). First segment 7× typical size. |
| Unified Streaming Tears of Steel HLS | OK (200, `application/vnd.apple.mpegurl`) — fetched, not probed further. |
| yt-dlp `-J` on YouTube BBB (`youtube.com/watch?v=aqz-KE-bpKQ`) | Bot-gated from this VM ("Sign in to confirm you're not a bot") — known environmental limit, NOT a code issue; no cookies used (spec §10). YouTube `contentLength` mapping verified in code instead (`_video.py:3461`). |

## 5. Sources

Repos cloned to `~/workspace/downloader-size-study/`: putyy/res-downloader,
yt-dlp/yt-dlp, subhra74/xdm, aria2/aria2, xifangczy/cat-catch,
nilaoda/N_m3u8DL-RE, streamlink/streamlink, puemos/hls-downloader,
amir1376/ab-download-manager (+ ab-download-manager-browser-integration as
`abdm-ext`), agalwood/Motrix. Earlier studies:
`docs/v02-phases/downloader-study.md`,
`docs/v02-phases/github-downloader-access-study.md`.
