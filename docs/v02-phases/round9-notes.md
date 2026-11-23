# Fix round 9 — notes (2026-11-23)

General hidden-stream detection in the extension (Tier 0 + Tier 1).
Input: `novahd-stream-research.md` (the Tier 0/1 design, novahd as
the proving case) and `github-downloader-access-study.md`
(independent validation: cat-catch uses exactly Tier 0 + Tier 1;
the yt-dlp embedded-data scan idea). Extension 0.2.7 → **0.2.8**.

Scope: extension only. No engine, bridge, or desktop Rust changes
(round-8 lesson: the desktop crate cannot be type-checked on this
VM, so an engine API change there breaks Windows CI unseen).

## 1. Tier 0 — MAIN-world hook as a manifest content script

**The defect:** the round-6 hook was a stringified function
injected as an inline `<script>`. A strict page CSP (`script-src
'self'`, which novahd sets) blocks inline scripts, so the hook
never ran on exactly the sites that hide their streams.

**The fix:** the hook now lives in `extensions/{chromium,firefox}/
hook-main.js`, a real file declared in both manifests as a second
content-script entry with `"world": "MAIN"`, `run_at:
"document_start"`, `all_frames: true`. Manifest content scripts
are not subject to page CSP, so the hook runs on novahd. The
hook logic is unchanged from round 6/7 (fetch + XHR wrapping,
playlist-URL or playlist-Content-Type gate, text-shape gate,
256 KB cap, Content-Length guard) — only its delivery changed.

**Token delivery:** the old design passed the anti-spoof token as
an argument to the stringified function. A manifest script takes
no arguments, so the token now travels through the DOM: content.js
publishes it on `<html>` as `data-fetchforge-hook-token`
(`publishHookToken`, same MutationObserver + DOMContentLoaded
backstop pattern as the old injector), and hook-main.js reads it
from there. The token may land after the hook runs (both run at
document_start, and `<html>` may not exist yet), so the hook
queues captures (max 50) and flushes them once the token appears;
if no token appears within 30 s it stops watching and drops the
queue rather than observing the DOM forever. The message listener
in content.js is unchanged (it still accepts only `ff-manifest`
messages carrying the token).

**Dropped:** `manifestHookMain`, `injectManifestHook`, and the
injection bootstrap are deleted from content.js. No
`createElement("script")` injection remains (harness test (a)
asserts all of this). The old CSP caveat comment is replaced by
a round-9 note, and the comment that used to say the hook's
`isPlaylistUrl` lives "inside manifestHookMain" now points at
hook-main.js.

**Firefox floor:** `"world": "MAIN"` needs Firefox 128+, so the
firefox manifest's `strict_min_version` goes 109.0 → 128.0.
Chrome 111+ is required for the MAIN-world entry; the user's
Chrome is current.

## 2. Tier 0 — timing-log hardening

Four changes in content.js, all in the scan path:

1. **Decoupled playlist scan.** `findPlaylistFromResources` used
   to run only inside `mediaSource`, so a frame with no `<video>`
   element (shadow-DOM video, blob:-iframe player, player that
   has not created its element yet) never reported a playlist.
   `reportOverlayCandidates` now runs the scan itself: if no
   reported candidate carries a playlist, it adds a synthetic
   candidate from the timing log (`playing: true`, duration 0 —
   unknown, so a real playing video still outranks it on
   duration inside the playlist pool). The election is unchanged:
   the synthetic candidate enters the same playlist pool.
2. **Widened URL tests.** `STREAM_EXT_RE`, `MEDIA_FILE_EXT_RE`,
   `isManifestUrl`, `isDashUrl`, `isVideoFileUrl`, and
   `isMp4FamilyUrl` (the clip-guard probe gate — a wrapped mp4
   must not skip the probe) now match
   `.m3u8`/`.mpd`/media extensions ANYWHERE in the URL, as the
   hook has since round 7. Proxy-wrapped playlists (a worker URL
   with `?url=https://cdn/...m3u8&...` mid-URL) carry the
   extension followed by `&`, which the old anchored test missed
   entirely. `BG_STREAM_EXT_RE` in background.js is widened the
   same way, and `BG_MEDIA_FILE_EXT_RE` / `BG_PLAYLIST_MIME_RE`
   are added for the observer.
3. **Shadow-DOM traversal.** `querySelectorAllDeep` queries the
   light DOM and every OPEN shadow root recursively; `collectMedia`
   and `scan` use it instead of `document.querySelectorAll`.
   Closed roots are unreachable by design and stay invisible.
