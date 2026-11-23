// FetchForge content script (MV3, shared by the Chromium and Firefox builds).
// Phase 4 (MediaDetectionEngine): detects media on http(s) pages, shows a
// floating download button fixed to the viewport's bottom-right corner,
// answers the popup's getMedia request, and reports the detected set to
// the background script (which forwards it to the desktop app via POST
// /media/detected).
//
// Runs in the page's isolated world: it can read the DOM but never sees
// page JS variables, cookies, or the bridge token (the token lives only
// in the background script and is never passed to this script). Media
// URLs are only sent to the app when the user clicks Download, or as a
// detected-media report (URL + confirmed metadata only, no credentials).
//
// Round 6 adds a MAIN-world fetch/XHR hook (see the manifest-hook
// section below) that captures playlist TEXT the player itself
// received — the playlist body, never cookies, credentials, or page
// content — so a single-use signed playlist still downloads. Round 9
// moved the hook into hook-main.js, a manifest-declared content
// script ("world": "MAIN") that page CSP cannot block; the hook is
// still best-effort, and the resource-timing fallback below still
// applies when it misses.
//
// It never runs on chrome://, about:, or extension pages: the manifest
// matches http(s) only, and the protocol check below is a second line
// of defence.
//
// Detection layers (round 9): DOM elements, the resource-timing
// log (including extensionless playlists, identified by
// Content-Type, and playlists in frames with no <video> element),
// the MAIN-world hook, stream URLs declared in the page's embedded
// data (JSON-LD, application/json blobs, player configs), plus an
// observe-only webRequest layer in the background script, which
// sees worker- and service-worker-fetched streams the page layers
// cannot. The webRequest layer needs the optional <all_urls> host
// permission, requested at runtime from the popup; without it,
// detection falls back to the page-side layers. blob: sources (e.g.
// YouTube's MSE player) cannot be downloaded directly, so for those the
// button sends the PAGE URL and the desktop app extracts the media
// server-side — unless the resource-timing log reveals the real stream
// URL the player fetched (see findStreamUrlFromResources), in which
// case that URL is sent instead. When the log shows a playlist, the
// playlist wins over any direct file in the DOM: a direct .mp4 is
// often a preview/ad clip, not the movie that is playing (novahd
// 2026-11-22). data: URLs are skipped entirely. No DRM, auth,
// paywall, or encryption bypass is attempted anywhere.
//
// Stacked videos (a preview/ad <video> on top of the main player,
// common on streaming sites) share one on-screen box: only one
// button is shown per tab, downloading the playing (or largest)
// element — see pickOverlayRepresentatives and the cross-frame
// election below. The button is NOT positioned on the video: it is
// a viewport-anchored floating button (round 4), because the
// on-video button broke when its host became a popover (the UA
// overflow:auto rule clipped the translated button entirely — see
// the floating-button section header). It lives only in the top
// frame's document (round 5): the top layer is per-document, so a
// button in a player iframe sits under the parent page's overlays.
(function () {
  "use strict";
  if (window.location.protocol !== "http:" && window.location.protocol !== "https:") return;
  // Avoid double-injection if the script is ever loaded twice.
  if (window.__fetchforgeContentLoaded) return;
  window.__fetchforgeContentLoaded = true;

  // The default resource-timing buffer holds 150 entries: on a
  // streaming page the flood of segment fetches evicts the manifest
  // long before the user clicks. Grow it now — this script runs at
  // document_start, before the player loads — so the playlist is
  // still in the log when the stream picker looks for it.
  try { performance.setResourceTimingBufferSize(5000); } catch (_) {}

  // ---- MAIN-world manifest hook (round 6, study B2/A8) ----
  //
  // TECHNIQUE ONLY, reimplemented from scratch: the idea of hooking
  // fetch/XHR in the page's MAIN world to clone playlist responses
  // comes from the published description of XDM's hls-hook.js
  // (GPL-3.0). No code, identifiers, comments, or structure were
  // copied from it; everything below is original FetchForge code.
  //
  // Why: on novahd the playlist the player loads never appears in
  // any frame's resource-timing log (fetched before this script
  // runs, or via a path the timing API does not surface), so the
  // picker fell back to a 15-second preview clip. The hook sees
  // what the player sees: it clones 2xx playlist responses and
  // forwards the TEXT to this content script, which keeps it per
  // URL and attaches it to the download. Some CDNs sign a playlist
  // link for a single request, so re-fetching it from the app gets
  // 403 — the captured text is the only copy that works.
  //
  // Privacy: only playlist text + its URL are captured — never
  // cookies, credentials, headers, or page content. A worker-loaded
  // playlist is NOT seen by this hook (workers have their own
  // fetch) — and NOT by the timing log either: per the W3C
  // Resource Timing spec, worker fetches go to the worker's own
  // timeline, not the document's. Only the background's
  // observe-only webRequest layer (round 9, Tier 1) sees them.
  //
  // Round 9: the hook is a manifest-declared content script
  // (hook-main.js, "world": "MAIN"), NOT an injected inline
  // <script> — manifest content scripts are not subject to the
  // page's CSP, so a strict script-src no longer kills the hook
  // (that is what blocked it on novahd). The hook is still
  // best-effort: its failure must NEVER break the button or the
  // timing-log fallback — every hook step is guarded, and a
  // failure is silent.
  //
  // Anti-spoof token: a random per-page-load token, published on
  // <html> as data-fetchforge-hook-token (see publishHookToken
  // below); the hook reads it from there and echoes it in every
  // message, and the listener below only accepts messages carrying
  // it, so a page script cannot forge a fake manifest into the
  // download path. (Not a security boundary — a page script can
  // read the attribute — but it stops casual postMessage
  // spoofing, and the token never leaves the page.)
  const HOOK_TOKEN = (function () {
    try {
      const b = new Uint8Array(16);
      crypto.getRandomValues(b);
      return Array.from(b, function (x) { return ("0" + x.toString(16)).slice(-2); }).join("");
    } catch (_) { return "ff-" + Math.random().toString(36).slice(2) + Date.now().toString(36); }
  })();
  // 256 KB cap per captured playlist, enforced in BOTH worlds: a VOD
  // master/variant is a few KB, but a live playlist can grow without
  // bound and must never bloat the download message. Text over the
  // cap is dropped here and in the hook; the URL is still sent and
  // the engine fetches the playlist normally.
  const MANIFEST_TEXT_MAX = 262144;
  // Captured playlist text by URL: url -> { text, at } (at is
  // performance.now() at capture time, for ranking). First capture
  // wins per URL: the player's first fetch carries the signed link;
  // later re-fetches of the same URL add nothing for a VOD stream.
  const capturedManifests = new Map();
  window.addEventListener("message", function (e) {
    if (e.source !== window) return;
    const d = e.data;
    if (!d || d.type !== "ff-manifest" || d.token !== HOOK_TOKEN) return;
    if (typeof d.text !== "string" || d.text.length > MANIFEST_TEXT_MAX) return;
    const url = resolveUrl(d.url);
    if (!url || !/^https?:\/\//i.test(url)) return;
    if (capturedManifests.has(url)) return;
    capturedManifests.set(url, { text: d.text, at: performance.now() });
    // A newly captured playlist may change this frame's stream
    // (the clip becomes ineligible — see scanResourceStreams), so
    // re-scan and re-report to the election promptly.
    scheduleScan();
  });
  // The hook itself lives in hook-main.js, a manifest-declared
  // content script running in the page's MAIN world ("world":
  // "MAIN" in manifest.json). This script only publishes the
  // anti-spoof token where the hook can read it: a DOM attribute
  // on <html>. The hook forwards captured playlists only with
  // this token (and queues captures until the token appears, so
  // an early playlist is never lost when the attribute lands
  // after the hook runs — see hook-main.js).
  const HOOK_TOKEN_ATTR = "data-fetchforge-hook-token";
  let hookTokenPublished = false;
  function publishHookToken() {
    if (hookTokenPublished) return true;
    const root = document.documentElement;
    if (!root) return false;
    try {
      root.setAttribute(HOOK_TOKEN_ATTR, HOOK_TOKEN);
      hookTokenPublished = true;
      return true;
    } catch (_) { return false; }
  }
  if (!publishHookToken()) {
    // documentElement does not exist yet at document_start: retry
    // as soon as it appears, with DOMContentLoaded as a backstop.
    const tokenObserver = new MutationObserver(function () {
      if (publishHookToken()) tokenObserver.disconnect();
    });
    try { tokenObserver.observe(document, { childList: true }); } catch (_) {}
    document.addEventListener("DOMContentLoaded", function () {
      tokenObserver.disconnect();
      publishHookToken();
    }, { once: true });
  }

  const api = typeof browser !== "undefined" ? browser : chrome;
  const isFirefox = typeof browser !== "undefined";

  // runtime.sendMessage is Promise-based under Firefox (`browser`) and
  // callback-based under Chrome (`chrome`); normalise to a Promise.
  function sendRuntimeMessage(msg) {
    if (isFirefox) return api.runtime.sendMessage(msg);
    return new Promise((resolve) => api.runtime.sendMessage(msg, resolve));
  }

  // Anything smaller than this area is treated as hidden for detection.
  const MIN_VISIBLE_AREA = 400; // px^2

  function resolveUrl(raw) {
    try { return new URL(String(raw).trim(), document.baseURI).href; }
    catch (_) { return null; }
  }

  function isVisible(el) {
    if (!el.isConnected) return false;
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden" || style.visibility === "collapse") return false;
    const rect = el.getBoundingClientRect();
    if (rect.width * rect.height < MIN_VISIBLE_AREA) return false;
    return true;
  }

  function mediaTitle(el) {
    return (el.getAttribute("title") || el.getAttribute("aria-label") || "").trim()
      || document.title || "Media";
  }

  function fileNameFromUrl(url) {
    try {
      const base = decodeURIComponent(new URL(url).pathname.split("/").pop() || "");
      return base || undefined;
    } catch (_) { return undefined; }
  }

  // ---- Detector registry (Phase 7 plug-in point) ----
  //
  // A detector is { kind, selector, overlay, extract }:
  //   kind     - item kind reported to the app ("video" | "audio" |
  //              "image" | "model" | "document")
  //   selector - CSS selector for candidate elements
  //   overlay  - true when detected elements get the download button
  //   extract  - (el) => item | null; item is { kind, url, viaPage,
  //              title, filename? } with only CONFIRMED metadata
  //              (unknown fields are omitted, never guessed).
  // Phase 7 adds image/3D detection by registering new detectors here;
  // collectMedia, the overlay manager, reporting, and the popup need no
  // changes for a new detector type.
  const detectors = [];
  function registerDetector(detector) { detectors.push(detector); }

  // Best downloadable source for a <video>/<audio> element, or null.
  // Returns { url, viaPage, viaStream?, manifestText?, masterText? }:
  // url is a direct http(s) media URL, or null with viaPage=true when
  // the only source is a blob: URL (the page URL must be sent instead
  // so the desktop app can extract the media). viaStream is set when
  // the URL came from the resource-timing log (or a captured manifest)
  // rather than the DOM (see below). manifestText is the captured
  // playlist text for url (if the MAIN-world hook saw it); masterText
  // is the captured text of this frame's master playlist (if any) —
  // the quality popup parses it into variant rows.
  //
  // Playlist-first: a playlist observed in the resource-timing log
  // beats ANY direct file in the DOM. The direct file is often a
  // preview/ad clip while the movie plays through MSE (novahd
  // 2026-11-22: a 15-second /assets/p/*.mp4 clip was sent while a
  // 2:52 HLS movie was playing). The stream that is playing is the
  // playlist, never the clip.
  function mediaSource(el) {
    const candidates = [];
    if (el.currentSrc) candidates.push(el.currentSrc);
    const attrSrc = el.getAttribute && el.getAttribute("src");
    if (attrSrc) candidates.push(attrSrc);
    const sources = el.querySelectorAll ? el.querySelectorAll("source") : [];
    for (const s of sources) {
      const u = (s.getAttribute && s.getAttribute("src")) || s.src;
      if (u) candidates.push(u);
    }
    let sawBlob = false;
    let directFile = null;
    for (const raw of candidates) {
      const u = resolveUrl(raw);
      if (!u || u.startsWith("data:")) continue; // data: URLs are skipped
      if (u.startsWith("blob:")) { sawBlob = true; continue; }
      if (/^https?:\/\//i.test(u)) {
        // A playlist URL in the DOM is the stream itself, not a
        // file: it wins immediately.
        if (STREAM_EXT_RE.test(u)) {
          const cap = capturedManifests.get(u);
          const out = { url: u, viaPage: false };
          if (cap) {
            out.manifestText = cap.text;
            // The DOM URL may itself be the master (its text
            // contains #EXT-X-STREAM-INF); otherwise the frame's
            // master text, if any, comes from the scan.
            if (cap.text.indexOf("#EXT-X-STREAM-INF") !== -1 || cap.text.indexOf("<MPD") !== -1) {
              out.masterText = cap.text;
            } else {
              const pl = findPlaylistFromResources();
              if (pl && pl.masterText) out.masterText = pl.masterText;
            }
          }
          return out;
        }
        if (!directFile) directFile = u;
      }
    }
    // A playlist in the resource-timing log (or captured by the
    // MAIN-world hook) beats the direct file: see the playlist-first
    // note above.
    const playlist = findPlaylistFromResources();
    if (playlist) return { url: playlist.url, viaPage: false, viaStream: true, audioUrl: playlist.audioUrl, manifestText: playlist.manifestText, masterText: playlist.masterText };
    if (directFile) return { url: directFile, viaPage: false };
    if (sawBlob) {
      // The player loads its stream through Media Source Extensions:
      // the real URL is not in the DOM, but the browser's
      // resource-timing log lists every URL this document fetched,
      // including the manifest the player loaded. Send that instead
      // of the bare page URL when it is found. (A playlist would
      // already have been returned above; this only ever yields a
      // direct file the player fetched.)
      const stream = findStreamUrlFromResources();
      if (stream) return { url: stream.url, viaPage: false, viaStream: true, audioUrl: stream.audioUrl, manifestText: stream.manifestText, masterText: stream.masterText };
      return { url: null, viaPage: true };
    }
    return null;
  }

  // Resource-timing stream detection for blob:/JS players.
  // Entry names are visible cross-origin (only the detailed timing
  // fields need a Timing-Allow-Origin header), so this works for
  // third-party stream CDNs too. No cookies, headers, or credentials
  // are involved: the names are read-only metadata about this
  // document's own fetches. Manifests (.m3u8/.mpd) beat direct
  // files; within a kind, a fetch the player itself made
  // (initiator "video"/"audio"/"fetch"/"xmlhttprequest") beats page
  // furniture, and the most recent fetch wins — the currently
  // playing stream is the freshest entry. Only this document's
  // entries are visible (an iframe player has its own log, read by
  // the content script running in that frame).
  //
  // Master vs variant: a master playlist (master/index/playlist/
  // manifest.m3u8) describes the whole stream — video AND audio —
  // while a variant (v0.m3u8, audio_1.m3u8) is one track only.
  // Sending the first manifest seen used to pick a bare video
  // variant (v0.m3u8) and deliver a silent file. A master is now
  // preferred whenever one was observed; when only variants were
  // observed, the best video variant is sent TOGETHER with its
  // sibling audio variant, so the app downloads both and merges
  // them instead of delivering half a movie.
  // Round 9: match .m3u8/.mpd ANYWHERE in the URL, as the MAIN-world
  // hook has since round 7. Proxy-wrapped playlists (a worker URL
  // with ?url=https://cdn/...m3u8&... mid-URL) carry the extension
  // followed by &, which the old anchored test missed entirely.
  const STREAM_EXT_RE = /\.(m3u8|mpd)/i;
  const MEDIA_FILE_EXT_RE = /\.(mp4|webm|mov|mkv)/i;
  // Playlist Content-Types, for extensionless playlist URLs
  // (novahd's /api/gs-hls): identified by what the server says the
  // response is, not by the URL's shape. Used on timing-log
  // entries (PerformanceResourceTiming.contentType, Chrome 109+)
  // and by the background's webRequest observer.
  const PLAYLIST_MIME_RE = /(?:vnd\.apple\.mpegurl|x-mpegurl|dash\+xml)/i;
  const MASTER_STEM_RE = /^(master|index|playlist|manifest)$/i;
  const AUDIO_STEM_RE = /audio/i;
  function manifestKind(url) {
    const name = url.split(/[?#]/)[0].split("/").pop() || "";
    const stem = name.replace(/\.(m3u8|mpd)$/i, "");
    if (MASTER_STEM_RE.test(stem)) return "master";
    if (AUDIO_STEM_RE.test(stem)) return "audio";
    return "video";
  }
  // One scan of the resource-timing log, shared by the playlist
  // and stream pickers below. Returns { master, video, audio,
  // file }: the best observed URL of each kind (or null). Best =
  // player-initiated fetch first, then most recent (see the
  // ranking comment on findStreamUrlFromResources). Captured
  // manifests (MAIN-world hook) are merged into the same buckets
  // with the highest rank: they are the playlist the player itself
  // requested, even when the timing log never shows them.
  //
  // Per-frame playlist suppression (study B1, XDM m3u8MpdTabs
  // TECHNIQUE reimplemented): once ANY playlist is seen among a
  // frame's candidates, direct-file candidates from that frame are
  // suppressed (file is nulled below). A direct file alongside a
  // playlist is a preview/ad clip, never the stream that is
  // playing — it must become ineligible the moment a playlist
  // exists, in this frame's log OR in its captured manifests.
  function scanResourceStreams() {
    let entries = [];
    try {
      entries = performance.getEntriesByType("resource") || [];
    } catch (_) { entries = []; } // fall through: captured manifests may still exist
    const rankOf = function (e) {
      const it = e.initiatorType || "";
      return (it === "video" || it === "audio" || it === "fetch" || it === "xmlhttprequest") ? 1 : 0;
    };
    const better = function (a, b) {
      return !b || a.rank > b.rank || (a.rank === b.rank && a.startTime >= b.startTime);
    };
    const out = { master: null, video: null, audio: null, file: null };
    for (const e of entries) {
      const name = e && e.name;
      if (!name || !/^https?:\/\//i.test(name)) continue;
      const cand = { url: name, rank: rankOf(e), startTime: typeof e.startTime === "number" ? e.startTime : 0 };
      // Extensionless playlist, identified by Content-Type
      // (round 9): a playlist endpoint like /api/gs-hls carries
      // no .m3u8/.mpd in its URL. contentType is undefined on
      // older browsers — the URL tests below still apply.
      const isPlaylistMime = !!(e.contentType && PLAYLIST_MIME_RE.test(e.contentType));
      if (isPlaylistMime || STREAM_EXT_RE.test(name)) {
        const kind = manifestKind(name);
        if (kind === "master") {
          if (better(cand, out.master)) out.master = cand;
        } else if (kind === "audio") {
          if (better(cand, out.audio)) out.audio = cand;
        } else {
          if (better(cand, out.video)) out.video = cand;
        }
      } else if (MEDIA_FILE_EXT_RE.test(name)) {
        if (better(cand, out.file)) out.file = cand;
      }
    }
    // Merge captured manifests: kind by playlist STRUCTURE (B3) —
    // a master contains #EXT-X-STREAM-INF, a DASH manifest is the
    // whole stream — with the stem heuristic only to spot an audio
    // media playlist. Rank 2 beats any timing entry; the captured
    // text rides along so the download can carry it (see
    // pickPlaylist). A captured URL with no playlist extension
    // (matched by content-type in the hook) still counts: its text
    // proves it is a playlist.
    capturedManifests.forEach(function (cap, url) {
      const isMaster = cap.text.indexOf("#EXT-X-STREAM-INF") !== -1 || cap.text.indexOf("<MPD") !== -1;
      const kind = isMaster ? "master" : (AUDIO_STEM_RE.test(url.split(/[?#]/)[0].split("/").pop() || "") ? "audio" : "video");
      const cand = { url: url, rank: 2, startTime: cap.at, text: cap.text };
      if (kind === "master") {
        if (better(cand, out.master)) out.master = cand;
      } else if (kind === "audio") {
        if (better(cand, out.audio)) out.audio = cand;
      } else {
        if (better(cand, out.video)) out.video = cand;
      }
    });
    // Attach captured text to timing-log winners too: a manifest
    // the hook captured AND the log shows is the same playlist.
    for (const k of ["master", "video", "audio"]) {
      if (out[k] && !out[k].text) {
        const cap = capturedManifests.get(out[k].url);
        if (cap) out[k].text = cap.text;
      }
    }
    // Playlist suppression (see the header note): a playlist
    // anywhere in this frame makes its direct files ineligible.
    if (out.master || out.video || out.audio) out.file = null;
    return out;
  }
  // Pick the playlist from a scan: a master is the whole stream
  // (send it alone); otherwise pair the video variant with its
  // audio sibling; an audio-only variant goes alone. Returns
  // { url, audioUrl?, manifestText?, masterText? } or null.
  // manifestText is the captured text for the returned URL (if
  // any); masterText is the captured text of the frame's master
  // playlist (if any) — the popup parses it into variant rows even
  // when the download URL itself is a variant.
  function pickPlaylist(s) {
    const masterText = (s.master && s.master.text) || undefined;
    if (s.master) return { url: s.master.url, manifestText: s.master.text, masterText: masterText };
    if (s.video) {
      const out = { url: s.video.url, manifestText: s.video.text, masterText: masterText };
      if (s.audio) out.audioUrl = s.audio.url;
      return out;
    }
    if (s.audio) return { url: s.audio.url, manifestText: s.audio.text, masterText: masterText };
    return null;
  }
  // Best playlist (master or variant) observed in the log, or
  // null. Direct files are deliberately NOT returned here: a
  // playlist is the stream that is playing; a file may be a
  // preview/ad clip (see the playlist-first note on mediaSource).
  function findPlaylistFromResources() {
    return pickPlaylist(scanResourceStreams());
  }
  // Returns { url, audioUrl?, manifestText?, masterText? } or null:
  // url is the manifest/file to send; audioUrl is set only when
  // url is a video variant and a sibling audio variant was observed
  // alongside it. manifestText/masterText ride along from
  // pickPlaylist (see above) when the hook captured them.
  function findStreamUrlFromResources() {
    const s = scanResourceStreams();
    return pickPlaylist(s) || (s.file ? { url: s.file.url } : null);
  }

  // ---- Embedded-data scan (round 9, yt-dlp GenericIE idea) ----
  //
  // Some players never request their stream through a URL the
  // page layers can observe (the manifest URL is assembled in JS
  // from pieces, or the player has not fetched it yet), but the
  // page DECLARES the stream in its embedded data: JSON-LD, a
  // <script type="application/json"> blob (Next.js/Nuxt payloads),
  // or a player-config object in an inline script. Scan those for
  // stream URLs as an additional detection source, feeding the
  // same election as the timing log. Conservative: only URL
  // shapes the extension already accepts as playlists or direct
  // media files (MEDIA_FILE_EXT_RE, which excludes .ts/.m2ts —
  // segments are never downloadable files), only inline scripts
  // (src scripts have no inline text), and scripts over the
  // 256 KB cap are skipped outright (a bundle that big is not a
  // player config). The result is cached for 10 s — the scan runs
  // on the same schedule as the candidate re-report, and
  // re-reading every inline script on every mutation would be
  // wasteful. Returns { at, playlist, file } (URLs or null).
  const EMBEDDED_URL_RE = /https?:\/\/[^\s"'<>\\]+/g;
  const EMBEDDED_TEXT_MAX = 262144;
  const EMBEDDED_CACHE_MS = 10000;
  let embeddedCache = { at: 0, playlist: null, file: null };
  function scanEmbeddedData() {
    const now = Date.now();
    if (now - embeddedCache.at < EMBEDDED_CACHE_MS) return embeddedCache;
    const result = { at: now, playlist: null, file: null };
    embeddedCache = result;
    const consider = function (text) {
      if (!text || (result.playlist && result.file)) return;
      const urls = String(text).match(EMBEDDED_URL_RE);
      if (!urls) return;
      for (const raw of urls) {
        const u = resolveUrl(raw);
        if (!u || !/^https?:\/\//i.test(u)) continue;
        if (!result.playlist && isManifestUrl(u)) { result.playlist = u; continue; }
        if (!result.file && MEDIA_FILE_EXT_RE.test(u)) result.file = u;
      }
    };
    let scripts = [];
    try { scripts = document.querySelectorAll("script"); } catch (_) { scripts = []; }
    for (const s of scripts) {
      if (result.playlist && result.file) break;
      if (s.src) continue;
      const type = (s.type || "").trim().toLowerCase();
      if (type && type !== "text/javascript" && type !== "application/javascript"
          && type !== "application/json" && type !== "application/ld+json") continue;
      const text = s.textContent || "";
      if (!text || text.length > EMBEDDED_TEXT_MAX) continue;
      consider(text);
    }
    if (!result.playlist && !result.file) {
      // OpenGraph video URL: a page-level declaration of its video.
      let metas = [];
      try { metas = document.querySelectorAll('meta[property="og:video"], meta[property="og:video:url"]'); } catch (_) { metas = []; }
      for (const m of metas) {
        const content = m.getAttribute && m.getAttribute("content");
        if (content) consider(content);
        if (result.playlist || result.file) break;
      }
    }
    return result;
  }

  // Filename for a resolved media source: none for a page URL (the
  // app derives it from the title) and none for a stream manifest
  // (the app names it from the manifest, not "master.m3u8"); a
  // direct file keeps its own name.
  function filenameForSource(src) {
    if (!src || src.viaPage) return undefined;
    if (src.viaStream && STREAM_EXT_RE.test(src.url)) return undefined;
    return fileNameFromUrl(src.url);
  }

  function extractHtmlMedia(kind) {
    return function (el) {
      if (!isVisible(el)) return null;
      const src = mediaSource(el);
      if (!src) return null;
      const item = {
        kind: kind,
        url: src.viaPage ? window.location.href : src.url,
        viaPage: src.viaPage,
        title: mediaTitle(el),
        filename: filenameForSource(src),
      };
      if (src.viaStream) item.viaStream = true;
      if (src.audioUrl) item.audioUrl = src.audioUrl;
      if (kind === "video") {
        // Poster + duration are confirmed metadata for the Media
        // Detector panel (round 6): the poster only when the
        // element actually has one, the duration only when it is a
        // finite number of seconds (NaN = no metadata yet,
        // Infinity = live stream — both omitted, never guessed).
        const posterRaw = el.getAttribute && el.getAttribute("poster");
        if (posterRaw) {
          const poster = resolveUrl(posterRaw);
          if (poster && /^https?:\/\//i.test(poster)) item.poster = poster;
        }
        if (typeof el.duration === "number" && isFinite(el.duration)) item.duration = el.duration;
      }
      return item;
    };
  }

  // Video/audio only in this wave; image + 3D detectors arrive in Phase 7.
  registerDetector({ kind: "video", selector: "video", overlay: true, extract: extractHtmlMedia("video") });
  registerDetector({ kind: "audio", selector: "audio", overlay: false, extract: extractHtmlMedia("audio") });

  // ---- Image + 3D detection (Phase 7) ----
  //
  // No overlay button on images: a button on every image would spam
  // galleries. Images and 3D assets flow through the existing
  // reportMedia path into the popup list and the Media Detector panel
  // (Phase 9), where the user picks them. CSS background-image is
  // deliberately NOT detected: it is mostly page furniture (icons,
  // sprites, ads) and cannot be told apart from real content.
  // WebGL apps that fetch models from JS are not DOM-detectable and
  // are not claimed to be.

  // Parse a srcset attribute into [{ url, width, density }].
  // Malformed entries are skipped, never fatal.
  function parseSrcset(srcset) {
    const out = [];
    for (const part of String(srcset).split(",")) {
      const tokens = part.trim().split(/\s+/).filter(Boolean);
      if (!tokens.length) continue;
      const entry = { url: tokens[0], width: null, density: null };
      for (const t of tokens.slice(1)) {
        const w = /^(\d+)w$/.exec(t);
        if (w) { entry.width = parseInt(w[1], 10); continue; }
        const d = /^(\d+(?:\.\d+)?)x$/.exec(t);
        if (d) entry.density = parseFloat(d[1]);
      }
      out.push(entry);
    }
    return out;
  }

  // The full-resolution URL across one or more srcsets: largest width
  // wins, else highest density, else the first candidate. Ties prefer
  // the later candidate (srcsets are normally smallest-first). This
  // mirrors the Rust parse_srcset / pick_best_srcset_url in the
  // engine, so the extension and the desktop agree on "full size".
  function pickBestSrcsetUrl(srcsets) {
    let best = null;
    for (const srcset of srcsets) {
      for (const c of parseSrcset(srcset)) {
        if (!best) { best = c; continue; }
        if (c.width != null && best.width != null) {
          if (c.width >= best.width) best = c;
        } else if (c.width != null && best.width == null) {
          best = c;
        } else if (c.width == null && best.width == null
          && c.density != null && (best.density == null || c.density >= best.density)) {
          best = c;
        }
      }
    }
    return best ? best.url : null;
  }

  function extractImage(el) {
    if (!isVisible(el)) return null;
    // Best candidate: the largest srcset entry across the <img> and
    // its <picture> <source> elements, else the browser's chosen
    // source (currentSrc), else src / data-src (lazy-loaded images).
    // srcset wins over currentSrc: currentSrc is the resolution the
    // page chose for this viewport, not the full-resolution original.
    const srcsets = [];
    const imgSrcset = el.getAttribute && el.getAttribute("srcset");
    if (imgSrcset) srcsets.push(imgSrcset);
    const picture = el.closest ? el.closest("picture") : null;
    if (picture && picture.querySelectorAll) {
      const sources = picture.querySelectorAll("source[srcset]");
      for (const s of sources) {
        const ss = s.getAttribute && s.getAttribute("srcset");
        if (ss) srcsets.push(ss);
      }
    }
    let raw = pickBestSrcsetUrl(srcsets)
      || el.currentSrc
      || (el.getAttribute && (el.getAttribute("src") || el.getAttribute("data-src")));
    if (!raw) return null;
    const u = resolveUrl(raw);
    if (!u || u.startsWith("data:") || u.startsWith("blob:")) return null;
    if (!/^https?:\/\//i.test(u)) return null;
    return {
      kind: "image",
      url: u,
      viaPage: false,
      title: (el.getAttribute && el.getAttribute("alt")) || mediaTitle(el),
      filename: fileNameFromUrl(u),
    };
  }

  // 3D model file extensions (same list as the engine's threed.rs).
  const MODEL_EXT_RE = /\.(glb|gltf|obj|fbx|stl|usdz)([?#]|$)/i;

  function extractModelViewer(el) {
    if (!isVisible(el)) return null;
    const raw = el.getAttribute && el.getAttribute("src");
    const u = raw && resolveUrl(raw);
    if (!u || !MODEL_EXT_RE.test(u)) return null;
    return {
      kind: "model",
      url: u,
      viaPage: false,
      title: mediaTitle(el),
      filename: fileNameFromUrl(u),
    };
  }

  function extractModelLink(el) {
    if (!isVisible(el)) return null;
    const raw = el.getAttribute && el.getAttribute("href");
    const u = raw && resolveUrl(raw);
    if (!u || !MODEL_EXT_RE.test(u)) return null;
    return {
      kind: "model",
      url: u,
      viaPage: false,
      title: mediaTitle(el),
      filename: fileNameFromUrl(u),
    };
  }

  registerDetector({ kind: "image", selector: "img", overlay: false, extract: extractImage });
  registerDetector({ kind: "model", selector: "model-viewer", overlay: false, extract: extractModelViewer });
  registerDetector({ kind: "model", selector: "a[href]", overlay: false, extract: extractModelLink });

  // ---- Document detection (round 6) ----
  //
  // Links ending in .pdf, reported as kind "document" with the
  // filename from the URL. No overlay button on documents (same
  // reasoning as images: a button on every link would spam the
  // page); documents flow through reportMedia into the popup list
  // and the Media Detector panel, where the user picks them.
  const DOCUMENT_EXT_RE = /\.pdf([?#]|$)/i;

  function extractDocumentLink(el) {
    if (!isVisible(el)) return null;
    const raw = el.getAttribute && el.getAttribute("href");
    const u = raw && resolveUrl(raw);
    if (!u || !DOCUMENT_EXT_RE.test(u)) return null;
    return {
      kind: "document",
      url: u,
      viaPage: false,
      title: mediaTitle(el),
      filename: fileNameFromUrl(u),
    };
  }

  registerDetector({ kind: "document", selector: "a[href]", overlay: false, extract: extractDocumentLink });

  // Full media list for the popup and for reporting, deduplicated by URL.
  // querySelectorAll that also looks inside OPEN shadow roots
  // (round 9): players built with web components render their
  // <video> inside a shadow tree, where document.querySelectorAll
  // never sees it. Closed roots are unreachable by design and
  // stay invisible. Each root is visited once; the light DOM is
  // queried first, then each open shadow root found.
  function querySelectorAllDeep(selector) {
    const out = [];
    const roots = [document];
    const seenRoots = new Set(roots);
    while (roots.length) {
      const root = roots.pop();
      let found;
      try { found = root.querySelectorAll(selector); } catch (_) { found = []; }
      for (const el of found) out.push(el);
      let all;
      try { all = root.querySelectorAll("*"); } catch (_) { all = []; }
      for (const el of all) {
        let sr = null;
        try { sr = el.shadowRoot; } catch (_) { sr = null; }
        if (sr && !seenRoots.has(sr)) { seenRoots.add(sr); roots.push(sr); }
      }
    }
    return out;
  }

  function collectMedia() {
    const items = [];
    const seen = new Set();
    for (const detector of detectors) {
      const els = querySelectorAllDeep(detector.selector);
      for (const el of els) {
        const item = detector.extract(el);
        if (!item) continue;
        const dedupKey = item.kind + "|" + item.url;
        if (seen.has(dedupKey)) continue;
        seen.add(dedupKey);
        items.push(item);
      }
    }
    return items;
  }

  // ---- Detected-media reporting (Phase 4 data flow) ----
  // Report the detected set to the background only when it changes, so
  // the app's Media Detector panel (Phase 9) stays current without
  // spamming the bridge. Fire-and-forget: reporting must never break
  // detection or downloading if the background/bridge is unavailable.
  let lastReportedKey = "";
  function reportMedia(items) {
    const key = JSON.stringify(items);
    if (key === lastReportedKey) return;
    lastReportedKey = key;
    sendRuntimeMessage({ type: "reportMedia", items: items }).catch(function () { /* background unreachable */ });
  }

  // ---- Popunder shield (streaming sites) ----
  //
  // Some streaming pages register capture-phase click/mousedown
  // handlers on window/document that open a popunder tab on ANY
  // click — including a click on our floating button. The button's
  // own bubble-phase listener runs too late: the popunder has
  // already fired by the time it stops propagation. So a
  // capture-phase interceptor is registered on window FIRST (this
  // script runs at document_start, before page scripts), and it
  // swallows the event before the page ever sees it. It only
  // touches events whose target path includes our own floating
  // button; every other click passes through untouched.
  // The button's own click listener stays as a fallback (it never
  // double-fires: the interceptor stops propagation, and a disabled
  // button does not fire click events at all).
  // Only the top frame has a button (see below), so in subframes
  // the shield is inert — but it is registered everywhere, because
  // the top frame is the one that needs it.
  // The shield also covers the quality popup (round 6): clicks
  // inside the popup are swallowed before the page's capture-phase
  // handlers see them, exactly like clicks on the button — and the
  // popup's own action is dispatched directly from here, because a
  // swallowed event never reaches the popup's bubble listeners
  // (same pattern as the button above).
  function overlayButtonFromEvent(e) {
    if (!floatOverlay) return null;
    const path = typeof e.composedPath === "function" ? e.composedPath() : [];
    if (path.indexOf(floatOverlay.button) !== -1) {
      return { button: floatOverlay.button };
    }
    if (floatOverlay.popup && path.indexOf(floatOverlay.popup) !== -1) {
      return { popup: true, path: path };
    }
    return null;
  }
  function onOverlayEventCapture(e) {
    const hit = overlayButtonFromEvent(e);
    if (!hit) return; // not our button/popup: leave the page alone
    if (hit.popup && floatOverlay.filenameInput && hit.path.indexOf(floatOverlay.filenameInput) !== -1) {
      // The filename field: a swallowed mousedown never focuses the
      // input (preventDefault blocks focus), so focus it directly
      // and select the whole name — one click, type a new name.
      // The event is still swallowed below, so the page's popunder
      // handlers never see the click. Typing needs no shield
      // handling: key events never reach those handlers.
      if (e.type === "mousedown" || e.type === "pointerdown") {
        try { floatOverlay.filenameInput.focus(); floatOverlay.filenameInput.select(); } catch (_) {}
      }
    }
    if (e.type === "click") {
      if (hit.button && !hit.button.disabled) {
        onOverlayClick(hit.button);
      } else if (hit.popup) {
        // Dispatch the clicked row's (or the X button's) action
        // directly: the swallowed event never reaches the popup's
        // own bubble listener. The first path element carrying a
        // popup action wins (rows and the X button carry one).
        for (const el of hit.path) {
          if (el && el.dataset && (el.dataset.rowIdx !== undefined || el.dataset.ffClose !== undefined)) {
            handlePopupClick(el);
            break;
          }
        }
      }
    }
    e.stopImmediatePropagation();
    e.stopPropagation();
    e.preventDefault();
  }
  window.addEventListener("click", onOverlayEventCapture, true);
  window.addEventListener("mousedown", onOverlayEventCapture, true);
  window.addEventListener("pointerdown", onOverlayEventCapture, true);
  window.addEventListener("mouseup", onOverlayEventCapture, true);
  window.addEventListener("pointerup", onOverlayEventCapture, true);

  // Clicking anywhere outside the popup (or the button) dismisses
  // the popup. Bubble phase, so it never sees button/popup clicks:
  // the capture-phase shield above swallows those first. A click
  // outside is the page's own business (its popunder handlers may
  // fire, as for any click); the popup still closes.
  window.addEventListener("click", function (e) {
    if (!popupOpen || !floatOverlay) return;
    const path = typeof e.composedPath === "function" ? e.composedPath() : [];
    if (path.indexOf(floatOverlay.button) !== -1) return;
    if (floatOverlay.popup && path.indexOf(floatOverlay.popup) !== -1) return;
    closeQualityPopup();
  });
  window.addEventListener("keydown", function (e) {
    if (e.key === "Escape" && popupOpen) closeQualityPopup();
  });

  // ---- Floating download button (viewport-anchored, Top Layer) ----
  //
  // Round 4 redesign. The button used to sit ON the video: its
  // host was positioned at the video's top-right corner and the
  // button was shifted left by its own width with
  // translateX(-100%), so the button painted entirely OUTSIDE the
  // host's box. That broke when the host became a popover in round
  // 3: Chrome's UA stylesheet sets overflow:auto on [popover], and
  // the round-3 inline styles never overrode it, so the host
  // became a scroll container that clips everything outside its
  // padding box — the translated button was clipped away entirely.
  // Verified 2026-11-22 in headless Chromium against the exact
  // 0.2.2 code: the button's rect looked correct and the popover
  // was open, but 0/50 hit-test points landed on it (elementFromPoint
  // hit the video instead); with overflow:visible added, or with
  // the plain pre-popover host, 50/50. The live symptom was a
  // 1-2px green sliver at the video's right edge (the button's edge
  // bleeding through at the clip boundary) or nothing at all.
  //
  // The button is therefore no longer positioned on the video at
  // all. It is a single floating button fixed to the bottom-right
  // corner of the VIEWPORT (right/bottom anchoring, no transform,
  // no video rect, no scroll tracking), still in the Top Layer via
  // a manual popover so no page overlay can cover it, with
  // overflow:visible so nothing about the host can clip it. It
  // stays visible and clickable regardless of player state, scroll,
  // or overlays; in fullscreen it is embedded inside the fullscreen
  // element (see the fullscreen section below), because a popover
  // over a fullscreen element is not reliably clickable.
  //
  // Round 5: the button lives ONLY in the top frame's document.
  // The top layer is per-document, so a button in a player iframe
  // sits UNDER the parent page's overlays, which cover the iframe
  // area — on nime the site's overlay stole every click even
  // though the button was visible. The top frame's top layer sits
  // above everything on the page, including those overlays. The
  // button downloads the stream the background elected (see the
  // election section): the URL comes from whichever frame holds
  // the video, passed via the background in the verdict. Subframes
  // never create a button; they only report candidates.
  // Exactly one button per tab, always in the top frame.

  // True only in the top frame: only it ever shows the button.
  const isTopFrame = window === window.top;

  // The single floating button in this frame, created lazily on
  // first show: { host, button, popover }. Null until created.
  // Only ever created in the top frame.
  let floatOverlay = null;
  // The download the button currently sends, from the latest
  // election verdict: { url, filename?, audioUrl?, referer? }, or
  // null when no frame has a downloadable stream. Set only by the
  // verdict handler; refreshed as frames re-report (every scan
  // change, and every 10 s).
  let currentDownload = null;
  // True while the button host is embedded inside the fullscreen
  // element (see the fullscreen section below): in that state the
  // host is a plain element, not a popover, so show/hide use
  // display instead of showPopover/hidePopover.
  let embeddedInFullscreen = false;

  // Short button-sized reason from the engine's friendly error
  // string (the full reason goes in the button's tooltip).
  function shortFailureReason(error) {
    const e = String(error || "").toLowerCase();
    if (e.indexOf("not a supported video") !== -1) return "unsupported page";
    if (e.indexOf("drm") !== -1) return "DRM-protected";
    if (e.indexOf("private") !== -1 || e.indexOf("sign-in") !== -1 || e.indexOf("sign in") !== -1) return "needs sign-in";
    if (e.indexOf("unavailable") !== -1) return "unavailable";
    if (e.indexOf("403") !== -1 || e.indexOf("access denied") !== -1) return "access denied";
    if (e.indexOf("404") !== -1 || e.indexOf("not found") !== -1) return "not found";
    if (e.indexOf("timed out") !== -1 || e.indexOf("too long") !== -1) return "timed out";
    if (e.indexOf("could not reach") !== -1) return "unreachable";
    if (e.indexOf("not reach the fetchforge app") !== -1) return "app not running";
    return "see app";
  }

  // Send one download to the background and drive the button's
  // Sending…/Sent/Already downloaded/Failed states (the states
  // themselves are unchanged from round 5). opts overrides the
  // verdict's fields for a popup choice: { url?, audioUrl?,
  // formatId?, manifestText? }. `"key" in opts` distinguishes an
  // EXPLICIT undefined (a variant pick has no audio sibling and no
  // captured text of its own) from an absent key (fall back to the
  // verdict's value) — passing the master's text for a variant URL
  // would hand the engine the wrong playlist.
  //
  // Filename from the page title (round 6): the movie name beats
  // the manifest stem ("index_287.ts"). The popup always offers a
  // rename field (see openQualityPopup); these helpers compute its
  // default and the one-click fallback name. The name is the STEM
  // only — the engine appends the extension for the chosen format
  // (.mp4 video, .m4a audio-only, .ts muxed HLS) and re-sanitises
  // on receipt, so the JS sanitise below is for display: what the
  // user sees in the field is what gets saved.
  function sanitizeFilenameStem(name) {
    // Mirror of the engine's sanitize_filename
    // (crates/engine/src/filename.rs): Windows forbids
    // < > : " / \ | ? * and control characters; trailing dots and
    // spaces are silently mangled by Windows Explorer.
    let out = String(name || "").replace(/[<>:\"/\\|?*\u0000-\u001f]/g, "_");
    out = out.replace(/[. ]+$/, "").trim();
    if (out.length > 100) out = out.slice(0, 100).replace(/[. ]+$/, "").trim();
    return out;
  }
  function pageTitleStem() {
    let title = (document.title || "").trim();
    if (!title) return "";
    // A spam notification overlay can rewrite the tab title to
    // "(1) New Message!" — strip a leading "(N)" badge (1–3
    // digits, looped for stacked badges) so the file is not named
    // after the spam (2026-11-23: a nime download was literally
    // saved as "(1) New Message!.mp4"). A 4-digit parenthesised
    // year like "(2026) Dune" is NOT a badge and survives.
    while (/^\(\d{1,3}\)\s*/.test(title)) title = title.replace(/^\(\d{1,3}\)\s*/, "").trim();
    if (!title) return "";
    // Streaming sites wrap the title: "Watch The Odyssey (2026) |
    // NovaHD", or "Watching The Odyssey — Atlantic" (the
    // "Watching " prefix became a filename on 2026-11-23). Strip
    // a leading "Watching "/"Watch " and a trailing " | <site>"
    // suffix — all guarded so a real title is never emptied. A
    // " - " suffix is NOT stripped: real titles contain hyphens
    // ("Spider-Man - Homecoming"). Whatever survives, the user can
    // still edit it in the popup's filename field.
    title = title.replace(/^watching\s+/i, "").trim();
    title = title.replace(/^watch\s+/i, "").trim();
    if (!title) return "";
    title = title.replace(/\s*\|\s*[^|]*$/, "").trim();
    return sanitizeFilenameStem(title);
  }
  function stemOfFilename(filename) {
    // The verdict filename ("index_287.ts", "movie.mp4") without
    // its extension — the fallback name when no page title or
    // server title is available.
    const base = String(filename || "").split(/[/\\]/).pop() || "";
    const dot = base.lastIndexOf(".");
    const stem = dot > 0 ? base.slice(0, dot) : base;
    return sanitizeFilenameStem(stem);
  }
  function popupDefaultName(dl, serverTitle) {
    // Best name known for the popup's filename field: the server's
    // title (the formats response carries yt-dlp's clean title),
    // then the page title, then the verdict filename's stem.
    if (serverTitle) { const s = sanitizeFilenameStem(serverTitle); if (s) return s; }
    const t = pageTitleStem();
    if (t) return t;
    const f = stemOfFilename(dl && dl.filename);
    if (f) return f;
    return "download";
  }
  // NOTE: do NOT use isPlaylistUrl here — that helper lives inside
  // hook-main.js (the MAIN-world hook) and is invisible to this
  // scope; calling it threw a ReferenceError that silently killed
  // the one-click send (caught by the popup harness, 2026-11-22).
  // Round 9: same anywhere-match as STREAM_EXT_RE — a wrapped
  // playlist/file URL is still a manifest/file (the popup must
  // open for it, not one-click send).
  function isManifestUrl(url) { return /\.(m3u8|mpd)/i.test(String(url || "")); }
  function isDashUrl(url) { return /\.mpd/i.test(String(url || "")); }
  function isMp4FamilyUrl(url) { return /\.(mp4|mov|m4v)/i.test(String(url || "")); }
  // Any direct video file (round 8): every download click on one
  // shows the popup — no silent one-click sends for video.
  function isVideoFileUrl(url) { return /\.(mp4|mov|m4v|mkv|webm|avi|ts|m2ts)/i.test(String(url || "")); }

  // ---- Direct-file clip guard (round 7, Fix 6) ----
  //
  // The verdict can still be a short ad clip when the movie's
  // playlist is invisible (novahd 2026-11-23: a 15-second
  // /assets/p/*.mp4 clip was sent as "the movie" three times).
  // Before a direct-file download on a long-form watch page,
  // probe the file's duration from its moov/mvhd boxes; a
  // known-short clip is refused honestly on the button. Any probe
  // failure fails OPEN (the popup opens) — the button must
  // never break a legitimate download. Same-origin only in
  // practice: a cross-origin fetch without CORS fails and
  // returns null (fail open). No host permissions are added.
  function parseMp4Duration(bytes) {
    // Minimal moov/mvhd walk, mirroring the engine's
    // mp4_duration_secs (crates/engine/src/extract.rs): top-level
    // boxes to moov, then mvhd. Returns seconds, or null when
    // unknown (moov at end of file, truncated boxes, non-mp4).
    // Null is "unknown", never "short".
    try {
      if (!bytes || bytes.length < 8) return null;
      const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
      const boxType = function (off) {
        return String.fromCharCode(bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7]);
      };
      let off = 0;
      while (off + 8 <= bytes.length) {
        const size = dv.getUint32(off);
        if (size < 8) return null;
        if (boxType(off) === "moov") {
          const end = Math.min(off + size, bytes.length);
          let inner = off + 8;
          while (inner + 8 <= end) {
            const isize = dv.getUint32(inner);
            if (isize < 8) return null;
            if (boxType(inner) === "mvhd") {
              const payload = inner + 8;
              const mvhdEnd = Math.min(inner + isize, end);
              const version = bytes[payload];
              if (version === 0) {
                if (payload + 20 > mvhdEnd) return null;
                const timescale = dv.getUint32(payload + 12);
                const duration = dv.getUint32(payload + 16);
                if (!timescale) return null;
                return duration / timescale;
              }
              if (version === 1) {
                if (payload + 32 > mvhdEnd) return null;
                const timescale = dv.getUint32(payload + 20);
                const duration = Number(dv.getBigUint64(payload + 24));
                if (!timescale) return null;
                return duration / timescale;
              }
              return null;
            }
            inner += isize;
          }
          return null;
        }
        off += size;
      }
      return null;
    } catch (_) { return null; }
  }
  async function probeDirectFileDuration(url) {
    // Range-probe the first 64 KB (same window as the engine's
    // HTML-fallback probe) with a 5 s timeout.
    const ctrl = new AbortController();
    const timer = setTimeout(function () { try { ctrl.abort(); } catch (_) {} }, 5000);
    try {
      const r = await fetch(url, { headers: { "Range": "bytes=0-65535" }, signal: ctrl.signal });
      if (!r.ok && r.status !== 206) return null;
      const buf = new Uint8Array(await r.arrayBuffer());
      return parseMp4Duration(buf);
    } catch (_) { return null; }
    finally { clearTimeout(timer); }
  }
  function defaultFilenameFor(dl) {
    // One-click fallback name (no popup). Page URLs (YouTube etc.)
    // keep the verdict's filename — usually none, so the app names
    // the file from the extractor's title, which is cleaner than
    // the page title ("Video Title - YouTube"). Manifest streams
    // prefer the page title over the manifest stem. Direct files
    // keep their real filename.
    if (!dl) return undefined;
    if (dl.viaPage) return dl.filename;
    if (isManifestUrl(dl.url)) return pageTitleStem() || dl.filename;
    return dl.filename;
  }
  function currentPopupFilename() {
    const input = floatOverlay && floatOverlay.filenameInput;
    const v = input ? sanitizeFilenameStem(input.value) : "";
    return v || "download";
  }
  function sendCurrentDownload(btn, dl, opts) {
    opts = opts || {};
    const url = opts.url || dl.url;
    const audioUrl = ("audioUrl" in opts) ? opts.audioUrl : dl.audioUrl;
    const manifestText = ("manifestText" in opts) ? opts.manifestText : dl.manifestText;
    const filename = ("filename" in opts && opts.filename) ? opts.filename : defaultFilenameFor(dl);
    // The popup's "Audio only" choice (round 8): the app delivers
    // an audio file instead of the video. Undefined for a video
    // send (the background drops it from the request).
    const audioOnly = !!opts.audioOnly;
    btn.disabled = true;
    btn.textContent = "Sending…";
    btn.title = "";
    sendRuntimeMessage({ type: "downloadMedia", url: url, filename: filename, audioUrl: audioUrl, referer: dl.referer, formatId: opts.formatId, manifestText: manifestText, audioOnly: audioOnly }).then(function (resp) {
      if (resp && resp.duplicate) {
        // Nothing new was queued: the file is already in the
        // app's downloads. Say so honestly — "Sent" would be a
        // lie (novahd 2026-11-22: a re-sent clip showed success
        // while no download was created at all).
        btn.textContent = "Already downloaded";
        btn.title = "This file is already in your downloads.";
      } else if (resp && resp.viaBrowser) {
        // The app could not be reached at all, so the browser
        // downloaded the file itself — say so honestly instead of
        // claiming the app got it (2026-11-23: files landed in
        // Chrome's Downloads folder while the button said "Sent
        // to FetchForge").
        btn.textContent = "Downloaded by browser";
        btn.title = "The FetchForge app could not be reached, so your browser downloaded this file instead.";
      } else if (resp && resp.ok) {
        btn.textContent = "Sent to FetchForge";
      } else {
        // Show WHY it failed on the button (and the full reason as
        // a tooltip) instead of silently reverting: a failed send
        // used to look identical to a successful one.
        btn.textContent = "Failed — " + shortFailureReason(resp && resp.error);
        btn.title = (resp && resp.error) || "Download failed";
      }
      window.setTimeout(function () {
        btn.disabled = false;
        btn.textContent = "↓ FetchForge";
        btn.title = "";
      }, 5000);
    }).catch(function () {
      btn.textContent = "Failed — app not running";
      btn.title = "Could not reach the FetchForge app";
      window.setTimeout(function () {
        btn.disabled = false;
        btn.textContent = "↓ FetchForge";
        btn.title = "";
      }, 5000);
    });
  }

  function onOverlayClick(btn) {
    // The download source comes from the election verdict (the
    // stream may live in a subframe this document cannot see).
    // No verdict yet, or no stream anywhere: nothing to send.
    if (!currentDownload || !currentDownload.url) return;
    // The button toggles the popup: a click while it is open
    // closes it instead of sending.
    if (popupOpen) { closeQualityPopup(); return; }
    const dl = currentDownload;
    // HLS master with choices: parse the captured master text
    // (from the verdict) into variant rows and let the user pick.
    // Round 8: a master with even ONE variant opens the popup —
    // the audio choice alone is worth showing, and no video click
    // sends silently any more.
    const masterText = dl.masterText || dl.manifestText;
    if (masterText) {
      const parsed = parseHlsMaster(masterText, dl.url);
      if (parsed && parsed.rows.length >= 1) { openHlsPopup(btn, dl, parsed); return; }
    }
    // Page URL (viaPage, e.g. YouTube): ask the app for the
    // page's formats and let the user pick a quality.
    if (dl.viaPage) { openFormatsPopup(btn, dl); return; }
    // DASH manifest (nime's VidLink index_web.mpd): ask the app
    // for the manifest's formats and let the user pick a quality,
    // like a page URL. The bridge fetches the .mpd itself; if it
    // cannot, the popup still shows with the default quality.
    if (isDashUrl(dl.url)) { openFormatsPopup(btn, dl); return; }
    // Direct mp4-family files on a long-form watch page (a video
    // over 10 minutes is playing somewhere on the page): probe
    // the file's duration first — a known-short clip is refused
    // honestly instead of being sent as the movie (round-7 clip
    // guard). The refusal shows INSTEAD of the popup, never
    // behind it; a probe failure fails OPEN to the popup.
    if (!isManifestUrl(dl.url) && isMp4FamilyUrl(dl.url) && (dl.pageVideoDuration || 0) > 600) {
      btn.disabled = true;
      btn.textContent = "Checking…";
      btn.title = "";
      probeDirectFileDuration(dl.url).then(function (secs) {
        if (secs != null && secs < 60) {
          btn.textContent = "Only a short clip was found";
          btn.title = "The file behind this button is a short clip (" + Math.round(secs) + "s), not the full video.";
          window.setTimeout(function () {
            btn.disabled = false;
            btn.textContent = "↓ FetchForge";
            btn.title = "";
          }, 5000);
        } else {
          // The probe disabled the button: restore it before the
          // popup opens, or the button sits disabled on
          // "Checking…" while the user picks a quality.
          btn.disabled = false;
          btn.textContent = "↓ FetchForge";
          btn.title = "";
          openSimplePopup(btn, dl);
        }
      }).catch(function () {
        btn.disabled = false;
        btn.textContent = "↓ FetchForge";
        btn.title = "";
        openSimplePopup(btn, dl);
      });
      return;
    }
    // Single-variant HLS (a media playlist, no master text) and
    // direct video files: the popup still appears (round 8 — no
    // silent one-click sends for video) with the one known quality
    // plus the audio choice.
    if (isManifestUrl(dl.url) || isVideoFileUrl(dl.url)) { openSimplePopup(btn, dl); return; }
    // Images, 3D models, PDFs and documents keep the one-click
    // send: there is no quality or audio choice to make.
    sendCurrentDownload(btn, dl, {});
  }

  // ---- HLS master parsing (round 6, study C2/C3) ----
  //
  // Parses captured master-playlist text into variant rows reading
  // like `1080p · 5.2 Mbps · H.264` (resolution · bandwidth ·
  // friendly codec name), sorted by HEIGHT descending, then
  // bandwidth (round 8: the quality ladder is highest-first —
  // 1080 or higher, then 720, then 480, then lower), plus
  // audio-only rows from EXT-X-MEDIA audio groups. The top row is
  // the highest variant, labelled "Best" and pre-selected.
  // Returns { rows: [{ label, url, audioUrl?, audioOnly?, selected }], hasAudioRows }
  // or null when the text is not a master playlist (no
  // #EXT-X-STREAM-INF). Relative variant URIs resolve against the
  // master URL. hasAudioRows says whether any audio-only row was
  // added: when false the audio is muxed into the variants, and
  // the popup offers an "Audio only" row for the best variant.
  function parseHlsAttrs(s) {
    const out = {};
    const re = /([A-Z0-9-]+)=("[^"]*"|[^,]*)/g;
    let m;
    while ((m = re.exec(s))) {
      let v = m[2];
      if (v.length >= 2 && v.charAt(0) === '"' && v.charAt(v.length - 1) === '"') v = v.slice(1, -1);
      out[m[1]] = v;
    }
    return out;
  }
  function resolveAgainst(raw, base) {
    try { return new URL(String(raw).trim(), base).href; } catch (_) { return null; }
  }
  function friendlyCodec(codecs) {
    const first = String(codecs || "").split(",")[0].trim().toLowerCase();
    if (first.indexOf("avc1") === 0 || first.indexOf("avc3") === 0) return "H.264";
    if (first.indexOf("hev1") === 0 || first.indexOf("hvc1") === 0) return "H.265";
    if (first.indexOf("av01") === 0) return "AV1";
    if (first.indexOf("vp09") === 0 || first.indexOf("vp9") === 0) return "VP9";
    if (first.indexOf("mp4a") === 0) return "AAC";
    if (first.indexOf("opus") === 0) return "Opus";
    if (first.indexOf("ec-3") === 0) return "Dolby Digital+";
    if (first.indexOf("ac-3") === 0) return "Dolby Digital";
    return "";
  }
  function formatBandwidth(bw) {
    const n = Number(bw);
    if (!isFinite(n) || n <= 0) return "";
    if (n >= 1000000) return (n / 1000000).toFixed(1).replace(/\.0$/, "") + " Mbps";
    return Math.round(n / 1000) + " kbps";
  }
  function hlsVariantHeight(attrs) {
    const m = /^(\d+)x(\d+)$/.exec(String(attrs.RESOLUTION || ""));
    return m ? parseInt(m[2], 10) : 0;
  }
  function parseHlsMaster(text, masterUrl) {
    const lines = String(text || "").split(/\r?\n/);
    const audioGroups = {}; // GROUP-ID -> { uri, name }
    const variants = [];
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i].trim();
      if (line.indexOf("#EXT-X-MEDIA:") === 0) {
        const attrs = parseHlsAttrs(line.slice("#EXT-X-MEDIA:".length));
        if ((attrs.TYPE || "").toUpperCase() === "AUDIO" && attrs["GROUP-ID"] && !audioGroups[attrs["GROUP-ID"]]) {
          audioGroups[attrs["GROUP-ID"]] = {
            uri: attrs.URI ? resolveAgainst(attrs.URI, masterUrl) : null,
            name: attrs.NAME || attrs.LANGUAGE || "",
          };
        }
      } else if (line.indexOf("#EXT-X-STREAM-INF:") === 0) {
        const attrs = parseHlsAttrs(line.slice("#EXT-X-STREAM-INF:".length));
        // The variant URI is the next non-empty, non-comment line.
        let uri = null;
        for (let j = i + 1; j < lines.length; j++) {
          const l = lines[j].trim();
          if (!l) continue;
          if (l.charAt(0) === "#") break;
          uri = l; i = j; break;
        }
        if (!uri) continue;
        const url = resolveAgainst(uri, masterUrl);
        if (url) variants.push({ attrs: attrs, url: url });
      }
    }
    if (!variants.length) return null; // not a master playlist
    // Quality ladder, highest first: height, then bandwidth.
    // (A 720p variant at a fat bitrate must never outrank 1080p.)
    variants.sort(function (a, b) {
      const ah = hlsVariantHeight(a.attrs), bh = hlsVariantHeight(b.attrs);
      if (bh !== ah) return bh - ah;
      const ab = Number(a.attrs.BANDWIDTH || a.attrs["AVERAGE-BANDWIDTH"] || 0);
      const bb = Number(b.attrs.BANDWIDTH || b.attrs["AVERAGE-BANDWIDTH"] || 0);
      return bb - ab;
    });
    const rows = [];
    const seenUrls = new Set();
    variants.forEach(function (v, idx) {
      if (seenUrls.has(v.url)) return; // duplicate variant URI
      seenUrls.add(v.url);
      const parts = [];
      const h = hlsVariantHeight(v.attrs);
      if (h) parts.push(h + "p");
      const bw = formatBandwidth(v.attrs.BANDWIDTH || v.attrs["AVERAGE-BANDWIDTH"]);
      if (bw) parts.push(bw);
      const codec = friendlyCodec(v.attrs.CODECS);
      if (codec) parts.push(codec);
      let label = parts.join(" · ") || v.url.split("/").pop();
      if (idx === 0) label = "Best · " + label;
      // A variant with a separate audio group needs its audio
      // sibling URL so the engine merges them; without an AUDIO
      // group the audio is muxed into the variant itself. The
      // master text is authoritative here — the timing-log sibling
      // (dl.audioUrl) belongs to the download URL, not necessarily
      // to this variant.
      let audioUrl;
      const groupId = v.attrs.AUDIO;
      if (groupId && audioGroups[groupId] && audioGroups[groupId].uri) audioUrl = audioGroups[groupId].uri;
      rows.push({ label: label, url: v.url, audioUrl: audioUrl, selected: idx === 0 });
    });
    // Audio-only rows, one per audio group with a downloadable URI.
    // A group without a URI has its audio embedded in the variants
    // and offers nothing to download separately. audioOnly marks
    // the row so the popup sends it with the audio-only flag —
    // without it the audio playlist would download as a video file.
    let hasAudioRows = false;
    Object.keys(audioGroups).forEach(function (gid) {
      const g = audioGroups[gid];
      if (!g.uri || seenUrls.has(g.uri)) return;
      seenUrls.add(g.uri);
      hasAudioRows = true;
      rows.push({ label: "Audio only" + (g.name ? " · " + g.name : ""), url: g.uri, audioUrl: undefined, audioOnly: true, selected: false });
    });
    return { rows: rows, hasAudioRows: hasAudioRows };
  }

  // ---- IDM-style quality popup (round 6, study C1–C3) ----
  //
  // Lives in the same top-layer popover host as the floating button
  // (same shadow DOM), positioned above the button, so it inherits
  // the button's top-layer placement and fullscreen embedding: it
  // moves with the host and is never covered by page overlays.
  // Clicking a row sends that choice immediately (the pre-selected
  // row is only a highlighted default, not a separate confirm
  // step). Dismissed via the X button, a click outside it, Escape,
  // a click on the floating button, or a verdict change.
  let popupOpen = false;
  let popupRows = []; // [{ pick }] for the open popup, indexed by data-row-idx
  let popupRequestId = 0; // invalidates a pending getFormats on close
  let popupFilenameDirty = false; // true once the user edits the filename field
  const FORMATS_TIMEOUT_MS = 15000;
  function handlePopupClick(el) {
    if (!el || !el.dataset) return;
    if (el.dataset.ffClose !== undefined) { closeQualityPopup(); return; }
    if (el.dataset.rowIdx !== undefined) {
      const row = popupRows[Number(el.dataset.rowIdx)];
      if (row) row.pick();
    }
  }
  function closeQualityPopup() {
    popupRequestId++; // a pending formats fetch must not send after a close
    popupRows = [];
    popupOpen = false;
    if (floatOverlay && floatOverlay.popup) floatOverlay.popup.style.display = "none";
  }
  function openQualityPopup(titleText, subtitleText, defaultName) {
    if (!floatOverlay) return false;
    floatOverlay.popupTitle.textContent = titleText;
    if (subtitleText) {
      floatOverlay.popupSubtitle.textContent = subtitleText;
      floatOverlay.popupSubtitle.style.display = "block";
    } else {
      floatOverlay.popupSubtitle.style.display = "none";
    }
    // The filename field is part of the popup chrome (not the body,
    // so a rows refresh never wipes it): always present, pre-filled
    // with the best name known. The formats popup upgrades it to
    // the server's clean title once the response arrives, unless
    // the user already typed their own name (dirty flag).
    popupFilenameDirty = false;
    if (floatOverlay.filenameInput) floatOverlay.filenameInput.value = defaultName || "download";
    floatOverlay.popupBody.textContent = "";
    popupRows = [];
    floatOverlay.popup.style.display = "flex";
    popupOpen = true;
    return true;
  }
  function addQualityRow(label, selected, pick) {
    const idx = popupRows.length;
    popupRows.push({ pick: pick });
    const row = document.createElement("button");
    row.type = "button";
    row.dataset.rowIdx = String(idx);
    row.textContent = label;
    row.title = label;
    row.style.cssText = "display:block;width:100%;text-align:left;background:" + (selected ? "rgba(34,197,94,.25)" : "transparent")
      + ";color:#f9fafb;border:0;border-radius:6px;padding:8px 10px;cursor:pointer;"
      + "font:600 13px system-ui,sans-serif;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;";
    if (!selected) row.style.fontWeight = "400";
    row.addEventListener("mouseenter", function () { row.style.background = selected ? "rgba(34,197,94,.35)" : "rgba(255,255,255,.08)"; });
    row.addEventListener("mouseleave", function () { row.style.background = selected ? "rgba(34,197,94,.25)" : "transparent"; });
    floatOverlay.popupBody.appendChild(row);
    return row;
  }
  // Captured text for a URL in THIS frame, if the hook saw it.
  // A variant URL from the master usually has no capture in the top
  // frame (the player fetched it in a subframe); undefined then,
  // and the engine fetches it normally from the fresh master URL.
  function capturedTextFor(url) {
    const cap = capturedManifests.get(url);
    return cap ? cap.text : undefined;
  }
  function openHlsPopup(btn, dl, parsed) {
    if (!openQualityPopup("Choose quality", null, popupDefaultName(dl, null))) return;
    parsed.rows.forEach(function (r) {
      addQualityRow(r.label, !!r.selected, function () {
        const name = currentPopupFilename();
        closeQualityPopup();
        sendCurrentDownload(btn, dl, { url: r.url, audioUrl: r.audioUrl, manifestText: capturedTextFor(r.url), filename: name, audioOnly: !!r.audioOnly });
      });
    });
    // Muxed master (no separate audio track to pick): offer the
    // audio of the best variant — the engine extracts it after
    // the download (round 8).
    if (!parsed.hasAudioRows && parsed.rows.length) {
      const best = parsed.rows[0];
      addQualityRow("Audio only", false, function () {
        const name = currentPopupFilename();
        closeQualityPopup();
        sendCurrentDownload(btn, dl, { url: best.url, audioUrl: undefined, manifestText: capturedTextFor(best.url), filename: name, audioOnly: true });
      });
    }
  }
  // The popup for downloads with exactly one known quality (round
  // 8): a single-variant HLS media playlist or a direct video
  // file. No silent one-click send for video — the user always
  // sees the video choice and the audio choice.
  function openSimplePopup(btn, dl) {
    if (!openQualityPopup("Choose quality", null, popupDefaultName(dl, null))) return;
    // The one known quality: the container when the URL names one.
    const m = /\.([a-z0-9]+)([?#]|$)/i.exec(dl.url || "");
    const container = m ? m[1].toUpperCase() : "";
    addQualityRow("Video" + (container ? " · " + container : ""), true, function () {
      const name = currentPopupFilename();
      closeQualityPopup();
      sendCurrentDownload(btn, dl, { filename: name });
    });
    // Audio only: a paired audio sibling downloads alone (the
    // captured text belongs to the video playlist, so it is NOT
    // sent along); otherwise the engine extracts the audio from
    // the video after downloading it.
    addQualityRow("Audio only", false, function () {
      const name = currentPopupFilename();
      closeQualityPopup();
      if (dl.audioUrl) {
        sendCurrentDownload(btn, dl, { url: dl.audioUrl, audioUrl: undefined, manifestText: undefined, filename: name, audioOnly: true });
      } else {
        sendCurrentDownload(btn, dl, { filename: name, audioOnly: true });
      }
    });
  }
  // Sort bridge formats into the popup ladder (round 8): video
  // highest-first (height, then bitrate), audio-only rows last.
  // The server already sorts best-first; this is defence in depth
  // (and the unit under test for the ladder order).
  function sortFormatsForPopup(formats) {
    const list = (formats || []).slice();
    function audioRank(f) { return f && f.kind === "audio_only" ? 1 : 0; }
    list.sort(function (a, b) {
      const ar = audioRank(a), br = audioRank(b);
      if (ar !== br) return ar - br;
      const ah = Number(a.height || 0), bh = Number(b.height || 0);
      if (ah !== bh) return bh - ah;
      const av = Number(a.vbr || 0), bv = Number(b.vbr || 0);
      return bv - av;
    });
    return list;
  }
  function openFormatsPopup(btn, dl) {
    if (!openQualityPopup("Choose quality", null, popupDefaultName(dl, null))) return;
    const loading = document.createElement("div");
    loading.textContent = "Loading qualities…";
    loading.style.cssText = "padding:10px;color:#9ca3af;font:400 13px system-ui,sans-serif;";
    floatOverlay.popupBody.appendChild(loading);
    const reqId = ++popupRequestId;
    let settled = false;
    // Round 8: NEVER a silent one-click send. When the formats
    // cannot be determined but the bridge IS reachable, the popup
    // still shows with the single known quality plus the audio
    // choice (DASH only — a page URL's audio needs the quality
    // list, yt-dlp has no usable audio path without a format id).
    // Only a total failure to reach the bridge shows an error
    // state in the popup. Every path checks reqId: if the popup
    // was closed meanwhile (X, outside click, verdict change),
    // nothing is shown or sent at all.
    function showDegradedPopup() {
      if (reqId !== popupRequestId) return;
      floatOverlay.popupTitle.textContent = "Choose quality";
      floatOverlay.popupSubtitle.textContent = "Quality info unavailable";
      floatOverlay.popupSubtitle.style.display = "block";
      floatOverlay.popupBody.textContent = "";
      popupRows = [];
      addQualityRow("Video · default quality", true, function () {
        const name = currentPopupFilename();
        closeQualityPopup();
        sendCurrentDownload(btn, dl, { filename: name });
      });
      if (isDashUrl(dl.url)) {
        addQualityRow("Audio only", false, function () {
          const name = currentPopupFilename();
          closeQualityPopup();
          sendCurrentDownload(btn, dl, { filename: name, audioOnly: true });
        });
      }
    }
    function showUnreachableError() {
      if (reqId !== popupRequestId) return;
      floatOverlay.popupTitle.textContent = "FetchForge app not reachable";
      floatOverlay.popupSubtitle.textContent = "Open the FetchForge app and check the extension is paired, then try again.";
      floatOverlay.popupSubtitle.style.display = "block";
      floatOverlay.popupBody.textContent = "";
      popupRows = [];
    }
    const timer = window.setTimeout(function () {
      if (settled) return;
      settled = true;
      // Timeout: the bridge is likely busy (yt-dlp extraction is
      // slow), not down — show the degraded popup, not an error.
      showDegradedPopup();
    }, FORMATS_TIMEOUT_MS);
    sendRuntimeMessage({ type: "getFormats", url: dl.url }).then(function (resp) {
      if (settled || reqId !== popupRequestId) return;
      settled = true;
      window.clearTimeout(timer);
      if (resp && resp.unreachable) { showUnreachableError(); return; }
      if (!resp || !resp.ok || !Array.isArray(resp.formats) || !resp.formats.length) {
        showDegradedPopup();
        return;
      }
      floatOverlay.popupTitle.textContent = "Choose quality";
      if (resp.title) {
        floatOverlay.popupSubtitle.textContent = resp.title;
        floatOverlay.popupSubtitle.style.display = "block";
        // Upgrade the filename field to the server's clean title
        // (yt-dlp's title beats the page title's " - YouTube"
        // suffix) — unless the user already typed their own name.
        if (!popupFilenameDirty && floatOverlay.filenameInput) {
          floatOverlay.filenameInput.value = sanitizeFilenameStem(resp.title) || floatOverlay.filenameInput.value;
        }
      }
      floatOverlay.popupBody.textContent = "";
      popupRows = [];
      let anySelected = false;
      // A single format ALSO shows the popup now (round 8): the
      // one row plus the filename field, no silent send.
      sortFormatsForPopup(resp.formats).forEach(function (f) {
        const selected = !anySelected && f.format_id === resp.default_format_id;
        if (selected) anySelected = true;
        addQualityRow(f.label || f.format_id, selected, function () {
          const name = currentPopupFilename();
          closeQualityPopup();
          // Pass captured manifest text along (a DASH .mpd the
          // hook captured from the player): signed links may fail
          // on a second fetch, so the engine uses this copy.
          // Page URLs carry no captured text — undefined there,
          // and the engine fetches normally.
          sendCurrentDownload(btn, dl, { formatId: f.format_id, audioUrl: undefined, manifestText: dl.manifestText || dl.masterText, filename: name });
        });
      });
      // No format matched default_format_id: highlight the first
      // row (the server lists its best first) instead of none.
      if (!anySelected && popupRows.length) {
        const first = floatOverlay.popupBody.querySelector("button");
        if (first) { first.style.background = "rgba(34,197,94,.25)"; first.style.fontWeight = "600"; }
      }
    }).catch(function () {
      if (settled || reqId !== popupRequestId) return;
      settled = true;
      window.clearTimeout(timer);
      // The runtime message itself failed (the background script
      // is unreachable): the bridge cannot be reached from here.
      showUnreachableError();
    });
  }

  // Show/hide the floating button's host. A popover host is shown
  // with showPopover() (top layer) and hidden with hidePopover();
  // a plain host — and an embedded host, which is plain while it
  // lives inside the fullscreen element — uses display. NEVER set
  // display:none on a popover host: an inline display beats the UA
  // :popover-open rule, so showPopover() would never display it.
  // showPopover() on an already-shown popover throws; the
  // try/catch makes show/hide idempotent.
  function showOverlayHost(o) {
    if (o.popover && !embeddedInFullscreen) {
      try { o.host.showPopover(); } catch (_) {}
    } else {
      o.host.style.display = "block";
    }
  }
  function hideOverlayHost(o) {
    if (o.popover && !embeddedInFullscreen) {
      try { o.host.hidePopover(); } catch (_) {}
    } else {
      o.host.style.display = "none";
    }
  }

  function createFloatingButton() {
    const host = document.createElement("div");
    // Top layer (popover API): z-index alone loses to a page
    // overlay in another stacking context, and to the fullscreen
    // element. A manual popover renders in the browser's top
    // layer, above ALL page content, so no page overlay can cover
    // the button. Falls back to the plain fixed host (z-index max)
    // where popover is unsupported.
    // The inline styles override the UA popover rules (which set
    // inset:0, margin:auto and overflow:auto): inset:auto plus
    // explicit right/bottom anchors the host to the viewport's
    // bottom-right corner; overflow:visible is REQUIRED — the UA
    // overflow:auto turns the host into a scroll container that
    // clips anything painted outside its box (that clipped the
    // 0.2.2 button entirely, see the section header); the
    // transparent background/border keep the host itself invisible.
    let isPopover = false;
    try {
      host.setAttribute("popover", "manual");
      isPopover = typeof host.showPopover === "function";
    } catch (_) { isPopover = false; }
    if (!isPopover) host.removeAttribute("popover");
    host.style.cssText = "position:fixed;inset:auto;right:16px;bottom:16px;z-index:2147483647;"
      + "pointer-events:none;overflow:visible;background:transparent;border:0;padding:0;margin:0;";
    const shadow = host.attachShadow({ mode: "open" });
    const btn = document.createElement("button");
    btn.textContent = "↓ FetchForge";
    // Shadow DOM isolates the button from page CSS. The button
    // paints at its layout position INSIDE the host's box: no
    // transform, so nothing about the host can clip it, and the
    // host (fit-content per the UA popover rules) hugs it. The
    // host is right/bottom anchored, so when the label grows
    // ("Sending…", "Sent to FetchForge") the button grows
    // leftward/upward and stays in the corner.
    btn.style.cssText = "pointer-events:auto;background:#22c55e;color:#fff;"
      + "border:0;border-radius:6px;padding:6px 10px;font:600 12px system-ui,sans-serif;"
      + "cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,.4);opacity:.92;white-space:nowrap;";
    btn.addEventListener("mouseenter", function () { btn.style.opacity = "1"; });
    btn.addEventListener("mouseleave", function () { btn.style.opacity = ".92"; });
    btn.addEventListener("click", function (e) {
      e.stopPropagation();
      e.preventDefault();
      onOverlayClick(btn);
    });
    shadow.appendChild(btn);
    // Quality popup (round 6): same shadow DOM, same top-layer
    // host as the button, positioned ABOVE the button (absolute,
    // bottom edge 8px above the host's top — the host's
    // overflow:visible lets it paint outside the host's box, and
    // the host hugs the button so the popup sits right above it).
    // It inherits the button's fullscreen embedding: when the host
    // moves inside the fullscreen element, the popup moves too.
    const popup = document.createElement("div");
    popup.style.cssText = "display:none;position:absolute;right:0;bottom:calc(100% + 8px);"
      + "width:300px;max-height:340px;background:#111827;color:#f9fafb;"
      + "border:1px solid rgba(255,255,255,.14);border-radius:10px;"
      + "box-shadow:0 8px 24px rgba(0,0,0,.5);font:400 13px system-ui,sans-serif;"
      + "pointer-events:auto;overflow:hidden;flex-direction:column;";
    const popupHeader = document.createElement("div");
    popupHeader.style.cssText = "display:flex;align-items:center;justify-content:space-between;"
      + "padding:10px 12px 8px;border-bottom:1px solid rgba(255,255,255,.1);flex:none;";
    const popupTitle = document.createElement("span");
    popupTitle.textContent = "Choose quality";
    popupTitle.style.cssText = "font-weight:600;";
    const popupClose = document.createElement("button");
    popupClose.type = "button";
    popupClose.dataset.ffClose = "1";
    popupClose.textContent = "×";
    popupClose.title = "Close";
    popupClose.style.cssText = "background:transparent;color:#9ca3af;border:0;cursor:pointer;"
      + "font:400 18px/1 system-ui,sans-serif;padding:0 2px;";
    popupHeader.appendChild(popupTitle);
    popupHeader.appendChild(popupClose);
    const popupSubtitle = document.createElement("div");
    popupSubtitle.style.cssText = "display:none;padding:8px 12px 0;font-size:12px;color:#9ca3af;"
      + "white-space:nowrap;overflow:hidden;text-overflow:ellipsis;flex:none;";
    // Filename field (round 6): part of the popup chrome, above the
    // rows, so a rows refresh (formats response) never wipes it. It
    // is ALWAYS shown — even when no title could be fetched, the
    // field is there with the best fallback name. The value is the
    // stem only; the app adds the extension for the chosen format.
    const popupNameRow = document.createElement("div");
    popupNameRow.style.cssText = "padding:8px 12px 0;flex:none;";
    const popupNameLabel = document.createElement("div");
    popupNameLabel.textContent = "Filename";
    popupNameLabel.style.cssText = "font-size:11px;color:#9ca3af;padding-bottom:4px;";
    const popupNameInput = document.createElement("input");
    popupNameInput.type = "text";
    popupNameInput.placeholder = "download";
    popupNameInput.dataset.ffFilename = "1";
    popupNameInput.style.cssText = "width:100%;box-sizing:border-box;background:#1f2937;color:#f9fafb;"
      + "border:1px solid rgba(255,255,255,.14);border-radius:6px;padding:7px 8px;"
      + "font:400 13px system-ui,sans-serif;outline:none;";
    popupNameInput.addEventListener("input", function () { popupFilenameDirty = true; });
    popupNameRow.appendChild(popupNameLabel);
    popupNameRow.appendChild(popupNameInput);
    const popupNameNote = document.createElement("div");
    popupNameNote.textContent = "The file type (.mp4, .m4a, …) is added automatically.";
    popupNameNote.style.cssText = "font-size:11px;color:#6b7280;padding:4px 12px 0;flex:none;";
    const popupBody = document.createElement("div");
    popupBody.style.cssText = "overflow-y:auto;padding:6px;max-height:260px;";
    popup.appendChild(popupHeader);
    popup.appendChild(popupSubtitle);
    popup.appendChild(popupNameRow);
    popup.appendChild(popupNameNote);
    popup.appendChild(popupBody);
    // Fallback for the capture-phase shield (which dispatches
    // popup clicks directly and swallows the event before it
    // reaches this listener): same pattern as the button above.
    popup.addEventListener("click", function (e) {
      e.stopPropagation();
      e.preventDefault();
      handlePopupClick(e.target);
    });
    shadow.appendChild(popup);
    (document.body || document.documentElement).appendChild(host);
    return { host: host, button: btn, popover: isPopover, popup: popup, popupTitle: popupTitle, popupSubtitle: popupSubtitle, popupBody: popupBody, filenameInput: popupNameInput };
  }

  // Show or hide the floating button from the current state.
  // Only the top frame ever shows it (see the section header);
  // it is shown only when the latest election verdict carries a
  // download source — i.e. some frame in this tab has a
  // downloadable stream. The button is created lazily on first
  // show and never removed (it is hidden, not destroyed, when
  // there is nothing to download). Player state, scroll position,
  // and video size do not hide it: the button is viewport-anchored,
  // not on-video.
  function refreshFloatingButton() {
    if (!isTopFrame) return;
    if (!currentDownload) {
      if (floatOverlay) hideOverlayHost(floatOverlay);
      // No stream anywhere: any open quality popup describes a
      // stream that no longer exists.
      if (popupOpen) closeQualityPopup();
      return;
    }
    if (!floatOverlay) floatOverlay = createFloatingButton();
    showOverlayHost(floatOverlay);
    // If a fullscreen element is active, the button must live
    // inside it (see the fullscreen section below).
    updateFullscreenEmbedding();
  }

  // ---- Fullscreen: embed the button inside the fullscreen element ----
  //
  // A popover shown after an element enters fullscreen PAINTS above
  // it (verified by screenshot in headless Chromium: the popover
  // covers the fullscreen video and the player's control bar), but
  // hit-testing a popover over a fullscreen element is unreliable:
  // in headless Chromium the fullscreen subtree wins every click
  // even after the popover is re-shown (elementFromPoint and real
  // mouse clicks both land on the fullscreen element's controls).
  // The top-layer spec says the popover should win, but rather than
  // bet the button on it, the button is EMBEDDED while fullscreen
  // lasts: the host is moved inside the fullscreen element as a
  // plain positioned element (no popover), where normal DOM
  // hit-testing inside the fullscreen subtree applies and z-index
  // max puts it above the player's own controls. On exit it moves
  // back to <body> and becomes a popover again. The one exception
  // is a fullscreen <video> (or <audio>) element: its children are
  // fallback content and are not rendered while it plays, so
  // embedding is impossible there and the popover (re-shown, so it
  // re-enters the top layer above the fullscreen element) is the
  // fallback — visible, and clickable per the top-layer spec.
  // Moving the host preserves its shadow tree and listeners.
  function embedButtonIn(fsEl) {
    // Hide in its current form first (while embeddedInFullscreen
    // is still false the popover branch runs), then drop the
    // popover attribute and move inside the fullscreen element.
    hideOverlayHost(floatOverlay);
    if (floatOverlay.popover) floatOverlay.host.removeAttribute("popover");
    fsEl.appendChild(floatOverlay.host);
    embeddedInFullscreen = true;
    floatOverlay.host.style.display = "block";
  }
  function unembedButton() {
    (document.body || document.documentElement).appendChild(floatOverlay.host);
    // Clear the inline display: an inline display:block would beat
    // the UA :popover-open rule and break the popover once the
    // attribute is restored.
    floatOverlay.host.style.display = "";
    if (floatOverlay.popover) floatOverlay.host.setAttribute("popover", "manual");
    embeddedInFullscreen = false;
  }
  // Reconcile the button's fullscreen state with the document's:
  // embed when an element is fullscreen (and embeddable), unembed
  // when fullscreen ends or moves to another element.
  function updateFullscreenEmbedding() {
    if (!floatOverlay || !currentDownload) return;
    const fsEl = document.fullscreenElement;
    if (embeddedInFullscreen && (!fsEl || floatOverlay.host.parentNode !== fsEl)) {
      unembedButton();
      showOverlayHost(floatOverlay);
    }
    if (fsEl && !embeddedInFullscreen) {
      if (fsEl.tagName === "VIDEO" || fsEl.tagName === "AUDIO" || fsEl.tagName === "IFRAME") {
        // Cannot embed in a media element or an iframe: their
        // children are fallback content and are not rendered
        // while they play (an iframe is the fullscreen element in
        // the top document when its inner player goes fullscreen).
        // Re-show the popover so it re-enters the top layer above
        // the fullscreen element.
        if (floatOverlay.popover) { hideOverlayHost(floatOverlay); showOverlayHost(floatOverlay); }
        return;
      }
      embedButtonIn(fsEl);
    }
  }
  document.addEventListener("fullscreenchange", updateFullscreenEmbedding);

  // ---- One button per tab: cross-frame overlay election ----
  //
  // The same-document dedupe above cannot help when the player
  // lives in an iframe: the content script in the top page and the
  // one in the frame each see their own video and each inject a
  // button, so the user gets two identical buttons on one player
  // (novahd). Frames therefore report their representative
  // candidates to the background script, which elects exactly ONE
  // winner per tab (playing first, then largest area) and tells
  // the top frame which stream to download.
  //
  // Each candidate carries its frame's RESOLVED stream source
  // (the mediaSource result for that video): resource timing is
  // per-document, so only the frame that fetched the manifest can
  // see the playlist. The background picks the best stream across
  // ALL frames — a playlist anywhere beats a direct file anywhere
  // (novahd 2026-11-22: the top frame's clip won the election
  // while the movie's playlist sat in the player iframe's timing
  // log, and the click sent the clip). The button itself lives
  // only in the top frame (see the floating-button section).
  const overlayKeys = new WeakMap();
  let overlayKeyCounter = 0;
  function overlayKeyFor(el) {
    let k = overlayKeys.get(el);
    if (!k) { k = "v" + (++overlayKeyCounter); overlayKeys.set(el, k); }
    return k;
  }
  let lastCandidatesKey = "";
  function reportOverlayCandidates(reps) {
    const cands = [];
    for (const el of reps) {
      const r = el.getBoundingClientRect();
      const cand = {
        key: overlayKeyFor(el),
        playing: isPlaying(el),
        area: Math.round(r.width * r.height),
        // Duration in seconds, only when finite and > 0 (study B4):
        // the background prefers the stream attached to the longest
        // PLAYING video, so a 15s clip loses to a 2h52m film even
        // when no playlist was observed anywhere. NaN (no metadata
        // yet) and Infinity (live) report 0 — unknown, not long.
        duration: (typeof el.duration === "number" && isFinite(el.duration) && el.duration > 0) ? el.duration : 0,
      };
      // The stream this video would download, resolved in THIS
      // frame (only this frame's timing log can see its playlist).
      // A viaPage source has no URL yet: the background fills in
      // this frame's page URL as the extraction target.
      // manifestText/masterText ride along for the download: the
      // captured playlist text for this frame's stream (see
      // mediaSource).
      const src = mediaSource(el);
      if (src) {
        cand.source = {
          url: src.viaPage ? null : src.url,
          viaPage: !!src.viaPage,
          filename: filenameForSource(src),
          audioUrl: src.audioUrl,
          manifestText: src.manifestText,
          masterText: src.masterText,
        };
      }
      cands.push(cand);
    }
    // Round 9 — decoupled playlist scan: the playlist is a stream
    // even when NO <video> element exists to attach it to (a
    // shadow-DOM video, a blob:-iframe player, a player that has
    // not created its element yet). findPlaylistFromResources used
    // to run only inside mediaSource, so those frames never
    // reported. If no reported candidate carries a playlist, add
    // a synthetic one from the timing log (preferred: observed
    // reality) or from the page's embedded data (declared).
    // playing: true — the playlist is the stream the player
    // fetched; duration 0 — unknown, so a real playing video
    // still outranks it on duration inside the playlist pool.
    // An embedded direct file only fills the gap when NO
    // candidate has any source at all: a real video's own source
    // always beats a declared one.
    const hasPlaylist = cands.some(function (c) {
      return c.source && c.source.url && isManifestUrl(c.source.url);
    });
    if (!hasPlaylist) {
      const pl = findPlaylistFromResources();
      if (pl) {
        cands.push({
          key: "playlist",
          playing: true,
          area: 0,
          duration: 0,
          source: { url: pl.url, viaPage: false, filename: undefined, audioUrl: pl.audioUrl, manifestText: pl.manifestText, masterText: pl.masterText },
        });
      } else {
        const hasAnySource = cands.some(function (c) { return c.source && c.source.url; });
        const emb = scanEmbeddedData();
        if (emb.playlist) {
          cands.push({
            key: "playlist",
            playing: true,
            area: 0,
            duration: 0,
            source: { url: emb.playlist, viaPage: false, filename: undefined, audioUrl: undefined, manifestText: undefined, masterText: undefined },
          });
        } else if (!hasAnySource && emb.file) {
          cands.push({
            key: "file",
            playing: false,
            area: 0,
            duration: 0,
            source: { url: emb.file, viaPage: false, filename: fileNameFromUrl(emb.file), audioUrl: undefined, manifestText: undefined, masterText: undefined },
          });
        }
      }
    }
    // The source is part of the key, so a stream appearing in the
    // timing log (the player starts, the manifest arrives)
    // re-reports even when the candidate set itself is unchanged.
    const key = JSON.stringify(cands);
    if (key === lastCandidatesKey) return;
    lastCandidatesKey = key;
    sendRuntimeMessage({ type: "overlayCandidates", candidates: cands }).catch(function () { /* background unreachable */ });
  }

  // ---- Overlay dedupe by position (stacked videos) ----
  //
  // Streaming players often stack two <video> elements in one box
  // (a preview/ad video under the main player, or a pre-roll ad
  // over it). Each element would get its own button — two identical
  // FetchForge buttons on one video, both working. Group elements
  // whose boxes overlap almost exactly and show ONE button per
  // group, attached to the playing element (else the largest).
  // Genuinely separate videos (side by side, different boxes) still
  // get one button each. Only same-document stacking is deduped: a
  // video inside an iframe is seen only by the content script
  // running in that frame.
  const STACK_TOLERANCE_PX = 8;
  function rectsStacked(a, b) {
    return Math.abs(a.left - b.left) <= STACK_TOLERANCE_PX
      && Math.abs(a.top - b.top) <= STACK_TOLERANCE_PX
      && Math.abs(a.right - b.right) <= STACK_TOLERANCE_PX
      && Math.abs(a.bottom - b.bottom) <= STACK_TOLERANCE_PX;
  }

  function isPlaying(el) {
    return !el.paused && !el.ended && el.readyState >= 2;
  }

  // One representative element per stacked group, as a Set.
  function pickOverlayRepresentatives(candidates) {
    const groups = [];
    for (const el of candidates) {
      const rect = el.getBoundingClientRect();
      let placed = false;
      for (const g of groups) {
        if (rectsStacked(rect, g.rect)) { g.els.push(el); placed = true; break; }
      }
      if (!placed) groups.push({ rect: rect, els: [el] });
    }
    const reps = new Set();
    for (const g of groups) {
      let best = g.els[0];
      for (const el of g.els) {
        const elPlaying = isPlaying(el);
        const bestPlaying = isPlaying(best);
        if (elPlaying && !bestPlaying) { best = el; continue; }
        if (elPlaying === bestPlaying) {
          const r = el.getBoundingClientRect();
          const br = best.getBoundingClientRect();
          if (r.width * r.height > br.width * br.height) best = el;
        }
      }
      reps.add(best);
    }
    return reps;
  }

  function scan() {
    for (const detector of detectors) {
      if (!detector.overlay) continue;
      const els = querySelectorAllDeep(detector.selector);
      const candidates = [];
      for (const el of els) {
        if (!detector.extract(el)) continue; // no usable source yet; caught later
        candidates.push(el);
      }
      const reps = pickOverlayRepresentatives(candidates);
      // Cross-frame election: report this frame's representatives
      // (with their resolved stream sources) so the background
      // can elect one winner per tab and tell the top frame which
      // stream its button downloads.
      reportOverlayCandidates(reps);
    }
    refreshFloatingButton();
    reportMedia(collectMedia());
  }

  // Debounced rescan: catches dynamically added players and src changes.
  let scanTimer = 0;
  function scheduleScan() {
    if (scanTimer) return;
    scanTimer = window.setTimeout(function () { scanTimer = 0; scan(); }, 150);
  }

  const observer = new MutationObserver(scheduleScan);

  // Fullscreen: the button is embedded inside the fullscreen
  // element while fullscreen lasts (see updateFullscreenEmbedding
  // above); no scroll/resize tracking is needed, because the
  // button is viewport-anchored, not video-anchored.

  // Popup asks for the media on this tab; background sends the
  // overlay-election verdict for this tab (which stream the top
  // frame's button downloads, or null when no frame has one).
  api.runtime.onMessage.addListener(function (msg, _sender, sendResponse) {
    if (msg && msg.type === "getMedia") {
      sendResponse({ items: collectMedia() });
    } else if (msg && msg.type === "overlayVerdict") {
      const newDownload = (msg.download && msg.download.url) ? msg.download : null;
      // A changed download invalidates any open quality popup:
      // its rows described the previous stream.
      if (popupOpen && (!newDownload || !currentDownload || newDownload.url !== currentDownload.url)) {
        closeQualityPopup();
      }
      currentDownload = newDownload;
      refreshFloatingButton();
    }
  });

  // Startup: this script runs at document_start (so the popunder
  // shield registers before page scripts), when documentElement
  // may not exist yet. Defer the observer + first scan until the
  // DOM is ready.
  function start() {
    observer.observe(document.documentElement, {
      childList: true,
      subtree: true,
      attributes: true,
      attributeFilter: ["src"],
    });
    scan();
  }
  if (document.documentElement) {
    start();
  } else {
    document.addEventListener("DOMContentLoaded", start, { once: true });
  }

  // Re-report candidates every 10 s so a restarted service worker
  // rebuilds its election state (and a stale verdict is refreshed).
  window.setInterval(function () {
    lastCandidatesKey = "";
    scan();
  }, 10000);
})();
