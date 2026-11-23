// Round 7 background-script harness: Fix 4 (viaBrowser reply
// passthrough, end to end through sendDownload) and Fix 6
// (pageVideoDuration in the election verdict).
// The Fix 4 test evaluates the WHOLE background.js with a stubbed
// chrome API; the Fix 6 test extracts bestDownloadSource (+ its
// helpers) from the shipped file — see extract.js.
"use strict";
const vm = require("vm");
const { readSource, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const bgSrc = readSource("extensions/chromium/background.js");

// ---------- stub chrome API (Fix 4) ----------
function makeChromeStub() {
  const store = {};
  const chrome = {
    runtime: {
      lastError: undefined,
      onInstalled: { addListener() {} },
      onMessage: { addListener(fn) { chrome.__onMessage = fn; } },
      sendNativeMessage: (host, msg, cb) => {
        // No native host installed: Chrome sets lastError and
        // calls back with undefined, so callApi rejects.
        chrome.runtime.lastError = { message: "Specified native messaging host not found." };
        cb(undefined);
      },
    },
    storage: {
      local: {
        get: (key, cb) => {
          chrome.runtime.lastError = undefined;
          if (typeof key === "string") cb(store[key] !== undefined ? { [key]: store[key] } : {});
          else cb({ ...store });
        },
        set: (items, cb) => {
          chrome.runtime.lastError = undefined;
          Object.assign(store, items);
          if (cb) cb();
        },
      },
    },
    downloads: {
      download: (opts, cb) => {
        chrome.runtime.lastError = undefined;
        chrome.__downloads.push(opts);
        if (chrome.__downloadsShouldFail) throw new Error("download failed");
        if (cb) cb(42);
      },
    },
    tabs: {
      onActivated: { addListener() {} },
      onRemoved: { addListener() {} },
      sendMessage: () => {},
    },
    contextMenus: {
      onClicked: { addListener() {} },
      create: () => {},
    },
    action: { setBadgeText: () => {}, setBadgeBackgroundColor: () => {} },
    __downloads: [],
    __downloadsShouldFail: false,
    __onMessage: null,
  };
  return chrome;
}

function loadBackground() {
  const chrome = makeChromeStub();
  const ctx = makeContext({ chrome: chrome }, [bgSrc]);
  assert(chrome.__onMessage, "background must register an onMessage listener");
  return { chrome: chrome, ctx: ctx };
}

function sendMessage(chrome, msg) {
  return new Promise((resolve) => {
    const sender = { tab: { id: 7 }, url: "https://example.com/watch" };
    chrome.__onMessage(msg, sender, resolve);
  });
}

test("Fix 4: downloadMedia reply carries viaBrowser when the browser downloads", async () => {
  const { chrome } = loadBackground();
  const reply = await sendMessage(chrome, {
    type: "downloadMedia",
    url: "https://example.com/v.mp4",
    filename: "v.mp4",
  });
  assertEq(reply.ok, true);
  assertEq(reply.viaBrowser, true, "the reply must say the browser downloaded it");
  assertEq(chrome.__downloads.length, 1, "the browser-download fallback must have fired");
  assertEq(chrome.__downloads[0].url, "https://example.com/v.mp4");
});

test("Fix 4: sendUrl reply also carries viaBrowser", async () => {
  const { chrome } = loadBackground();
  const reply = await sendMessage(chrome, { type: "sendUrl", url: "https://example.com/v.mp4" });
  assertEq(reply.ok, true);
  assertEq(reply.viaBrowser, true);
});

test("Fix 4: total failure still reports an honest error", async () => {
  const { chrome } = loadBackground();
  chrome.__downloadsShouldFail = true;
  const reply = await sendMessage(chrome, {
    type: "downloadMedia",
    url: "https://example.com/v.mp4",
    filename: "v.mp4",
  });
  assertEq(reply.ok, false);
  assert(reply.error && reply.error.indexOf("Could not reach") !== -1, "error: " + reply.error);
});

// ---------- bestDownloadSource (Fix 6) ----------
const ctxB = makeContext(
  {},
  [
    "const tabOverlayCandidates = new Map();\n",
    extractConst(bgSrc, "BG_STREAM_EXT_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_MASTER_STEM_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_AUDIO_STEM_RE", 0) + "\n",
    extractMany(bgSrc, ["bgManifestKind", "bestDownloadSource"], 0),
  ]
);
function setFrames(frames) {
  // frames: [{ frameId, frameUrl, candidates: [{ key, source, playing, duration, area }] }]
  const map = new Map();
  for (const f of frames) {
    map.set(f.frameId, { candidates: f.candidates, frameUrl: f.frameUrl, at: Date.now() });
  }
  ctxB.frames = map;
  runIn(ctxB, "tabOverlayCandidates.clear(); tabOverlayCandidates.set(7, frames);");
  return runIn(ctxB, "bestDownloadSource(7, null)");
}

test("Fix 6: verdict carries the longest video duration across frames", () => {
  const v = setFrames([
    { frameId: 0, frameUrl: "https://novahd.cc/watch/m/1368337", candidates: [
      { key: "a", source: { url: "https://novahd.cc/assets/p/41876ddc05627e720d53f4a2.mp4", filename: "41876ddc05627e720d53f4a2.mp4" }, playing: true, duration: 15, area: 100000 },
    ]},
    { frameId: 1, frameUrl: "https://player.example.com/e", candidates: [
      { key: "b", source: { url: "https://cdn.example.com/film.mp4", filename: "film.mp4" }, playing: true, duration: 10356, area: 50000 },
    ]},
  ]);
  assert(v, "a verdict must be returned");
  assertEq(v.pageVideoDuration, 10356);
});

test("Fix 6: a paused long video still proves a long-form page", () => {
  const v = setFrames([
    { frameId: 0, frameUrl: "https://example.com/watch", candidates: [
      { key: "a", source: { url: "https://example.com/clip.mp4", filename: "clip.mp4" }, playing: true, duration: 15, area: 100000 },
      { key: "b", source: { url: "https://example.com/film.mp4", filename: "film.mp4" }, playing: false, duration: 7200, area: 200000 },
    ]},
  ]);
  assertEq(v.pageVideoDuration, 7200);
});

test("Fix 6: playlist verdict still wins and still carries the duration", () => {
  const v = setFrames([
    { frameId: 0, frameUrl: "https://example.com/watch", candidates: [
      { key: "a", source: { url: "https://example.com/clip.mp4", filename: "clip.mp4" }, playing: true, duration: 15, area: 100000 },
    ]},
    { frameId: 1, frameUrl: "https://player.example.com/e", candidates: [
      { key: "b", source: { url: "https://cdn.example.com/master.m3u8", filename: "master.m3u8" }, playing: true, duration: 10356, area: 50000 },
    ]},
  ]);
  assertEq(v.url, "https://cdn.example.com/master.m3u8", "playlist anywhere beats direct file anywhere");
  assertEq(v.pageVideoDuration, 10356);
});

test("Fix 6: no candidates means no verdict", () => {
  const v = setFrames([]);
  assertEq(v, null);
});

run("background.test.js");
