# Downloader Study — what other open download managers teach FetchForge

Date: 2026-11-22. Research only: no code from any studied project was copied
into FetchForge, and none should be. This report records TECHNIQUES and design
ideas, with file/function evidence, so FetchForge can reimplement them in its
own Rust/JS.

## The three questions

1. **ATLANTIC** — our HLS engine times out fetching segment 1 from
   totallyacdn.cc. The manifest fetches fine; segment bodies time out. How do
   other downloaders fetch HLS segments reliably from stubborn CDNs?
2. **NOVAHQ** — our extension's stream picker grabs a short `/assets/p/*.mp4`
   preview clip instead of the HLS playlist actually playing. How do other
   tools detect and rank streams on a page?
3. **QUALITY POPUP** — we are adding an IDM-style popup (resolution +
   video/audio-only choice at click time). How do others present format choice?

## Our current baseline (for comparison)

- Engine (`crates/engine/src/media.rs`): segments fetched **strictly
  sequentially**, one at a time, in order; `fetch_bytes_range` builds a **new
  reqwest client per segment** (no connection reuse, fresh TCP+TLS handshake
  per segment); UA `FetchForge/0.2`; Referer carried; `Accept-Encoding:
  identity`; 120 s per-segment timeout; 3 attempts with 500 ms/1 s backoff;
  fail-fast at 0 bytes on timeout; resume via `.segments` sidecar; encrypted
  playlists refused; default variant = best at or below 1080p.
- Extension (`extensions/chromium/content.js`, `background.js`): stream
  detection from the per-document **resource-timing log** (5000-entry buffer);
  ranking master > video variant (+ audio sibling) > audio > direct file;
  master recognised by stem regex (`master|index|playlist|manifest`);
  cross-frame election in the background script; button only in the top frame.

## License key (applies throughout)

| Licence | Posture for FetchForge |
|---|---|
| MIT / Apache-2.0 | Techniques freely reimplementable; attribute if code is ever adapted. |
| GPL-3.0 | **Techniques only. Never paste code, identifiers, comments, or structure.** |
| Unverified / no source found | Techniques from public docs only; treat as closed. |

---

## 1. XDM — Xtreme Download Manager (`subhra74/xdm`, 8.7k★, Kotlin)

**Licence: GPL-3.0-or-later — techniques only, never paste code.**
The richest project in this study: own HLS/DASH engine, own transmuxer
(no ffmpeg), capture pipeline, unusually thorough comments explaining *why*.

### Stream detection
- Extension network sniffing (`browser-extension/chrome-extension/request-watcher.js`):
  `chrome.webRequest.onSendHeaders` + `onHeadersReceived`, GET-only, 2xx-only,
  media extensions + content-type fragments; `resourceType` forwarded
  (`"media"` vs `"xmlhttprequest"`). YouTube blocked.
- **Playlist-body capture** (`hls-hook.js`): runs in the page's MAIN world
  from `document_start` in every frame; hooks `fetch` and `XMLHttpRequest.send`,
  clones 2xx HLS responses (`.m3u8` path or `mpegurl` content-type, max
  256 KB, requires `#EXTM3U`), forwards the text to the background. Reason
  (file comment): some CDNs sign a playlist link for a single request, so a
  re-fetch gets 403. `request-watcher.js` holds an HLS response up to 1500 ms
  for its body. Worker-loaded playlists are NOT seen by the hook; the app
  fetches those.
- Desktop capture (`xdm-capture/.../CaptureHandler.onMedia`): strips
  non-replayable headers via blocklist (`accept`, `connection`, `expect`,
  `te`, `upgrade`, `range`, `cookie` handled separately, `content-*`,
  `accept-encoding`, `if-*`, `proxy-*`).

### Stream ranking — the NOVAHQ model
- `VideoHelper.requestVerdict`: `video/iso.segment` → fragment, never a video;
  `resourceType == "media"` → whole file, no min size; otherwise heuristic.
- `isHttpVideo`: min size 1 MB default (`AppConfig.minVideoSize`); **once a
  tab has produced an m3u8/mpd (`m3u8MpdTabs`), plain HTTP videos from that
  tab are suppressed**; an `init.mp4` URL marks its whole folder as suspected
  fragments; exclusions for localhost ports, `fbcdn`, `m4s`/`f4f`.
- Master preference (`VideoHelper.processHLSVideo`): a master becomes one
  entry per variant grouped in `HlsMasterGroup`; variant media playlists the
  player loads afterwards **size the master's entries and are never listed
  separately**; a media playlist arriving before its master still counts.
  Captured manifest text is cached (`PlaylistTextCache` / `HlsPlaylistStore`)
  because "the link may not give it a second time".
- Duration/size from the playlist: sums `#EXTINF` for duration; sizes variants
  (EXACT from byte ranges, AVERAGE from `AVERAGE-BANDWIDTH`, PEAK from
  `BANDWIDTH`).

### Segment fetching
- Concurrency: fixed thread pool, default **8** (`AppConfig.kt`), hard cap 256.
  Video + audio + subtitle manifests fetched in parallel.
- HTTP client (`network/http/impl/HttpClientImpl.kt`, OkHttp): **HTTP/1.1 only**;
  connect 30 s; read 60 s (a *stall* timeout: seconds a connection may go
  without receiving data); `retryOnConnectionFailure(false)` (XDM retries
  itself); `fastFallback(true)` Happy Eyeballs (250 ms per address);
  pooled connections; **`HostFirst` interceptor moves the `Host` header to
  first position** — a CDN answered 410 to every segment until Host moved up.
- Per-segment (`StreamingChunkRetriever`): resume via `Range` from the
  downloaded offset; byte-range segments use absolute inclusive end and MUST
  get 206; 416 on resume → restart the segment from 0; 200 to a resumed
  request → rewrite from byte 0. Shared `DownloadClock.onData()` on every read.
- **Retry model (`util/RetryPolicy.kt` + `DownloadClock`) — the standout
  design in this study.** A clock shared by all segments of a download tracks
  when the download last received *any* data. While the download is not
  stalled, every failure class only waits and retries (a connection-limiting
  server just slows the download). Only after 15 s of total silence do
  refusals (403/410/404-on-segment) give up as `LinkExpired`; server errors
  get a 5 min budget; network errors count against `maxRetries` (5) only
  while stalled. Backoff 1 s doubling to a 10 s cap + up to 25% jitter.
  `Retry-After` honoured (delta-seconds or HTTP date, capped 5 min).
