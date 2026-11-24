// Round 10 harness: observed-playlist popup routing (item 1),
// send confirmation + popup send errors (items 5/7), source
// ranking (item 8), formats dedupe (item 10), junk-title
// auto-fill (item 11), formats-failure reasons (item 12),
// browser identity for the paired count (item 15), and the
// 0.2.10 manifest + chromium/firefox byte-identity checks.
// Every function under test is the exact source extracted from
// extensions/chromium/content.js / background.js — see extract.js.
"use strict";
const fs = require("fs");
const path = require("path");
const nodeCrypto = require("crypto");
const { readSource, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");
const tick = () => new Promise((r) => setTimeout(r, 10));

// ---------- Context A: pure content helpers (items 8/10/11) ----------
const docA = {
  title: "",
  baseURI: "https://example.com/watch",
  querySelector: function (sel) { return docA._qs[sel] || null; },
  _qs: {},
};
const ctxA = makeContext(
  { document: docA, window: {} },
  [
    "const capturedManifests = new Map();\n"
    + "function findPlaylistFromResources() { return null; }\n"
    + "function findStreamUrlFromResources() { return null; }\n",
    extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n",
    extractMany(contentSrc, ["qualityHintScore", "mediaSource", "sameDirectoryEncodes", "resolveUrl", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "sanitizeFilenameStem", "popupDefaultName", "stemOfFilename", "sortFormatsForPopup"], 2),
  ]
);
function setDocA(title, qs) {
  docA.title = title;
  docA._qs = qs || {};
}
function fakeVideo(currentSrc, sourceSrcs) {
  return {
    currentSrc: currentSrc || "",
    getAttribute: function () { return null; },
    querySelectorAll: function () {
      return (sourceSrcs || []).map(function (u) {
        return { getAttribute: function () { return u; }, src: u };
      });
    },
  };
}
function pickSource(currentSrc, sourceSrcs) {
  ctxA.el = fakeVideo(currentSrc, sourceSrcs);
  const out = runIn(ctxA, "mediaSource(el)");
  return out && out.url;
}

test("Item 11: junk titles are rejected (notification, error page, site tagline)", () => {
  setDocA("(1) New Message!");
  assertEq(runIn(ctxA, "pageTitleStem()"), "", "fake notification title");
  setDocA("Something's not right");
  assertEq(runIn(ctxA, "pageTitleStem()"), "", "Roku error page");
  setDocA("Movies Online for Free at Lookmovie2");
  assertEq(runIn(ctxA, "pageTitleStem()"), "", "LookMovie generic title");
  setDocA("Soap2Day - Watch Movies & TV Series Online For Free");
  assertEq(runIn(ctxA, "pageTitleStem()"), "", "Soap2Day tagline");
  setDocA("404 - Page not found");
  assertEq(runIn(ctxA, "pageTitleStem()"), "", "404 page");
});

test("Item 11: real titles still survive the junk check", () => {
  setDocA("The Odyssey (2026) | NovaHD");
  assertEq(runIn(ctxA, "pageTitleStem()"), "The Odyssey (2026)");
  setDocA("Watch The Odyssey (2026) | NovaHD");
  assertEq(runIn(ctxA, "pageTitleStem()"), "The Odyssey (2026)");
  setDocA("Dune: Part Two (2024)");
  assertEq(runIn(ctxA, "pageTitleStem()"), "Dune_ Part Two (2024)", "colon is sanitised, title kept");
});

test("Item 11: pageNameHint falls through video title, h1, og:title", () => {
  setDocA("Something's not right", {
    "video": { getAttribute: function (n) { return n === "title" ? "Night of the Living Dead" : null; } },
    "h1": { textContent: "H1 Title" },
  });
  assertEq(runIn(ctxA, "pageNameHint()"), "Night of the Living Dead", "video title wins");
  setDocA("Something's not right", { "h1": { textContent: "Night of the Living Dead" } });
  assertEq(runIn(ctxA, "pageNameHint()"), "Night of the Living Dead", "h1 next");
  setDocA("Something's not right", {
    "meta[property=\"og:title\"]": { getAttribute: function () { return "Night of the Living Dead"; } },
  });
  assertEq(runIn(ctxA, "pageNameHint()"), "Night of the Living Dead", "og:title last");
  setDocA("Something's not right", { "h1": { textContent: "Something's not right" } });
  assertEq(runIn(ctxA, "pageNameHint()"), "", "a junk h1 is skipped, not returned");
  setDocA("Something's not right", {});
  assertEq(runIn(ctxA, "pageNameHint()"), "", "no hint anywhere");
});

test("Item 11: popupDefaultName falls through junk title to the hint, then the verdict stem", () => {
  setDocA("Movies Online for Free at Lookmovie2", { "h1": { textContent: "In the Lost Lands" } });
  ctxA.dl = { filename: "index_287.ts" };
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "In the Lost Lands");
  setDocA("Movies Online for Free at Lookmovie2", {});
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "index_287", "no hint: verdict stem");
  setDocA("The Odyssey (2026) | NovaHD", { "h1": { textContent: "Wrong H1" } });
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "The Odyssey (2026)", "a good title still beats the hint");
});

