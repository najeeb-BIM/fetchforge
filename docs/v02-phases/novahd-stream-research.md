# Hidden-stream detection research — novahd as case study #1

Date: 2026-11-23. Research only: no source files were changed for this
document. Scope was widened by the user on the 2026-11-23 voice call from
novahd-only to GENERAL hidden-stream detection, with novahd as the first
proving case.

Boundaries (spec §10, standing): detection only — finding the stream the
browser itself already plays. No DRM bypass, no auth/paywall bypass, no
cookie theft. Nothing in this document proposes any of those.

## TL;DR

Novahd hides its stream with three ordinary mechanisms stacked together,
none of them exotic:

1. **Strict CSP `script-src 'self'`** blocks FetchForge's inline
   MAIN-world fetch/XHR hook — the hook never runs on novahd at all.
   Proven by the site's own code (see §1.2).
2. **Extensionless / proxy-wrapped playlist URLs** (`/api/gs-hls?...`,
   `nova-edge*.workers.dev/...`, `?url=<real-url>` wrappers) are
   invisible to the extension's timing-log scan, whose regex is still
   anchored on `.m3u8`/`.mpd` (content.js:420). Round 7 widened only
   the hook's URL test, not the timing scan's.
3. **The video element's `currentSrc` is a `blob:` URL** (hls.js +
   MSE), so DOM scanning finds no usable source, and the only direct
   file the timing log offers is the pre-roll ad clip
   (`/assets/p/*.mp4`), which the round-7 clip guard correctly refuses.

The stream itself is NOT hidden behind DRM, authentication, a worker, or
an iframe. hls.js fetches the manifest on the main thread via fetch/XHR
in the top document, so the playlist URL **is** in the document's
resource-timing log — just unrecognized — and **would** be caught by the
MAIN-world hook if the hook ran. The hook's logic is already correct for
novahd (it accepts playlist content-types and body shapes, not just URL
extensions); only its delivery mechanism (inline `<script>`) fails.

**Recommendation (layered, general):**

- **Tier 0 — no new permissions (do first):** move the fetch/XHR hook
  into a manifest-declared MAIN-world content script (`"world":
  "MAIN"`, Chrome 111+, Firefox 128+) — this bypasses page CSP with
  no permission expansion; decouple the timing-log scan from `<video>`
  elements; widen the timing regex and add content-type identification
  via `PerformanceResourceTiming.contentType` (Chrome 148+,
  Firefox 129+). This tier alone should fix novahd.
- **Tier 1 — the general solution:** observe-only `chrome.webRequest`
  (`webRequest` permission + `<all_urls>` host permission) in the
  background service worker, classifying by URL + response
  Content-Type. This sees everything Tier 0 misses: worker fetches,
  service-worker fetches, cross-origin iframes, and any extensionless
  URL. It is what the field-standard open-source sniffer does
  (cat-catch, 23K stars). The `<all_urls>` cost can be softened with
  an optional host permission requested at runtime.
- **Tier 2 — niche fallbacks, not recommended as primary:**
  Worker-constructor wrapping (covers dedicated workers only, fragile);
  `chrome.debugger`/CDP (sees everything but shows a permanent
  "debugging this browser" banner and can read HttpOnly cookies —
  wrong for a privacy-positioned product); DNR `getMatchedRules`
  (not a detection path — returns rule IDs only, debug-only).

## 0. What the extension already does (grounding, v0.2.6)

Read in full: `extensions/chromium/content.js`,
`extensions/chromium/background.js`, `extensions/chromium/manifest.json`,
`docs/v02-phases/round7-notes.md` (hook capture section).

- **DOM scanning:** video/audio/img/model/document detectors;
  per-document resource-timing log (buffer grown to 5000 at
  `document_start`, content.js:60).
- **MAIN-world fetch/XHR hook** (round 6, content.js:62-~250):
  stringified and injected as an **inline `<script>`** into the page's
  MAIN world, where it wraps `fetch`/`XMLHttpRequest` and clones
  playlist responses (≤256 KB), forwarding the TEXT via `postMessage`
  with a per-page-load anti-spoof token. Round 7 widened the URL
  pre-filter to match `.m3u8`/`.mpd` ANYWHERE in the URL, and added a
  content-type gate (`mpegurl`/`dash+xml`) plus a body-shape gate
  (`#EXTM3U`/`<MPD`). The code's own comment (content.js:84-91)
  acknowledges the CSP caveat: a strict page CSP blocks the injected
  inline script.
- **Timing-log scan:** `findPlaylistFromResources()` (content.js:533)
  scans `performance.getEntriesByType("resource")` with
  `STREAM_EXT_RE = /\.(m3u8|mpd)([?#]|$)/i` (content.js:420) —
  **still anchored**. It is only called from `mediaSource(el)`
  (content.js:368, 380), i.e. **only when a `<video>` element is
  found in the frame**.
