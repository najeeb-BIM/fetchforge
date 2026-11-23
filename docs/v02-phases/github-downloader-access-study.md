# How existing open-source downloaders access and detect video streams — code-level study

Date: 2026-11-23. Research only: no source files were changed for this
document, no downloader was run against any live site, no laptop or browser
was touched.

## Scope and how this study relates to the two earlier ones

This is the third study in the series, and it goes one level deeper than the
first two:

- `downloader-study.md` (2026-11-22) covered 10 tools at engine level
  (parallel segments, retry policy, header handling, ranking). It is not
  repeated here; XDM, AB Download Manager, Motrix, res-downloader, ODM,
  JDownloader and the three yt-dlp GUIs are referenced only where a
  detection mechanism in this study overlaps one.
- `novahd-stream-research.md` (2026-11-23) ranked detection techniques
  against novahd's three hiding mechanisms (strict CSP blocking the inline
  MAIN-world hook, extensionless/proxy-wrapped playlist URLs, blob:/MSE
  playback) and recommended Tier 0 (manifest-declared MAIN-world script +
  timing-log hardening) and Tier 1 (observe-only `chrome.webRequest` +
  optional `<all_urls>`).

This study answers the user's explicit question from the 2026-11-23 voice
call: **how do the downloaders already out there get at streams?** Each tool
below is documented with (a) the exact detection/access mechanism(s) with
file/function references or links, (b) what it sees that FetchForge
currently misses, (c) the permissions/APIs it needs, and (d) its licence.
The final section ranks what FetchForge should adopt, in what order, and
what each mechanism unlocks — novahd first, then general hidden-stream
sites.

**FetchForge baseline this study maps against** (extension v0.2.7, round 8):
DOM scanning; resource-timing-log scan (5000-entry buffer, regex anchored
on `.m3u8`/`.mpd`, gated on finding a `<video>` element); MAIN-world
fetch/XHR hook injected as an inline `<script>` (blocked by strict CSP);
cross-frame election in background.js; clip guard (Range-probes direct mp4
on long-form pages, refuses known <60s clips); always-popup with quality
ladder. Manifest permissions: `activeTab`, `contextMenus`,
`declarativeNetRequest`, `declarativeNetRequestFeedback`, `downloads`,
`nativeMessaging`, `storage`; host_permissions ONLY
`http://127.0.0.1:18443/*` (the bridge). NO `<all_urls>`, NO `webRequest`
yet. Round 9 (approved 2026-11-23 12:41 PKT) adds Tier 0 + Tier 1.

## Boundaries (spec §10, standing)

Detection/access research ONLY. No DRM bypass, no auth/paywall bypass, no
cookie theft, no session hijacking. Where a tool's mechanism crosses one of
those lines it is flagged **EXCLUDED** in its section and kept out of the
ranked recommendations (§9).

## Licence rules applied in this document

- **GPL-3.0** (cat-catch, XDM, JDownloader): TECHNIQUES ONLY. No code,
  identifiers, comments or structure are reproduced here; FetchForge must
  reimplement any adopted mechanism from scratch.
- **AGPL-3.0** (cobalt API): techniques only; no code reuse, and no
  bundling into a possibly-paid product without legal review.
- **Unlicense / MIT / BSD-2-Clause / Apache-2.0**: permissive; code reuse
  is allowed subject to each licence's attribution terms, but this study
  still describes mechanisms rather than pasting code.
- Star counts are as found on 2026-11-23 and are approximate; they date
  fast.

## Summary table