test("Item 8: qualityHintScore ranks heights over bitrates", () => {
  assert(runIn(ctxA, "qualityHintScore('https://x/1080p.mp4')") > runIn(ctxA, "qualityHintScore('https://x/720p.mp4')"), "1080p > 720p");
  assert(runIn(ctxA, "qualityHintScore('https://x/720p.mp4')") > runIn(ctxA, "qualityHintScore('https://x/512kb.mp4')"), "720p > 512kb");
  assert(runIn(ctxA, "qualityHintScore('https://x/4k.mp4')") > runIn(ctxA, "qualityHintScore('https://x/1080p.mp4')"), "4k > 1080p");
  assertEq(runIn(ctxA, "qualityHintScore('https://x/movie.mp4')"), 0, "no hint scores 0");
  assert(runIn(ctxA, "qualityHintScore('https://x/1920x1080.mp4')") > runIn(ctxA, "qualityHintScore('https://x/1280x720.mp4')"), "1920x1080 > 1280x720");
});

test("Item 8: without a playing source, the best-ranked encode wins (Archive shape)", () => {
  const url = pickSource("", [
    "https://archive.org/download/Night.Of.The.Living.Dead_1080p/NightOfTheLivingDead_1080p_512kb.mp4",
    "https://archive.org/download/Night.Of.The.Living.Dead_1080p/NightOfTheLivingDead_1080p.mp4",
  ]);
  // Round 11, item 4: hints are read from the filename only, so
  // the 512kb file scores 512 (its "1080p" is followed by "_",
  // no word boundary) and the plain 1080p file scores 11080 —
  // the better encode wins. (Under round 10's full-URL scoring
  // both tied at 11080 on the directory's "1080p" and document
  // order stood, keeping the worse 512kb file.)
  const ranked = pickSource("", [
    "https://example.com/film_512kb.mp4",
    "https://example.com/film_1080p.mp4",
  ]);
  assertEq(ranked, "https://example.com/film_1080p.mp4", "1080p beats the first-listed 512kb");
  assertEq(url, "https://archive.org/download/Night.Of.The.Living.Dead_1080p/NightOfTheLivingDead_1080p.mp4", "the better Archive encode wins");
  // Genuinely hint-less URLs keep document order.
  const plain = pickSource("", [
    "https://example.com/a.mp4",
    "https://example.com/b.mp4",
  ]);
  assertEq(plain, "https://example.com/a.mp4", "hint-less URLs keep document order");
});

test("Item 8: the playing source always wins; a blob currentSrc does not count", () => {
  const playing = pickSource("https://example.com/film_720p.mp4", [
    "https://example.com/film_1080p.mp4",
  ]);
  assertEq(playing, "https://example.com/film_720p.mp4", "currentSrc is what is playing");
  const blob = pickSource("blob:https://example.com/abc", [
    "https://example.com/film_512kb.mp4",
    "https://example.com/film_1080p.mp4",
  ]);
  assertEq(blob, "https://example.com/film_1080p.mp4", "MSE blob: fallbacks are ranked");
});

test("Item 10: sortFormatsForPopup dedupes by format id, first occurrence wins", () => {
  ctxA.formats = [
    { format_id: "140", label: "Audio only · m4a · 129 kbps", kind: "audio_only" },
    { format_id: "140", label: "Audio only · m4a · 129 kbps", kind: "audio_only" },
    { format_id: "137", label: "1080p", height: 1080, kind: "combined" },
    { format_id: "137", label: "1080p", height: 1080, kind: "combined" },
    { format_id: "136", label: "720p", height: 720, kind: "combined" },
  ];
  const out = runIn(ctxA, "sortFormatsForPopup(formats)");
  assertEq(out.length, 3, "two duplicates removed");
  assertEq(out[0].format_id, "137", "ladder order kept: 1080 first");
  assertEq(out[1].format_id, "136");
  assertEq(out[2].format_id, "140", "audio last");
});

