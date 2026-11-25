// Round 13 harness: stale-verdict page scoping (item 3), PrimeWire
// player-failure no-stream signal (item 4), YouTube visible-identity
// dedupe (item 5). Extension version 0.2.12.
//
// Each section was written FAIL-BEFORE against the 0.2.11 code:
// the stale-verdict tests fail because the election has no page
// scoping and pick-time revalidation only compares URLs against a
// currentDownload that is itself stale; the PrimeWire tests fail
// because mediaSource only flags el.error, not the failure text
// the player paints; the dedupe tests fail because the visible
// key still carries invisible bitrate/size measurements.
"use strict";
const vm = require("vm");
const { readSource, extractMany, extractFunction, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");
const tick = () => new Promise((r) => setTimeout(r, 10));

const LOOKMOVIE_PAGE = "https://lookmovie2.uk/movies/view/the-mongoose-2025";
const MOVIESJOY_PAGE = "https://moviesjoy.st/watch/film";
const MONGOOSE_VARIANT = "https://cdn.lookmovie.example/mongoose/v0.m3u8";
const MOVIESJOY_MASTER = "https://vidrift.example.com/pl/abc123/master.m3u8";

// Extract-or-stub: a helper that does not exist yet in the 0.2.11
// source extracts as its CURRENT behaviour (no check / no signal),
// so the fail-before run fails on assertions instead of crashing
// the whole file at load time. Once the helper exists, the real
// source is extracted and the tests exercise it; deleting the
// helper later still fails loudly, because the callers then throw.
function extractOr(source, name, indent, fallback) {
  try { return extractFunction(source, name, indent); }
  catch (_) { return fallback; }
}

// ---------- Context B: sortFormatsForPopup (item 5) ----------
const ctxB = makeContext(
  {},
  [extractMany(contentSrc, ["sortFormatsForPopup", "visibleFormatLabel", "effectiveFormatHeight"], 2)]
);

test("Item 5: Big Buck Bunny twins with identical visible labels collapse to one per height", () => {
  // The retest #5 listing: same label, same height, different
  // codecs — yt-dlp reports them as separate formats with
  // different vbr, so the old key (label+kind+bitrate+size) kept
  // both. The user sees one row twice; the row shows no bitrate.
  ctxB.formats = [
    { format_id: "299", label: "1080p 60fps · mp4 · video only (merged with best audio)", kind: "video_only", height: 1080, vbr: 8000 },
    { format_id: "390", label: "1080p 60fps · mp4 · video only (merged with best audio)", kind: "video_only", height: 1080, vbr: 6500 },
    { format_id: "137", label: "1080p · mp4 · video only (merged with best audio)", kind: "video_only", height: 1080, vbr: 4500 },
    { format_id: "160", label: "144p · mp4 · video only (merged with best audio)", kind: "video_only", height: 144, vbr: 100 },
    { format_id: "394", label: "144p · mp4 · video only (merged with best audio)", kind: "video_only", height: 144, vbr: 120 },
    { format_id: "395", label: "144p · mp4 · video only (merged with best audio)", kind: "video_only", height: 144 },
    { format_id: "233", label: "Audio only · mp4", kind: "audio_only" },
    { format_id: "234", label: "Audio only · mp4", kind: "audio_only" },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 5, "1080p60 x1, 1080p x1, 144p x1, 233 + 234: " + out.map((f) => f.format_id).join(","));
  assertEq(out.filter((f) => f.height === 144).length, 1, "the 144p triple collapses to one");
  assertEq(out.filter((f) => f.height === 1080 && (f.label || "").indexOf("60fps") !== -1).length, 1, "the 1080p60 pair collapses to one");
  assert(out.some((f) => f.format_id === "233") && out.some((f) => f.format_id === "234"), "233/234 both survive");
  assertEq(out[0].format_id, "299", "first occurrence wins, ladder order kept");
});

test("Item 5: a sized twin collapses with its size-less twin (the size is not in the visible key)", () => {
  ctxB.formats = [
    { format_id: "a", label: "720p · mp4 · video only (merged with best audio) · 12 MB", kind: "video_only", height: 720, vbr: 3000, filesize: 12000000 },
    { format_id: "b", label: "720p · mp4 · video only (merged with best audio)", kind: "video_only", height: 720, vbr: 2800 },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 1, "one row for the same visible quality: " + out.map((f) => f.format_id).join(","));
});

test("Item 5: the height is recovered from the label when the entry carries none", () => {
  ctxB.formats = [
    { format_id: "a", label: "720p · mp4 · video only (merged with best audio)", kind: "video_only", height: 720, vbr: 3000 },
    { format_id: "b", label: "720p · mp4 · video only (merged with best audio)", kind: "video_only", vbr: 2800 },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 1, "the label's 720p supplies the missing height");
});

test("Item 5: different heights stay distinct, and measured audio still dedupes (regressions)", () => {
  ctxB.formats = [
    { format_id: "v1080", label: "1080p · mp4 · video only (merged with best audio)", kind: "video_only", height: 1080, vbr: 4500 },
    { format_id: "v720", label: "720p · mp4 · video only (merged with best audio)", kind: "video_only", height: 720, vbr: 2500 },
    { format_id: "140", label: "Audio only · m4a", kind: "audio_only", abr: 128 },
    { format_id: "140-drc", label: "Audio only · m4a", kind: "audio_only", abr: 128 },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 3, "two video heights + one audio: " + out.map((f) => f.format_id).join(","));
  assert(out.some((f) => f.format_id === "140") && !out.some((f) => f.format_id === "140-drc"), "140-drc still dedupes");
});

// ---------- Context D: bestDownloadSource page scoping (item 3) ----------
const ctxD = makeContext(
  {},
  [
    "const tabOverlayCandidates = new Map();\nconst tabObserved = new Map();\n",
    extractConst(bgSrc, "BG_STREAM_EXT_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_MASTER_STEM_RE", 0) + "\n"
    + extractConst(bgSrc, "BG_AUDIO_STEM_RE", 0) + "\n"
    + extractConst(bgSrc, "OBSERVED_TTL_MS", 0) + "\n",
    extractMany(bgSrc, ["bgManifestKind", "bestDownloadSource", "samePage"], 0),
  ]
);
function setElectionD(frames, observed, tabUrl) {
  const map = new Map();
  for (const f of frames) {
    map.set(f.frameId, { candidates: f.candidates, frameUrl: f.frameUrl, pageUrl: f.pageUrl, at: Date.now() });
  }
  const obs = new Map();
  for (const o of observed || []) {
    obs.set(o.url, { url: o.url, kind: o.kind, initiator: o.initiator || "", pageUrl: o.pageUrl || o.initiator || "", at: Date.now() });
  }
  ctxD.frames = map;
  ctxD.observed = obs;
  ctxD.tabUrl = tabUrl || null;
  runIn(ctxD, "tabOverlayCandidates.clear(); tabOverlayCandidates.set(7, frames); tabObserved.clear(); tabObserved.set(7, observed);");
  return runIn(ctxD, "bestDownloadSource(7, null, tabUrl)");
}
function frameCand(url, opts) {
  opts = opts || {};
  return { key: "a", source: { url: url, filename: undefined }, playing: true, duration: 0, area: 100000 };
}

test("Item 3: a stale frame candidate from the previous page cannot win on the new page", () => {
  // The retest #3 hole, frame side: the MoviesJoy frame reported
  // its master just before the navigation (still inside the 12 s
  // TTL), the tab is now on LookMovie, and no LookMovie frame has
  // reported yet. The election must return nothing, not MoviesJoy.
  const v = setElectionD(
    [{ frameId: 0, frameUrl: MOVIESJOY_PAGE, pageUrl: MOVIESJOY_PAGE, candidates: [frameCand(MOVIESJOY_MASTER)] }],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: MOVIESJOY_PAGE }],
    LOOKMOVIE_PAGE
  );
  assert(!v || v.url !== MOVIESJOY_MASTER, "the stale MoviesJoy verdict must not win: " + JSON.stringify(v));
  assertEq(v, null, "nothing from the current page has been seen yet");
});