- **Cross-frame election** in background.js: one button per tab,
  playlist anywhere beats direct file anywhere; per-frame playlist
  suppression of direct files; blob: sources → viaPage (page URL sent,
  app extracts via yt-dlp).
- **Clip guard** (round 7): on a long-form page (>600s) a direct
  `.mp4` is Range-probed (first 64 KB, moov/mvhd) before sending; a
  known <60s clip is refused on the button.
- **Manifest (0.2.6):** permissions `activeTab`, `contextMenus`,
  `declarativeNetRequest`, `declarativeNetRequestFeedback`,
  `downloads`, `nativeMessaging`, `storage`; host_permissions ONLY
  `http://127.0.0.1:18443/*` (the bridge). Deliberately NO `<all_urls>`,
  NO `webRequest`. Content script: `all_frames`, `document_start`,
  matches `http://*/*`, `https://*/*`. No `web_accessible_resources`
  (so the hook can only be injected inline).

Two defects found in the current code while researching:

1. **The comment at content.js:80-82 is wrong:** "A worker-loaded
   playlist is NOT seen by this hook (workers have their own fetch);
   the timing-log fallback below still covers those." Per the W3C
   Resource Timing spec, "Resource Requests fetched by a non-null
   client are included as PerformanceResourceTiming objects in the
   client's global object's Performance Timeline" — a fetch by a
   dedicated worker goes into the WORKER's timeline, NOT the
   document's. The timing log does NOT cover worker-loaded playlists.
   (Source: W3C Resource Timing Level 2/3 spec, "resources included
   in the PerformanceResourceTiming interface".)
2. **Playlist detection is gated on finding a `<video>` element**
   (`findPlaylistFromResources` is only called from `mediaSource`).
   A frame whose video sits in a shadow DOM, or a blob:-sourced
   iframe with no content script, never gets its timing log scanned.

## 1. How novahd hides its stream (case study #1)

### 1.1 What the site is

Novahd (`novahd.cc`) is a Vite/React single-page app. Evidence:
Wayback Machine snapshot 2026-09-19 of
`https://novahd.cc/watch/m/1101383` (8,422 B HTML): `<div id="root">`,
entry `/assets/index-CIazh_mN.js`, modulepreload chunks including
`hls-BPFGIxGZ.js` (= hls.js), `lucide`, `vendor`. **No iframe** in
the static HTML; the player is hls.js in the TOP document.

Caveat: the code analysed is the **2026-09-19 archived snapshot**;
the live site (Nov 2026) may differ in detail, though the observed
live behaviour matches it exactly (see §1.5).

### 1.2 Strict CSP blocks the MAIN-world hook — the main mechanism

The site's own `prefetch.js` carries a comment stating it is "Kept as
an external same-origin file (not inline) so it satisfies the strict
CSP (script-src 'self')". **This blocks FetchForge's inline
MAIN-world hook on novahd entirely** — the hook script element is
refused by the page's CSP before it ever runs. This was round-7
notes finding #2 (listed as a possible cause); it is now proven as
the actual mechanism on this site.

Consequence: the hook's round-7 widening (match `.m3u8`/`.mpd`
anywhere + content-type gate) is irrelevant on novahd, because the
hook code never executes. The hook's LOGIC would work here (see
§1.4); only its DELIVERY fails.

### 1.3 How the player gets its stream

- Sources come from same-origin **`/api/sources?type=<movie|show>&tmdbId=<id>`**
  (NDJSON stream, `x-nova-visitor` header; Cloudflare Turnstile
  attestation via `/api/attest`, invisible `interaction-only`
  widget; 429 → timeout/streamCap). Each source has `type` (`hls`),
  `url`, `provider`.
- Bundle filters: `provider === 'Torrent'` excluded;
  `nova-edge[a-z0-9-]*.[a-z0-9-]+\.workers\.dev` URLs (the site's own
  Cloudflare Workers edge proxies) ranked highest; `[?&]url=`
  proxy-wrapped URLs ranked lower.
- **Playlist URLs are extensionless or proxy-wrapped:**
  `/api/gs-hls?...` (same-origin API endpoint serving the playlist)
  or `nova-edge*.workers.dev/...` or `?url=<real-url>` wrappers.
  The extension's timing-log regex `STREAM_EXT_RE` is still
  ANCHORED, so these URLs sit in the timing log unrecognized. The
  site's own app probes playlists by fetching the URL and checking
  for `#EXTM3U` (up to 3 candidates) — i.e. the site itself
  identifies playlists by BODY SHAPE, not by URL extension.
- **No iframe, no worker, no service worker** in the app bundle
  (0 matches each). hls.js loads the manifest via fetch/XHR on the
  main thread (`a.loadSource(n.url), a.attachMedia(e)`); direct
  files via `e.src = n.url`. So the worker hypothesis is WEAK for
  novahd specifically.
