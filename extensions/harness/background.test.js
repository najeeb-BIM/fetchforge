// Round 7 background-script harness: Fix 4 (viaBrowser reply
// passthrough, end to end through sendDownload) and Fix 6
// (pageVideoDuration in the election verdict).
// Round 8: audioOnly passthrough (downloadMedia -> native/bridge)
// and getFormats unreachable classification.
// The Fix 4 test evaluates the WHOLE background.js with a stubbed
// chrome API; the Fix 6 test extracts bestDownloadSource (+ its
// helpers) from the shipped file — see extract.js.
"use strict";
const vm = require("vm");
const nodeCrypto = require("crypto");
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
        chrome.__nativeCalls.push({ host: host, msg: msg });
        if (chrome.__nativeShouldSucceed) {
          // A working native host: the bridge accepted the add.
          chrome.runtime.lastError = undefined;
          cb({ type: "added", download: { status: "queued" } });
          return;
        }
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
    __nativeCalls: [],
    __nativeShouldSucceed: false,
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

test("Round 8: downloadMedia audioOnly reaches the native host end to end", async () => {
  const { chrome } = loadBackground();
  chrome.__nativeShouldSucceed = true;
  const reply = await sendMessage(chrome, {
    type: "downloadMedia",
    url: "https://example.com/v.mp4",
    filename: "v.mp4",
    audioOnly: true,
  });
  assertEq(reply.ok, true);
  assertEq(chrome.__nativeCalls.length, 1, "the native path must take the send");
  assertEq(chrome.__nativeCalls[0].msg.audio_only, true, "the flag must reach the native host");
  assertEq(chrome.__downloads.length, 0, "no browser fallback when the app takes it");
  // A video send carries the flag as false (never undefined).
  const reply2 = await sendMessage(chrome, {
    type: "downloadMedia",
    url: "https://example.com/v2.mp4",
    filename: "v2.mp4",
    audioOnly: false,
  });
  assertEq(reply2.ok, true);
  assertEq(chrome.__nativeCalls[1].msg.audio_only, false);
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

// ---------- sendViaBridge + getFormats (Round 8) ----------
let token8 = "test-token-123";
let fetchBehaviour8 = "ok"; // ok | formats-500 | formats-throw | challenge-bad | challenge-throw
const fetchCalls8 = [];
const ctx8 = makeContext(
  {
    chrome: {
      runtime: { lastError: undefined },
      storage: {
        local: {
          get: (key, cb) => { cb(token8 ? { token: token8 } : {}); },
          set: (items, cb) => { if (cb) cb(); },
        },
      },
    },
    crypto: nodeCrypto.webcrypto,
    fetch: async (url, opts) => {
      fetchCalls8.push({ url: url, body: opts && opts.body });
      if (url.endsWith("/challenge")) {
        if (fetchBehaviour8 === "challenge-throw") throw new TypeError("Failed to fetch");
        const nonce = JSON.parse(opts.body).nonce;
        const hmac = fetchBehaviour8 === "challenge-bad"
          ? "00".repeat(32)
          : nodeCrypto.createHmac("sha256", token8).update(nonce).digest("hex");
        return { ok: true, json: async () => ({ hmac: hmac }) };
      }
      if (url.endsWith("/media/formats")) {
        if (fetchBehaviour8 === "formats-throw") throw new TypeError("Failed to fetch");
        if (fetchBehaviour8 === "formats-500") return { ok: false, status: 500, json: async () => ({}) };
        return { ok: true, json: async () => ({ formats: [{ format_id: "v1" }], default_format_id: "v1" }) };
      }
      if (url.endsWith("/downloads")) {
        return { ok: true, json: async () => ({ status: "queued" }) };
      }
      return { ok: false, status: 404, json: async () => ({}) };
    },
  },
  [
    extractConst(bgSrc, "api", 0) + "\n"
    + extractConst(bgSrc, "isFirefox", 0) + "\n"
    + extractConst(bgSrc, "BRIDGE", 0) + "\n"
    + extractConst(bgSrc, "HOST", 0) + "\n",
    extractMany(bgSrc, ["callApi", "getToken", "verifyServer", "sendViaBridge", "getFormats"], 0),
  ]
);
function reset8(behaviour) {
  token8 = "test-token-123";
  fetchBehaviour8 = behaviour || "ok";
  fetchCalls8.length = 0;
}

test("Round 8: sendViaBridge carries audio_only in the POST body", async () => {
  reset8();
  const r = await runIn(ctx8, "sendViaBridge('https://example.com/v.mp4', 'v.mp4', undefined, undefined, undefined, undefined, true)");
  assertEq(r.ok, true);
  const post = fetchCalls8.find((c) => c.url.endsWith("/downloads"));
  assert(post, "the bridge POST must have happened");
  assertEq(JSON.parse(post.body).audio_only, true);
  reset8();
  await runIn(ctx8, "sendViaBridge('https://example.com/v.mp4', 'v.mp4', undefined, undefined, undefined, undefined, false)");
  const post2 = fetchCalls8.find((c) => c.url.endsWith("/downloads"));
  assertEq(JSON.parse(post2.body).audio_only, false);
});

test("Round 8: getFormats succeeds when the bridge answers", async () => {
  reset8();
  const j = await runIn(ctx8, "getFormats('https://example.com/watch')");
  assertEq(j.formats.length, 1);
});

test("Round 8: getFormats marks no-token, challenge and network failures unreachable", async () => {
  reset8();
  token8 = "";
  let err = await runIn(ctx8, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
  assertEq(err.message, "no-token");
  assertEq(err.unreachable, true);
  reset8("challenge-bad");
  err = await runIn(ctx8, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
  assertEq(err.message, "challenge-failed");
  assertEq(err.unreachable, true);
  reset8("challenge-throw");
  err = await runIn(ctx8, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
  assertEq(err.unreachable, true, "a dead bridge on the challenge is unreachable");
  reset8("formats-throw");
  err = await runIn(ctx8, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
  assertEq(err.unreachable, true, "a fetch TypeError is unreachable");
});

test("Round 8: getFormats bridge-500 is reachable (listing failed, not the bridge)", async () => {
  reset8("formats-500");
  const err = await runIn(ctx8, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
  assertEq(err.message, "bridge-500");
  assertEq(err.unreachable, undefined, "a server-side failure must NOT be flagged unreachable");
});

run("background.test.js");