4. **Content-Type identification.** `scanResourceStreams` now
   treats a timing entry as a playlist when its
   `PerformanceResourceTiming.contentType` is a playlist MIME
   type (`vnd.apple.mpegurl`, `x-mpegurl`, `dash+xml`) — this is
   what identifies extensionless playlist endpoints like novahd's
   `/api/gs-hls`. `contentType` is undefined on older browsers;
   the URL tests still apply. Content-Type is used for playlists
   ONLY, not for direct files: a `video/mp4` response may be an
   fMP4 segment, and the clip guard already handles direct mp4s
   by extension.

**Wrong comment fixed (task item 3):** the hook-section comment
used to claim the timing log covers worker-loaded playlists. Per
the W3C Resource Timing spec, worker fetches go to the worker's
own timeline, not the document's — the comment now says so, and
points at the webRequest layer as the only one that sees them.

## 3. Tier 0 — embedded-data scan (yt-dlp idea)

`scanEmbeddedData` (content.js) scans the page's embedded data
for stream URLs as an additional detection source, feeding the
same election: JSON-LD, `<script type="application/json">` blobs
(Next.js/Nuxt payloads), and player-config objects in inline
scripts, plus `og:video` / `og:video:url` meta tags. Conservative
by design:

- Only URL shapes the extension already accepts: playlists via
  `isManifestUrl`, files via `MEDIA_FILE_EXT_RE` (which excludes
  `.ts`/`.m2ts` — segments are never downloadable files).
- Only inline scripts (`src` scripts have no inline text);
  scripts over the 256 KB cap are skipped outright (a bundle
  that big is not a player config); only `text/javascript`,
  `application/javascript`, `application/json`, and
  `application/ld+json` script types are read.
- Cached for 10 s (the scan runs on the candidate re-report
  schedule; re-reading every inline script on every mutation
  would be wasteful).

Feeding the election: an embedded playlist becomes a synthetic
candidate when the timing log has no playlist (observed reality
beats declared data); an embedded direct file only fills the gap
when NO candidate has any source at all (a real video's own
source always beats a declared one). The click path is unchanged,
so the clip guard still probes an embedded direct file at click
time.

## 4. Tier 1 — observe-only webRequest sniffing

**Permission (user-approved 2026-11-23 12:41 PKT):** `webRequest`
is added to `permissions`, and `<all_urls>` is added as an
OPTIONAL host permission (`optional_host_permissions`), requested
at runtime from the popup (see §5). Without it, the listeners
simply never fire for CDN origins and detection falls back to
Tier 0 — no error, no nagging.

**Observer (background.js):** `onResponseStarted` with
`["responseHeaders"]`, observe-only — no `blocking`
extraInfoSpec (removed for standard MV3 extensions anyway), no
request headers, no bodies, no cookies or auth headers, nothing
leaves the machine. Findings feed only the local election.
`classifyObservedRequest(url, contentType, resourceType)`:
playlist by URL or playlist Content-Type (extensionless
endpoints); file by URL, by `resourceType === "media"`, or by a
video/audio Content-Type; `video/mp2t` is a TS segment and is
never a file. `onResponseStarted` fires for redirects too, so a
playlist URL that 302s is still classified by its own URL.
`onSendHeaders` was deliberately NOT used: the URL + Content-Type
classification in `onResponseStarted` covers the same URLs, and
a playlist the player needs always produces a response.

**Election integration:** findings live in `tabObserved`
(tabId → Map(url → { url, kind, initiator, at })), capped at
100 per tab (oldest dropped), 30-minute TTL, cleared on tab
close. `bestDownloadSource` merges them as additional pool
entries (`playing: true`, duration 0, `frameId: -1`, initiator
as the referer); `isPlaylist` honours the classified `kind`, so
an extensionless playlist enters the playlist pool even though
its URL has no `.m3u8`/`.mpd`. Stale findings from a previous
page on the same tab are filtered by initiator origin: an entry
only counts when its initiator origin matches a frame the tab
currently has (when no frame has reported yet, nothing is
filtered — the observation may precede the first report).
`electOverlay` now runs on observed findings alone and sends the
verdict to frame 0 when no frame has reported yet, so the button
appears on a tab whose frames never reported a candidate.

**tabId -1:** requests from a service worker (or the extension
itself) have no tab. `resolveObservedTab` attributes them to a
tab whose URL shares the request's initiator origin, else the
active tab, else drops the finding.

