// FetchForge MAIN-world manifest hook (round 9, Tier 0).
//
// Runs as a manifest-declared content script with "world": "MAIN"
// (see manifest.json), so it executes in the page's own JS world —
// and, unlike the old inline <script> injection it replaces, it is
// NOT subject to the page's CSP: a strict script-src no longer
// kills the hook (that is what blocked it on novahd, whose CSP is
// script-src 'self'). Manifest content scripts are the standard
// way to run page-world code; the hook itself is original
// FetchForge code (the fetch/XHR-hooking TECHNIQUE comes from the
// published description of XDM's hls-hook.js, GPL-3.0 — no code,
// identifiers, comments, or structure were copied from it).
//
// What it does: wraps the page's fetch and XMLHttpRequest, clones
// 2xx playlist responses (.m3u8/.mpd by URL or Content-Type, 256 KB
// cap, never a body over the cap) and forwards the playlist TEXT
// to the isolated content script by window.postMessage. The text
// is needed because some CDNs sign a playlist link for a single
// request — re-fetching it from the app gets 403, so the captured
// text is the only copy that works. Only playlist text + its URL
// are captured: never cookies, credentials, headers, or page
// content. The original fetch promise and the original XHR
// behaviour are always preserved, and every step is guarded so
// the hook can never throw into page code.
//
// Anti-spoof token: the isolated content script publishes a
// random per-page-load token on <html> as data-fetchforge-hook-token
// (see content.js publishHookToken). The hook reads it from there
// and echoes it in every message; the listener only accepts
// messages carrying it, so a page script cannot forge a fake
// manifest into the download path. (Not a security boundary — a
// page script can read the attribute — but it stops casual
// postMessage spoofing, and the token never leaves the page.)
// The token may land AFTER this script runs (both run at
// document_start, and <html> may not exist yet): captures made
// before the token appears are queued (max 50) and flushed once
// it does, so an early playlist is never lost. If the token never
// appears within 30 s, the watcher stops and the queue is dropped
// — the hook keeps running but forwards nothing.
(function () {
  "use strict";
  // Avoid double-installation if the script is ever loaded twice:
  // wrapping fetch/XHR twice would forward every playlist twice.
  if (window.__fetchforgeHookLoaded) return;
  window.__fetchforgeHookLoaded = true;

  var MAX_TEXT = 262144; // 256 KB cap, mirrored in the content script
  var TOKEN_ATTR = "data-fetchforge-hook-token";
  var TOKEN_WAIT_MS = 30000;
  var PENDING_MAX = 50;
  var token = null;
  var pending = [];
  var tokenObserver = null;
  var tokenTimer = null;

  function readToken() {
    try {
      var el = document.documentElement;
      if (el && el.getAttribute) {
        var t = el.getAttribute(TOKEN_ATTR);
        if (t) return t;
      }
    } catch (_) {}
    return null;
  }
  function stopWatching() {
    if (tokenObserver) {
      try { tokenObserver.disconnect(); } catch (_) {}
      tokenObserver = null;
    }
    if (tokenTimer) {
      try { clearTimeout(tokenTimer); } catch (_) {}
      tokenTimer = null;
    }
  }
  function flushPending() {
    if (!token || !pending.length) return;
    var queued = pending;
    pending = [];
    for (var i = 0; i < queued.length; i++) postManifest(queued[i].url, queued[i].text);
  }
  function watchForToken() {
    if (tokenObserver || token) return;
    try {
      tokenObserver = new MutationObserver(function () {
        var t = readToken();
        if (t) {
          token = t;
          stopWatching();
          flushPending();
        }
      });
      tokenObserver.observe(document, { childList: true, subtree: true, attributes: true });
      tokenTimer = setTimeout(function () {
        // The content script never published a token (restricted
        // page, or it failed to load): stop watching and drop the
        // queue rather than observing the DOM forever.
        stopWatching();
        pending = [];
      }, TOKEN_WAIT_MS);
    } catch (_) {}
  }
  function postManifest(url, text) {
    if (typeof text !== "string" || text.length > MAX_TEXT) return;
    if (!token) {
      pending.push({ url: url, text: text });
      if (pending.length > PENDING_MAX) pending.shift();
      watchForToken();
      return;
    }
    try { window.postMessage({ type: "ff-manifest", token: token, url: url, text: text }, "*"); } catch (_) {}
  }
  // Match .m3u8/.mpd ANYWHERE in the URL (round 7): proxy-wrapped
  // playlists carry the extension mid-URL, followed by &. The
  // text-shape gate (looksLikePlaylist) still guards against false
  // positives: a URL that merely mentions .m3u8 is only forwarded
  // when its BODY is a playlist.
  function isPlaylistUrl(u) { return /\.m3u8/i.test(u) || /\.mpd/i.test(u); }
  function isPlaylistType(ct) {
    ct = String(ct || "").toLowerCase();
    return ct.indexOf("mpegurl") !== -1 || ct.indexOf("dash+xml") !== -1;
  }
  function looksLikePlaylist(text, url) {
    if (/\.m3u8/i.test(url)) return text.indexOf("#EXTM3U") !== -1;
    if (/\.mpd/i.test(url)) return text.indexOf("<MPD") !== -1;
    // URL gave no hint (matched by content-type only): accept
    // either playlist shape.
    return text.indexOf("#EXTM3U") !== -1 || text.indexOf("<MPD") !== -1;
  }
  function absUrl(u) {
    try { return new URL(String(u), window.location.href).href; } catch (_) { return String(u || ""); }
  }
  function maybeForward(url, text, contentType) {
    var u = absUrl(url);
    if (!u) return;
    if (!isPlaylistUrl(u) && !isPlaylistType(contentType)) return;
    if (!looksLikePlaylist(text, u)) return;
    postManifest(u, text);
  }
  try {
    var origFetch = window.fetch;
    if (typeof origFetch === "function") {
      window.fetch = function (input) {
        var url = "";
        try {
          if (typeof input === "string") url = input;
          else if (input && typeof input.url === "string") url = input.url;
        } catch (_) {}
        var p = origFetch.apply(this, arguments);
        if (!url) return p;
        return p.then(function (resp) {
          try {
            if (resp && resp.ok) {
              var clone = resp.clone();
              var ct = "";
              try { ct = clone.headers.get("content-type") || ""; } catch (_) {}
              if (isPlaylistUrl(absUrl(url)) || isPlaylistType(ct)) {
                // Skip the read when the body is known to be far
                // larger than any playlist: with the widened URL
                // test, a segment URL that happens to contain
                // ".m3u8" must never trigger a multi-MB text read.
                // Chunked responses carry no Content-Length and
                // still read (playlists are small; the forward cap
                // drops anything over 256 KB anyway).
                var len = 0;
                try { len = parseInt(clone.headers.get("content-length") || "0", 10) || 0; } catch (_) {}
                if (!len || len <= MAX_TEXT) {
                  clone.text().then(function (t) { maybeForward(url, t, ct); }).catch(function () {});
                }
              }
            }
          } catch (_) {}
          return resp;
        });
      };
    }
  } catch (_) {}
  try {
    var origOpen = XMLHttpRequest.prototype.open;
    var origSend = XMLHttpRequest.prototype.send;
    XMLHttpRequest.prototype.open = function (method, url) {
      try { this.__ffManifestUrl = url; } catch (_) {}
      return origOpen.apply(this, arguments);
    };
    XMLHttpRequest.prototype.send = function () {
      var xhr = this;
      try {
        xhr.addEventListener("load", function () {
          try {
            if (xhr.status >= 200 && xhr.status < 300 && typeof xhr.responseText === "string") {
              var ct = "";
              try { ct = xhr.getResponseHeader("Content-Type") || ""; } catch (_) {}
              maybeForward(xhr.__ffManifestUrl || "", xhr.responseText, ct);
            }
          } catch (_) {}
        });
      } catch (_) {}
      return origSend.apply(this, arguments);
    };
  } catch (_) {}

  // Read the token now (the content script may already have
  // published it), else watch for it.
  token = readToken();
  if (!token) watchForToken();
})();