- **"Decrypting streams" is UI copy only** ("Resolving sources
  across multiple indexers…", "Your decrypted stream aggregator") —
  no client-side decryption of the stream in the bundle (no
  atob/CryptoJS on the stream path), no DRM found. Sources resolve
  server-side via `/api/sources`. There is no DRM or real
  authentication on the stream itself; the Turnstile attestation
  gates the site's `/api/sources` endpoint, not the media, and
  FetchForge does not need that endpoint — it needs the playlist URL
  the player already fetched.
- Ad clips `/assets/p/*.mp4` are a first-class player feature:
  config (`/api/config`) carries `player.clipLeadSources` (pre-roll
  "clip lead" before the film) and `ads` settings.

### 1.4 Why the hook's logic would work if it ran

The round-7 hook forwards a response when the URL contains
`.m3u8`/`.mpd` ANYWHERE **or** the response Content-Type contains
`mpegurl`/`dash+xml`, and the body looks like a playlist
(`#EXTM3U`/`<MPD`). A `/api/gs-hls` response is a playlist served
with (presumably) an HLS content-type, so the content-type gate
would catch it even though the URL has no extension. The only thing
standing between the hook and novahd is CSP blocking the inline
injection. (Whether `/api/gs-hls` actually serves a playlist
Content-Type is UNVERIFIED — see §6.)

### 1.5 The observed behaviour, explained

During the film the video's `currentSrc` is a `blob:` (MSE), so the
extension's DOM scan finds no usable source and falls back to the
timing log; the timing log contains the playlist URL but the
anchored regex doesn't recognise it; the only direct file it offers
is the ad clip, which the round-7 clip guard correctly refuses.
Result: button refuses, nothing downloads. This matches every live
observation on novahd (2026-11-22 and 2026-11-23).

### 1.6 Sources that did NOT pan out

- Direct curl to novahd.cc: blocked 403 by Cloudflare.
- urlscan.io search API works (7 scans of novahd.cc, most recent
  2026-11-18) but the result API returns 403 "You're not logged
  in!" and the web UI returned 429 (do not retry the urlscan result
  page).