| Tool | Type | Stars (approx) | Licence | Core detection mechanism |
| --- | --- | --- | --- | --- |
| cat-catch | Browser extension (MV3) | ~23K | GPL-3.0 | webRequest sniffing + response-header classification + MAIN-world MSE byte capture + deep API search |
| yt-dlp | CLI (Python) | ~210K | Unlicense (source) | Extractor registry; page/JSON/player-config parsing; manifest parsers |
| Video DownloadHelper | Browser extension | n/a | source NOT public | **Negative finding** — no code-level study possible |
| streamlink | CLI/library (Python) | ~13K | BSD-2-Clause | Plugin registry; declarative validate schemas over HTML/JSON; HLS/DASH stream classes |
| Lux (annie) | CLI/library (Go) | ~31.5K | MIT | Domain-keyed extractor registry; per-site page/API parsing; universal direct-resource fallback |
| cobalt | Web service + API (Node) | ~45K | AGPL-3.0 (API) + CC-BY-NC-SA-4.0 (web) | Per-service handlers; redirect/tunnel/picker responses; no in-browser detection |
| hls-downloader | Browser extension (MV2+MV3) | not verified | MIT | HLS sniffing incl. extensionless playlists and generic MIME types (v5.5.9) |
| N_m3u8DL-RE | CLI (C#) | ~8.9K | MIT | Engine only — takes a manifest URL as input, no detection |
| you-get | CLI (Python) | ~48.8K | not verified here | Site extractors; largely unmaintained since 2022 |
| gallery-dl | CLI (Python) | not verified | GPL-2.0 (not verified here) | Extractor registry, image-focused; noted only |
| mediago | Desktop app (Go) | ~9K | not verified here | Claims m3u8/HLS sniffing; not studied in depth |

---

## 1. cat-catch — the deepest in-browser detector studied

Repo: https://github.com/xifangczy/cat-catch — ~23K stars, **GPL-3.0**
(TECHNIQUES ONLY). Version studied: 2.7.4, Manifest V3. Studied from a
local shallow clone at `/tmp/gh-study/cat-catch` (read-only inspection;
nothing was run, built, or copied).

cat-catch is the most instructive tool in this study because it is the
only one that combines **all four** detection layers in one extension:
network sniffing, response classification, MAIN-world API hooking, and
MSE byte capture. It is also the tool whose design most directly validates
FetchForge's approved round-9 direction — and the tool whose header/cookie
handling FetchForge must NOT copy.

### 1.1 Layer 1 — webRequest sniffing (background.js)

Three `chrome.webRequest` listeners do the work, all on `<all_urls>`:

- **`onSendHeaders`** (with `requestHeaders` + `EXTRA_HEADERS`): saves
  the request headers per `requestId` in a Map, then runs the URL-regex
  path (`findMedia(data, true)`). This fires before the response, so a
  URL matching a user-configured regex rule is captured even if the
  response later fails or is a redirect.
- **`onResponseStarted`** (with `responseHeaders`): joins the stored
  request headers and runs the classification path (`findMedia(data)`).
  Firing on first byte means a stream is detected before it finishes
  downloading — important for long media.
- **`onErrorOccurred`**: cleans up stored headers and blacklist entries
  for failed requests.

An `onBeforeRequest` listener (URL regex + requestBody inspection) exists
in the file but is commented out — the project moved that work to
`onSendHeaders`/`onResponseStarted`.

**What it sees that FetchForge misses:** every request the browser makes,
regardless of origin frame, worker, or service worker — including
extensionless playlist URLs, `?url=`-wrapped playlist URLs, and requests
from cross-origin iframes. FetchForge's timing-log scan only sees entries
in the page's own performance timeline, misses worker/service-worker
fetches entirely (per the W3C spec, worker fetches go to the worker's own
timeline), and its regex is anchored on `.m3u8`/`.mpd`.

**Service-worker / tabId == -1 resolution** (background.js, `findMedia`
and `findInitiatorTab`): requests from service workers arrive with
`tabId == -1`. cat-catch resolves the owning tab by (1) taking the
request's initiator origin and querying tabs whose URL starts with that
origin, (2) exact referer match against a page URL, (3) service-worker
scope-prefix match, (4) the active tab, (5) the most-recently-accessed
tab, falling back to the last-known active tab. FetchForge has no
equivalent; any stream fetched by a service worker is invisible to it
today.

**Dedup and caps:** per-tab `Set` of URL fingerprints; cache capped at
9999 items per tab, then cleared. FetchForge should adopt a similar cap
if it adds webRequest sniffing, to bound memory on media-heavy pages.

**Permissions needed:** `webRequest`, `webRequest` with `extraHeaders`
(`EXTRA_HEADERS` in MV3 requires the `webRequest` permission plus host
permission for the URL), and host permission `<all_urls>` (or the
narrower per-site permissions the user grants). This is exactly the
round-9 Tier 1 permission set the user already approved.

### 1.2 Layer 2 — response-header classification (background.js)

`findMedia` classifies a response in this order:

1. **URL regex rules** (user-configurable `G.Regex`): a rule can
   blacklist a requestId (suppressing later capture of that request) or
   extract/transform the URL (capture groups joined, then
   `decodeURIComponent`). This is how cat-catch handles `?url=`-wrapped
   playlists and tracking-parameter stripping — the rule extracts the
   real URL from inside a wrapper URL. Four rules ship disabled by
   default (Instagram/Facebook CDN `bytestart` stripping, a Bilibili
   live blacklist, an Instagram cache-video DASH rule).
2. **Extension check** (`CheckExtension`): the URL's extension is looked
   up in a user-configurable map (`G.Ext`) — flv, mp4, mp3, webm, m4s,
   m3u8, mpd, ts (disabled by default), srt/vtt (disabled by default),
   and ~25 more — each with an enabled flag and a size-operator check
   (e.g. "only if larger than X").
3. **MIME-type check** (`CheckType`): the response `Content-Type` is
   looked up in a user-configurable map (`G.Type`) — `audio/*`,
   `video/*`, `application/vnd.apple.mpegurl`, `application/x-mpegurl`,
   `application/dash+xml`, `application/m4s`, and others — again with a
   size check. **This is the mechanism that catches extensionless
   playlists**: a playlist served from `/api/gs-hls` with
   `Content-Type: application/vnd.apple.mpegurl` is classified by its
   MIME type, not its URL.
4. **Content-Disposition check**: an `attachment` disposition with a
   filename is captured regardless of type.
5. **`resourceType == "media"` passes unconditionally** — if the browser
   itself classified the request as media, cat-catch trusts it.
6. **Generic-subtype inference**: for `application/octet-stream` and
   similar generic types, the `sec-fetch-dest` request header is used to
   infer mp3/mp4.

Header values are read by `getResponseHeadersValue`: `Content-Length`
→ size, `Content-Type` (split on `;`) → type, `Content-Disposition` →
attachment filename, `Content-Range` → total size (so a Range response
still yields the full file size).

**What it sees that FetchForge misses:** extensionless playlists
(novahd's `/api/gs-hls` case), playlists behind `?url=` wrappers,
media served with generic MIME types, and the true total size of
Range-served files. FetchForge's round-7 clip guard already Range-probes
direct mp4s; adopting the Content-Range total-size read would make that
probe cheaper (one response header instead of a size estimate).

**Permissions needed:** none beyond the webRequest layer above; the
classification is pure logic over headers the browser already exposes to
the extension.

### 1.3 Layer 3 — MAIN-world injected scripts (catch-script/)

cat-catch injects page-world scripts **on demand** via
`chrome.scripting.executeScript` with `world: "MAIN"`,
`injectImmediately: true`, and `allFrames` (background.js, the
`script`/`scriptI18n` message handlers). The scripts are NOT always-on:
the user triggers them (e.g. the "catch" / deep-search commands), which
limits both performance cost and the chance of breaking a page.

**`catch.js` — MediaSource / SourceBuffer byte capture**
(`proxyMediaSourceMethods`, the "core function" per its own comment):

- Proxies `MediaSource.prototype.addSourceBuffer`: when the player
  creates a SourceBuffer, cat-catch records its MIME type and creates a
  per-buffer capture record (held in a `WeakMap` keyed by the
  SourceBuffer).
- Proxies `SourceBuffer.prototype.appendBuffer` **on the prototype**
  (the comment notes an earlier instance-level hook was bypassed when
  players called the prototype directly): every appended chunk is
  pushed into the buffer's capture list, with a running byte total and
  an optional auto-download at 1 GB.
- Proxies `MediaSource.prototype.endOfStream`: marks capture complete
  and optionally auto-downloads.

This is the **last-resort capture for pure-MSE streams**: when no
retrievable URL exists at all (the player fetches segments through an
opaque path, or the manifest itself is unreachable), the bytes the
player appends to its SourceBuffers are still observable, and cat-catch
reassembles them into a downloadable file. The cost is that capture only
starts when the user enables it, and the file is only complete if the
user plays the media through (it also has an auto-restart feature that
seeks the video back to the start so capture begins at the beginning).

- **Anti-detection:** `Function.prototype.toString` is patched (via a
  Proxy on `Function.prototype.toString` plus a map of proxy →
  native-function string) so hooked functions still report native code
  to any page script that inspects them.

**`search.js` — deep search.** Wraps a wide set of page APIs and scans
the data flowing through them for media URLs: the `Worker` constructor
(so workers created after injection are also instrumented),
`JSON.parse`, `XMLHttpRequest.prototype.open` (with a readystatechange
listener that scans response text), `fetch` (clones the response and
scans its bytes), `TextDecoder.prototype.decode`,
`Array.prototype.slice`, typed-array `subarray` (`Int8Array`,
`Uint8Array`, …), `btoa`/`atob`. Each wrapper's `toString` is likewise
restored to look native. The point: a stream URL that only ever exists
inside a JSON API response, a base64 blob, or a decoded text buffer —
never as a request URL the network layer sees in a classifiable form —
is still found.

**`ump.js` / `ump.background.js` — YouTube UMP decoding.** YouTube's
newer UMP (Unified Media Protocol) transport is decoded in the page
world and the resulting tracks are registered with the background. Gated
behind a flag and disabled on Firefox. FetchForge does not need this:
YouTube is already handled via yt-dlp.

**`catch.js` also strips iframe `sandbox` attributes** by cloning
sandboxed iframes without the attribute (MutationObserver-driven,
referencing issue #576). Sandboxed iframes block content-script
injection, so this is how cat-catch sees inside them.
**EXCLUDED for FetchForge**: it mutates the page's DOM and security
posture. The legitimate alternative is `all_frames: true` +
`match_about_blank: true` content-script matching (which cat-catch also
uses in its manifest), which covers non-sandboxed cross-origin iframes
without touching the page.

**What it sees that FetchForge misses:** (a) streams whose segments are
only reachable through the player's own fetch path (MSE byte capture);
(b) stream URLs embedded in JSON/base64/decoded buffers (deep search);
(c) workers created after page load (Worker-constructor wrapping).
FetchForge's inline MAIN-world hook covers fetch/XHR only, is blocked by
strict CSP, and does not touch MediaSource at all.

**Permissions needed:** `scripting` (for `executeScript` with
`world: "MAIN"`), plus host permission for the target tab. Note that a
manifest-declared MAIN-world content script (the round-9 Tier 0 plan)
achieves the same injection without a user gesture and survives strict
CSP, because manifest-declared scripts are not subject to the page's
CSP; cat-catch's on-demand `executeScript` route is the alternative when
always-on injection is not wanted.

### 1.4 Layer 4 — manifest parsing and download (js/m3u8.js, js/mpd.js)

cat-catch re-fetches captured m3u8/mpd manifests from the extension and
parses them in-extension (m3u8.js is ~2461 lines; mpd.js ~185 lines),
then downloads segments itself or hands the job to an external tool
(N_m3u8DL-RE integration via a custom protocol handler, aria2 RPC, or a
local POST to `http://127.0.0.1:8000/` — a bridge shape very similar to
FetchForge's own desktop bridge). The in-extension parser handles key
rotation, byte ranges, and caption tracks. FetchForge already has its
own HLS/DASH engine in Rust, so this layer is **not** something to adopt
— but the *hand-off* pattern (extension detects, desktop engine
downloads) is the same architecture FetchForge already uses, which
validates the split.

### 1.5 Manifest and service-worker engineering (manifest.json, background.js)

- Permissions: `tabs`, `webRequest`, `downloads`, `storage`,
  `webNavigation`, `alarms`, `declarativeNetRequest`, `scripting`,
  `sidePanel`, `contextMenus`; optional `clipboardRead`. Host
  permissions: `*://*/*` + `<all_urls>`. Content scripts:
  `document_start`, `all_frames`, `match_about_blank`, http/https.
- `declarativeNetRequest` is used for mobile-UA emulation
  (`updateSessionRules`), not for detection — confirming the
  novahd-study finding that DNR is not a detection path.
- Service-worker keep-alive: no-op `webNavigation.onBeforeNavigate` /
  `onHistoryStateUpdated` listeners, a HeartBeat port, a 25-second
  `setInterval(chrome.runtime.getPlatformInfo)`, and alarms. MV3 service
  workers sleep; a webRequest-based detector must keep its worker alive
  or it misses requests. FetchForge's background script will need the
  same treatment when it adopts webRequest.

### 1.6 EXCLUDED mechanisms (do not adopt)

- **Cookie and auth-token capture.** `getRequestHeaders` saves a
  `DIRECT_INCLUDE_HEADERS` list that includes `cookie`, `authorization`,
  `auth`, `token`, `key`, `access-token`, `api-key`, `app-token`,
  `authtoken`, `session-id`, plus all non-browser custom headers (site
  tokens/signs); captured items store the cookie separately and the
  N_m3u8DL-RE hand-off includes a `${cookie}` placeholder. This is
  session-credential forwarding and is **excluded** by spec §10.
  FetchForge's bridge must continue to send only the URL plus
  non-credential headers (Referer, User-Agent).
- **Iframe sandbox stripping** (§1.3 above): DOM/security mutation.
  Excluded.

### 1.7 cat-catch → FetchForge mapping (summary)

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| webRequest onSendHeaders + onResponseStarted sniffing | **Yes — rank 1** (round 9 Tier 1, approved) | novahd extensionless playlists, worker/SW requests, cross-origin iframe requests |
| Response-header classification (extension + MIME + Content-Disposition + resourceType media) | **Yes — rank 1** (same work) | Extensionless playlists, generic-MIME media, Range total size |
| tabId == -1 resolution via initiator/referer/scope | **Yes — rank 1** (same work) | Service-worker-fetched streams |
| URL-regex transform rules (extract real URL from wrapper) | **Yes — rank 2** | `?url=`-wrapped playlists |
| Manifest-declared MAIN-world script (CSP-proof injection) | **Yes — rank 1** (round 9 Tier 0, approved) | Any strict-CSP site, novahd included |
| MediaSource/SourceBuffer byte capture | **Later — rank 4** | Pure-MSE streams with no retrievable URL (record mode) |
| Deep search (JSON.parse/fetch/XHR/TextDecoder/btoa wrapping) | **Later — rank 5**, opt-in | URLs hidden inside JSON/base64/decoded buffers |
| Service-worker keep-alive pattern | **Yes — rank 1** (supporting work) | Reliable webRequest capture in MV3 |
| Cookie/auth capture, sandbox stripping | **No — excluded** | — |

Sources: repo https://github.com/xifangczy/cat-catch (GPL-3.0); files cited
are from the v2.7.4 tree (`manifest.json`, `js/background.js`,
`js/init.js`, `js/content-script.js`, `js/m3u8.js`, `js/mpd.js`,
`catch-script/catch.js`, `catch-script/search.js`,
`catch-script/ump.js`, `js/ump.background.js`).

---

## 2. yt-dlp — extractors: how a page URL becomes a stream URL

Repo: https://github.com/yt-dlp/yt-dlp — ~210K stars (Nov 2026),
**Unlicense** for the source repository. Caveat, from yt-dlp's own README
licensing section: the PyInstaller-bundled executables include GPLv3+
code, so the combined binary is GPLv3+; the git repo, source tarball,
and PyPI distributions contain only Unlicense code. FetchForge already
shells out to yt-dlp for YouTube; this section is about what its
*extraction architecture* teaches, not about adopting yt-dlp itself.

yt-dlp does not run in a browser. It fetches pages and API responses
over plain HTTP and finds streams by **parsing** — which makes it the
complement to cat-catch's network sniffing: it finds streams that are
*declared* in a page, even when the browser has not (yet) requested
them.

### 2.1 The registry and the generic fallback (extractors.py, generic.py)

- Every site extractor is a class with a `_VALID_URL` regex and a
  `_real_extract(url)` method. `extractors.py` builds a `_CLASS_LOOKUP`
  dictionary, prioritising YouTube extractors first, then standard site
  extractors, with `GenericIE` last.
- `GenericIE` (`yt_dlp/extractor/generic.py:53`) matches **any** URL
  (`_VALID_URL = r'.*'`) and is the universal fallback. Its
  `_real_extract` runs a chain of heuristics:
  1. **Direct-URL detection**: `urlhandle_detect_ext()` inspects the
     response headers and extension of the URL itself. Direct video
     (`.mp4`, `.webm`, `.flv`), direct audio (`.mp3`, `.opus`,
     `.flac`), HLS (`.m3u8`, `application/vnd.apple.mpegurl`) via
     `_extract_m3u8_formats()`, DASH (`.mpd`,
     `application/dash+xml`) via `_extract_mpd_formats()`, SMIL
     (`.smil`) via XML parsing. Note the MIME-type check alongside
     the extension check — the same lesson as cat-catch §1.2, and
     yt-dlp explicitly supports "mpd manifests without extension".
  2. **Embedded-player detection** (`_extract_embeds`): scans the HTML
     for known player signatures and delegates to the matching
     extractor — JW Player (script/iframe src on
     `content.jwplatform.com` / `cdn.jwplayer.com`, or a
     `data-video-jw-id` attribute → `JWPlatformIE`), Vidyard
     (`play.vidyard.com/player/{id}.json`), Brightcove legacy
     parameters, Cloudflare Stream (`cloudflarestream.com` /
     `videodelivery.net` embeds), ViewLift
     (`prod-api.viewlift.com` entitlement + assets), Crooks and Liars
     (inline JSON manifests). This is a **player-signature registry**:
     the extractor does not need to understand the site, only the
     player embedded in it.
  3. **Structured metadata**: JSON-LD via `_search_json_ld()` (including
     m3u8 URLs inside JSON-LD, and `Article`-type JSON-LD), OpenGraph
     video URLs via `_og_search_video_url()`, meta tags via
     `_html_search_meta()`, plus `_search_nextjs_data()` and
     `_search_nuxt_data()` for framework-embedded page data.
  4. **HTML5 media elements**: `<video>`/`<audio>` `src` attributes.
  5. **Safety**: `determine_ext()` + `_UnsafeExtensionError` reject
     dangerous/executable extensions before any download.
- Extractor args allow TLS-fingerprint impersonation (e.g. Chrome
  profiles) — an anti-bot measure, not a detection mechanism; noted,
  not recommended for FetchForge's extension (the browser already
  presents a genuine fingerprint).

**What it sees that FetchForge misses:** streams declared in page JSON
(JSON-LD, `__NEXT_DATA__`, Nuxt payloads, player-config objects) that
the browser may not have requested yet, and streams behind embedded
third-party players whose URLs never appear in the page's own network
traffic in an obvious form. FetchForge's DOM scan looks for `<video>`
elements and timing entries; it does not parse inline JSON or player
configs. This is the cheapest mechanism in this study to adopt: it needs
no new permissions, just a content-script scan of `<script>` text and
meta tags.

### 2.2 YouTube extractor — player-config extraction (extractor/youtube.py)

The YouTube extractor is the most elaborate example of the
"player-config" pattern, and the one most relevant to FetchForge's
YouTube handling (currently delegated to yt-dlp, which is the right
call — the detail below explains *why* it is not worth reimplementing):

- **Inline player response**: the watch-page HTML contains
  `var ytInitialPlayerResponse = {...}`; yt-dlp extracts it with
  `_YT_INITIAL_PLAYER_RESPONSE_RE` via `_search_json` in
  `_extract_player_responses`. The response's `streamingData` holds
  `formats` (progressive muxed), `adaptiveFormats` (separate
  video/audio with bitrate, dimensions, init/index ranges),
  `hlsManifestUrl`, and `dashManifestUrl`.
- **InnerTube API**: for each requested client (`web`, `android`,
  `ios`, `mweb`, plus `_music`, `_embedded`, `_agegate`, `_creator`
  variants), `_extract_player_response` POSTs to
  `/youtubei/v1/player` with a `context.client` block. Different
  clients return different format sets; Android/iOS clients return
  plain (non-ciphered) URLs.
- **Signature deciphering**: web-client URLs are ciphered; yt-dlp
  downloads the player JavaScript (`base.js`), extracts the signature
  timestamp (`_extract_signature_timestamp`), and runs the deciphering
  functions. This is the part that breaks constantly and is why the
  YouTube extractor is updated almost daily.
- **Anti-bot layer**: `po_token` / BotGuard proof-of-origin tokens are
  required by some clients; yt-dlp has a PO-token provider mechanism.
  This is anti-bot circumvention tooling, not stream detection — noted
  here for completeness, **not recommended** for FetchForge (YouTube
  stays delegated to yt-dlp, which maintains it).

### 2.3 A small site extractor as a pattern example (extractor/yappy.py)

The Yappy extractor (a short-form video site) shows the minimal
extractor pattern in ~60 lines: download the webpage, search JSON-LD,
search Next.js data, traverse the resulting JSON for the media link,
fall back to a site API call (`/api/video/{id}`), and read title /
thumbnail / description from JSON-LD or OpenGraph meta tags. The
pattern — *webpage JSON first, site API second, meta tags for
metadata* — is the template for any per-site extractor FetchForge
might ever add to its desktop app for a stubborn site.

### 2.4 yt-dlp → FetchForge mapping (summary)

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| Generic page-JSON scan (JSON-LD, og:video, Next.js/Nuxt data, player-config objects) in the content script | **Yes — rank 3** | Streams declared in page JSON; no new permissions |
| Embedded-player signature registry (JW Player, Cloudflare Stream, Vidyard, …) | **Yes — rank 3** (same work) | Third-party-player sites where the URL is in the embed, not the page |
| MIME-type alongside extension in URL classification | **Yes — rank 1** (with cat-catch §1.2) | Extensionless manifests |
| Per-site extractors in the desktop app (paste-link fallback) | **Later — rank 6** | Stubborn sites, one at a time, without touching the extension |
| YouTube player-config / InnerTube / signature deciphering | **No — keep delegating to yt-dlp** | — (already works) |
| PO-token / anti-bot tooling | **No — out of scope** | — |

Sources: repo https://github.com/yt-dlp/yt-dlp; DeepWiki GenericIE page
https://deepwiki.com/yt-dlp/yt-dlp/3.3-social-media-extractors;
licence: PyPI https://pypi.org/project/yt-dlp/ and README licensing
section; YouTube player-response flow:
https://github.com/yt-dlp/yt-dlp/commit/5eedc208ec89d6284777060c94aadd06502338b9
and https://github.com/iarchep/meetube/blob/HEAD/docs/INNERTUBE_API.md.

---

## 3. Video DownloadHelper — negative finding (source not public)

Video DownloadHelper (VDH) is the best-known video-downloader browser
extension, and the task named it explicitly — but **its extension source
code is not public**, so no code-level study is possible. The GitHub
organisation `aclap-dev` hosts `video-downloadhelper`, which contains
no source code (discussion/Q&A board only), and the companion app
`vdhcoapp` was open source but was archived in December 2025 and is no
longer maintained. A Mozilla Discourse thread documents the gap:
https://discourse.mozilla.org/t/video-downloadhelper-claims-to-be-open-source-but-the-source-is-not-public/147576.

What can be said from public discussion threads only (docs/discussions,
not code):

- VDH uses a **companion native app** (CoApp) for advanced processing
  (aggregation, conversion) — the same extension-detects /
  native-app-downloads split FetchForge already has.
- Discussion threads report detection issues on Google Drive since
  2024 — a reminder that even the market leader does not solve every
  site, and that Drive is a genuinely hard target (relevant to
  FetchForge's Google Drive goal).
- No mechanism from VDH can be adopted, because none is verifiable.

**Verdict: no adoptable mechanism; recorded here so the gap is explicit
rather than silently missing.**

---

## 4. streamlink — plugin architecture and declarative extraction

Repo: https://github.com/streamlink/streamlink — **BSD-2-Clause**
(verified in `pyproject.toml`: `license = "BSD-2-Clause"`, and Homebrew /
NetBSD package metadata). Python library + CLI that pipes streams from
services into a video player. Forked from Livestreamer in 2016.

### 4.1 Plugin registry (src/streamlink/plugin/plugin.py)

- `Plugin` base class; `@pluginmatcher` decorator registers a URL regex
  with a priority: HIGH_PRIORITY 30, NORMAL_PRIORITY 20, LOW_PRIORITY 10,
  NO_PRIORITY 0. `@pluginargument` declares per-plugin options.
- Each plugin implements `_get_streams()`, which returns a dict of
  named `Stream` objects (or yields them).
- `session.resolve_url(url)` iterates registered plugins and picks the
  highest-priority match; no match raises `NoPluginError`.
- `stream_weight` ranks streams (bitrate/2.8, resolution base plus FPS
  and bitrate additions) — the same ranking problem FetchForge's popup
  ladder solves, and a useful reference for ordering quality options.

### 4.2 Declarative extraction schemas (streamlink.plugin.api.validate)

The distinctive mechanism: plugins declare **validation schemas** that
extract stream URLs from HTTP responses in one composable pipeline —
`validate.parse_html()` → `validate.xml_xpath_string(...)` →
`validate.parse_json()` → field traversal. Example from the docs: fetch
the page, XPath-select a `data-player` attribute, parse it as JSON, and
pull the stream URL out. A real plugin (`plugins/tv360.py`) does exactly
this in a few lines: XPath
`.//video/source[@src][@type='application/x-mpegURL'][1]/@src`, then
`HLSStream.parse_variant_playlist(self.session, hls_url)`.

Two consequences worth noting:

- The schema layer **validates** as it extracts: a plugin whose expected
  data is missing fails cleanly with a schema error instead of a bare
  exception. (A 2026 PR review on the sportschau plugin explicitly asked
  for the schema to validate, "or else ugly errors will be raised by the
  plugin when there's an issue with the expected data".)
- Plugins commonly fetch a **site API** rather than scraping HTML (the
  Stripchat plugin calls the site's API, validates the JSON with a
  schema, then hands the returned server URL to
  `HLSStream.parse_variant_playlist` with a `Referer` header).

### 4.3 Stream classes (streamlink/stream/hls.py, dash.py, http.py)

- `HLSStream.parse_variant_playlist(session, url, **request_params)`
  fetches a multivariant playlist, parses it, and returns one named
  stream per variant. `DASHStream` does the same for MPD. `HTTPStream`
  covers direct files.
- Streams are opened as file-like objects (`stream.open()`), so the
  player/CLI reads through them; segment fetching, retries and
  (for HLS) playlist reloading live inside the stream class.
- Known limitation, stated in the project's own PR discussion: HLS
  streams with **packed audio** are unsupported (#4721) — a reminder
  that even mature HLS implementations have gaps, and FetchForge's
  engine should be tested against packed-audio playlists.

### 4.4 streamlink → FetchForge mapping (summary)

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| Priority-ordered plugin/extractor registry with a clean no-match error | **Yes — rank 6** (desktop paste-link fallback design) | Per-site extractors that compose instead of conflicting |
| Declarative validate-then-extract schemas (parse HTML → XPath → parse JSON → traverse) | **Yes — rank 6** (same work) | Robust per-site extraction with clean failures |
| `stream_weight`-style quality ranking | **Already have** (popup ladder) | — |
| HLS/DASH stream classes with playlist reload | **Already have** (Rust engine) | — |
| Cookie passing to stream requests (`HLSStream(..., cookies={...})`) | **No — excluded** (credential forwarding) | — |

Sources: repo https://github.com/streamlink/streamlink; docs
https://streamlink.github.io/latest/api_guide/validate.html,
https://streamlink.github.io/api/plugin.html,
https://streamlink.github.io/latest/api/session.html; plugin example
https://github.com/streamlink/streamlink/blob/master/src/streamlink/plugins/tv360.py;
licence: https://github.com/streamlink/streamlink/blob/master/pyproject.toml.

---

## 5. Lux (formerly annie) — Go extractor registry

Repo: https://github.com/iawia002/lux — ~31.5K stars, **MIT** (verified
on the repo page). Go library + CLI, 40+ platforms. Activity has slowed
(last commit ~8 months before this study; latest release v0.24.1, May
2024) but the architecture is the cleanest of the CLI downloaders.

### 5.1 Registry and routing (extractors/extractors.go, app/register.go)

- A thread-safe map (`extractorMap`, guarded by a `sync.RWMutex`) maps
  domain strings to `Extractor` implementations.
- Each extractor self-registers in its package `init()` via
  `Register()`; `app/register.go` blank-imports every extractor package
  so all `init()` functions run.
- The `Extractor` interface is a single method:
  `Extract(url string, option Options) ([]*Data, error)`.
- `Extract()` routes by domain, with special cases (Bilibili short codes
  `av`/`BV`/`ep` expanded to full URLs; specific hosts mapped to custom
  domain keys) and a **universal fallback**: if no domain extractor
  matches, `extractors/universal/universal.go` treats the URL as a
  direct resource.
- The `Data` struct carries site, title, type, and a map of named
  `Stream` objects (quality → URL + size + format) — the same shape as
  FetchForge's popup ladder data.

### 5.2 Per-extractor behaviour

Each site extractor fetches the page and/or a site API and parses the
result (Bilibili: API + JSON; iQIYI: page-data parsing; Rumble: playlist
resolution; Weibo/QQ: metadata fetch). The download phase then selects a
stream quality and downloads parts (single-threaded, multi-threaded, or
via aria2), merging with ffmpeg when needed. Nothing here is a detection
mechanism FetchForge lacks — the value is architectural: the
**one-method interface + domain map + universal fallback** is the
simplest possible extractor system, and it is the right template if
FetchForge ever adds per-site extractors to the desktop app.

### 5.3 Lux → FetchForge mapping (summary)

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| Domain-keyed registry + single-method extractor interface + universal fallback | **Yes — rank 6** (desktop paste-link fallback design) | Clean per-site extractor growth |
| Cookie / custom-Referer CLI options | **No for auto-capture** (user-supplied cookies are a user choice, not an extension behaviour) | — |

Sources: repo https://github.com/iawia002/lux; DeepWiki
https://deepwiki.com/iawia002/lux/6-extractor-system and
https://deepwiki.com/iawia002/lux/3-architecture;
https://gitdiagram.com/iawia002/lux.

---

## 6. cobalt — service handlers behind a JSON API (no in-browser detection)

Repo: https://github.com/imputnet/cobalt — ~45K stars (Oct 2026),
**AGPL-3.0 (API) + CC-BY-NC-SA-4.0 (web frontend)**. Monorepo
(pnpm workspaces): `api` (Express/Node backend), `web` (SvelteKit
frontend), shared packages. Version 11.7 (April 2026); no GitHub
releases/tags.

### 6.1 How it resolves a URL

- The API receives `POST /` with a URL, matches it to a **service
  handler** (platform-specific logic in the API that knows how to turn
  a URL into a media source), and replies in one of three ways:
  `redirect` (the site's own direct file URL), `tunnel` (cobalt
  streams the file through itself, using ffmpeg to stitch separate
  video/audio tracks when needed), or `picker` (the post has several
  items; the user chooses one).
- Philosophy, stated in the README: a "fancy proxy" — it never caches
  content; a tunnel is a pass-through pipe. It only handles free,
  publicly accessible content, and the project states the same content
  can be obtained with a browser's developer tools.
- YouTube is handled via YouTube.js (InnerTube API); the main public
  instance disables YouTube because of YouTube-side restrictions and
  maintenance cost at scale, and points users at self-hosting.
- The frontend does on-device remuxing/encoding with libav.js (ffmpeg
  compiled to WASM) and WebCodecs, so the server only tunnels bytes.

### 6.2 What cobalt does NOT do

cobalt has **no in-browser detection at all**: it never sees a page the
user is watching, never sniffs network traffic, and never runs in the
page. It is a paste-a-link service. Its relevance to FetchForge is
therefore limited to two ideas:

1. **The redirect/tunnel/picker response model** is a clean contract for
   a resolver: either hand back a direct URL, proxy the bytes, or ask
   the user to choose. FetchForge's bridge already does the equivalent
   of "redirect" (URL → desktop engine); a "picker" step is what the
   popup already is.
2. **On-device remux with ffmpeg.wasm / libav.js** is a proven pattern
   for merging separate audio/video tracks without a server — relevant
   only if FetchForge ever wants an in-extension merge path (it does not
   need one; the desktop app has real ffmpeg).

### 6.3 cobalt → FetchForge mapping (summary)

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| Service-handler registry behind one endpoint | **Reference only** (rank 6 design input) | — |
| Redirect/tunnel/picker response model | **Already have** (bridge + popup) | — |
| On-device libav.js remux | **No** (desktop ffmpeg covers it) | — |
| Any cobalt code | **No — AGPL-3.0** (techniques only, and no reuse in a possibly-paid product without review) | — |

Sources: repo https://github.com/imputnet/cobalt; DeepWiki
https://deepwiki.com/imputnet/cobalt;
https://github.com/ZhenningLang/GitHub-Michelin/blob/HEAD/categories/media-download/cobalt.md;
https://www.ithub.directory/media-downloaders/cobalt.

---

## 7. hls-downloader (puemos) — extensionless HLS detection, MIT

Repo: https://github.com/puemos/hls-downloader — **MIT**. Browser
extension (Manifest V2 for Firefox, Manifest V3 for Chromium) that
sniffs HLS playlists, offers a quality/audio-track picker, and merges
locally with ffmpeg.wasm. Star count not verified in this study.

The finding that matters: **v5.5.9 (2026-10-30) explicitly added
detection of "HLS streams served without a `.m3u8` extension, as
`.m3u`, or with valid alternate and generic MIME types"**, plus direct
media-playlist URLs for TS, fMP4 and audio-only streams without
requiring a master playlist. That is independent confirmation, from a
second extension project, that extensionless-playlist + MIME-type
detection is the correct answer to the novahd class of problem — and
that it is a solved problem in the extension world, not a research
question. The same release notes confirm AES-128 caption handling with
key rotation, implicit/explicit IVs and byte ranges.

**Caveat:** the exact detection API hls-downloader uses (webRequest
vs. timing log vs. fetch hook) was NOT read at code level in this
study, so no file/function reference is given for it; only the
released behaviour is claimed. If FetchForge wants the implementation
detail, the repo is MIT-licensed and small enough to read directly.

| Mechanism | Adopt? | Unlocks |
| --- | --- | --- |
| Extensionless + alternate/generic-MIME HLS detection | **Yes — rank 1** (same work as cat-catch §1.2) | novahd-class playlists |
| In-extension ffmpeg.wasm merge | **No** (desktop ffmpeg covers it) | — |

Sources: https://github.com/puemos/hls-downloader;
https://newreleases.io/project/github/puemos/hls-downloader/release/v5.5.9;
https://www.blog.brightcoding.dev/2026/05/06/hls-downloader-the-essential-tool-every-developer-needs.

---

## 8. Other tools noted (brief)

- **N_m3u8DL-RE** (https://github.com/nilaoda/N_m3u8DL-RE, ~8.9K
  stars, **MIT**, C#): a cross-platform DASH/HLS/MSS download engine.
  It takes a manifest URL as input and has **no detection** — it is the
  engine cat-catch hands off to, and the class of tool FetchForge's Rust
  engine already is. Noted for completeness; nothing to adopt except as
  an engine benchmark (it handles live recording, key rotation, byte
  ranges). Its `--cookies` option loads a *user-supplied* cookie file —
  a user choice, not credential capture; FetchForge does not need it.
- **you-get** (https://github.com/soimort/you-get, ~48.8K stars):
  Python site extractors in the yt-dlp mould, but largely unmaintained
  (last release July 2022). Licence not verified in this study. Nothing
  to adopt beyond what yt-dlp already teaches better.
- **gallery-dl** (https://github.com/mikf/gallery-dl): extractor
  registry, image/gallery-focused. Not studied in depth; its
  architecture is the same registry pattern as streamlink/Lux.
- **mediago** (https://github.com/mediago-dev/mediago, ~9K stars):
  Go desktop app advertising "sniff and grab m3u8/HLS streams".
  Not studied in depth; flagged as a candidate for a follow-up read if
  FetchForge wants a second opinion on desktop-side sniffing.
- **XDM, AB Download Manager, JDownloader** (from `downloader-study.md`):
  XDM's MAIN-world `hls-hook.js` playlist-body capture and ABDM's
  per-tab playlist re-fetch are the two detection mechanisms from the
  earlier study most relevant here; both are already reflected in
  FetchForge rounds 6–8. Not repeated.

---

## 9. Mechanisms EXCLUDED from all recommendations

| Mechanism | Found in | Why excluded |
| --- | --- | --- |
| Cookie / Authorization / token capture and forwarding to the downloader | cat-catch (`getRequestHeaders` DIRECT_INCLUDE_HEADERS; `${cookie}` in N_m3u8DL-RE args) | Session-credential forwarding; spec §10 |
| Cookie passing into stream requests | streamlink docs (discussion #5323) | Same; user-supplied cookies in a CLI are a user choice, extension auto-capture is not |
| Iframe `sandbox` attribute stripping (clone iframe without sandbox) | cat-catch catch.js | Mutates the page's DOM and security posture |
| MITM proxy + custom CA | res-downloader (prior study) | Intercepts all traffic; out of scope |
| `chrome.debugger` / CDP attachment | novahd study Tier 2 (rejected) | Permanent banner; can read HttpOnly cookies |
| PO-token / BotGuard anti-bot tooling | yt-dlp YouTube extractor | Anti-bot circumvention, not detection; YouTube stays delegated to yt-dlp |
| Worker-constructor wrapping as a *primary* mechanism | TwitchNoSub technique (novahd study) | Fragile, page-mutating; deep-search opt-in only (§10 rank 5) |

---

## 10. Ranked adoption mapping for FetchForge

Ordered by (value × certainty) / cost. Ranks 1–3 are the answer to the
user's question; ranks 4–6 are the follow-on programme.

### Rank 1 — webRequest observe-only sniffing + response-header classification (cat-catch §1.1–1.2; hls-downloader §7)

**Adopt now — this is round 9 Tier 1, already approved.**
`onSendHeaders` (URL rules + header save) + `onResponseStarted`
(extension/MIME/Content-Disposition/`resourceType == "media"`
classification) on `<all_urls>`, with tabId == -1 resolution via
initiator/referer/scope, per-tab URL dedup with a cap, and the MV3
service-worker keep-alive pattern. Include the Content-Range total-size
read and the Content-Type check alongside the extension check.

- **Unlocks novahd first:** its extensionless `/api/gs-hls` playlist is
  classified by MIME type, and its worker/proxied requests are visible
  to webRequest regardless of frame or timeline.
- **Unlocks generally:** every hidden-stream site whose playlist or
  media request the browser makes but the timing log misses or the
  anchored regex cannot match — workers, service workers, cross-origin
  iframes, `?url=` wrappers, generic-MIME media.
- **Cost:** one permission the user has already approved
  (`webRequest` + optional `<all_urls>`); no page mutation; no new
  attack surface beyond observing request metadata the browser already
  exposes to extensions.
- **Must NOT include:** cookie/auth header capture (§9).

### Rank 1 (same round) — manifest-declared MAIN-world content script (cat-catch §1.3; novahd study Tier 0)

**Adopt now — this is round 9 Tier 0, already approved.** Move the
fetch/XHR hook from an inline `<script>` to a manifest-declared
content script with `"world": "MAIN"`, `document_start`,
`all_frames: true`, `match_about_blank: true`. Manifest-declared
scripts are not subject to the page's CSP, so the hook runs on
strict-CSP sites where the inline version never loads.

- **Unlocks novahd first:** the hook actually runs there.
- **Unlocks generally:** any strict-CSP site; plus cross-origin iframes
  via `all_frames`.
- **Note:** cat-catch injects on demand via `executeScript`; FetchForge
  should prefer the manifest-declared route (always-on, no gesture,
  CSP-proof) and keep on-demand injection as a fallback for sites where
  always-on injection breaks playback.

### Rank 2 — URL-regex transform rules (cat-catch §1.2)

A small rule engine in the background script: ordered rules that
blacklist a requestId or extract/transform a URL (capture groups +
`decodeURIComponent`). Ship with a handful of rules for known wrapper
patterns (`?url=`, tracking-parameter stripping) and let the list grow
from sweep findings.

- **Unlocks:** `?url=`-wrapped playlists and other wrapper URLs that
  webRequest sees but whose outer URL is not itself a manifest.
- **Cost:** small; pure background-script logic, no new permissions.

### Rank 3 — generic page-JSON + embedded-player scan in the content script (yt-dlp §2.1)

Extend the content-script DOM scan to parse inline page data: JSON-LD
(`<script type="application/ld+json">`), OpenGraph `og:video` /
`og:video:url` meta tags, `__NEXT_DATA__` / Nuxt payloads, and
player-config objects (JW Player, Cloudflare Stream, Vidyard,
Brightcove) via a small signature registry. Any manifest or media URL
found is sent to the background for the same classification and popup
flow as a sniffed URL.

- **Unlocks:** streams declared in page JSON that the browser has not
  requested yet (or requests through an opaque player path), and
  third-party-player sites where the URL lives in the embed config.
- **Cost:** no new permissions; content-script string/regex work only.
  This is the cheapest mechanism in the study and the only one that
  finds streams *before* the browser requests them.

### Rank 4 — MediaSource / SourceBuffer byte capture as a "record" mode (cat-catch §1.3)

Proxy `MediaSource.prototype.addSourceBuffer`,
`SourceBuffer.prototype.appendBuffer` (prototype-level, so direct
prototype calls are caught) and `MediaSource.prototype.endOfStream` in
the MAIN-world script, behind an explicit user action ("Record this
video"). Reassemble appended chunks into a file in the extension, then
hand it to the desktop app (or download via `chrome.downloads`).

- **Unlocks:** pure-MSE streams with no retrievable URL at all — the
  class of site where ranks 1–3 all fail because there is no manifest
  URL to find.
- **Cost and caveats:** capture only starts when enabled; the file is
  complete only if the user plays through (cat-catch auto-seeks to the
  start for this reason); memory use must be bounded (spill to disk /
  chunked hand-off, not an unbounded in-memory list); the reassembled
  file may need remuxing. This is a real feature, not a quick fix —
  schedule it after ranks 1–3 prove out on the sweep.

### Rank 5 — deep search as an opt-in diagnostic (cat-catch search.js)

Wrap `JSON.parse`, `fetch`, `XMLHttpRequest.open`,
`TextDecoder.decode`, typed-array `subarray`, `btoa`/`atob` and the
`Worker` constructor in the MAIN-world script, scanning data for media
URLs — enabled only on user request for a specific stubborn site, never
always-on (it touches hot paths and can break pages).

- **Unlocks:** URLs that exist only inside JSON API responses, base64
  blobs, or decoded buffers.
- **Cost:** moderate; keep it a diagnostic tool, not a default.

### Rank 6 — per-site extractor registry in the desktop app (streamlink §4, Lux §5, yt-dlp §2.3)

For the paste-link fallback path, add a small extractor registry to the
desktop app: domain-keyed, single-method interface, universal fallback
(direct-resource handling), priority ordering, and declarative
validate-then-extract steps (fetch page → parse HTML/JSON → traverse).
Add extractors one stubborn site at a time, driven by sweep failures.

- **Unlocks:** sites that defeat all browser-side detection but expose
  their stream to a server-side fetch + parse (the yt-dlp/streamlink
  model), without touching the extension.
- **Cost:** the largest item here; justified only after ranks 1–3 have
  been swept, because each extractor is per-site maintenance.

### What NOT to do (from this study)

- Do not copy cat-catch's cookie/auth capture (§9).
- Do not strip iframe sandboxes (§9).
- Do not reimplement YouTube extraction — yt-dlp owns that problem and
  updates daily (§2.2).
- Do not build a MITM proxy or attach the debugger (§9).
- Do not treat VDH as a source — there is no public code to learn from
  (§3).

---

## 11. What could not be verified

- **Video DownloadHelper internals**: source not public; no mechanism
  verified (§3).
- **hls-downloader's exact detection API**: only its released behaviour
  (extensionless + MIME detection, v5.5.9) is verified; the code path
  was not read (§7).
- **mediago's sniffing mechanism**: not studied in depth (§8).
- **you-get's licence and gallery-dl's licence**: not verified in this
  study; both are recorded as unverified rather than asserted (§8).
- **Star counts**: approximate, as found on 2026-11-23; they change
  daily.
- **cat-catch `js/m3u8.js` internals**: the file was located (2461
  lines) and its role confirmed (in-extension manifest parsing +
  download/hand-off), but its parsing logic was not read line by line;
  nothing in the recommendations depends on it, since FetchForge has
  its own engine.
- **cobalt service-handler internals**: the handler architecture and
  response model are verified from docs/DeepWiki; individual service
  modules (e.g. how the TikTok or Instagram handler extracts its URL)
  were not read. Nothing in the recommendations depends on them.

---

## 12. Source index

- cat-catch — https://github.com/xifangczy/cat-catch (GPL-3.0)
- yt-dlp — https://github.com/yt-dlp/yt-dlp (Unlicense, source);
  GenericIE DeepWiki https://deepwiki.com/yt-dlp/yt-dlp/3.3-social-media-extractors;
  licence https://pypi.org/project/yt-dlp/;
  YouTube player response https://github.com/yt-dlp/yt-dlp/commit/5eedc208ec89d6284777060c94aadd06502338b9;
  InnerTube reference https://github.com/iarchep/meetube/blob/HEAD/docs/INNERTUBE_API.md
- Video DownloadHelper (negative finding) —
  https://discourse.mozilla.org/t/video-downloadhelper-claims-to-be-open-source-but-the-source-is-not-public/147576
- streamlink — https://github.com/streamlink/streamlink (BSD-2-Clause);
  docs https://streamlink.github.io/latest/api_guide/validate.html,
  https://streamlink.github.io/api/plugin.html;
  plugin example https://github.com/streamlink/streamlink/blob/master/src/streamlink/plugins/tv360.py;
  licence https://github.com/streamlink/streamlink/blob/master/pyproject.toml
- Lux — https://github.com/iawia002/lux (MIT);
  https://deepwiki.com/iawia002/lux/6-extractor-system;
  https://deepwiki.com/iawia002/lux/3-architecture
- cobalt — https://github.com/imputnet/cobalt (AGPL-3.0 API +
  CC-BY-NC-SA-4.0 web); https://deepwiki.com/imputnet/cobalt;
  https://www.ithub.directory/media-downloaders/cobalt
- hls-downloader — https://github.com/puemos/hls-downloader (MIT);
  https://newreleases.io/project/github/puemos/hls-downloader/release/v5.5.9
- N_m3u8DL-RE — https://github.com/nilaoda/N_m3u8DL-RE (MIT)
- you-get — https://github.com/soimort/you-get
- gallery-dl — https://github.com/mikf/gallery-dl
- mediago — https://github.com/mediago-dev/mediago
- Prior studies in this repo: `downloader-study.md` (2026-11-22),
  `novahd-stream-research.md` (2026-11-23)