test("Item 10 (round 11): sortFormatsForPopup dedupes measured rows by visible identity; measurement-less rows stay", () => {
  ctxA.formats = [
    // The same stream under two ids (YouTube 140 / 140-drc):
    // identical label + kind + bitrate + size collapses.
    { label: "Audio only", kind: "audio_only", abr: 128, filesize: 10000000, format_id: "140" },
    { label: "Audio only", kind: "audio_only", abr: 128, filesize: 10000000, format_id: "140-drc" },
    { label: "Audio only", kind: "combined", abr: 128, filesize: 10000000 },
    // No measurement at all: too thin to prove identical
    // (YouTube HLS 233/234 shape) — both survive.
    { label: "Audio only", kind: "audio_only" },
    { label: "Audio only", kind: "audio_only" },
  ];
  const out = runIn(ctxA, "sortFormatsForPopup(formats)");
  assertEq(out.length, 4, "140/140-drc collapse; different kind stays; measurement-less pair stays");
});

// ---------- Context B: sendCurrentDownload + pickAndSend (items 5/7) ----------
let nextReplyB = { ok: true };
const sendCallsB = [];
const timerCallsB = [];
const clearedB = [];
let nextTimerIdB = 0;
const ctxB = makeContext(
  {
    document: { title: "The Odyssey | NovaHD", querySelector: function () { return null; } },
    window: {
      setTimeout: function (fn, ms) { const id = ++nextTimerIdB; timerCallsB.push({ id: id, ms: ms }); return id; },
      clearTimeout: function (id) { clearedB.push(id); },
    },
    sendRuntimeMessage: function (msg) { sendCallsB.push(msg); return Promise.resolve(nextReplyB); },
    sendRevertTimer: null,
  },
  [
    "let popupOpen = false;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet floatOverlay = null;\n"
    + "globalThis.__popupOpen = function () { return popupOpen; };\n",
    extractMany(contentSrc, ["sendCurrentDownload", "pickAndSend", "showPopupSendError", "closeQualityPopup", "currentPopupFilename", "defaultFilenameFor", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "stemOfFilename", "sanitizeFilenameStem", "isManifestUrl", "shortFailureReason"], 2),
  ]
);
function makeOverlayB(filename) {
  return {
    popup: { style: { display: "flex" } },
    popupTitle: { textContent: "Choose quality" },
    popupSubtitle: { textContent: "", style: { display: "none" } },
    popupBody: { textContent: "" },
    filenameInput: { value: filename || "The Odyssey (2026)" },
  };
}
function resetB(reply) {
  nextReplyB = reply;
  sendCallsB.length = 0;
  timerCallsB.length = 0;
  clearedB.length = 0;
  nextTimerIdB = 0;
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxB.btn = btn;
  ctxB.dl = { url: "https://example.com/v.mp4", filename: "v.mp4" };
  runIn(ctxB, "popupOpen = false; popupRows = []; sendRevertTimer = null;");
  return btn;
}

test("Item 7: sendCurrentDownload returns a promise resolving to the response", async () => {
  resetB({ ok: true });
  const p = runIn(ctxB, "sendCurrentDownload(btn, dl, {})");
  assert(p && typeof p.then === "function", "a promise must be returned");
  const resp = await p;
  assertEq(resp.ok, true);
  const btn = ctxB.btn;
  assertEq(btn.textContent, "Sent to FetchForge");
});

test("Item 7: the Sent display lasts 10 seconds, not 5", async () => {
  resetB({ ok: true });
  runIn(ctxB, "sendCurrentDownload(btn, dl, {})");
  await tick();
  assertEq(timerCallsB.length, 1, "one revert timer scheduled");
  assertEq(timerCallsB[0].ms, 10000, "the revert delay must be 10 s");
});

test("Item 7: a second send clears the first send's pending revert timer", async () => {
  resetB({ ok: true });
  runIn(ctxB, "sendCurrentDownload(btn, dl, {})");
  await tick();
  assertEq(timerCallsB.length, 1);
  const firstId = timerCallsB[0].id;
  runIn(ctxB, "sendCurrentDownload(btn, dl, {})");
  await tick();
  assert(clearedB.indexOf(firstId) !== -1, "the stale timer must be cleared at send start");
  assertEq(timerCallsB.length, 2, "the second send schedules its own timer");
});