- GitHub search for "novahd": no extractor/downloader found (only a
  list entry in ameyajnarvekar/free-streaming-resources: "NOVA |
  novahd.cc | Movies, TV, Anime aggregator").
- "nova-video-player" DeepWiki results are an unrelated Android app.
- Wayback Machine CDX API works and was the productive source
  (watch-page snapshots + JS bundles).

## 2. General hidden-stream techniques (the taxonomy)

Sites hide streams in a small number of ways. Each row names the
mechanism, which FetchForge layer it defeats, and what sees through
it.

| # | Hiding mechanism | Defeats today | Sees through it |
|---|---|---|---|
| H1 | Strict CSP (`script-src 'self'`, no `unsafe-inline`) blocks inline hook injection | MAIN-world hook (never runs) | Manifest MAIN-world content script (§3.1); webRequest (§3.4) |
| H2 | Extensionless / proxy-wrapped playlist URLs (`/api/gs-hls`, `*.workers.dev/...`, `?url=...`) | Anchored `STREAM_EXT_RE` in timing scan | Content-type identification (§3.3); hook content-type gate (already exists, needs H1 fixed); webRequest response headers (§3.4) |
| H3 | Blob:/MSE playback — `video.currentSrc` is `blob:` | DOM source scanning | Timing log / hook / webRequest (the manifest fetch is still network-visible) |
| H4 | Player in a cross-origin iframe | DOM scanning of the iframe's document (content script runs `all_frames`, but a blob:-sourced or `sandbox`ed iframe may have no content script; the iframe's own timing log is separate) | webRequest (sees all tab requests regardless of frame); per-frame content script where it runs |
| H5 | Player fetches in a dedicated worker | MAIN-world hook (workers have their own `fetch`); timing log (worker's fetches go to the worker's timeline — §0 defect 1) | webRequest (sees worker requests; `tabId` may be the tab's); Worker-constructor wrapping (§3.5) |
| H6 | Service worker serves/caches the stream | Hook + timing log (SW fetches are not in the document timeline; a cache-served response may make no network request at all) | webRequest (sees SW network requests; `tabId == -1`, resolvable via initiator — cat-catch does this); residual gap if served purely from cache with no network fetch |
| H7 | Video inside shadow DOM (open or closed) | DOM `querySelectorAll("video")` (closed roots are unreachable; open roots need explicit traversal) | Recursive shadow-root traversal for open roots (§3.2); webRequest/timing log regardless of DOM |
| H8 | Obfuscated / encrypted URLs in page scripts (URL built at runtime, token in JS) | Static analysis of page source | Runtime observation (hook / webRequest see the final URL after the page builds it); no static analysis needed |
| H9 | Playlist fetched before the content script runs (race) | Timing log (buffer eviction — mitigated by the 5000-entry buffer) and the hook (installed at `document_start`, before page scripts — mostly safe) | webRequest (service worker listener is always on, no race); larger timing buffer (already done) |

Notes on specific mechanisms:

- **hls.js workers do NOT fetch.** hls.js's worker is a TRANSMUXER
  worker (demux/remux TS → fMP4); network loading (manifest +
  segments) happens on the main thread via the fetch/XHR loader.
  With the ESM build (which novahd uses — Vite + modulepreload),
  the worker isn't even bundled unless `workerPath` is set.
  Sources: liveapi.com hls.js guide ("Transmuxing runs in a Web
  Worker when available"); dev.to "Check whether hls.js is actually
  using a worker (most ESM setups aren't)". So H5 is rare for
  hls.js/dash.js players — but real for players that fetch in
  workers (and Chrome 108+ supports MediaSource inside dedicated
  workers, so worker-based playback will grow).
- **Service workers** cannot be injected into: a page's SW script
  is fetched by the browser, not constructed in-page, so there is
  no constructor to wrap. webRequest is the only extension-side
  window into SW network activity.
- **Blob URLs** themselves are not the problem — the blob is just
  the MSE handle. The manifest fetch that feeds it is always a
  normal network request, visible to the hook, the timing log, or
  webRequest. The extension's current viaPage fallback (send the
  page URL, let yt-dlp extract) is a reasonable last resort for
  blob:-only pages, but it cannot work on sites yt-dlp has no
  extractor for (novahd included).

## 3. Detection approaches, ranked

### 3.1 Manifest-declared MAIN-world content script — RANK 1 (no new permissions)

**What:** add a second content script entry with `"world": "MAIN"`
in the manifest, containing the fetch/XHR hook. The MAIN-world
script cannot use `chrome.*` APIs; it `postMessage`s captured
playlists to the isolated content script — exactly the current hook
architecture, minus the inline injection.

**Why it works:** manifest-declared content scripts are NOT subject
to the page's CSP (CSP governs page resources, not extension
injection). This removes the novahd CSP block with **no permission
expansion** — it needs no extra permission beyond the manifest
`matches` the extension already has.

**Verified:** `"world": "MAIN"` in `content_scripts` needs no extra
permission beyond `matches`; supported Chrome 111+, Firefox 128+,
Safari 18. Source: extension.js.org (MAIN world content scripts).
The alternative, `chrome.scripting.executeScript({ world: "MAIN" })`,
needs the `scripting` permission + host permissions — the option
round-7 notes considered and rejected; the manifest route is
strictly better.

**Covers:** H1, H2 (via the hook's existing content-type + body-shape
gates), H3, H8, H9 (runs at `document_start`, before page scripts).
Does NOT cover H5 (worker fetches), H6 (service workers), H4
(partially — runs in all frames where the manifest allows, same as
today).

**Cost:** no new permissions, no new warning. Moderate code change:
the hook moves from a stringified function to a real file; the
isolated script keeps the `postMessage` listener and the
anti-spoof token (the token can be passed via a DOM attribute or a
shared `chrome.storage.session` value — or dropped, since a
manifest MAIN-world script's messages can be distinguished by other
means; keep the token, it costs nothing).

### 3.2 Timing-log hardening — RANK 2 (no new permissions)

Three small fixes, all in content.js, no manifest change:

1. **Decouple the scan from `<video>` elements.** Call
   `findPlaylistFromResources()` on the scan schedule and on
   report, not only from `mediaSource(el)`. Fixes the §0 defect 2
   (shadow-DOM video, blob:-iframe frames never scanned).
2. **Widen `STREAM_EXT_RE`** to match `.m3u8`/`.mpd` ANYWHERE in
   the URL, as the round-7 hook already does (proxy-wrapped
   playlists carry the extension mid-URL, followed by `&`).
3. **Recursive shadow-DOM traversal** for video elements: walk
   `el.shadowRoot` (open roots) when scanning for `<video>`/`<audio>`.
   Closed shadow roots remain unreachable by design — no fix
   exists from a content script; webRequest (§3.4) covers those
   cases at the network layer.

**Covers:** H2 (partially — extension-bearing wrapped URLs), H7
(open roots), H3 (timing log already covers blob: playback when
the scan runs). Does NOT cover H1 (irrelevant — the timing log is
CSP-proof), H5, H6, or extensionless URLs (needs §3.3).

### 3.3 Content-type identification in the timing log — RANK 3 (no new permissions)

**What:** read `entry.contentType` on timing entries and treat
`application/vnd.apple.mpegurl`, `application/x-mpegurl`,
`application/dash+xml`, `video/mp2t`, `video/mp4` (+ the
`+xml`/`+json` minimised forms) as playlist/direct-file signals,
even when the URL has no extension. This identifies novahd's
`/api/gs-hls` (same-origin, so `contentType` is visible
regardless of CORS) and any extensionless playlist a player
fetches with CORS (players MUST fetch playlists with CORS to read
the body, so the CORS check passes in practice).

**Availability — verify live:** `PerformanceResourceTiming.contentType`
shipped in **Chrome 148+** (Chrome Platform Showcase, multiple
demo pages; Chrome 160 release notes reference the property's
behaviour) and **Firefox 129+**. MDN's browser-compat data still
lists it as unsupported in Chrome — that data appears stale, but
treat availability as version-dependent and verify on the user's
Chrome with a one-line console check before relying on it.
Cross-origin `contentType` is empty unless the request was
CORS-mode AND passed the CORS check (MDN: "The `contentType`,
`encodedBodySize`, `decodedBodySize`, and `responseStatus`
properties return 0 or an empty string for cross-origin resources
unless the resource was requested in `cors` mode and passed the
CORS check"); same-origin entries are always visible.

**Covers:** H2 (extensionless URLs) for same-origin and CORS
playlists. Fallback when `contentType` is unavailable: the hook
(§3.1) and webRequest (§3.4) both see the real Content-Type header.

### 3.4 Observe-only `chrome.webRequest` — RANK 4 (new permissions, the general solution)

**What:** in background.js, register `chrome.webRequest.onSendHeaders`
(with `requestHeaders`) and `chrome.webRequest.onResponseStarted`
(with `responseHeaders`) for `<all_urls>`, plus `onErrorOccurred`
for cleanup. Classify each request by URL (playlist extensions
anywhere, media extensions) AND by response Content-Type
(`application/vnd.apple.mpegurl`, `application/dash+xml`,
`video/*`, `audio/*`, `application/octet-stream` with a media-ish
URL). Attribute requests to tabs via `details.tabId`; for
`tabId == -1` (service-worker / extension-initiated requests),
resolve the tab from `details.initiator`'s origin. Feed matches
into the existing per-tab election as a new source, ranked
alongside the hook and timing-log findings.

**Why it is the general solution:** webRequest sees EVERY request
in the tab at the browser-process layer — main-thread fetch/XHR,
dedicated-worker fetches, service-worker fetches, cross-origin
iframe subresources — regardless of CSP, document_start races,
shadow DOM, or blob: playback. It is the only approach that
covers H1–H9 except the pure-cache service-worker residual (H6).

**MV3 status — verified:** observe-only webRequest still works in
MV3. Only *blocking* webRequest was removed for standard
extensions (Chrome honours `webRequestBlocking` only for
force-installed enterprise-policy extensions). Sources: Chrome
`chrome.webRequest` docs; zproxy `docs/CAPABILITIES.md` (MV3
capability matrix, Chrome 124+): "Observe requests (method, URL,
headers, timing, type) — `chrome.webRequest` (non-blocking) —
Yes, but only for hosts with a granted host permission…
`onBeforeRequest`, `onSendHeaders`, `onHeadersReceived`,
`onCompleted`, `onErrorOccurred`." Note the Chrome docs detail:
"To intercept a sub-resource request, the extension must have
access to both the requested URL and its initiator" — so host
permissions must cover the CDN origins, not just the page origin.

**Field precedent — verified:** **cat-catch** (`xifangczy/cat-catch`,
23K stars, GPL-3.0 for v2.0 / MIT for 1.0) is an MV3 extension
that sniffs page resources and downloads m3u8 streams. Its
manifest declares `webRequest` + host_permissions `<all_urls>` +
`*://*/*`; its background service worker uses
`webRequest.onSendHeaders` (requestHeaders + EXTRA_HEADERS),
`onResponseStarted` (responseHeaders), `onCompleted`,
`onErrorOccurred`, classifies by response headers (Content-Type /
Content-Length / Content-Disposition) + URL, and resolves
`tabId == -1` requests via the initiator origin. (Techniques only
— GPL-3.0, never copy its code; same rule as XDM/JDownloader.)

**Permission cost:** `webRequest` + `<all_urls>` host permission.
Chrome's install warning for `<all_urls>` is "Read and change all
your data on all websites" — the scariest warning in the list, and
it does not distinguish observe-only from modify. Mitigations:

- **Optional host permission:** declare `<all_urls>` under
  `optional_host_permissions` and request it at runtime via
  `chrome.permissions.request()` (e.g. on first button click, or
  from the popup with an explanation). Install-time warning stays
  minimal; if the user declines, detection falls back to Tier 0.
- **Narrower host permissions don't work here:** the point is
  ANY site, so a fixed host list defeats the purpose.
- **`activeTab` alone is insufficient:** it grants access to the
  active tab's origin, not to the third-party CDN origins the
  playlist actually comes from (and webRequest needs access to
  both the requested URL and its initiator).
- **Privacy framing (true and important):** observation is local
  only — the extension sees request URLs and response headers on
  the user's own machine and sends nothing anywhere except the
  chosen playlist URL to the local FetchForge app over loopback.
  No browsing data leaves the machine. This is consistent with
  "Download anything. Share nothing." — but the permission itself
  still LOOKS broad, so the optional-permission route is the
  honest way to carry it.

**Firefox:** the same observe-only API works in Firefox MV3
(Firefox additionally kept blocking webRequest, which FetchForge
does not need). The chromium/firefox copies can stay identical
for this code.

**Covers:** H1–H9 except the pure-cache SW residual. The only
approach that covers H5 and H6.

### 3.5 Worker-constructor wrapping — RANK 5 (niche fallback)

**What:** in a MAIN-world script (manifest-declared, §3.1), override
`window.Worker`: when the page constructs a worker, synchronously
fetch the original worker script, prepend an `importScripts(patch)`
that wraps `self.fetch`/`self.XMLHttpRequest`, build a new blob
URL, and construct the native worker from it.

**Precedent — verified:** TwitchNoSub (`besuper/TwitchNoSub`)
does exactly this to intercept Twitch's worker fetches (DeepWiki
"Worker Interception System": override `window.Worker`, sync-XHR
the blob-URL script, prepend `importScripts()`, new Blob, native
constructor).