test("Item 3: a stale observation alone cannot win before the new page reports", () => {
  // The retest #3 hole, observed side: no frame has reported for
  // LookMovie yet, so the old origin filter (which only runs once
  // frames exist) let the 30-minute MoviesJoy observation win.
  const v = setElectionD(
    [],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: MOVIESJOY_PAGE }],
    LOOKMOVIE_PAGE
  );
  assertEq(v, null, "the stale observation is filtered by the tab's current page: " + JSON.stringify(v));
});

test("Item 3: the same observation still elects when the tab is still on its page (control)", () => {
  const v = setElectionD(
    [],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: MOVIESJOY_PAGE }],
    MOVIESJOY_PAGE
  );
  assert(v, "observed-alone election still works on its own page");
  assertEq(v.url, MOVIESJOY_MASTER, "the observed playlist is the verdict");
});

test("Item 3: the current page's stream still wins over a stale observation", () => {
  const v = setElectionD(
    [{ frameId: 0, frameUrl: LOOKMOVIE_PAGE, pageUrl: LOOKMOVIE_PAGE, candidates: [frameCand(MONGOOSE_VARIANT)] }],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: MOVIESJOY_PAGE }],
    LOOKMOVIE_PAGE
  );
  assert(v, "a verdict must be returned");
  assertEq(v.url, MONGOOSE_VARIANT, "the Mongoose stream wins, not the stale observation");
});

