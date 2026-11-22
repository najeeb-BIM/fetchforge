// FetchForge content script (MV3, shared by the Chromium and Firefox builds).
// Phase 4 (MediaDetectionEngine): detects media on http(s) pages, shows an
// IDM-style download button over videos, answers the popup's getMedia
// request, and reports the detected set to the background script (which
// forwards it to the desktop app via POST /media/detected).
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
// case that URL is sent instead. data: URLs are skipped entirely. No
// DRM, auth, paywall, or encryption bypass is attempted anywhere.
//
// Stacked videos (a preview/ad <video> on top of the main player,
// common on streaming sites) share one on-screen box: only one
// overlay button is shown for the whole stack, attached to the
// playing (or largest) element — see pickOverlayRepresentatives.
(function () {
  "use strict";
  if (window.location.protocol !== "http:" && window.location.protocol !== "https:") return;
  // Avoid double-injection if the script is ever loaded twice.
  if (window.__fetchforgeContentLoaded) return;
  window.__fetchforgeContentLoaded = true;

  const api = typeof browser !== "undefined" ? browser : chrome;
  const isFirefox = typeof browser !== "undefined";

  // runtime.sendMessage is Promise-based under Firefox (`browser`) and
  // callback-based under Chrome (`chrome`); normalise to a Promise.
  function sendRuntimeMessage(msg) {
    if (isFirefox) return api.runtime.sendMessage(msg);
    return new Promise((resolve) => api.runtime.sendMessage(msg, resolve));
  }

  // A video smaller than this gets no overlay button (too small to be a
  // real player, e.g. a tracking pixel or preview thumbnail).
  const MIN_OVERLAY_W = 120;
  const MIN_OVERLAY_H = 68;
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
    for (const raw of candidates) {
      const u = resolveUrl(raw);
      if (!u || u.startsWith("data:")) continue; // data: URLs are skipped
      if (u.startsWith("blob:")) { sawBlob = true; continue; }
      if (/^https?:\/\//i.test(u)) return { url: u, viaPage: false };
    }
    if (sawBlob) {
      // The player loads its stream through Media Source Extensions:
      // the real URL is not in the DOM, but the browser's
      // resource-timing log lists every URL this document fetched,
      // including the manifest the player loaded. Send that instead
      // of the bare page URL when it is found.
      const stream = findStreamUrlFromResources();
      if (stream) return { url: stream, viaPage: false, viaStream: true };
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
  const STREAM_EXT_RE = /\.(m3u8|mpd)([?#]|$)/i;
  const MEDIA_FILE_EXT_RE = /\.(mp4|webm|mov|mkv)([?#]|$)/i;
  function findStreamUrlFromResources() {
    let entries = [];
    try {
      entries = performance.getEntriesByType("resource") || [];
    } catch (_) { return null; }
    let bestManifest = null; // { url, rank, startTime }
    let bestFile = null;
    for (const e of entries) {
      const name = e && e.name;
      if (!name || !/^https?:\/\//i.test(name)) continue;
      const it = e.initiatorType || "";
      const rank = (it === "video" || it === "audio" || it === "fetch" || it === "xmlhttprequest") ? 1 : 0;
      const t = typeof e.startTime === "number" ? e.startTime : 0;
      const cand = { url: name, rank: rank, startTime: t };
      const better = function (a, b) {
        return !b || a.rank > b.rank || (a.rank === b.rank && a.startTime >= b.startTime);
      };
      if (STREAM_EXT_RE.test(name)) {
        if (better(cand, bestManifest)) bestManifest = cand;
      } else if (MEDIA_FILE_EXT_RE.test(name)) {
        if (better(cand, bestFile)) bestFile = cand;
      }
    }
    const best = bestManifest || bestFile;
    return best ? best.url : null;
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

  // ---- IDM-style overlay button on videos ----

  // media element -> { host, button }
  const overlays = new Map();

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
    const src = mediaSource(video);
    if (!src) return;
    const url = src.viaPage ? window.location.href : src.url;
    const filename = filenameForSource(src);
    btn.disabled = true;
    btn.textContent = "Sending…";
    btn.title = "";
    sendRuntimeMessage({ type: "downloadMedia", url: url, filename: filename }).then(function (resp) {
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

  function createOverlay(video) {
    const host = document.createElement("div");
    host.style.cssText = "position:fixed;left:0;top:0;display:none;z-index:2147483647;pointer-events:none;";
    const shadow = host.attachShadow({ mode: "open" });
    const btn = document.createElement("button");
    btn.textContent = "↓ FetchForge";
    // Shadow DOM isolates the button from page CSS. translateX(-100%)
    // anchors its right edge to the host's left position (top-right
    // corner of the video) without measuring the button width.
    btn.style.cssText = "pointer-events:auto;transform:translateX(-100%);background:#22c55e;color:#fff;"
      + "border:0;border-radius:6px;padding:6px 10px;font:600 12px system-ui,sans-serif;"
      + "cursor:pointer;box-shadow:0 2px 8px rgba(0,0,0,.4);opacity:.92;white-space:nowrap;";
    btn.addEventListener("mouseenter", function () { btn.style.opacity = "1"; });
    btn.addEventListener("mouseleave", function () { btn.style.opacity = ".92"; });
    btn.addEventListener("click", function (e) {
      e.stopPropagation();
      e.preventDefault();
      onOverlayClick(video, btn);
    });
    shadow.appendChild(btn);
    (document.body || document.documentElement).appendChild(host);
    return { host: host, button: btn };
  }

  function positionOverlays() {
    for (const entry of overlays) {
      const video = entry[0];
      const o = entry[1];
      if (!video.isConnected) { o.host.remove(); overlays.delete(video); continue; }
      const rect = video.getBoundingClientRect();
      const offscreen = rect.bottom < 0 || rect.top > window.innerHeight
        || rect.right < 0 || rect.left > window.innerWidth;
      if (offscreen || !isVisible(video) || rect.width < MIN_OVERLAY_W || rect.height < MIN_OVERLAY_H) {
        o.host.style.display = "none";
        continue;
      }
      o.host.style.display = "block";
      o.host.style.left = Math.round(rect.right - 8) + "px";
      o.host.style.top = Math.round(rect.top + 8) + "px";
    }
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
      // Prune overlays for elements that are no longer their group's
      // representative (e.g. the main player started playing and
      // took over from the preview).
      for (const el of Array.from(overlays.keys())) {
        if (candidates.indexOf(el) !== -1 && !reps.has(el)) {
          overlays.get(el).host.remove();
          overlays.delete(el);
        }
      }
      for (const el of reps) {
        if (overlays.has(el)) continue;
        overlays.set(el, createOverlay(el));
      }
    }
    positionOverlays();
    reportMedia(collectMedia());
  }

  // Debounced rescan: catches dynamically added players and src changes.
  let scanTimer = 0;
  function scheduleScan() {
    if (scanTimer) return;
    scanTimer = window.setTimeout(function () { scanTimer = 0; scan(); }, 150);
  }

  const observer = new MutationObserver(scheduleScan);
  observer.observe(document.documentElement, {
    childList: true,
    subtree: true,
    attributes: true,
    attributeFilter: ["src"],
  });

  let rafPending = false;
  function scheduleReposition() {
    if (rafPending) return;
    rafPending = true;
    window.requestAnimationFrame(function () { rafPending = false; positionOverlays(); });
  }
  window.addEventListener("scroll", scheduleReposition, true);
  window.addEventListener("resize", scheduleReposition);

  // Popup asks for the media on this tab.
  api.runtime.onMessage.addListener(function (msg, _sender, sendResponse) {
    if (msg && msg.type === "getMedia") {
      sendResponse({ items: collectMedia() });
    }
  });

  scan();
})();