**Limitations (why it ranks low):** synchronous XHR blocks the
main thread (deprecated API); module workers (`type: "module"`)
cannot use `importScripts`, so the prepend breaks; it only covers
workers created AFTER the override is installed; it does NOT cover
service workers (no in-page constructor to wrap — SW scripts are
fetched by the browser); and the players that matter (hls.js,
dash.js) do not fetch in workers anyway (§2). Only worth building
if a specific site is proven to fetch its stream in a dedicated
worker AND webRequest (§3.4) is unavailable for some reason.

### 3.6 `chrome.debugger` / CDP Network domain — RANK 6 (last resort)

**What:** `chrome.debugger.attach({tabId})` + CDP `Network.enable`
(+ `Fetch.enable` if bodies are needed). Sees everything,
including response bodies, worker and service-worker traffic,
with no host-permission gaps.

**Why last:** the `debugger` permission is the most powerful an
extension can hold — it can read ANY cookie including HttpOnly
session tokens, synthesise input, and run arbitrary JS in the
page's main world (blog.extenshi.io, "debugger permission: what
extensions can really do"). Chrome shows a **persistent yellow
infobar** — `"FetchForge" started debugging this browser` — for
as long as any tab is attached, and it cannot be hidden; attach
fails if the user has DevTools open on that tab ("Another debugger
is already attached"); it cannot attach to `chrome://` pages or
the Web Store. Needs `debugger` + `<all_urls>`. For a product
whose tagline is "Download anything. Share nothing.", holding a
permission that CAN read session cookies — even if never used
that way — is the wrong trust trade, and the infobar is a worse
user experience than the optional-permission prompt for
webRequest. Keep as a documented last resort only if webRequest
observation is ever removed from MV3.

Sources: blog.extenshi.io debugger-permission explainer;
medium.com/@dzianisv CDP-from-extensions; zproxy CAPABILITIES.md
(CDP `Fetch` domain is the only MV3 path for true request
pausing — which FetchForge does not need).

### 3.7 DNR `getMatchedRules` — NOT A DETECTION PATH

`chrome.declarativeNetRequest.getMatchedRules()` returns
`MatchedRuleInfo` = `{ rule: {ruleId, rulesetId}, tabId,
timeStamp }` — **no request URL** (Chrome DNR reference docs,
`MatchedRuleInfo` type). The `request` field exists only on
`MatchedRuleInfoDebug`, delivered by the `onRuleMatchedDebug`
event, which is a **debugging feature for unpacked extensions**
only (requires `declarativeNetRequestFeedback`; Chrome DNR docs:
"Enables debugging features for unpacked extensions, specifically
`getMatchedRules()` and `onRuleMatchedDebug`"). It is
quota-limited, returns only the last 5 minutes of matches, only
tells you about requests matching YOUR rules (and DNR has no
"observe" action — you'd abuse an `allow` rule matching all URLs),
and is Chrome-only. Useful as a diagnostic while developing, not
as a detection mechanism. (One third-party reference shows
`match.request.url` on `getMatchedRules` results — that is wrong;
the official type has no `request` field.)

### 3.8 What was considered and rejected

- **`chrome.scripting.executeScript({ world: "MAIN" })`:** needs
  `scripting` permission + host permissions; the manifest-declared
  MAIN-world content script (§3.1) does the same with no new
  permission. (Round-7 notes reached the same conclusion.)
- **Re-fetching extensionless URLs from the content script to
  identify them:** double-fetch cost, breaks single-use signed
  URLs, and CORS may block the read. The hook (§3.1) and
  webRequest (§3.4) see the response the player already fetched —
  no re-fetch needed. (The engine-side probe the clip guard
  already does for mp4s is the right place for any probing, and
  only for candidates the button is about to send.)
- **Site-specific API parsing (e.g. reading novahd's
  `/api/sources` responses):** brittle, per-site work, and
  unnecessary — the general mechanisms catch the playlist the
  player fetches. Do not build per-site extractors; the general
  layers are the product.
- **Reading a page's service-worker cache:** no extension API
  exposes another origin's SW cache. Not possible; webRequest is
  the window into SW network activity instead.

## 4. How the recommended approach applies to novahd

Tier 0 alone should fix novahd, in this order:

1. **Manifest MAIN-world hook (§3.1):** the hook runs despite the
   strict CSP; hls.js fetches `/api/gs-hls` on the main thread;
   the hook's content-type gate (or the body-shape gate) forwards
   the playlist text; the picker gets a real playlist instead of
   the ad clip. Expected outcome: the button offers the film (with
   the quality popup, per the standing popup goal) instead of
   refusing.
2. **Timing-log hardening (§3.2 + §3.3):** even if the hook misses
   a playlist, the decoupled scan + widened regex + `contentType`
   identification find `/api/gs-hls` in the timing log
   (same-origin, so `contentType` is visible).
3. **Tier 1 webRequest (§3.4)** is the insurance layer: it catches
   the playlist even if both Tier-0 layers fail, and it is what
   generalises the fix to the next site that hides its stream
   differently (worker, iframe, service worker).

What does NOT need to change for novahd: no per-site code, no
`/api/sources` parsing, no Turnstile interaction, no cookie or
header handling. The playlist URL the player fetches is a normal
same-origin fetch; FetchForge just needs to see it.

## 5. What would change in the extension

### 5.1 Tier 0 (no manifest permission change)

- `extensions/chromium/manifest.json` (+ firefox copy):
  add a second `content_scripts` entry:
  `{ "matches": ["http://*/*", "https://*/*"], "js": ["hook-main.js"],
  "run_at": "document_start", "all_frames": true, "world": "MAIN" }`.
  No new permission. (Firefox 128+ supports `"world": "MAIN"`;
  keep the inline-injection path as a fallback for older Firefox
  if the firefox copy must support it — or accept Firefox 128+ as
  the floor.)
- New file `extensions/chromium/hook-main.js` (+ firefox copy,
  byte-identical): the current `manifestHookMain` function, as a
  real file instead of a stringified function. Keep the 256 KB cap,
  the URL/content-type/body-shape gates, and the anti-spoof token
  (pass the token via a DOM attribute the isolated script sets, or
  keep the existing `postMessage` token scheme).
- `content.js`: delete the inline-injection code; keep the
  `message` listener and `capturedManifests`; call
  `findPlaylistFromResources()` from the scan schedule, not only
  from `mediaSource(el)`; widen `STREAM_EXT_RE` to match
  `.m3u8`/`.mpd` anywhere; add `contentType`-based identification
  in `scanResourceStreams` (guard with a feature check —
  `entry.contentType` may be `undefined` on older Chrome);
  recursive shadow-root traversal in the video scan; fix the wrong
  worker comment (§0 defect 1).
- Extension harnesses (`extensions/harness/`): add cases for the
  widened regex, `contentType` identification, and the decoupled
  scan (playlist found with no `<video>` element present).

### 5.2 Tier 1 (manifest permission change — needs the user's call)

- `manifest.json` (+ firefox copy): add `"webRequest"` to
  `permissions`; add `<all_urls>` under
  `optional_host_permissions` (preferred) or `host_permissions`.
  If optional: request it at runtime from the popup or on first
  button click, with a one-line explanation ("FetchForge watches
  network requests on pages you visit to find the video stream.
  Nothing leaves your computer except the download you choose.").
- `background.js`: add the webRequest listeners (§3.4), a
  per-tab map of observed media URLs (URL, content-type, size if
  known, time), `tabId == -1` resolution via initiator, and feed
  into the existing election as a new source. Keep the observation
  strictly local: no request data is sent anywhere except the
  playlist URL the user chooses to download, to the local app.
- No engine/bridge/app changes needed: the webRequest findings
  arrive through the existing `reportMedia` / election path.

### 5.3 What does NOT change

- No new permissions for Tier 0. No `debugger`, no `cookies`, no
  `scripting`, no `webRequestBlocking`.
- No per-site code. No yt-dlp changes. No bridge/app changes for
  Tier 0 or Tier 1 (the detection output feeds the existing
  election and download paths).
- The chromium/firefox copies stay byte-identical for
  content.js, background.js, popup.js, rules/ads.json — and for
  the new hook-main.js.

## 6. What could NOT be verified

Live page behaviour needs the user's browser (this research used
archived code + documentation only):

1. **Whether `/api/gs-hls` serves a playlist Content-Type**
   (`application/vnd.apple.mpegurl` etc.). The hook's
   content-type gate and §3.3 depend on it; if it serves
   `text/plain` or `application/octet-stream`, only the
   body-shape gate (hook) or a probe would identify it. Check in
   the user's Chrome: DevTools → Network → the `/api/gs-hls`
   request → Response Headers.
2. **Whether the playlist URL is in the document's timing log on
   the live site** (it should be — hls.js fetches on the main
   thread — but the live player may differ from the archived
   bundle). Check: `performance.getEntriesByType("resource").map(e
   => e.name)` in the console on the watch page while the film
   plays.
3. **Whether `PerformanceResourceTiming.contentType` is populated
   in the user's Chrome version** (Chrome 148+ per the Chrome
   Platform Showcase; MDN/BCD still lists it as unsupported —
   verify live, one line in the console).
4. **Whether a manifest-declared MAIN-world content script runs on
   novahd** (it should — CSP does not apply to extension
   injection — but this is untested on this site).
5. **The live novahd bundle** (Nov 2026) vs the 2026-09-19
   archived snapshot: the player architecture may have changed in
   detail (e.g. a different playlist endpoint, a worker, an
   iframe). The observed live behaviour (ad clip only, clip-guard
   refusal, blob: currentSrc) matches the archived code, so the
   mechanism is very likely unchanged — but only a live check
   confirms it.
6. **The user's Chrome version** (needed for §3.1: Chrome 111+ for
   MAIN-world manifest scripts — near-certain; and §3.3:
   Chrome 148+ for `contentType`).
7. **Turnstile's effect on re-fetching:** if the engine ever
   re-fetches a novahd playlist URL directly (rather than using
   captured text), the request may lack the site's attestation
   headers — but the playlist endpoint is not the attested
   `/api/sources` endpoint, so this is unlikely to matter. The
   captured-text path (hook) avoids the question entirely.
   Unverified either way; the hook path is the primary one.

## 7. Sources

- Novahd watch page + JS bundles: Wayback Machine snapshot
  2026-09-19, `https://novahd.cc/watch/m/1101383` (CDX API).
  The site's own `prefetch.js` CSP comment; `/api/sources`,
  `/api/gs-hls`, `/api/config` (`player.clipLeadSources`),
  `nova-edge*.workers.dev` ranking, `?url=` proxy wrapping,
  Turnstile `/api/attest` — all from the archived bundle.
- W3C Resource Timing (Level 2/3): worker fetches go to the
  worker's own timeline, not the document's.
- MDN `PerformanceResourceTiming` (incl. `contentType` CORS
  gating) and `declarativeNetRequest.getMatchedRules`.
- Chrome Platform Showcase (paulkinlan): `contentType` in
  Resource Timing, Chrome 148+; Chrome 160 release notes
  (chromestatus.com) referencing `PerformanceResourceTiming.contentType`.
- extension.js.org: manifest MAIN-world content scripts
  (`"world": "MAIN"`), Chrome 111+, Firefox 128+, Safari 18, no
  extra permission beyond `matches`.
- Chrome `chrome.webRequest` reference docs (observe-only in MV3;
  `webRequestBlocking` policy-installed-only; sub-resource
  requests need access to both URL and initiator).
- Chrome `chrome.declarativeNetRequest` reference docs
  (`MatchedRuleInfo` has no request URL; `getMatchedRules` /
  `onRuleMatchedDebug` are debugging features for unpacked
  extensions).
- zproxy `docs/CAPABILITIES.md` (jozeta/zproxy, GitHub): MV3
  capability matrix, Chrome 124+ (webRequest non-blocking
  observation; CDP `Fetch` as the only MV3 pause path).
- cat-catch (`xifangczy/cat-catch`, GitHub, 23K stars, GPL-3.0):
  manifest (`webRequest` + `<all_urls>`) and background.js
  (`onSendHeaders` / `onResponseStarted` / `onCompleted` /
  `onErrorOccurred`, header-based classification, `tabId == -1`
  initiator resolution). Techniques only — no code copied.
- TwitchNoSub (`besuper/TwitchNoSub`, DeepWiki "Worker
  Interception System"): Worker-constructor override +
  `importScripts` prepend technique.
- hls.js worker role: liveapi.com hls.js guide (transmuxing in a
  worker; network on the main thread); dev.to "Check whether
  hls.js is actually using a worker (most ESM setups aren't)".
- chrome.debugger: blog.extenshi.io ("debugger permission: what
  extensions can really do"); medium.com/@dzianisv (CDP from
  extensions; persistent infobar; "Another debugger is already
  attached").
- FetchForge sources read: `extensions/chromium/content.js`,
  `extensions/chromium/background.js`,
  `extensions/chromium/manifest.json`,
  `docs/v02-phases/round7-notes.md`.

## 8. Recommendation for the parent agent

Build Tier 0 in the next fix round after round 8 (or fold it into
round 8 if the builder has not yet cut the extension changes —
Tier 0 is extension-only and does not conflict with round 8's
byterange + popup work, but it is a separate scope and should not
be smuggled into a running build). Tier 1 (webRequest +
`<all_urls>`) needs the user's explicit call on the permission,
because the install warning is the scariest Chrome shows — present
it as: "to see streams on every site, the extension needs
permission to observe network requests; nothing leaves your
computer; you approve it once." Do not build Tier 1 without that
approval. Tier 2 items are documented fallbacks, not work items.
