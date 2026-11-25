// Round 9 harness: general hidden-stream detection (Tier 0 + Tier 1).
//
// Decision tests:
//   (a) MAIN-world hook file exists, is wired in the manifest, and no
//       inline injection remains; the hook actually forwards a
//       captured playlist (and queues it until the token appears).
//   (b) Timing scan finds an extensionless playlist URL (Content-Type).
//   (c) Timing scan finds a playlist with no <video> element present
//       (decoupled scan -> synthetic candidate), and the embedded
//       scan fills the same gap when the timing log is empty.
//   (d) webRequest observer classifies a playlist by Content-Type.
//   (e) Embedded-data scan finds a stream URL in page JSON.
// Plus: widened URL tests, and the election merging an observed
// extensionless playlist ahead of a direct file.
"use strict";
const fs = require("fs");
const path = require("path");
const vm = require("vm");
const { readSource, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");
const hookSrc = readSource("extensions/chromium/hook-main.js");
const manifestCr = JSON.parse(readSource("extensions/chromium/manifest.json"));
const manifestFx = JSON.parse(readSource("extensions/firefox/manifest.json"));

// ---------- (a) MAIN-world hook wiring ----------
test("(a) hook-main.js exists, parses, and is wired as a MAIN-world content script", () => {
  new vm.Script(hookSrc, { filename: "hook-main.js" }); // parse check
  for (const [name, m] of [["chromium", manifestCr], ["firefox", manifestFx]]) {
    assertEq(m.version, "0.2.11", name + " version");
    assert(m.permissions.includes("webRequest"), name + " must declare webRequest");
    assertEq(JSON.stringify(m.optional_host_permissions), JSON.stringify(["<all_urls>"]), name + " optional <all_urls>");
    const main = m.content_scripts.find((e) => e.world === "MAIN");
    assert(main, name + " must have a MAIN-world content script entry");
    assertEq(JSON.stringify(main.js), JSON.stringify(["hook-main.js"]), name + " MAIN entry js");
    assertEq(main.run_at, "document_start", name + " MAIN entry run_at");
    assertEq(main.all_frames, true, name + " MAIN entry all_frames");
    assertEq(JSON.stringify(main.matches), JSON.stringify(["http://*/*", "https://*/*"]), name + " MAIN entry matches");
    const isolated = m.content_scripts.find((e) => !e.world);
    assert(isolated && isolated.js.includes("content.js"), name + " must keep the isolated content.js entry");
    assert(m.content_scripts.indexOf(isolated) < m.content_scripts.indexOf(main), name + ": content.js must run before hook-main.js (token first)");
  }
  assert(parseInt(manifestFx.browser_specific_settings.gecko.strict_min_version, 10) >= 128,
    "firefox floor must be 128+ for world: MAIN");
});

test("(a) no inline injection remains in content.js", () => {
  assert(!contentSrc.includes("manifestHookMain"), "manifestHookMain must be gone");
  assert(!contentSrc.includes("injectManifestHook"), "injectManifestHook must be gone");
  assert(!contentSrc.includes('createElement("script")'), "no script-element injection must remain");
  assert(!contentSrc.includes(".toString() + \")(\""), "no stringified-function injection must remain");
  assert(contentSrc.includes("publishHookToken"), "token publication must replace injection");
  assert(contentSrc.includes("data-fetchforge-hook-token"), "token attribute must be named in content.js");
  assert(hookSrc.includes("data-fetchforge-hook-token"), "token attribute must be named in hook-main.js");
  assert(hookSrc.includes("window.fetch"), "hook must wrap fetch");
  assert(hookSrc.includes("XMLHttpRequest.prototype"), "hook must wrap XHR");
  assert(hookSrc.includes("ff-manifest"), "hook must forward ff-manifest messages");
});

function hookContext(tokenAtLoad) {
  const posted = [];
  const fetchCalls = [];
  let currentToken = tokenAtLoad;
  let observerCb = null;
  const sandbox = {
    setTimeout: function () { return 1; }, // the 30 s token timer must not keep node alive
    clearTimeout: function () {},
    MutationObserver: class {
      constructor(cb) { observerCb = cb; }
      observe() {}
      disconnect() {}
    },
    XMLHttpRequest: undefined,
    document: {
      documentElement: {
        getAttribute: function (name) {
          return name === "data-fetchforge-hook-token" ? currentToken : null;
        },
      },
    },
  };
  const windowObj = {
    location: { href: "https://novahd.cc/watch/m/1368337" },
    postMessage: function (msg) { posted.push(msg); },
    fetch: function (input) {
      fetchCalls.push(input);
      return Promise.resolve({
        ok: true,
        clone: function () {
          return {
            headers: {
              get: function (name) {
                const n = String(name).toLowerCase();
                if (n === "content-type") return "application/vnd.apple.mpegurl";
                if (n === "content-length") return "42";
                return null;
              },
            },
            text: function () { return Promise.resolve("#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:6,\nseg1.ts\n"); },
          };
        },
      });
    },
  };
  sandbox.window = windowObj;
  const ctx = makeContext(sandbox, [hookSrc]);
  return {
    ctx: ctx,
    posted: posted,
    fetchCalls: fetchCalls,
    setToken: function (t) { currentToken = t; },
    fireObserver: function () { if (observerCb) observerCb(); },
  };
}

test("(a) hook forwards a captured playlist with the token", async () => {
  const h = hookContext("test-token-9");
  runIn(h.ctx, "window.fetch('https://cdn.example.com/master.m3u8')");
  await new Promise((r) => setTimeout(r, 50));
  assertEq(h.posted.length, 1, "exactly one manifest message");
  assertEq(h.posted[0].type, "ff-manifest");
  assertEq(h.posted[0].token, "test-token-9");
  assertEq(h.posted[0].url, "https://cdn.example.com/master.m3u8");
  assert(h.posted[0].text.indexOf("#EXTM3U") !== -1, "playlist text must ride along");
});

test("(a) hook queues a capture until the token appears, then flushes", async () => {
  const h = hookContext(null); // token not published yet
  runIn(h.ctx, "window.fetch('https://cdn.example.com/master.m3u8')");
  await new Promise((r) => setTimeout(r, 50));
  assertEq(h.posted.length, 0, "nothing forwarded without the token");
  h.setToken("late-token-1");
  h.fireObserver(); // the DOM mutation that publishes the token
  assertEq(h.posted.length, 1, "queued capture flushed once the token appears");
  assertEq(h.posted[0].token, "late-token-1");
  assertEq(h.posted[0].url, "https://cdn.example.com/master.m3u8");
});

// ---------- (b) timing scan: extensionless playlist ----------
const TIMING_CONSTS = extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n"
  + extractConst(contentSrc, "MEDIA_FILE_EXT_RE", 2) + "\n"
  + extractConst(contentSrc, "MASTER_STEM_RE", 2) + "\n"
  + extractConst(contentSrc, "AUDIO_STEM_RE", 2) + "\n"
  + extractConst(contentSrc, "PLAYLIST_MIME_RE", 2) + "\n"
  + extractConst(contentSrc, "capturedManifests", 2) + "\n";
function timingContext(entries) {
  return makeContext(
    {
      performance: { getEntriesByType: function () { return entries; }, now: function () { return 0; } },
    },
    [
      TIMING_CONSTS,
      extractMany(contentSrc, ["manifestKind", "scanResourceStreams", "pickPlaylist", "findPlaylistFromResources", "findStreamUrlFromResources"], 2),
    ]
  );
}

test("(b) timing scan finds an extensionless playlist by Content-Type", () => {
  const ctx = timingContext([
    { name: "https://novahd.cc/api/gs-hls?x=1", initiatorType: "fetch", startTime: 100, contentType: "application/vnd.apple.mpegurl" },
    { name: "https://novahd.cc/assets/p/clip.mp4", initiatorType: "video", startTime: 50, contentType: "video/mp4" },
  ]);
  const pl = runIn(ctx, "findPlaylistFromResources()");
  assert(pl, "an extensionless playlist must be found");
  assertEq(pl.url, "https://novahd.cc/api/gs-hls?x=1");
  const stream = runIn(ctx, "findStreamUrlFromResources()");
  assertEq(stream.url, "https://novahd.cc/api/gs-hls?x=1", "the playlist beats the clip (suppression)");
});

test("(b) a TS segment (video/mp2t) is not a playlist", () => {
  const ctx = timingContext([
    { name: "https://cdn.example.com/seg_00001.ts", initiatorType: "fetch", startTime: 100, contentType: "video/mp2t" },
  ]);
  assertEq(runIn(ctx, "findPlaylistFromResources()"), null);
});

test("(b) a DASH manifest by Content-Type, and a wrapped playlist URL", () => {
  const ctx = timingContext([
    { name: "https://worker.dev/api/dash?id=9", initiatorType: "fetch", startTime: 100, contentType: "application/dash+xml" },
  ]);
  assertEq(runIn(ctx, "findPlaylistFromResources()").url, "https://worker.dev/api/dash?id=9");
  const ctx2 = timingContext([
    { name: "https://frappe.hybrows.workers.dev/?url=https://cdn.example.com/v.m3u8&sig=1", initiatorType: "fetch", startTime: 100 },
  ]);
  assertEq(runIn(ctx2, "findPlaylistFromResources()").url, "https://frappe.hybrows.workers.dev/?url=https://cdn.example.com/v.m3u8&sig=1");
});

// ---------- (c) decoupled scan: playlist with no <video> ----------
const REPORT_CONSTS = TIMING_CONSTS
  + extractConst(contentSrc, "overlayKeys", 2) + "\n"
  + extractConst(contentSrc, "overlayKeyCounter", 2) + "\n"
  + extractConst(contentSrc, "lastCandidatesKey", 2) + "\n"
  + extractConst(contentSrc, "EMBEDDED_URL_RE", 2) + "\n"
  + extractConst(contentSrc, "EMBEDDED_TEXT_MAX", 2) + "\n"
  + extractConst(contentSrc, "EMBEDDED_CACHE_MS", 2) + "\n"
  + extractConst(contentSrc, "embeddedCache", 2) + "\n";
function reportContext(entries, scripts) {
  const sent = [];
  const ctx = makeContext(
    {
      Date: Date,
      performance: { getEntriesByType: function () { return entries; }, now: function () { return 0; } },
      document: {
        baseURI: "https://example.com/watch",
        querySelectorAll: function (sel) {
          if (sel === "script") return scripts || [];
          return [];
        },
      },
    },
    [
      REPORT_CONSTS,
      "function sendRuntimeMessage(msg) { sent.push(msg); return { catch: function () {} }; }\n",
      extractMany(contentSrc, ["resolveUrl", "fileNameFromUrl", "manifestKind", "scanResourceStreams", "pickPlaylist", "findPlaylistFromResources", "isManifestUrl", "scanEmbeddedData", "overlayKeyFor", "isPlaying", "reportOverlayCandidates"], 2),
    ]
  );
  ctx.sent = sent;
  return { ctx: ctx, sent: sent };
}

test("(c) a playlist in the timing log is reported with no <video> element", () => {
  const r = reportContext([
    { name: "https://cdn.example.com/master.m3u8", initiatorType: "fetch", startTime: 100 },
  ], []);
  runIn(r.ctx, "reportOverlayCandidates(new Set())");
  assertEq(r.sent.length, 1, "a report must be sent");
  assertEq(r.sent[0].type, "overlayCandidates");
  assertEq(r.sent[0].candidates.length, 1, "one synthetic candidate");
  assertEq(r.sent[0].candidates[0].source.url, "https://cdn.example.com/master.m3u8");
  assertEq(r.sent[0].candidates[0].playing, true);
});

test("(c) with no playlist anywhere and no video, nothing spurious is reported", () => {
  const r = reportContext([], []);
  runIn(r.ctx, "reportOverlayCandidates(new Set())");
  assertEq(r.sent.length, 1, "the empty report still goes out (first report)");
  assertEq(r.sent[0].candidates.length, 0, "no synthetic candidate without a stream");
});

test("(c) embedded playlist fills the gap when the timing log is empty", () => {
  const r = reportContext([], [
    { src: "", type: "application/ld+json", textContent: '{"@type":"VideoObject","contentUrl":"https://cdn.example.com/stream/master.m3u8"}' },
  ]);
  runIn(r.ctx, "reportOverlayCandidates(new Set())");
  assertEq(r.sent[0].candidates.length, 1);
  assertEq(r.sent[0].candidates[0].source.url, "https://cdn.example.com/stream/master.m3u8");
});

// ---------- (d) webRequest observer classification ----------
const ctxD = makeContext(
  {},
  [
    extractConst(bgSrc, "BG_STREAM_EXT_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_MEDIA_FILE_EXT_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_PLAYLIST_MIME_RE", 0) + "\n",
    extractMany(bgSrc, ["classifyObservedRequest"], 0),
  ]
);
function classify(url, ct, type) {
  return runIn(ctxD, "classifyObservedRequest(" + JSON.stringify(url) + ", " + JSON.stringify(ct) + ", " + JSON.stringify(type) + ")");
}

test("(d) webRequest observer classifies a playlist by Content-Type", () => {
  assertEq(classify("https://novahd.cc/api/gs-hls?x=1", "application/vnd.apple.mpegurl", "xmlhttprequest").kind, "playlist");
  assertEq(classify("https://worker.dev/api/dash?id=9", "application/dash+xml; charset=utf-8", "fetch").kind, "playlist");
  assertEq(classify("https://cdn.example.com/master.m3u8", "text/html", "fetch").kind, "playlist", "URL alone is enough");
  assertEq(classify("https://frappe.hybrows.workers.dev/?url=https://cdn.example.com/v.m3u8&sig=1", "", "fetch").kind, "playlist", "wrapped playlist URL");
});

test("(d) webRequest observer rejects segments and non-media, accepts files", () => {
  assertEq(classify("https://cdn.example.com/seg_00001.ts", "video/mp2t", "xmlhttprequest"), null, "TS segment is not a file");
  assertEq(classify("https://example.com/page.html", "text/html", "document"), null);
  assertEq(classify("https://example.com/track.png", "image/png", "image"), null);
  assertEq(classify("ftp://example.com/v.mp4", "video/mp4", "media"), null, "non-http(s) rejected");
  assertEq(classify("https://cdn.example.com/film.mp4", "", "xmlhttprequest").kind, "file");
  assertEq(classify("https://cdn.example.com/stream/9", "video/mp4", "fetch").kind, "file", "content-type file");
  assertEq(classify("https://cdn.example.com/audio/9", "audio/mpeg", "fetch").kind, "file");
  assertEq(classify("https://cdn.example.com/media/9", "", "media").kind, "file", "media resourceType");
});

// ---------- (e) embedded-data scan ----------
function embeddedContext(scripts, metas) {
  return makeContext(
    {
      Date: Date,
      document: {
        baseURI: "https://example.com/watch",
        querySelectorAll: function (sel) {
          if (sel === "script") return scripts || [];
          if (sel.indexOf("meta") === 0) return metas || [];
          return [];
        },
      },
    },
    [
      extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n"
      + extractConst(contentSrc, "MEDIA_FILE_EXT_RE", 2) + "\n"
      + extractConst(contentSrc, "EMBEDDED_URL_RE", 2) + "\n"
      + extractConst(contentSrc, "EMBEDDED_TEXT_MAX", 2) + "\n"
      + extractConst(contentSrc, "EMBEDDED_CACHE_MS", 2) + "\n"
      + extractConst(contentSrc, "embeddedCache", 2) + "\n",
      extractMany(contentSrc, ["resolveUrl", "isManifestUrl", "scanEmbeddedData"], 2),
    ]
  );
}

test("(e) embedded scan finds a stream URL in page JSON", () => {
  const ctx = embeddedContext([
    { src: "", type: "application/ld+json", textContent: '{"@type":"VideoObject","contentUrl":"https://cdn.example.com/stream/master.m3u8"}' },
  ], []);
  const r = runIn(ctx, "scanEmbeddedData()");
  assertEq(r.playlist, "https://cdn.example.com/stream/master.m3u8");
});

test("(e) embedded scan finds a player-config playlist and an og:video file", () => {
  const ctx = embeddedContext([
    { src: "", type: "", textContent: 'jwplayer("p").setup({ file: "https://cdn.example.com/v/index.m3u8", image: "https://cdn.example.com/poster.jpg" });' },
  ], []);
  assertEq(runIn(ctx, "scanEmbeddedData()").playlist, "https://cdn.example.com/v/index.m3u8");
  const ctx2 = embeddedContext([], [
    { getAttribute: function (n) { return n === "content" ? "https://cdn.example.com/film.mp4" : null; } },
  ]);
  const r2 = runIn(ctx2, "scanEmbeddedData()");
  assertEq(r2.playlist, null);
  assertEq(r2.file, "https://cdn.example.com/film.mp4");
});

test("(e) embedded scan skips src scripts, over-cap scripts, and segments", () => {
  const big = "x".repeat(300000) + " https://cdn.example.com/big.m3u8";
  const ctx = embeddedContext([
    { src: "https://cdn.example.com/bundle.js", type: "", textContent: 'var u = "https://cdn.example.com/src.m3u8";' },
    { src: "", type: "", textContent: big },
    { src: "", type: "", textContent: 'var seg = "https://cdn.example.com/seg_00001.ts";' },
  ], []);
  const r = runIn(ctx, "scanEmbeddedData()");
  assertEq(r.playlist, null, "src + over-cap scripts must be skipped");
  assertEq(r.file, null, "a .ts segment is not a downloadable file");
});

// ---------- widened URL tests ----------
test("Round 9: STREAM_EXT_RE / isManifestUrl / isDashUrl match wrapped URLs", () => {
  const ctx = makeContext({}, [
    extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n",
    extractMany(contentSrc, ["isManifestUrl", "isDashUrl", "isVideoFileUrl"], 2),
  ]);
  assertEq(runIn(ctx, 'STREAM_EXT_RE.test("https://worker.dev/?url=https://cdn.example.com/v.m3u8&sig=1")'), true);
  assertEq(runIn(ctx, 'STREAM_EXT_RE.test("https://example.com/page")'), false);
  assertEq(runIn(ctx, 'isManifestUrl("https://worker.dev/?url=https://cdn.example.com/v.m3u8&sig=1")'), true);
  assertEq(runIn(ctx, 'isDashUrl("https://worker.dev/?url=https://cdn.example.com/v.mpd&sig=1")'), true);
  assertEq(runIn(ctx, 'isDashUrl("https://cdn.example.com/v.m3u8")'), false);
  assertEq(runIn(ctx, 'isVideoFileUrl("https://worker.dev/?url=https://cdn.example.com/f.mp4&sig=1")'), true);
});

// ---------- election merges observed findings ----------
const ctxE = makeContext(
  {},
  [
    "const tabOverlayCandidates = new Map();\nconst tabObserved = new Map();\n",
    extractConst(bgSrc, "BG_STREAM_EXT_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_MASTER_STEM_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_AUDIO_STEM_RE", 0) + "\n"
    + extractConst(bgSrc, "OBSERVED_TTL_MS", 0) + "\n",
    extractMany(bgSrc, ["bgManifestKind", "bestDownloadSource"], 0),
  ]
);
function setElection(frames, observed) {
  const fmap = new Map();
  for (const f of frames) {
    fmap.set(f.frameId, { candidates: f.candidates, frameUrl: f.frameUrl, at: Date.now() });
  }
  const omap = new Map();
  for (const o of observed) {
    omap.set(o.url, { url: o.url, kind: o.kind, initiator: o.initiator, at: Date.now() });
  }
  ctxE.frames = fmap;
  ctxE.observed = omap;
  runIn(ctxE, "tabOverlayCandidates.clear(); tabOverlayCandidates.set(7, frames); tabObserved.clear(); tabObserved.set(7, observed);");
  return runIn(ctxE, "bestDownloadSource(7, null)");
}

test("Round 9: an observed extensionless playlist beats a direct file", () => {
  const v = setElection(
    [{ frameId: 0, frameUrl: "https://novahd.cc/watch/m/1368337", candidates: [
      { key: "a", source: { url: "https://novahd.cc/assets/p/clip.mp4", filename: "clip.mp4" }, playing: true, duration: 40, area: 100000 },
    ]}],
    [{ url: "https://novahd.cc/api/gs-hls?x=1", kind: "playlist", initiator: "https://novahd.cc" }]
  );
  assert(v, "a verdict must be returned");
  assertEq(v.url, "https://novahd.cc/api/gs-hls?x=1", "observed playlist wins the playlist pool");
  assertEq(v.referer, "https://novahd.cc", "initiator is the referer");
});

test("Round 9: an observed finding from a previous page on the tab is filtered out", () => {
  const v = setElection(
    [{ frameId: 0, frameUrl: "https://example.com/other", candidates: [
      { key: "a", source: { url: "https://example.com/film.mp4", filename: "film.mp4" }, playing: true, duration: 7200, area: 100000 },
    ]}],
    [{ url: "https://novahd.cc/api/gs-hls?x=1", kind: "playlist", initiator: "https://novahd.cc" }]
  );
  assertEq(v.url, "https://example.com/film.mp4", "stale-origin observation must not win");
});

test("Round 9: observed findings alone (no frames) still produce a verdict", () => {
  const v = setElection([], [
    { url: "https://cdn.example.com/master.m3u8", kind: "playlist", initiator: "https://example.com" },
  ]);
  assert(v, "observed-only verdict must exist");
  assertEq(v.url, "https://cdn.example.com/master.m3u8");
});

run("round9.test.js");