test("Item 5: pickAndSend keeps the popup open and shows the error on failure", async () => {
  const btn = resetB({ ok: false, error: "This page is not a supported video or media page" });
  const overlay = makeOverlayB();
  ctxB.overlay = overlay;
  runIn(ctxB, "floatOverlay = overlay; popupOpen = true;");
  runIn(ctxB, "pickAndSend(btn, dl, {})");
  await tick();
  assertEq(runIn(ctxB, "__popupOpen()"), true, "the popup must stay open on failure");
  assertEq(overlay.popupTitle.textContent, "Download failed");
  assert(overlay.popupSubtitle.textContent.indexOf("not a supported video") !== -1, "the engine's reason is shown: " + overlay.popupSubtitle.textContent);
  assertEq(overlay.popupSubtitle.style.display, "block");
  assert(sendCallsB.length === 1, "the send happened");
  assertEq(sendCallsB[0].filename, "The Odyssey (2026)", "pickAndSend fills the filename from the field");
});

test("Item 5: pickAndSend closes the popup on success and on duplicate", async () => {
  resetB({ ok: true });
  const overlay = makeOverlayB();
  ctxB.overlay = overlay;
  runIn(ctxB, "floatOverlay = overlay; popupOpen = true;");
  runIn(ctxB, "pickAndSend(btn, dl, {})");
  await tick();
  assertEq(runIn(ctxB, "__popupOpen()"), false, "success closes the popup");
  resetB({ ok: true, duplicate: true });
  ctxB.overlay = overlay;
  runIn(ctxB, "floatOverlay = overlay; popupOpen = true;");
  runIn(ctxB, "pickAndSend(btn, dl, {})");
  await tick();
  assertEq(runIn(ctxB, "__popupOpen()"), false, "duplicate closes the popup (the button reports it)");
});

test("Item 5: showPopupSendError does nothing when the popup is already closed", () => {
  resetB({ ok: false, error: "boom" });
  const overlay = makeOverlayB();
  ctxB.overlay = overlay;
  runIn(ctxB, "floatOverlay = overlay; popupOpen = false;");
  runIn(ctxB, "showPopupSendError('boom')");
  assertEq(overlay.popupTitle.textContent, "Choose quality", "a closed popup is not resurrected");
});

