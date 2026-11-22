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
// It never runs on chrome://, about:, or extension pages: the manifest
// matches http(s) only, and the protocol check below is a second line
// of defence.
//
// Detection is DOM-based only (no webRequest observation): the extension
// deliberately keeps no <all_urls> host permission. blob: sources (e.g.
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
// the floating-button section header).
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
  //              "image" | "model")
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
  // Returns { url, viaPage, viaStream? }: url is a direct http(s)
  // media URL, or null with viaPage=true when the only source is a
  // blob: URL (the page URL must be sent instead so the desktop app
  // can extract the media). viaStream is set when the URL came from
  // the resource-timing log rather than the DOM (see below).
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
        if (STREAM_EXT_RE.test(u)) return { url: u, viaPage: false };
        if (!directFile) directFile = u;
      }
    }
    // A playlist in the resource-timing log beats the direct file:
    // see the playlist-first note above.
    const playlist = findPlaylistFromResources();
    if (playlist) return { url: playlist.url, viaPage: false, viaStream: true, audioUrl: playlist.audioUrl };
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
      if (stream) return { url: stream.url, viaPage: false, viaStream: true, audioUrl: stream.audioUrl };
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
  const STREAM_EXT_RE = /\.(m3u8|mpd)([?#]|$)/i;
  const MEDIA_FILE_EXT_RE = /\.(mp4|webm|mov|mkv)([?#]|$)/i;
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
  // ranking comment on findStreamUrlFromResources).
  function scanResourceStreams() {
    const empty = { master: null, video: null, audio: null, file: null };
    let entries = [];
    try {
      entries = performance.getEntriesByType("resource") || [];
    } catch (_) { return empty; }
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
      if (STREAM_EXT_RE.test(name)) {
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
    return out;
  }
  // Pick the playlist from a scan: a master is the whole stream
  // (send it alone); otherwise pair the video variant with its
  // audio sibling; an audio-only variant goes alone. Returns
  // { url, audioUrl? } or null.
  function pickPlaylist(s) {
    if (s.master) return { url: s.master.url };
    if (s.video) {
      const out = { url: s.video.url };
      if (s.audio) out.audioUrl = s.audio.url;
      return out;
    }
    if (s.audio) return { url: s.audio.url };
    return null;
  }
  // Best playlist (master or variant) observed in the log, or
  // null. Direct files are deliberately NOT returned here: a
  // playlist is the stream that is playing; a file may be a
  // preview/ad clip (see the playlist-first note on mediaSource).
  function findPlaylistFromResources() {
    return pickPlaylist(scanResourceStreams());
  }
  // Returns { url, audioUrl? } or null: url is the manifest/file
  // to send; audioUrl is set only when url is a video variant and
  // a sibling audio variant was observed alongside it.
  function findStreamUrlFromResources() {
    const s = scanResourceStreams();
    return pickPlaylist(s) || (s.file ? { url: s.file.url } : null);
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

  // Full media list for the popup and for reporting, deduplicated by URL.
  function collectMedia() {
    const items = [];
    const seen = new Set();
    for (const detector of detectors) {
      const els = document.querySelectorAll(detector.selector);
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
  // click — including a click on our overlay button. The button's
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
  function overlayButtonFromEvent(e) {
    if (!floatOverlay || !floatTarget) return null;
    const path = typeof e.composedPath === "function" ? e.composedPath() : [];
    if (path.indexOf(floatOverlay.button) !== -1) {
      return { button: floatOverlay.button, video: floatTarget };
    }
    return null;
  }
  function onOverlayEventCapture(e) {
    const hit = overlayButtonFromEvent(e);
    if (!hit) return; // not our button: leave the page alone
    if (e.type === "click" && !hit.button.disabled) {
      onOverlayClick(hit.video, hit.button);
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
  // Exactly one button per tab: the cross-frame election (round 2)
  // still decides which frame shows it, and within a frame the
  // button downloads the best representative (playing, then
  // largest). The click target is resolved at click time, so the
  // button always downloads the stream that is currently playing.

  // The single floating button in this frame, created lazily on
  // first show: { host, button, popover }. Null until created.
  let floatOverlay = null;
  // The video element the button currently downloads (this frame's
  // best representative), resolved fresh on every scan.
  let floatTarget = null;
  // This frame's best representative (or null); the election
  // verdict is evaluated against it.
  let currentBestRep = null;
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

  function onOverlayClick(video, btn) {
    // Re-resolve at click time: the source may have changed since scan.
    // A detached target (removed between scan and click) is refused.
    if (!video || !video.isConnected) return;
    const src = mediaSource(video);
    if (!src) return;
    const url = src.viaPage ? window.location.href : src.url;
    const filename = filenameForSource(src);
    btn.disabled = true;
    btn.textContent = "Sending…";
    btn.title = "";
    sendRuntimeMessage({ type: "downloadMedia", url: url, filename: filename, audioUrl: src.audioUrl }).then(function (resp) {
      if (resp && resp.ok) {
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
      onOverlayClick(floatTarget, btn);
    });
    shadow.appendChild(btn);
    (document.body || document.documentElement).appendChild(host);
    return { host: host, button: btn, popover: isPopover };
  }

  // Best representative of a set: the playing one, else the
  // largest (the same ranking the background election uses, so the
  // elected winner in this frame is always this frame's best).
  function bestRepresentative(reps) {
    let best = null;
    for (const el of reps) {
      if (!best) { best = el; continue; }
      const elPlaying = isPlaying(el);
      const bestPlaying = isPlaying(best);
      if (elPlaying && !bestPlaying) { best = el; continue; }
      if (elPlaying === bestPlaying) {
        const r = el.getBoundingClientRect();
        const br = best.getBoundingClientRect();
        if (r.width * r.height > br.width * br.height) best = el;
      }
    }
    return best;
  }

  // Show or hide the floating button from the current state:
  // shown only when this frame has a downloadable video AND this
  // frame holds the elected winner (or no verdict has arrived yet).
  // The button is created lazily on first show and never removed
  // (it is hidden, not destroyed, when there is nothing to
  // download). Player state, scroll position, and video size do
  // not hide it: the button is viewport-anchored, not on-video.
  function refreshFloatingButton() {
    // Cross-frame election verdict: undefined means no verdict has
    // arrived yet, so the button shows; null (or another key) means
    // another frame won, so this frame's button hides.
    const elected = electedKey === undefined
      || (currentBestRep && overlayKeyFor(currentBestRep) === electedKey);
    if (!currentBestRep || !elected) {
      floatTarget = null;
      if (floatOverlay) hideOverlayHost(floatOverlay);
      return;
    }
    floatTarget = currentBestRep;
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
    if (!floatOverlay || !floatTarget) return;
    const fsEl = document.fullscreenElement;
    if (embeddedInFullscreen && (!fsEl || floatOverlay.host.parentNode !== fsEl)) {
      unembedButton();
      showOverlayHost(floatOverlay);
    }
    if (fsEl && !embeddedInFullscreen) {
      if (fsEl.tagName === "VIDEO" || fsEl.tagName === "AUDIO") {
        // Cannot embed in a media element: re-show the popover so
        // it re-enters the top layer above the fullscreen element.
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
  // each frame whether it holds the winner. Buttons show until the
  // first verdict arrives, so election never delays the button.
  const overlayKeys = new WeakMap();
  let overlayKeyCounter = 0;
  function overlayKeyFor(el) {
    let k = overlayKeys.get(el);
    if (!k) { k = "v" + (++overlayKeyCounter); overlayKeys.set(el, k); }
    return k;
  }
  let electedKey = undefined;
  let lastCandidatesKey = "";
  function reportOverlayCandidates(reps) {
    const cands = [];
    for (const el of reps) {
      const r = el.getBoundingClientRect();
      cands.push({
        key: overlayKeyFor(el),
        playing: isPlaying(el),
        area: Math.round(r.width * r.height),
      });
    }
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
      const els = document.querySelectorAll(detector.selector);
      const candidates = [];
      for (const el of els) {
        if (!detector.extract(el)) continue; // no usable source yet; caught later
        candidates.push(el);
      }
      const reps = pickOverlayRepresentatives(candidates);
      // Cross-frame election: report this frame's representatives
      // so the background can elect one winner per tab.
      reportOverlayCandidates(reps);
      // The floating button downloads this frame's best
      // representative (playing, then largest); the election
      // verdict decides whether this frame shows it at all.
      currentBestRep = bestRepresentative(reps);
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
  // overlay-election verdict for this frame.
  api.runtime.onMessage.addListener(function (msg, _sender, sendResponse) {
    if (msg && msg.type === "getMedia") {
      sendResponse({ items: collectMedia() });
    } else if (msg && msg.type === "overlayVerdict") {
      electedKey = msg.winnerKey === undefined ? undefined : msg.winnerKey;
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