- Manifest fetch (`util/ManifestUtils.downloadManifestAsFile`): **no `Range`
  header** ("browsers never send one for a playlist, and some CDNs reject it
  (400 over HTTP/2)"); 32 MB cap; partial manifests deleted, never parsed.
  A finished captured playlist is used without any fetch.
- Parsing (`manifest/hls/HlsParser.kt`): hand-rolled; `EXT-X-STREAM-INF`
  bound to the URI line immediately following; `EXT-X-KEY` AES-128 `identity`
  only — SAMPLE-AES/SAMPLE-AES-CTR/CENC explicitly rejected; manifest query
  string propagated to child URLs only on the same origin (auth-token leak
  prevention). Keys fetched with retry, max 1024 bytes, must be exactly 16,
  persisted for resume.
- Merging: own transmuxer (`media/muxer/transmux/`, no ffmpeg). Fallback
  ladder: remux → playlist folder → side-by-side files → error with segments
  kept, so a mux failure never loses a download.

### Quality UI
- Browser popup (`popup.js`): detected videos newest-first; format badge
  (`[mp4]`), size, first-keyframe thumbnail (MP4 only, never HLS).
- Desktop dialog (`StreamDownloadDialog.kt` + `StreamChoices.kt`): formats
  labelled `resolution · bandwidth · codecs` (e.g. `1080p · 5.2 Mbps ·
  H.264`), HLS sorted by `BANDWIDTH` desc; audio modes CHOOSE / INCLUDED /
  NONE; audio-only variants as own entries. Size estimates carry uncertainty
  markers (`245.3 MB` exact, `≈` average, `~` peak, `Live`).

### Flags
No DRM bypass (SAMPLE-AES rejected; only standard HLS AES-128 identity keys
decrypted), no paywall bypass. The extension forwards the user's *own* browser
cookies to the desktop app over localhost — same-user, local-only, standard
download-manager behaviour; XDM logs only whether a cookie was present, never
its value. (FetchForge's spec forbids cookie use entirely — see §11.)

---

## 2. AB Download Manager (`amir1376/ab-download-manager`, 19.5k★, Kotlin + TS ext)

**Licence: Apache-2.0 (both repos).** Closest to FetchForge in spirit
(extension + desktop engine + HLS path). Its HLS engine is deliberately narrow
(media playlists only, `.ts` only, no encryption); variant handling lives in
the extension.

### Stream detection
- Extension (`abdm-ext/src/linkgrabber/DownloadLinkInterceptor.ts`):
  `webRequest.onResponseStarted` with `types: ["media"]` for direct media,
  plus URL filters for `*.m3u8` (with/without query). Direct media requires
  `video/*` or `audio/*` content-type. Configurable min file size
  (`captureFileSizeMinimumKb`); missing Content-Length accepted. YouTube
  blacklisted. TS/m3u8/m4s deliberately never count as direct media.
- Per-tab (`MediaRegistry.ts` / `MediaOnTab.ts`): re-fetches each intercepted
  playlist *from the extension* with the intercepted request headers
  (2xx only; mpegurl content-type or Content-Length < 2 MiB); parses with the
  `hls-parser` npm library. Master → one entry per variant, then **recursively
  fetches every variant media playlist**. Media playlist accepted only if the
  first segment is `.ts`; duration = sum of segment durations; encrypted
  playlists dropped.
- **No ranking — the cautionary tale:** items listed in insertion order; no
  dedupe, no master preference, no duration/size heuristic, no suppression of
  short preview clips. Every variant becomes its own item. This is the closest
  analogue to the NOVAHQ failure mode.
- Likely bug, do not copy: `createResolutionString` uses
  `resolution.width + "p"` (would label 1920×1080 as "1920p").

### Segment fetching
- HTTP client (OkHttp, `di/Di.kt`): dispatcher `maxRequests =
  maxRequestsPerHost = MAX` ; **HTTP/1.1 only**; no explicit timeouts (OkHttp
  defaults: connect 10 s, read 10 s). Default UA is a hardcoded Chrome 120
  string; `accept-encoding: identity` forced last; `Host` explicitly never
  overridden from captured credentials (redirect safety).
- Concurrency: `HLSDownloadJob` creates one `HLSPartDownloader` per segment,
  up to **8** (`DownloadSettings.DEFAULT_THREAD_COUNT`); parts start in ID
  order, each completion starts the next. Thread count changeable live.
- Per-segment: connects with **no Range** — a failed/cancelled part restarts
  from zero.
- Retries, two levels: part level `PART_MAX_TRIES = 10`, fixed 1 s delay, no
  backoff; job level is **progress-based** — if downloaded size grew since the
  last failure, the failure counter resets, else increments; retry after fixed
  3 s (job-level retry effectively off by default, `maxDownloadRetryCount = 0`).
- Manifest handling: media-playlist only (a master parses to zero segments →
  error); encrypted HLS rejected outright. A pre-parsed manifest can be
  injected (`HLSDownloadJobExtraConfig.hlsManifest`).
- Merging: plain concatenation of per-part temp files in ID order; output `.ts`.
  Part state persisted; completed parts re-validated on resume.

### Quality UI
- **Extension popup (in-page):** `MediaSelectionPopup.ts` injects a floating
  panel, repositioned above the **largest** `<video>`/`<audio>` element
  (`getBiggestMediaPlayer`, sorted by `clientWidth × clientHeight`).
  Collapsed header shows the item count; expanding lists one row per item:
  `duration · size · resolution · bandwidth · extension`; click a row to
  download. Dismissible.
- Desktop: no quality choice — variant choice happened entirely in the
  extension popup.

### Flags
None of the forbidden three. Encrypted HLS refused. Headers (which can
include the user's cookies) forwarded extension → desktop over localhost
(port 15151), same-user local-only. YouTube blacklisted.

---

## 3. Motrix v2 ("Turbo") (`agalwood/Motrix`, 58.7k★, TS/Electron)

**Licence: MIT.** Surprise find: v1 was a plain aria2 frontend, but the v2
rewrite has a real HLS/DASH media core. Distinctive idea: **delegate every
segment to aria2** while keeping manifest parsing, planning, decryption and
ffmpeg muxing in-app. No browser stream sniffing in-repo — media tasks come
from submitted manifest URLs.

### Stream detection / ranking
- None in-repo. `submit-download-adapter.ts` classifies submissions
  (`hls` / `dash` / `direct` / `magnet` / `mux`); `hls-dash-pipeline.ts`
  fetches and parses the manifest.
- Master handling (`core/media/hls-parser.ts` `parseHlsMaster`): picks the
  **highest-`BANDWIDTH` variant automatically** (plus matching `EXT-X-MEDIA`
  audio rendition). No user quality choice, no ranking beyond bandwidth.

### Segment fetching
- `core/download/segment-downloader.ts`: **one aria2 download per segment**
  (`aria2.addUri`, `out` like `000042.seg`); concurrency semaphore, default
  **16**. Video and audio plans download concurrently.
- Headers: `buildHeaders` replays caller-provided headers per segment
  (Referer, Cookie, etc.) and adds `Range` for byte-range parts.
- Retries, two layers: per aria2 call `max-tries: 5`, `retry-wait: 3 s`;
  on aria2 error the downloader re-submits up to 3 times — up to ~20 attempts
  per segment before the run fails. A retry restarts the segment from zero.
- Progress: 700 ms poll loop (`tellStatus` per active gid); completion/error
  via aria2 notifications; terminal results purged so aria2's store doesn't
  fill with thousands of segment rows.
- Manifest/key fetch (not aria2): `core/media/fetch-source.ts` uses undici
  `fetch` with `AbortSignal.timeout(10_000)` per request, **manual redirect
  handling** (max 5, strips `authorization`/`cookie`/`proxy-authorization`/
  `host` on cross-origin redirect). Manifests capped at 5 MB. Keys must be
  exactly 16 bytes. All URLs pass an SSRF guard (`admitHttpSource`) and
  `resolveUri` neutralises `.`/`..` segments.
- Parsing (`parseHlsMedia`): VOD only (no `#EXT-X-ENDLIST` and no
  `PLAYLIST-TYPE:VOD` → unsupported-live); AES-128 with explicit or
  sequence-number IV; SAMPLE-AES/unknown methods rejected.
- Merging: `segment-assembler.ts` stream-concatenates (init first, then parts
  in order, via `pipeline` — memory-efficient), then **ffmpeg muxes**
  (`FfmpegService`; the pipeline asserts ffmpeg is available *before* fetching
  anything). The coordinator probes mux inputs and diagnoses a known failure:
  a tiny "text/html?" segment is a 403/error body aria2 saved as complete —
  i.e. it validates that segments are really media.
- Manifest texts persisted with the task (`media-meta-store.ts`:
  `master.m3u8`/`video.m3u8`/`audio.m3u8`/`manifest.mpd`) for resume/debugging
  — the same "keep the manifest text" idea as XDM's `HlsPlaylistStore`.

### Quality UI
None in the media path — auto highest-bandwidth variant only.

### Flags
None of the forbidden three. SAMPLE-AES rejected; no Widevine/DRM code; live
streams rejected rather than worked around.

---

## 4. res-downloader (`putyy/res-downloader`, 21.3k★, Go)

**Licence: Apache-2.0.** Headline: **it has no HLS segment fetcher at all.**
It detects `.m3u8` URLs and lists them, but the UI refuses to download them
(`Action.vue` blocks `live`/`m3u8` rows; `batchDown` skips them) and the README
FAQ points users at an external web tool. No m3u8 parser, no segment fetching,
no ffmpeg, no merging. For ATLANTIC it offers zero directly applicable HLS
techniques — but its plain-file downloader and sniffing architecture still
yield transferable ideas, and one anti-pattern to avoid.

### Stream detection — a different architecture
- **System-wide MITM proxy, not a browser extension** (`core/app.go`,
  `core/proxy.go`, goproxy): installs a self-generated CA into the OS trust
  store, sets the system proxy, MITMs by default (rule `*`, with
  `*.domain`/`!` negation in `core/rule.go`).
- Detection on the response side (`plugin.default.go` `OnResponse`): status
  200/206/304 + **Content-Type matched against a configurable MIME map**
  (`config.go` `getDefaultMimeMap`) — not URL extensions. Four MIME types map
  to `m3u8`. No DOM, no resource timing, no player awareness.
- Dedup by MD5(URL). Per-domain plugin registry (only `default` + `qq.com`
  exist). The qq.com plugin extracts media metadata from the page's own JS
  objects — the only "smart" detection in the project.

### Stream ranking: none exists
No master-playlist preference, no size/duration heuristics, no ad/preview
distinction. Everything matching is listed in arrival order; the user picks.
Size is displayed and user-sortable, nothing more. **Negative lesson for
NOVAHQ:** without ranking, a preview `.mp4` and the real stream are
indistinguishable rows. The one useful idea is the qq.com approach — prefer
the URL the player itself loaded over anything that merely appeared in traffic.

### File downloader — transferable ideas (`core/downloader.go`)
- **HEAD probe first**: reads `Content-Length`/`Accept-Ranges`; multipart only
  if `Accept-Ranges: bytes` and size > 1 MB, else single-stream.
- **Concurrency**: default `NumCPU × 2` parts, 1 MB minimum part size.
- **Anti-pattern to avoid**: `buildClient()` creates a fresh `http.Client` /
  transport per task, defeating its own keep-alive settings
  (`MaxIdleConnsPerHost: 100`, 90 s idle). FetchForge should use one shared
  client/pool so segments reuse the warm manifest connection.
- **Retries**: 3 attempts, fixed 3 s delay, per-part resume from bytes already
  written via recomputed `Range`.
- **Parallel-first, sequential fallback**: if multipart fails after retries,
  the whole download restarts single-threaded — directly applicable to HLS.
- **Timeouts: none on the downloader** — reliability comes from retries +
  range resume, not timeouts.
- **Headers**: replays the sniffed browser request headers minus a denylist
  (`accept-encoding`, `host`, `connection`, `sec-fetch-*`, `sec-ch-ua*`,
  `if-*`, `x-forwarded-for`, `x-real-ip`, …). Default UA is a fixed Chrome 129
  string; default Referer is `scheme://host/` of the URL. **This header replay
  is likely why its downloads work on protected CDNs.**
- No explicit HTTP version or TLS config — Go defaults.

### Merging / quality UI
- Merging: none. Multipart files assembled via `WriteAt` into a pre-truncated
  file. No ffmpeg anywhere.
- Quality choice: none per-resource. UI is a data table with type filters,
  size sort, preview (video.js via a local proxy forwarding `Range`), batch
  download/export. The only quality control is a global setting affecting
  only WeChat videos.

### Flags
- **Session/credential reuse (present):** full request headers including
  cookies/authorization are captured and replayed. FetchForge must replay
  only the benign subset.
- **Content-obfuscation reversal (present):** WeChat videos are XOR-decrypted
  post-download using a key extracted from the page's JS
  (`Resource.decodeWxFile`). Not Widevine DRM, but in the same neighbourhood
  — do not implement.
- **In-flight JS rewriting (present):** the qq.com plugin rewrites
  third-party JS responses to inject fetch hooks exfiltrating media metadata
  — do not copy; extension APIs are the legitimate route.
- **Trust surface:** whole product is a MITM proxy with an installed CA
  decrypting all HTTPS by default — explains its detection completeness, and
  why FetchForge's extension approach is the safer design.
- No Widevine/DRM bypass or paywall bypass observed.

---

## 5. ODM / OpenDM (`henglyrepo/Open-Download-Manager`, 0★, Rust/Tauri)

**Licence: MIT.** Same stack as FetchForge (Rust + Tauri + reqwest) but v0.1.0
and early: **no stream capture at all** (README roadmap lists video detection
and a browser extension as unchecked; zero HLS/DASH/yt-dlp code). Included
because its reqwest chunk config is directly comparable — mostly as a
cautionary example.

### Segment fetching (plain-file chunking only)
- `src-tauri/src/http/mod.rs`: reqwest 0.12 (rustls), **300 s total timeout**,
  `tcp_nodelay(true)`, `tcp_keepalive(60 s)`. A `ClientPool` holds N separate
  clients (round-robin).
- `src-tauri/src/download/mod.rs`: HEAD for Content-Length; chunk count by
  size (<10 MB → 4, <100 MB → 8, <1 GB → 16, else 32); one tokio task per
  chunk; `Range` requests with resume; per-chunk `.chunk_N` + `.progress`
  files; merge = sort chunk files by ID and concatenate.
- Retry: `MAX_RETRIES = 5`, exponential backoff 1 s → 60 s cap.
- **Anti-patterns worth noting:** (1) `download_single_chunk` builds a **fresh
  reqwest `Client` per chunk attempt** — connection reuse defeated (the
  `ClientPool` is not used on this path). (2) **No custom headers at all** —
  no User-Agent, Referer, or Accept; only `Range`. (3) **300 s total timeout
  with no separate connect/read timeout** — a stalled segment hangs up to
  5 minutes before any retry. (4) `merge_chunks` reads each whole chunk file
  into memory before writing.

### Portable bits
`tcp_nodelay` + 60 s TCP keepalive on the reqwest builder; size-tiered chunk
counts (4/8/16/32); per-chunk progress files for resume; exp. backoff with a
60 s cap.

### Flags
None — no capture, DRM, or credential handling at all.

---

## 6. JDownloader (light study — single file from SVN mirror)

**Licence: unverified from the mirror (project states GPL) — techniques only
until verified.** No official GitHub repo (SVN-based). Studied one file only:
`HLSDownloader.java` (2,033 lines) from the unofficial mirror
`mycodedoesnotcompile2/jdownloader_mirror`.

### Approach — delegate to ffmpeg
`HLSDownloader.runDownload()` does **not** fetch segments itself. It builds an
ffmpeg command line and lets ffmpeg's HLS demuxer download the playlist,
parsing ffmpeg's stderr progress lines (`frame=` / `size=` / `time=`) for
progress and bitrate × duration for size estimates. Retries are mostly *ffmpeg
flag* retries via a per-playlist `retryMap` (e.g. disable the `aac_adtstoasc`
bitstream filter when ffmpeg reports an unsupported codec, then re-run). Its
own HTTP requests use 30 s connect + 30 s read timeouts. No variant/quality
selection in this file (selection lives in the plugin/LinkGrabber layer).

### Notable technique
ffmpeg-delegation is the pragmatic fallback when a native segment fetcher
keeps failing on a hostile CDN: ffmpeg's HLS demuxer handles retries, byte
ranges, and discontinuities itself. Trade-off: no per-segment control,
progress only via stderr parsing, external binary dependency. (Motrix takes
the middle path: native segment fetch + ffmpeg only for muxing.)

### Flags
None observed in the examined file. (JDownloader is known to include
captcha-solving features elsewhere — not examined, and not something
FetchForge should touch.)

---

## 7. TTsanchez/media-downloader (Python/Tkinter, yt-dlp as a library)

**Licence: MIT.** The strongest of the three yt-dlp GUIs for our purposes:
best yt-dlp tuning notes, a stall-detection reconnect, and the best format UI.

### yt-dlp / ffmpeg config (`downloader.py`)
- Probe (`extract_info`): minimal opts, `socket_timeout: 30`. No custom
  UA/referer/headers.
- Download: `continuedl`, **`retries: 10`, `fragment_retries: 10`**,
  **`concurrent_fragment_downloads: 8`** (comment: "noticeably faster on HLS
  and livestreams"), **`buffersize: 16 MB`**, **`http_chunk_size` deliberately
  NOT set** (comment: with it, yt-dlp reopens the connection every megabyte
  and is "several times slower on a fast link" — directly contradicts
  quinnuk; see §8). Default format `bv*+ba/b`; audio mode `ba/best` +
  `FFmpegExtractAudio`; `merge_output_format: mp4`.
- **ffmpeg reconnect flags** — only on the clipped-fragment path, via
  `external_downloader_args["ffmpeg_i"]`: `-reconnect 1`,
  `-reconnect_streamed 1`, `-reconnect_on_network_error 1`,
  `-reconnect_on_http_error 400,403,404,408,429,5xx`,
  `-reconnect_delay_max 5`, `-reconnect_max_retries 5`.
- **Client fallback ladder** (`CLIENT_FALLBACKS` / `client_order()`): retries
  across YouTube player clients (`visionos`, `tv_samsung`, `default`,
  `android_vr`, `web`); retry decision by substring matching against
  `RETRYABLE_MARKERS`, linear backoff.
- **Post-download verification** (`_verify_fragment_output`): ffprobe-checks
  the output for expected video/audio streams and, on a missing track, deletes
  the file and retries once with fresh links and an H.264-preferring selector.
  Design idea: verify the *output file's streams*, not just the exit code.
- **Stall detection in its own HTTP client** (`net.py`, used for tools/updates,
  not media): if throughput < 50 KB/s over a 60 s window, abort and reconnect
  (up to 3 times) with Range resume — comment says a fresh connect often lands
  on a different CDN node. The closest thing in this study to a fix for
  "connection open but segment body never arrives".
- Graceful degradation: if an optional postprocessing step fails, retry without
  that step rather than failing the job.

### Format / quality UI (`main.py`) — the model to copy
- **Preset dropdown** (Auto, 2160p, 1440p, 1080p, 720p, 480p): builds
  `bv*[height<=H]+ba/b[height<=H]`; choosing a preset clears any table
  selection and vice versa.
- **Format table** (Treeview: quality / type / codec / fps / size / note),
  from one `extract_info` call: row 1 "Best" (`bv*+ba/b`); one merged row per
  distinct height (descending) — best video at that height (max by filesize)
  + best audio, size = sum of both, note "merge to MP4"; then every raw
  video-only format (height, then size, descending); then top 12 audio-only
  formats by filesize desc. Audio-only selection changes merge/embed behaviour.
- **Size estimation**: sums `filesize`/`filesize_approx` of the chosen pair
  and scales by the fragment ratio when a time range is set.
- `extract_info` recomputes `info["height"]` as the max height across formats.

### Stream detection
None. Fully delegated to yt-dlp.

### Flags
- **PO-token server** (`pot.py` + `youtubepot-bgutilhttp` extractor arg):
  circumvents YouTube's bot checks. Not DRM/paywall/cookie theft, but an
  anti-bot circumvention mechanism — flagged, do not copy.
- Cookies: standard user-supplied yt-dlp `cookiefile` / `cookiesfrombrowser`;
  secrets registered for log redaction. No theft mechanism. No DRM/paywall
  bypass.

---

## 8. quinnuk/VideoDownloader (Python/CustomTkinter, yt-dlp as a library)

**Licence: MIT.**

### yt-dlp config (`downloader.py`)
- `FORMAT_SELECTORS`: `best` → `bestvideo+bestaudio/best`; each resolution →
  `bestvideo[height<=H]+bestaudio/best[height<=H]/best`; `audio_only` →
  `bestaudio/best`.
- `continuedl`, `nopart: False` (keeps `.part` for pause/resume),
  `retries`/`fragment_retries`: **3**, fixed 5 s retry sleep via
  `retry_sleep_functions` for both http and fragment retries.
- **`http_chunk_size: 10 MB`** — comment: large/high-bitrate streams are most
  likely to have the CDN drop one long-lived connection mid-stream ("X bytes
  read, Y more expected"); chunking means a dropped connection costs only one
  chunk, which then retries cleanly. **The opposite of TTsanchez's deliberate
  omission — the two projects disagree, and both give a reason.** For a CDN
  that stalls mid-body, quinnuk's rationale is the more relevant one.
- No `concurrent_fragment_downloads` (yt-dlp default 1), no `socket_timeout`
  override, no `buffersize`, no `external_downloader_args`.
- Pause/cancel via progress-hook exceptions + `continuedl` + kept `.part`.
- Error classification (`classify_error`) into user-facing reasons; URL
  normalisation (strips tracking params) for duplicate detection.
- UI failure suggestions include "Try Best Available quality" — quality
  downgrade as a user-facing retry hint.

### Format / quality UI
Dropdown only (`ui.py`): Best Available, 2160p/4K, 1440p, 1080p, 720p, 480p,
360p, Audio Only (MP3). Default `best`. No format table; the probe
(`get_video_info`) does not even fetch formats. Audio Only disables
container/subtitle controls and enables the bitrate menu.

### Stream detection
None. Fully delegated to yt-dlp.

### Flags
Cookies: `cookiesfrombrowser` only, user-selected browser, "never read,
displayed, logged, or transmitted" by the app. No DRM/paywall bypass, no
anti-bot circumvention.

---

## 9. shaoen901109/media-downloader-win (Python CLI + PowerShell GUI, yt-dlp binary)

**Licence: MIT.**

### yt-dlp config (`src/mdl.py`)
- Common args: `--newline --no-warnings --ignore-config
  --concurrent-fragments 4 --retries 10 --fragment-retries 10
  --socket-timeout 20 --no-overwrites --windows-filenames`.
  (`--ignore-config`: deliberately ignores the user's own yt-dlp config so
  behaviour is reproducible.)
- Format selectors: `bv*+ba/b`; capped
  `bv*[height<={H}]+ba/b[height<={H}]/bv*+ba/b`; sort `-S ext:mp4:m4a`;
  `--merge-output-format mp4 --embed-metadata`. Audio: `ba/b` + `-x
  --audio-format mp3`, quality choices `0/320K/256K/192K/128K`.
- **`--impersonate chrome` retry**: on first failure, the exact command is
  retried once with `--impersonate chrome` added (TLS fingerprint
  impersonation; needs the curl_cffi build). If the build lacks impersonate
  support, the error is detected and the *original* error is reported.
  ATLANTIC relevance: moderate — a retry-after-failure, not a first-attempt
  setting.
- **Vertical-video width fallback** (`_inject_width_fallback`): for portrait
  video the short side is the width, so `height<=N` matches nothing; the
  builder inserts `bv*[width<=N]+ba` / `b[width<=N]` before the uncapped
  fallback. Worth copying into any height-capped selector.
- **No-ffmpeg degradation**: if ffmpeg is absent, the format is rewritten to
  prefer already-merged single files and all ffmpeg-dependent flags are
  stripped, with a warning.
- **Windows MAX_PATH budgeting**: reserves ~50 chars for yt-dlp's fragment
  temp suffixes when capping titles, so the *temp* filename never exceeds 260
  chars even when the final name fits.
- Bulk pacing: gaussian sleep between batch items (mean 2 s).
- Own HTTP download (`_http_download`, IG photos only): stdlib `urllib`,
  `.part` then rename, 3 attempts, **4xx (except 429) not retried**; hardcoded
  Chrome 131 UA.

### Format / quality UI
- GUI dropdown only (`tools/gui.ps1`): `best, 2160, 1440, 1080, 720, 480, 360`;
  default `best`. No per-video format list in the GUI.
- CLI/MCP `formats` action (`list_formats`): runs `yt-dlp -J --no-playlist`,
  returns each format's `format_id, ext, resolution, fps, vcodec, acodec,
  filesize` — **in yt-dlp's native order, unsorted and unfiltered**. Useful as
  a data contract, not a UX.

### Stream detection
None for video. (Photo-related selection only: `_best_ig_image_url` picks the
largest IG thumbnail variant — variant selection by size.)

### Flags
- **Login-browser cookie export** (`tools/browser.ps1`): opens a real browser
  for the *user* to log in, then exports that profile's cookies to a temp
  `cookies.txt` for yt-dlp. The user's own session, exported with their
  action — not theft — but the closest thing to credential handling in this
  study. FetchForge will not build cookie export.
- No DRM bypass, no paywall bypass.

---

## 10. Hamax (hamax.app) — docs only, NO source available

**Licence: unknown / effectively closed for our purposes.** Hamax's site
repeatedly calls it "open source", but no repository exists on GitHub (API
searches for HAMAX / "HAMAX video downloader" / "hamax downloader" return no
downloader repo), the homepage shows no source-code link, and AlternativeTo
lists it as Proprietary. Studied ONLY from its public docs/blog (2026-11-22);
do not download or install it on the strength of this study.

Documented HLS approach (blog: "How to Download M3U8 & HLS Video Streams on
Windows", 2026-08-01):
- Pipeline: parse the `#EXTM3U` manifest → fetch the highest-bitrate video
  variant → **concurrently download all `.ts` chunks (32 parallel
  connections)** → concatenate sequentially → remux into `.mp4` (stream copy,
  no re-encode).
- Auto-detects `.m3u8` master playlists playing in the browser; merges `.ts`
  natively on disk without CLI tools.
- Comparison table claims multi-threaded parallel (32×) vs ffmpeg's
  single-threaded sequential download, and "instant NVMe remux".
- vs/IDM page: positions against IDM (paid, Windows-only, no torrents) with a
  feature table (video/stream capture HLS+DASH+direct media, resume, parallel
  connections, scheduling, browser hand-off Chrome/Edge/Firefox, no telemetry).
  Concedes IDM's real edge: "twenty-five years of edge cases… every strange
  server, every broken redirect, every site that fights downloaders — it has
  met them and has a workaround."

### Flag
- **AES-128 key handling claim:** the blog says an advanced tool "automatically
  fetches the session key from the browser request context and decrypts the
  `.ts` chunks during the merge phase". Fetching a key using the browser's
  session/request context crosses our no-cookie-theft line — **do not copy**.
  Note the distinction: fetching a key from the plain key URI printed in the
  manifest itself is standard HLS (ffmpeg, XDM, Motrix all do it); FetchForge
  currently refuses encrypted playlists outright, and changing that is a
  product decision for the owner, not something this study recommends.
- The same blog links to a guide on "bypassing hidden player restrictions" —
  not studied, not endorsed.

### What Hamax still teaches us
Its documented numbers (32-way parallelism, master-first, native merge) agree
with XDM/ABDM/Motrix: parallel segment fetching over a pooled client is the
industry-normal design, and our sequential single-client design is the outlier.

---

## 11. Guidelines for FetchForge (prioritised)

### A. Atlantic — segment timeouts (manifest OK, segment 1 times out)

**A1. One shared pooled HTTP client per download. (Highest priority.)**
Build a single `reqwest::blocking::Client` per download and pass it to every
manifest/segment fetch, so segments reuse the warm connection that fetched the
manifest. Today `fetch_bytes_range` builds a fresh client per segment — the
same anti-pattern res-downloader (`buildClient` per task) and ODM
(`download_single_chunk` per chunk) exhibit, and the opposite of XDM/ABDM's
single pooled OkHttp client. A cold TCP+TLS handshake per segment is a prime
suspect for a segment-1 timeout on a CDN that rate-limits new connections.
Evidence: XDM `HttpClientImpl.kt`; ABDM `Di.kt`; res-downloader
`core/downloader.go`; ODM `src-tauri/src/download/mod.rs`.

**A2. Replay benign browser headers on segment requests.**
Send a real browser User-Agent (current Chrome string, not `FetchForge/0.2`),
plus Referer (already carried), Origin, Accept, and Accept-Language — the
headers the browser itself sent when it played the stream. Every working
manager in this study replays browser headers (res-downloader, XDM, ABDM,
Motrix); ODM sends none and is the cautionary case. The extension can capture
the player request's headers (XDM does this via `webRequest.onSendHeaders`)
and forward the benign subset with the download request. Apply a denylist when
replaying (res-downloader `forbiddenDownloadHeaders`; XDM `CaptureHandler`
blocklist): drop `accept-encoding`, `host`, `connection`, `sec-fetch-*`,
`sec-ch-ua*`, `if-*`, `x-forwarded-for`, `x-real-ip`, `content-*`. **Never
Cookie or Authorization** (see §12).

**A3. Force HTTP/1.1 for HLS segment fetches.**
XDM and ABDM both force HTTP/1.1 (`protocols(listOf(HTTP_1_1))`); XDM's
`ManifestUtils.kt` notes some CDNs reject Range with 400 over HTTP/2.
reqwest negotiates HTTP/2 by default — try `http1_only(true)` on the HLS
client and A/B-test against totallyacdn.cc.

**A4. Test Host-header-first ordering.**
XDM's `HostFirst` interceptor exists because a CDN answered 410 to every
segment until `Host` was moved to the first header position. reqwest/hyper
header order differs from browsers. Testable on totallyacdn.cc; feasibility
(hyper sets Host automatically) to confirm during implementation.

**A5. Fetch segments in parallel (bounded), with sequential fallback.**
Move `download_segments` from strictly sequential to a bounded worker pool
(4–8 concurrent segments: XDM/ABDM default 8; yt-dlp GUIs 4–8; Hamax 32 is
the aggressive end). Write each segment to its own temp file (or `WriteAt` at
computed offsets) and assemble in order, keeping the sidecar resume design
(per-segment completion records instead of a single `done=` counter). On
failure of the parallel path, fall back to sequential fetching of the
remaining segments before giving up (res-downloader's multipart →
single-thread degradation). Parallelism also unlocks A7's retry model.

**A6. Per-read stall timeout + throughput-based stall detection.**
Replace reliance on the 120 s total per-segment timeout with: a per-read stall
timeout (XDM 60 s read; ABDM/Motrix 10 s) AND a throughput floor — abandon and
reconnect a segment fetch that stays below ~50 KB/s over a 60 s window, with
`Range` resume from bytes already received (TTsanchez `net.py` stall
detection, explicitly to dodge throttled CDN nodes; up to 3 reconnects). A
fresh connect may land on a different CDN node. Keep the existing 5-minute
whole-download stall watchdog as the outer bound.

**A7. Adopt XDM's shared-progress retry model (pairs with A5).**
Replace the current 3-attempt / fail-fast-at-0-bytes rule with a
`DownloadClock`: while the download as a whole receives *any* data, failures
only wait and retry; only after ~15 s of total silence do refusals
(403/410/404-on-segment) count as an expired link; server errors get a 5 min
budget; backoff 1 s doubling to a 10 s cap + up to 25% jitter so parallel
segments don't retry in lockstep; honour `Retry-After` (capped 5 min).
Note: with sequential fetching, fail-fast-at-0-bytes stays correct (nothing
else is progressing); this model only pays off once A5 lands. Keep the honest
error surfacing from round 5 (`reqwest_error` source-chain walk).

**A8. Capture the manifest BODY in the extension.**
Add a MAIN-world fetch/XHR hook (XDM `hls-hook.js`) that clones the playlist
response the player actually received and sends the text to the app with the
URL. Some CDNs sign a playlist link for a single request, so our re-fetch
gets 403; a finished playlist (`#EXT-X-ENDLIST`) should then never be
re-fetched. Persist the captured text with the download record (XDM
`HlsPlaylistStore`; Motrix `media-meta-store.ts`) for resume and debugging.
Caveat from XDM: worker-loaded playlists are not seen by the hook — the app
still fetches those.

**A9. Tune the yt-dlp fallback path for HLS.**
For downloads routed through yt-dlp: `concurrent_fragment_downloads` 4–8,
`fragment_retries` 10, `socket_timeout` 20–30 s (all three yt-dlp GUIs
converge here). A/B-test `http_chunk_size` 10 MB on totallyacdn.cc
(quinnuk's rationale — a dropped connection costs only one chunk — vs
TTsanchez's deliberate omission for speed on healthy links). Add a one-shot
`--impersonate chrome` retry after a first failure (shaoen; detect builds
lacking impersonate support so the real error is reported). Add ffmpeg
`-reconnect*` flags (`-reconnect_on_http_error 400,403,404,408,429,5xx`,
max retries 5) to any ffmpeg-driven HLS path (TTsanchez).

**A10. Verify output streams after download.**
After a fragment/HLS download, ffprobe-check the output for the expected
video/audio streams; a zero exit code with a missing track is a real failure
mode (TTsanchez `_verify_fragment_output`). On a missing track, delete and
retry once with fresh links. Also validate segments are really media before
muxing — Motrix's coordinator diagnoses tiny `text/html` segments as saved
403/error bodies.

**A11. Small HTTP details.**
No `Range` on manifests (XDM: "browsers never send one for a playlist");
Range only for segment resume, with absolute inclusive end, and 416 →
restart the segment from 0 (XDM `StreamingChunkRetriever`). Consider Happy
Eyeballs / fast fallback (XDM `fastFallback(true)` — one dead CDN address
costs 250 ms instead of a failed retry; reqwest/hyper exposure to confirm).
`tcp_nodelay(true)` + 60 s TCP keepalive on the reqwest builder (ODM's one
portable bit). If the native fetcher keeps failing on a hostile CDN, the
pragmatic fallback is ffmpeg delegation (JDownloader `HLSDownloader`): let
ffmpeg's HLS demuxer do the fetching and parse its stderr for progress.

### B. Novahd picker — preview clip vs the playing playlist

**B1. Suppress direct-file candidates once a manifest is seen on the tab.**
(XDM `m3u8MpdTabs`.) Today our picker ranks playlist-first but still sends the
clip whenever no playlist appears in any frame's timing log. Suppression plus
B2–B4 closes the gap from both ends: make the playlist observable, and make
the clip ineligible once any playlist exists on the tab.

**B2. Capture the manifest the player requested (MAIN-world hook).**
Same mechanism as A8: a `document_start` MAIN-world fetch/XHR hook in every
frame clones the playlist response the player received. This directly answers
why novahd's playlist never appears in the resource-timing log — the player
may fetch it before our content script runs, from a worker, or via a path the
timing API doesn't surface. The hook sees what the player sees. (XDM
`hls-hook.js`; res-downloader's qq.com lesson: prefer the URL the player
itself loaded.)

**B3. Master-first grouping with variant sizing.**
Parse the master (in the extension or the app) instead of recognising masters
by stem name alone: one entry per variant, grouped under the master; variant
media playlists the player loads size the entries and are never listed
separately (XDM `HlsMasterGroup` / `processHLSVideo`). A media playlist
arriving before its master still counts. This replaces stem-regex guessing
with structure.

**B4. Rank by duration and size from the playlist.**
Sum `#EXTINF` for duration; size variants from byte ranges /
`AVERAGE-BANDWIDTH` / `BANDWIDTH` (XDM `StreamSize.kt`). A 15-second clip vs
a 2:52 feature separates cleanly on duration; size alone does not (the novahd
clip is 3.1 MB, above XDM's 1 MB min-size threshold, so a size floor alone
would not have saved us). Apply a min-size floor for direct files anyway
(XDM 1 MB default; ABDM configurable) as a cheap first filter.

**B5. Fragment suppression rules.**
`video/iso.segment` content-type is always a fragment, never a video; an
`init.mp4` URL marks its whole folder as suspected fragments; distinguish
`resourceType` `"media"` (a `<video>`/`<audio>` element loaded a whole file)
from `"xmlhttprequest"` (script-fed MediaSource, heuristic path) (XDM
`requestVerdict` / `isHttpVideo`).

**B6. Anchor the picker to the biggest player.**
Sort page `<video>`/`<audio>` elements by `clientWidth × clientHeight` and
anchor stream choice to the largest — the playing feature, not a thumbnail or
ad player (ABDM `getBiggestMediaPlayer`).

**B7. Classify by Content-Type, not URL shape.**
A MIME map (`application/vnd.apple.mpegurl`, `application/x-mpegurl`, … →
playlist; `video/mp4` → file) distinguishes playlists from previews
regardless of URL shape (res-downloader `getDefaultMimeMap`). The extension
can read content types via `webRequest.onHeadersReceived`.

**B8. Per-domain adapter registry (later, if needed).**
A small registry keyed by effective domain lets site-specific quirks (like
novahd's preview-clip pattern) live in isolated adapters instead of polluting
the generic picker (res-downloader's per-domain plugin registry).

### C. Quality popup — IDM-style choice at click time

**C1. Copy TTsanchez's format table as the popup body.**
One `extract_info` (download=False) call, then: row 1 "Best" (`bv*+ba/b`);
one merged row per distinct height, descending — best video at that height
(max by filesize) + best audio, size = sum of both, note "merge to MP4";
then raw video-only formats; then audio-only formats (top 12 by size).
Columns: quality / type / codec / fps / size / note. Sorting: height desc,
then filesize desc. A preset row above the table (Auto / 2160p / 1440p /
1080p / 720p / 480p) builds `bv*[height<=H]+ba/b[height<=H]`; preset and
table selection mutually exclusive. Live size estimate for the current
selection. Default selection: Best (all three yt-dlp GUIs default to Best).

**C2. Use XDM's row vocabulary and audio modes.**
Rows read like `1080p · 5.2 Mbps · H.264` (resolution · bandwidth · friendly
codec name: H.264/H.265/AV1/VP9/AAC/Opus); HLS sorted by `BANDWIDTH` desc.
Audio modes: CHOOSE (separate tracks) / INCLUDED (muxed in) / NONE;
audio-only entries shown as their own rows. Size estimates carry uncertainty
markers: exact size, `≈` (average bandwidth), `~` (peak bandwidth), `Live`.

**C3. For extension-detected streams, use ABDM's in-page popup shape.**
Floating panel above the biggest player (B6), one row per variant:
`duration · size · resolution · bandwidth · extension`; collapsed header
shows the count; click a row to download; dismissible. Variant data comes
from the parsed master (B3), so no extra probe call is needed.

**C4. Width fallback for portrait video.**
In any height-capped selector, insert `bv*[width<=N]+ba` / `b[width<=N]`
before the uncapped fallback — for portrait video the short side is the
width, so `height<=N` matches nothing (shaoen `_inject_width_fallback`).

**C5. Data contract.**
Format-list payload per item: `format_id, ext, resolution, fps, vcodec,
acodec, filesize` (filesize else `filesize_approx`) — shaoen's
`list_formats` JSON shape. Recompute top-level height as the max across
formats (TTsanchez: without an explicit format, yt-dlp's top-level height
reflects an arbitrary format).

**C6. Failure-recovery hint.**
On a failed download, offer "Try Best Available quality" / "Try a different
format" as a one-click retry (quinnuk UI failure suggestions).

---

## 12. What NOT to copy

### Licence traps
- **XDM (GPL-3.0-or-later) and JDownloader (GPL per project statement):**
  techniques only. Never paste code, identifiers, comments, or structure.
  XDM is the most instructive project here and the most dangerous to copy
  from — reimplement its ideas (retry clock, HostFirst, manifest capture,
  suppression rules) from scratch in Rust/JS.
- **MIT/Apache-2.0 projects** (ABDM, Motrix, ODM, res-downloader, the three
  yt-dlp GUIs): techniques freely reimplementable; if code were ever adapted,
  keep the licence notice and note the change. Our rule stands regardless:
  reimplement, don't paste.
- **Hamax:** no source found; treat as closed. Docs only.

### Spec-forbidden lines (FetchForge spec §10: no DRM bypass, no cookie theft, no paywall bypass)
- **Cookie / Authorization replay.** res-downloader replays captured headers
  including cookies and auth tokens. XDM/ABDM/Motrix forward the user's own
  browser cookies to their desktop app over localhost (same-user, local-only,
  standard download-manager behaviour). FetchForge's spec forbids cookie use
  outright — we replay only the benign header subset (A2) and build no cookie
  forwarding or cookie export (cf. shaoen's login-browser cookie export:
  user's own session, but still not something we build).
- **Content-obfuscation reversal.** res-downloader XOR-decrypts WeChat videos
  with a key extracted from page JS. Not Widevine, but the same neighbourhood.
  Do not implement.
- **In-flight JS rewriting.** res-downloader's qq.com plugin rewrites
  third-party JS responses to inject fetch hooks. Do not copy — extension APIs
  (webRequest, MAIN-world hooks on our own content scripts) are the
  legitimate route to the same information.
- **MITM proxy + installed CA.** res-downloader's whole architecture decrypts
  all HTTPS by default. Explains its detection completeness; a trust posture
  FetchForge will not adopt.
- **Session-context key fetching.** Hamax's claim to fetch AES-128 keys "from
  the browser request context" crosses the cookie/session line. Standard
  manifest-URI key fetching is normal HLS (ffmpeg/XDM/Motrix) — but
  FetchForge currently refuses encrypted playlists, and changing that is an
  owner decision, not a study recommendation.
- **Anti-bot circumvention.** TTsanchez's YouTube PO-token server (`pot.py`)
  circumvents bot checks. Not in the forbidden three, but do not copy.
- **Captcha solving.** JDownloader is known to include it elsewhere in its
  tree (not examined). Do not touch.
- **"Bypassing hidden player restrictions".** Hamax blog links to such a guide;
  not studied, not endorsed.

### Bugs and anti-patterns to avoid (not lines, but traps)
- ABDM's resolution label uses `width + "p"` (1920×1080 → "1920p").
- ODM: fresh reqwest client per chunk attempt; no headers at all; 300 s total
  timeout with no read timeout; merge reads whole chunk files into memory.
- res-downloader: fresh HTTP client per task defeats its own keep-alive.
- ABDM's ranking: expanding every variant into separate unranked items is the
  NOVAHQ failure mode, not a model.
- The `http_chunk_size` disagreement (quinnuk 10 MB vs TTsanchez unset) is
  unresolved — A/B-test on totallyacdn.cc, don't pick a side on faith.
- Motrix's failure mode: one permanently failed segment fails the whole run,
  and aria2 can "successfully" save an error page as a segment — hence
  content validation (A10).

---

## Sources

| Project | Repo / URL | Licence |
|---|---|---|
| XDM | github.com/subhra74/xdm | GPL-3.0-or-later |
| AB Download Manager (+ ext) | github.com/amir1376/ab-download-manager (+ `-browser-integration`) | Apache-2.0 |
| Motrix v2 | github.com/agalwood/Motrix | MIT |
| res-downloader | github.com/putyy/res-downloader | Apache-2.0 |
| ODM/OpenDM | github.com/henglyrepo/Open-Download-Manager | MIT |
| JDownloader | jdownloader.org (SVN); mirror mycodedoesnotcompile2/jdownloader_mirror | GPL (project statement, unverified) |
| TTsanchez/media-downloader | github.com/TTsanchez/media-downloader | MIT |
| quinnuk/VideoDownloader | github.com/quinnuk/VideoDownloader | MIT |
| shaoen901109/media-downloader-win | github.com/shaoen901109/media-downloader-win | MIT |
| Hamax | hamax.app (docs/blog only; no source found) | Unknown / effectively closed |

Working notes from the three researchers:
`/tmp/downloader-study/notes/res-downloader-notes.md`,
`/tmp/downloader-study/notes/ytdlp-guis-notes.md`,
`/tmp/downloader-study/notes/other-managers-notes.md`.
Shallow clones under `/tmp/downloader-study/` (ephemeral; re-clone if needed).