**No keep-alive:** findings live in memory only, so a
service-worker restart loses them until the next matching
request. The Tier 0 layers cover the same streams in most cases,
and waking the worker on a timer drains battery for an insurance
layer. Documented as a limitation, not fixed.

## 5. Popup: the runtime permission request

`chrome.permissions.request` needs a user gesture, and content
scripts cannot call it — so the popup is the only place the
optional `<all_urls>` permission can be requested. popup.html
gains a "Full stream detection" status line + "Enable full stream
detection" button; popup.js gains `refreshStreamDetect` (shows
the current state via `permissions.contains`) and the button
handler (requests the permission, reports granted/denied in the
popup status line). The button only shows while the permission is
missing. This is a real control, not a fake one: it actually
requests the permission and reflects the state.

## 6. Tests

- **Harness: 66 passed, 0 failed** (baseline 47 + 19 new in
  `extensions/harness/round9.test.js`). The 5 decision tests
  from the brief, plus extras:
  - (a) hook-main.js exists, parses, and is wired as a
    MAIN-world content script in both manifests (version 0.2.8,
    webRequest declared, optional `<all_urls>`, Firefox floor
    128+); no inline injection remains in content.js; the hook
    forwards a captured playlist with the token; the hook queues
    a capture until the token appears, then flushes.
  - (b) timing scan finds an extensionless playlist by
    Content-Type (and suppresses the clip); a TS segment is not
    a playlist; a DASH manifest by Content-Type and a wrapped
    playlist URL.
  - (c) a playlist in the timing log is reported with no
    `<video>` element; nothing spurious without a stream; an
    embedded playlist fills the gap when the timing log is
    empty.
  - (d) webRequest observer classifies a playlist by
    Content-Type; rejects segments and non-media; accepts files.
  - (e) embedded scan finds a stream URL in page JSON, a
    player-config playlist, and an og:video file; skips src
    scripts, over-cap scripts, and segments.
  - Extras: widened URL tests; an observed extensionless
    playlist beats a direct file in the election; a stale-origin
    observation is filtered out; observed findings alone still
    produce a verdict.
- **Existing harness files unchanged in count:** background 12,
  content 32, progress-percent 3. The background.test.js ctxB
  prelude gained `tabObserved` + `OBSERVED_TTL_MS` because
  `bestDownloadSource` now references them (harness alignment,
  not a weakened test).
- **`cargo test --workspace`: 188 passed, 0 failed** — baseline
  preserved exactly (no Rust changed).
- `node --check` passes on all 8 extension JS files (chromium +
  firefox). Both manifests are valid JSON at 0.2.8.
  chromium/firefox copies of content.js, background.js,
  popup.js, popup.html, hook-main.js, and rules/ads.json are
  byte-identical (verified with `cmp`).

## 7. NOT verified (needs the user's browser)

None of the live novahd checks can be done from this VM:

1. **/api/gs-hls Content-Type** — whether novahd's playlist
   endpoint actually serves a playlist MIME type (the
   Content-Type identification depends on it).
2. **Playlist in the live timing log** — whether the playlist
   URL appears in the document's timing log on a live novahd
   page (vs. being worker-fetched, in which case only Tier 1
   sees it).
3. **`contentType` in his Chrome version** — the API exists
   since Chrome 109; the live check is whether novahd's entries
   actually carry it.
4. **MAIN-world script running on novahd** — whether the
   manifest hook executes on a page with a strict CSP, and
   whether it captures the playlist text.
5. **The webRequest permission flow** — whether the popup's
   "Enable full stream detection" button grants `<all_urls>`
   and the observer then sees novahd's playlist request.
6. **End to end on novahd** — whether the button now offers the
   real movie stream instead of refusing the 40s clip (the
   clip guard must still refuse when no stream is found).

## 8. Known limitations (by design, not bugs)

- The embedded scan cannot see streams assembled in JS from
  pieces (no URL ever appears in the page data) — that is the
  webRequest layer's job.
- The synthetic playlist candidate has no duration, so inside
  the playlist pool a real playing video with a known duration
  outranks it. That is the intended order.
- Observed findings are lost on a service-worker restart (see
  §4). The 10 s candidate re-report rebuilds the frame side of
  the election, but not the observed side.
- The hook still cannot see worker-loaded playlists (workers
  have their own fetch, and the timing log does not cover them
  either). Only the webRequest layer sees those — which is why
  Tier 1 exists.
- MediaSource byte capture ("record" mode) and deep search
  (ranks 4–5 in the GitHub study) are NOT in this round.