test("Item 3: an iframe player's frame is kept — its page tag is the current page", () => {
  // The VidFast player lives in a subframe on another origin;
  // its frameUrl differs from the tab URL, but the report is
  // tagged with the top page, so it must survive the scoping.
  const v = setElectionD(
    [{ frameId: 1, frameUrl: "https://vidfast.example/embed/123", pageUrl: LOOKMOVIE_PAGE, candidates: [frameCand(MONGOOSE_VARIANT)] }],
    [],
    LOOKMOVIE_PAGE
  );
  assert(v, "the iframe player's stream must still elect");
  assertEq(v.url, MONGOOSE_VARIANT, "the subframe stream is the verdict");
});

test("Item 3: the verdict carries the page it was elected for", () => {
  const v = setElectionD(
    [{ frameId: 0, frameUrl: LOOKMOVIE_PAGE, pageUrl: LOOKMOVIE_PAGE, candidates: [frameCand(MONGOOSE_VARIANT)] }],
    [],
    LOOKMOVIE_PAGE
  );
  assert(v, "a verdict must be returned");
  assertEq(v.pageUrl, LOOKMOVIE_PAGE, "pageUrl tags the verdict for the pick-time check");
});

// ---------- Context E: whole background, navigation + SPA tagging (item 3) ----------
function makeChromeStubE() {
  const store = {};
  const chrome = {
    runtime: {
      lastError: undefined,
      onInstalled: { addListener() {} },
      onMessage: { addListener(fn) { chrome.__onMessage = fn; } },
      sendNativeMessage: (host, msg, cb) => {
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
      onChanged: { addListener() {} },
    },
    downloads: { download: (opts, cb) => { if (cb) cb(42); } },
    tabs: {
      onActivated: { addListener() {} },
      onRemoved: { addListener() {} },
      onUpdated: { addListener(fn) { chrome.__onUpdated = fn; } },
      sendMessage: (tabId, msg, opts) => { chrome.__verdicts.push({ tabId: tabId, msg: msg, frameId: opts && opts.frameId }); },
      query: () => Promise.resolve([]),
    },
    contextMenus: {
      onClicked: { addListener() {} },
      create: () => {},
    },
    action: { setBadgeText: () => {}, setBadgeBackgroundColor: () => {} },
    webRequest: {
      onResponseStarted: { addListener(fn) { chrome.__onResponseStarted = fn; } },
    },
    __verdicts: [],
    __onMessage: null,
    __onUpdated: null,
    __onResponseStarted: null,
  };
  return chrome;
}
function loadBackgroundE() {
  const chrome = makeChromeStubE();
  const ctx = makeContext({ chrome: chrome }, [bgSrc]);
  assert(chrome.__onMessage, "background must register an onMessage listener");
  assert(chrome.__onUpdated, "background must listen for tab navigations");
  assert(chrome.__onResponseStarted, "background must observe responses");
  return { chrome: chrome, ctx: ctx };
}
function lastVerdictE(chrome) {
  for (let i = chrome.__verdicts.length - 1; i >= 0; i--) {
    const v = chrome.__verdicts[i];
    if (v.msg && v.msg.type === "overlayVerdict") return v.msg.download;
  }
  return undefined;
}
function observeE(chrome, url, initiator) {
  chrome.__onResponseStarted({
    url: url, tabId: 7, initiator: initiator, type: "media",
    responseHeaders: [{ name: "Content-Type", value: "application/vnd.apple.mpegurl" }],
  });
}

test("Item 3: a stale observation re-seen after navigation must not re-elect the old film", () => {
  const { chrome } = loadBackgroundE();
  observeE(chrome, MOVIESJOY_MASTER, MOVIESJOY_PAGE);
  const before = lastVerdictE(chrome);
  assert(before && before.url === MOVIESJOY_MASTER, "pre-navigation verdict is the MoviesJoy stream: " + JSON.stringify(before));
  // Same tab navigates to LookMovie / The Mongoose.
  chrome.__onUpdated(7, { url: LOOKMOVIE_PAGE }, { id: 7, url: LOOKMOVIE_PAGE });
  // A late MoviesJoy response is observed again after the clear
  // (a segment fetch in flight during the navigation, a
  // service-worker replay). The tab is on LookMovie now: the
  // election must not hand the button the MoviesJoy master.
  observeE(chrome, MOVIESJOY_MASTER, MOVIESJOY_PAGE);
  const after = lastVerdictE(chrome);
  assert(!after || after.url !== MOVIESJOY_MASTER, "post-navigation verdict must not be MoviesJoy: " + JSON.stringify(after));
});

test("Item 3: the new page's own observation still elects after navigation (control)", () => {
  const { chrome } = loadBackgroundE();
  observeE(chrome, MOVIESJOY_MASTER, MOVIESJOY_PAGE);
  chrome.__onUpdated(7, { url: LOOKMOVIE_PAGE }, { id: 7, url: LOOKMOVIE_PAGE });
  observeE(chrome, MONGOOSE_VARIANT, LOOKMOVIE_PAGE);
  const after = lastVerdictE(chrome);
  assert(after && after.url === MONGOOSE_VARIANT, "the Mongoose stream elects: " + JSON.stringify(after));
});

test("Item 3: a frame report tags the verdict with the page it came from", () => {
  const { chrome } = loadBackgroundE();
  chrome.__onMessage(
    { type: "overlayCandidates", candidates: [{ key: "a", playing: true, area: 100000, source: { url: MONGOOSE_VARIANT, viaPage: false } }] },
    { tab: { id: 7, url: LOOKMOVIE_PAGE }, url: LOOKMOVIE_PAGE, frameId: 0 },
    function () {}
  );
  const v = lastVerdictE(chrome);
  assert(v && v.url === MONGOOSE_VARIANT, "the frame's stream elects: " + JSON.stringify(v));
  assertEq(v.pageUrl, LOOKMOVIE_PAGE, "the verdict is tagged with the reporting page");
});

// ---------- Context F: pickAndSend + onOverlayClick page check (item 3) ----------
const sendCallsF = [];
const closeCallsF = [];
const ctxF = makeContext(
  {
    document: { title: "The Mongoose | LookMovie" },
    window: {
      location: { href: LOOKMOVIE_PAGE },
      setTimeout: (fn) => { return 1; },
      clearTimeout: () => {},
    },
  },
  [
    "let popupOpen = true;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet floatOverlay = null;\nlet currentDownload = null;\n"
    + "globalThis.__sendCalls = [];\n"
    + "function sendCurrentDownload(btn, dl, opts) { globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); return Promise.resolve({ ok: true }); }\n"
    + "function closeQualityPopup() { globalThis.__closeCalls.push(1); popupOpen = false; }\n"
    + "function currentPopupOptions() { return {}; }\n"
    + "function currentPopupFilename() { return 'The Mongoose'; }\n"
    + "function showPopupSendError(reason) { globalThis.__sendError = reason; }\n",
    extractOr(contentSrc, "verdictMatchesPage", 2, "function verdictMatchesPage(dl) { return true; }"),
    extractMany(contentSrc, ["pickAndSend"], 2),
  ]
);
ctxF.__sendCalls = sendCallsF;
ctxF.__closeCalls = closeCallsF;
function pickF(dl, current) {
  sendCallsF.length = 0;
  closeCallsF.length = 0;
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxF.btn = btn;
  ctxF.dl = dl;
  ctxF.current = current;
  runIn(ctxF, "popupOpen = true; currentDownload = current;");
  return { btn: btn, p: runIn(ctxF, "pickAndSend(btn, dl, {})") };
}

test("Item 3: a pick for a verdict tagged for another page sends nothing (SPA backstop)", async () => {
  // The popup's rows were built for the MoviesJoy verdict; the
  // user is now on the LookMovie page (a pushState navigation
  // fired no tab event, so the background never cleared, and
  // currentDownload still IS the MoviesJoy verdict — the URL
  // comparison alone passes). The page tag must refuse the pick.
  const { btn, p } = pickF(
    { url: MOVIESJOY_MASTER, filename: "master.m3u8", pageUrl: MOVIESJOY_PAGE },
    { url: MOVIESJOY_MASTER, pageUrl: MOVIESJOY_PAGE }
  );
  const r = await p;
  assertEq(sendCallsF.length, 0, "no send for a verdict from another page");
  assertEq(closeCallsF.length, 1, "the stale popup closes");
  assertEq(btn.textContent, "Stream changed — try again", "the button says why");
  assertEq(r.ok, false, "the pick resolves as a refusal, not a send");
});

test("Item 3: a pick for a verdict tagged for this page still sends", async () => {
  const dl = { url: MONGOOSE_VARIANT, filename: "v0.m3u8", pageUrl: LOOKMOVIE_PAGE };
  const { p } = pickF(dl, { url: MONGOOSE_VARIANT, pageUrl: LOOKMOVIE_PAGE });
  await p;
  await tick();
  assertEq(sendCallsF.length, 1, "the fresh pick sends");
  assertEq(sendCallsF[0].dl.url, MONGOOSE_VARIANT);
});

test("Item 3: a pick for an untagged (older) verdict still sends (backward compatible)", async () => {
  const dl = { url: MONGOOSE_VARIANT, filename: "v0.m3u8" };
  const { p } = pickF(dl, { url: MONGOOSE_VARIANT });
  await p;
  await tick();
  assertEq(sendCallsF.length, 1, "no page tag means no page check");
});

// onOverlayClick with the same page check.
const ctxF2 = makeContext(
  {
    document: { title: "The Mongoose | LookMovie" },
    window: {
      location: { href: LOOKMOVIE_PAGE },
      setTimeout: () => 1,
      clearTimeout: () => {},
    },
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
    extractOr(contentSrc, "verdictMatchesPage", 2, "function verdictMatchesPage(dl) { return true; }"),
    extractMany(contentSrc, ["onOverlayClick", "parseHlsMaster", "parseHlsAttrs", "resolveAgainst", "friendlyCodec", "formatBandwidth", "hlsVariantHeight", "probeDirectFileDuration", "parseMp4Duration", "isManifestUrl", "isDashUrl", "isMp4FamilyUrl", "isVideoFileUrl", "isOneClickFileUrl"], 2),
  ]
);
function clickF2(dl) {
  runIn(ctxF2, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.simple.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxF2.btn = btn;
  ctxF2.dl = dl;
  runIn(ctxF2, "currentDownload = dl; onOverlayClick(btn);");
  return { calls: runIn(ctxF2, "__calls"), btn: btn };
}

test("Item 3: clicking the button on a verdict from another page opens nothing", async () => {
  const { calls, btn } = clickF2({ url: MOVIESJOY_MASTER, kind: "playlist", pageUrl: MOVIESJOY_PAGE });
  await tick();
  assertEq(calls.formats.length, 0, "no formats popup for a stale-page verdict");
  assertEq(calls.send.length, 0, "nothing is sent");
  assertEq(btn.textContent, "Stream changed — try again", "the button says why");
});

test("Item 3: clicking the button on a verdict for this page still opens the popup (control)", async () => {
  const { calls } = clickF2({ url: MONGOOSE_VARIANT, kind: "playlist", pageUrl: LOOKMOVIE_PAGE });
  await tick();
  assertEq(calls.formats.length, 1, "the working path is untouched");
});

// ---------- Context H: handleOverlayVerdict (item 3, verdict handler) ----------
let ctxH = null;
function getCtxH() {
  if (ctxH) return ctxH;
  const prelude = "let popupOpen = false;\nlet currentDownload = null;\nlet isTopFrame = true;\n"
    + "const __calls = { close: 0, refresh: 0 };\n"
    + "globalThis.__calls = __calls;\n"
    + "function closeQualityPopup() { __calls.close++; popupOpen = false; }\n"
    + "function refreshFloatingButton() { __calls.refresh++; }\n";
  let verdictFn;
  try { verdictFn = extractFunction(contentSrc, "verdictMatchesPage", 2); }
  catch (_) { verdictFn = "function verdictMatchesPage(dl) { return true; }"; }
  let handlerFn;
  try { handlerFn = extractFunction(contentSrc, "handleOverlayVerdict", 2); }
  catch (_) { throw new Error("handleOverlayVerdict not found in content.js — the verdict handler is still inline (round 13, item 3)"); }
  ctxH = makeContext(
    { window: { location: { href: LOOKMOVIE_PAGE } } },
    [prelude, verdictFn, handlerFn]
  );
  return ctxH;
}
function verdictH(msg, popupWasOpen) {
  const ctx = getCtxH();
  runIn(ctx, "__calls.close = 0; __calls.refresh = 0; popupOpen = " + (popupWasOpen ? "true" : "false") + "; currentDownload = null;");
  ctx.msg = msg;
  runIn(ctx, "handleOverlayVerdict(msg);");
  return { current: runIn(ctx, "currentDownload"), calls: runIn(ctx, "__calls"), popupOpen: runIn(ctx, "popupOpen") };
}

test("Item 3: a verdict tagged for another page is dropped by the top frame", () => {
  const r = verdictH({ download: { url: MOVIESJOY_MASTER, pageUrl: MOVIESJOY_PAGE } }, false);
  assertEq(r.current, null, "the stale verdict is not adopted");
});

test("Item 3: a verdict tagged for another page closes an open popup", () => {
  const r = verdictH({ download: { url: MOVIESJOY_MASTER, pageUrl: MOVIESJOY_PAGE } }, true);
  assertEq(r.current, null, "the stale verdict is not adopted");
  assertEq(r.calls.close, 1, "the popup describing the old stream closes");
  assertEq(r.popupOpen, false, "the popup is closed");
});

test("Item 3: a verdict tagged for this page is adopted (control)", () => {
  const r = verdictH({ download: { url: MONGOOSE_VARIANT, pageUrl: LOOKMOVIE_PAGE } }, false);
  assert(r.current && r.current.url === MONGOOSE_VARIANT, "the fresh verdict is adopted");
});

test("Item 3: an untagged verdict is adopted (backward compatible)", () => {
  const r = verdictH({ download: { url: MONGOOSE_VARIANT } }, false);
  assert(r.current && r.current.url === MONGOOSE_VARIANT, "no page tag means no page check");
});

// ---------- Context G: mediaSource player-failure signal (item 4) ----------
let ctxG = null;
function getCtxG() {
  if (ctxG) return ctxG;
  const bodyStub = { textContent: "", children: [], parentElement: null };
  const htmlStub = { textContent: "", children: [], parentElement: null };
  bodyStub.parentElement = htmlStub;
  const docStub = {
    querySelectorAll: function () { return []; },
    body: bodyStub,
    documentElement: htmlStub,
  };
  const failFn = extractOr(contentSrc, "findPlayerFailure", 2, "function findPlayerFailure(el) { return false; }");
  ctxG = makeContext(
    {
      document: docStub,
      window: {
        location: { href: "https://primewire.example/film/123" },
        getComputedStyle: function () { return { display: "block", visibility: "visible" }; },
      },
    },
    [
      "const capturedManifests = new Map();\n"
      + "let __playlistStub = null;\n"
      + "function findPlaylistFromResources() { return __playlistStub; }\n"
      + "function findStreamUrlFromResources() { return __playlistStub; }\n",
      extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n",
      extractConst(contentSrc, "MIN_VISIBLE_AREA", 2) + "\n",
      extractConst(contentSrc, "PLAYER_FAILURE_RE", 2) + "\n",
      extractConst(contentSrc, "PLAYER_FAILURE_TEXT_MAX", 2) + "\n",
      failFn,
      extractMany(contentSrc, ["extractHtmlMedia", "isVisible", "mediaSource", "mediaTitle", "filenameForSource", "fileNameFromUrl", "sameDirectoryEncodes", "qualityHintScore", "resolveUrl"], 2),
    ]
  );
  ctxG.__bodyStub = bodyStub;
  return ctxG;
}
function domNode(text, parent, children) {
  return { textContent: text || "", children: children || [], parentElement: parent || null };
}
function makeVideoG(currentSrc, error, parent) {
  return {
    currentSrc: currentSrc || "",
    error: error || null,
    isConnected: true,
    textContent: "",
    parentElement: parent || null,
    getAttribute: function () { return null; },
    querySelectorAll: function () { return []; },
    getBoundingClientRect: function () { return { width: 640, height: 360, left: 0, top: 0, right: 640, bottom: 360 }; },
  };
}

test("Item 4: a failed player with overlay failure text reports noStream", () => {
  const ctx = getCtxG();
  // PrimeWire retest #4: the player paints "Sorry, the video
  // player failed to load. (Error Code: 101102)" in its own
  // overlay next to the <video>; video.error stays unset.
  const body = ctx.__bodyStub;
  const player = domNode("Sorry, the video player failed to load. (Error Code: 101102)", body);
  const overlay = domNode("Sorry, the video player failed to load. (Error Code: 101102)", player);
  player.children = [overlay];
  const el = makeVideoG("blob:https://primewire.example/abc", null, player);
  player.children.unshift(el);
  ctx.el = el;
  runIn(ctx, "__playlistStub = null;");
  const out = runIn(ctx, "mediaSource(el)");
  assertEq(out.url, null);
  assertEq(out.viaPage, true);
  assertEq(out.noStream, true, "the painted failure flags the player, not just el.error");
});

test("Item 4: the failure text two levels up (player container) still flags it", () => {
  const ctx = getCtxG();
  const body = ctx.__bodyStub;
  const player = domNode("Unable to play media", body);
  const wrap = domNode("", player);
  const overlay = domNode("Unable to play media", player);
  player.children = [wrap, overlay];
  const el = makeVideoG("blob:https://primewire.example/abc", null, wrap);
  wrap.children = [el];
  ctx.el = el;
  runIn(ctx, "__playlistStub = null;");
  const out = runIn(ctx, "mediaSource(el)");
  assertEq(out.noStream, true, "the container two levels up still counts");
});

test("Item 4: failure text elsewhere on the page does NOT flag a healthy player", () => {
  const ctx = getCtxG();
  // The same words in a comment/description block that is NOT an
  // ancestor of the video must not kill the download: the check
  // is scoped to the player container, never page-wide.
  const body = ctx.__bodyStub;
  body.textContent = "Comments: I was unable to play this on my phone, error code 101102 kept showing. Great film though, loved the ending and the soundtrack and the cast.";
  const player = domNode("", body);
  const el = makeVideoG("blob:https://example.com/abc", null, player);
  player.children = [el];
  ctx.el = el;
  runIn(ctx, "__playlistStub = null;");
  const out = runIn(ctx, "mediaSource(el)");
  assertEq(out.viaPage, true);
  assert(!out.noStream, "page-wide failure words must not flag the player");
  body.textContent = "";
});

test("Item 4: a healthy player with no failure text still offers the download (control)", () => {
  const ctx = getCtxG();
  const body = ctx.__bodyStub;
  const player = domNode("", body);
  const el = makeVideoG("blob:https://example.com/abc", null, player);
  player.children = [el];
  ctx.el = el;
  runIn(ctx, "__playlistStub = null;");
  const out = runIn(ctx, "mediaSource(el)");
  assertEq(out.viaPage, true);
  assert(!out.noStream, "no error, no flag");
});

test("Item 4: a failed player with an observed stream still returns the stream (regression)", () => {
  const ctx = getCtxG();
  const body = ctx.__bodyStub;
  const player = domNode("Sorry, the video player failed to load. (Error Code: 101102)", body);
  const el = makeVideoG("blob:https://example.com/abc", null, player);
  player.children = [el];
  ctx.el = el;
  runIn(ctx, "__playlistStub = { url: 'https://cdn.example.com/master.m3u8', manifestText: undefined, masterText: undefined };");
  const out = runIn(ctx, "mediaSource(el)");
  assertEq(out.url, "https://cdn.example.com/master.m3u8", "the observed stream wins over the failure text");
  assert(!out.noStream, "a real stream is not flagged");
});

test("Item 4: the Media Detector does not list a failure-text player as downloadable", () => {
  const ctx = getCtxG();
  const body = ctx.__bodyStub;
  const player = domNode("Sorry, the video player failed to load. (Error Code: 101102)", body);
  const el = makeVideoG("blob:https://primewire.example/abc", null, player);
  player.children = [el];
  ctx.el = el;
  runIn(ctx, "__playlistStub = null;");
  const extract = runIn(ctx, "extractHtmlMedia('video')");
  assertEq(extract(ctx.el), null, "no toolbar-popup row for the failed player");
  const player2 = domNode("", body);
  const el2 = makeVideoG("blob:https://example.com/abc", null, player2);
  player2.children = [el2];
  ctx.el2 = el2;
  const item = extract(ctx.el2);
  assert(item, "a healthy player is still listed");
});

run("round13.test.js");