// ---------- Context C: onOverlayClick kind routing (item 1) ----------
const ctxC = makeContext(
  {
    document: { title: "The Odyssey | NovaHD" },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
    fetch: async () => { throw new Error("no probe expected"); },
  },
  [
    "let currentDownload = null;\nlet popupOpen = false;\n"
    + "const __calls = { hls: [], formats: [], simple: [], send: [], close: 0 };\n"
    + "globalThis.__calls = __calls;\n"
    + "function closeQualityPopup() { __calls.close++; popupOpen = false; }\n"
    + "function openHlsPopup(btn, dl, parsed) { __calls.hls.push({ btn: btn, dl: dl, parsed: parsed }); popupOpen = true; }\n"
    + "function openFormatsPopup(btn, dl) { __calls.formats.push({ btn: btn, dl: dl }); popupOpen = true; }\n"
    + "function openSimplePopup(btn, dl) { __calls.simple.push({ btn: btn, dl: dl }); popupOpen = true; }\n"
    + "function sendCurrentDownload(btn, dl, opts) { __calls.send.push({ btn: btn, dl: dl, opts: opts }); }\n",
    extractMany(contentSrc, ["onOverlayClick", "parseHlsMaster", "parseHlsAttrs", "resolveAgainst", "friendlyCodec", "formatBandwidth", "hlsVariantHeight", "probeDirectFileDuration", "parseMp4Duration", "isManifestUrl", "isDashUrl", "isMp4FamilyUrl", "isVideoFileUrl", "isOneClickFileUrl"], 2),
  ]
);
function clickC(dl) {
  runIn(ctxC, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.simple.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxC.btn = btn;
  ctxC.dl = dl;
  runIn(ctxC, "currentDownload = dl; onOverlayClick(btn);");
  return runIn(ctxC, "__calls");
}

test("Item 1: an observed extensionless playlist opens the formats popup, never one-click", async () => {
  const calls = clickC({ url: "https://cdn.example.com/stream/abc123", filename: "abc123", kind: "playlist" });
  await tick();
  assertEq(calls.formats.length, 1, "playlist kind routes to the formats popup");
  assertEq(calls.send.length, 0, "no silent send of playlist text");
  assertEq(calls.simple.length, 0);
});

test("Item 1: a named file-kind verdict (image) keeps the one-click send; a master still wins over kind", async () => {
  const file = clickC({ url: "https://example.com/photo.jpg", filename: "photo.jpg", kind: "file" });
  await tick();
  assertEq(file.send.length, 1, "a file verdict still one-click sends");
  const master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\nmid.m3u8\n";
  const hls = clickC({ url: "https://example.com/master.m3u8", filename: "master.m3u8", kind: "playlist", masterText: master });
  await tick();
  assertEq(hls.hls.length, 1, "captured master text still opens the HLS popup first");
  assertEq(hls.formats.length, 0);
});

// ---------- Context D: background verdict carries kind (item 1) ----------
const ctxD = makeContext(
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
function verdictD(frames, observed) {
  const fmap = new Map();
  for (const f of (frames || [])) {
    fmap.set(f.frameId, { candidates: f.candidates, frameUrl: f.frameUrl, at: Date.now() });
  }
  const omap = new Map();
  for (const [k, rec] of Object.entries(observed || {})) omap.set(k, rec);
  ctxD.fmap = fmap;
  ctxD.omap = omap;
  runIn(ctxD, "tabOverlayCandidates.clear(); tabObserved.clear(); tabOverlayCandidates.set(7, fmap); tabObserved.set(7, omap);");
  return runIn(ctxD, "bestDownloadSource(7, null)");
}

test("Item 1: the verdict carries the observed finding's kind (playlist)", () => {
  const v = verdictD([], {
    k1: { url: "https://cdn.example.com/stream/abc123", kind: "playlist", at: Date.now(), initiator: "" },
  });
  assert(v, "an observed playlist alone yields a verdict");
  assertEq(v.url, "https://cdn.example.com/stream/abc123");
  assertEq(v.kind, "playlist", "the kind must survive the election");
});

test("Item 1: frame candidates get their kind from the URL when no kind was classified", () => {
  const pl = verdictD([
    { frameId: 0, frameUrl: "https://example.com/watch", candidates: [
      { key: "a", source: { url: "https://cdn.example.com/master.m3u8", filename: "master.m3u8" }, playing: true, duration: 10356, area: 50000 },
    ]},
  ], {});
  assertEq(pl.kind, "playlist", "a .m3u8 frame candidate is a playlist");
  const file = verdictD([
    { frameId: 0, frameUrl: "https://example.com/watch", candidates: [
      { key: "a", source: { url: "https://example.com/film.mp4", filename: "film.mp4" }, playing: true, duration: 10356, area: 50000 },
    ]},
  ], {});
  assertEq(file.kind, "file", "an mp4 frame candidate is a file");
});

// ---------- Context E: getFormats carries the bridge's reason (item 12) ----------
let tokenE = "test-token-123";
let fetchBehaviourE = "ok"; // ok | formats-400-error | formats-500-empty | formats-500-badjson | formats-throw
const ctxE = makeContext(
  {
    chrome: {
      runtime: { lastError: undefined },
      storage: {
        local: {
          get: (key, cb) => { cb(tokenE ? { token: tokenE } : {}); },
          set: (items, cb) => { if (cb) cb(); },
        },
      },
    },
    crypto: nodeCrypto.webcrypto,
    fetch: async (url, opts) => {
      if (url.endsWith("/challenge")) {
        const nonce = JSON.parse(opts.body).nonce;
        const hmac = nodeCrypto.createHmac("sha256", tokenE).update(nonce).digest("hex");
        return { ok: true, json: async () => ({ hmac: hmac }) };
      }
      if (url.endsWith("/media/formats")) {
        if (fetchBehaviourE === "formats-throw") throw new TypeError("Failed to fetch");
        if (fetchBehaviourE === "formats-400-error") {
          return { ok: false, status: 400, json: async () => ({ error: "YouTube asked for a sign-in — try again later" }) };
        }
        if (fetchBehaviourE === "formats-500-empty") {
          return { ok: false, status: 500, json: async () => ({}) };
        }
        if (fetchBehaviourE === "formats-500-badjson") {
          return { ok: false, status: 500, json: async () => { throw new Error("bad json"); } };
        }
        return { ok: true, json: async () => ({ formats: [{ format_id: "v1" }], default_format_id: "v1" }) };
      }
      return { ok: false, status: 404, json: async () => ({}) };
    },
  },
  [
    extractConst(bgSrc, "api", 0) + "\n"
    + extractConst(bgSrc, "isFirefox", 0) + "\n"
    + extractConst(bgSrc, "BRIDGE", 0) + "\n"
    + extractConst(bgSrc, "HOST", 0) + "\n",
    extractMany(bgSrc, ["callApi", "getToken", "verifyServer", "getFormats"], 0),
  ]
);
async function getFormatsErr(behaviour) {
  fetchBehaviourE = behaviour;
  return runIn(ctxE, "getFormats('https://example.com/watch').then(() => null, (e) => e)");
}

test("Item 12: getFormats carries the bridge's error text as the reason", async () => {
  const err = await getFormatsErr("formats-400-error");
  assertEq(err.message, "bridge-400");
  assertEq(err.reason, "YouTube asked for a sign-in — try again later");
  assertEq(err.unreachable, undefined, "a bridge error body is reachable, not unreachable");
});

test("Item 12: an empty or unreadable error body gets a fallback reason", async () => {
  let err = await getFormatsErr("formats-500-empty");
  assertEq(err.message, "bridge-500");
  assert(err.reason && err.reason.indexOf("HTTP 500") !== -1, "fallback names the status: " + err.reason);
  err = await getFormatsErr("formats-500-badjson");
  assert(err.reason && err.reason.indexOf("HTTP 500") !== -1, "unparseable body still gets the fallback: " + err.reason);
});

test("Item 12: unreachable failures carry no reason (the error state handles them)", async () => {
  const err = await getFormatsErr("formats-throw");
  assertEq(err.unreachable, true);
  assertEq(err.reason, undefined);
});

// ---------- Context F: browser identity (item 15) ----------
const storeF = {};
let uaF = "";
const ctxF = makeContext(
  {
    chrome: {
      runtime: { lastError: undefined },
      storage: {
        local: {
          get: (key, cb) => { cb(storeF[key] ? { [key]: storeF[key] } : {}); },
          set: (items, cb) => { Object.assign(storeF, items); if (cb) cb(); },
        },
      },
    },
    crypto: nodeCrypto.webcrypto,
    navigator: { get userAgent() { return uaF; } },
  },
  [
    extractConst(bgSrc, "api", 0) + "\n"
    + extractConst(bgSrc, "isFirefox", 0) + "\n",
    extractMany(bgSrc, ["callApi", "getBrowserId", "browserName"], 0),
  ]
);

test("Item 15: browserName reads Edge, Firefox, Chrome, and the fallback", () => {
  uaF = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";
  assertEq(runIn(ctxF, "browserName()"), "Edge", "Edge UA contains Chrome too — Edge must win");
  uaF = "Mozilla/5.0 (Windows NT 10.0; Win64; x64; rv:143.0) Gecko/20100101 Firefox/143.0";
  assertEq(runIn(ctxF, "browserName()"), "Firefox");
  uaF = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36";
  assertEq(runIn(ctxF, "browserName()"), "Chrome");
  uaF = "";
  assertEq(runIn(ctxF, "browserName()"), "Browser");
});

test("Item 15: getBrowserId is stable across calls and stored once", async () => {
  for (const k of Object.keys(storeF)) delete storeF[k];
  const id1 = await runIn(ctxF, "getBrowserId()");
  assert(id1 && id1.length >= 8, "an id must be generated: " + id1);
  assertEq(storeF.browserId, id1, "the generated id is stored");
  const id2 = await runIn(ctxF, "getBrowserId()");
  assertEq(id2, id1, "the second call returns the stored id");
  storeF.browserId = "fixed-id-99";
  assertEq(await runIn(ctxF, "getBrowserId()"), "fixed-id-99", "a stored id is reused");
});

// ---------- Manifests + byte identity ----------
test("Manifests are at 0.2.10 in both browsers", () => {
  for (const name of ["chromium", "firefox"]) {
    const m = JSON.parse(readSource("extensions/" + name + "/manifest.json"));
    assertEq(m.version, "0.2.10", name + " version");
  }
});

test("Shared extension files are byte-identical between chromium and firefox", () => {
  for (const f of ["content.js", "background.js", "popup.js", "popup.html", "hook-main.js", "rules/ads.json"]) {
    const a = fs.readFileSync(path.join(__dirname, "..", "chromium", f));
    const b = fs.readFileSync(path.join(__dirname, "..", "firefox", f));
    assert(a.equals(b), f + " differs between chromium and firefox");
  }
});

run("round10.test.js");
