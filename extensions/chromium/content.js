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
// server-side. data: URLs are skipped entirely. No DRM, auth, paywall,
// or encryption bypass is attempted anywhere.
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
  //   kind     - item kind reported to the app ("video" | "audio";
  //              Phase 7 adds "image" and "model")
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
  // Returns { url, viaPage }: url is a direct http(s) media URL, or null
  // with viaPage=true when the only source is a blob: URL (the page URL
  // must be sent instead so the desktop app can extract the media).
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
    if (sawBlob) return { url: null, viaPage: true };
    return null;
  }

  function extractHtmlMedia(kind) {
    return function (el) {
      if (!isVisible(el)) return null;
      const src = mediaSource(el);
      if (!src) return null;
      return {
        kind: kind,
        url: src.viaPage ? window.location.href : src.url,
        viaPage: src.viaPage,
        title: mediaTitle(el),
        filename: src.viaPage ? undefined : fileNameFromUrl(src.url),
      };
    };
  }

  // Video/audio only in this wave; image + 3D detectors arrive in Phase 7.
  registerDetector({ kind: "video", selector: "video", overlay: true, extract: extractHtmlMedia("video") });
  registerDetector({ kind: "audio", selector: "audio", overlay: false, extract: extractHtmlMedia("audio") });

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

  function onOverlayClick(video, btn) {
    // Re-resolve at click time: the source may have changed since scan.
    const src = mediaSource(video);
    if (!src) return;
    const url = src.viaPage ? window.location.href : src.url;
    const filename = src.viaPage ? undefined : fileNameFromUrl(src.url);
    btn.disabled = true;
    btn.textContent = "Sending…";
    sendRuntimeMessage({ type: "downloadMedia", url: url, filename: filename }).then(function (resp) {
      btn.textContent = resp && resp.ok ? "Sent to FetchForge" : "Failed — try the popup";
      window.setTimeout(function () {
        btn.disabled = false;
        btn.textContent = "↓ FetchForge";
      }, 2500);
    }).catch(function () {
      btn.textContent = "Failed — try the popup";
      window.setTimeout(function () {
        btn.disabled = false;
        btn.textContent = "↓ FetchForge";
      }, 2500);
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

  function scan() {
    for (const detector of detectors) {
      if (!detector.overlay) continue;
      const els = document.querySelectorAll(detector.selector);
      for (const el of els) {
        if (overlays.has(el)) continue;
        if (!detector.extract(el)) continue; // no usable source yet; caught later
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
