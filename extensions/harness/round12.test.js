// Round 12 harness: formats-popup audio row (item 1), hash-like
// titles with dots (item 2), video no-measure dedupe (item 3),
// stream-name filenames (item 6), single-format labelling (items
// 7/11), stale-verdict send (item 9), failed-player no-stream
// (item 10), DRM legal-alternative note (item 14).
// Extension version 0.2.11.
"use strict";
const vm = require("vm");
const { readSource, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");
const tick = () => new Promise((r) => setTimeout(r, 10));

// ---------- Context A: hash-like + stream-like names (items 2, 6) ----------
function makeDocA(title, opts) {
  opts = opts || {};
  return {
    title: title,
    querySelector: function (sel) {
      if (sel === "h1" && opts.h1) return { textContent: opts.h1 };
      if (sel === 'meta[property="og:title"]' && opts.og) return { getAttribute: function () { return opts.og; } };
      if (sel === "video") return null;
      return null;
    },
  };
}
const ctxA = makeContext(
  { document: makeDocA("The Mongoose | LookMovie") },
  [extractMany(contentSrc, ["isHashLikeTitle", "isStreamLikeName", "isJunkTitle", "stemOfFilename", "popupDefaultName", "pageTitleStem", "pageNameHint", "sanitizeFilenameStem", "defaultFilenameFor", "isManifestUrl"], 2)]
);
function setDocA(title, opts) { ctxA.document = makeDocA(title, opts); }

test("Item 2: isHashLikeTitle spots the real novahd dotted title", () => {
  assertEq(runIn(ctxA, "isHashLikeTitle('novahd-anon.0b8833fa881fe689.019314daf61830f3.1795600067')"), true, "the real novahd title has dots and no spaces");
  assertEq(runIn(ctxA, "isHashLikeTitle('0BGHIHfw0FTI-dNbLxEad3TYSrs97iR2W7IUCXXIJQ55hG38')"), true, "plain hash token still spotted");
  assertEq(runIn(ctxA, "isHashLikeTitle('The Odyssey')"), false, "real title has spaces");
  assertEq(runIn(ctxA, "isHashLikeTitle('Inception')"), false, "short single word safe");
});

test("Item 6: isStreamLikeName spots manifest/stream names, not real titles", () => {
  for (const s of ["index_266", "index-s1080p-v1-a1", "index_web", "video", "download", "index", "master", "playlist"]) {
    assertEq(runIn(ctxA, "isStreamLikeName('" + s + "')"), true, s + " is a stream name");
  }
  for (const s of ["The Mongoose", "Dune", "The Odyssey (2026)"]) {
    assertEq(runIn(ctxA, "isStreamLikeName('" + s + "')"), false, s + " is not a stream name");
  }
  // A raw filename with an extension still matches the prefix rule
  // ("index_287.ts" starts with "index_"); the stem check strips
  // the extension first and reaches the same verdict either way.
  assertEq(runIn(ctxA, "isStreamLikeName('index_287.ts')"), true, "index_287.ts is stream-like");
});

test("Item 6: stream-like verdict stems fall through instead of naming the file", () => {
  assertEq(runIn(ctxA, "stemOfFilename('index_266.ts')"), "", "ThugTV index_266");
  assertEq(runIn(ctxA, "stemOfFilename('index-s1080p-v1-a1.m3u8')"), "", "MoviesJoy index-s1080p-v1-a1");
  assertEq(runIn(ctxA, "stemOfFilename('index_web.mpd')"), "", "nime index_web");
  assertEq(runIn(ctxA, "stemOfFilename('video.mp4')"), "", "Soap2Day video.mp4");
  assertEq(runIn(ctxA, "stemOfFilename('movie.mp4')"), "movie", "a real stem still works");
});

test("Item 6: the page title beats a stream name in the popup pre-fill", () => {
  setDocA("The Mongoose | LookMovie");
  ctxA.dl = { url: "https://cdn.example.com/mongoose/v0.m3u8", filename: "index_web.m3u8" };
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "The Mongoose", "page title beats the stream stem");
  assertEq(runIn(ctxA, "popupDefaultName(dl, 'index_web')"), "The Mongoose", "a stream-like server title is skipped too");
  assertEq(runIn(ctxA, "popupDefaultName(dl, 'The Mongoose (2025)')"), "The Mongoose (2025)", "a real server title still wins");
  // Junk document title + h1 hint: the hint beats the stream stem.
  setDocA("Movies Online for Free at Lookmovie2", { h1: "The Mongoose" });
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "The Mongoose", "h1 hint beats the stream stem");
  // Nothing at all: "download", never the stream name.
  setDocA("Movies Online for Free at Lookmovie2", {});
  assertEq(runIn(ctxA, "popupDefaultName(dl, null)"), "download", "no title anywhere: download, not index_web");
});

test("Item 6: defaultFilenameFor never returns a stream stem for a manifest", () => {
  setDocA("Movies Online for Free at Lookmovie2", {});
  ctxA.dl = { url: "https://cdn.example.com/index-s1080p-v1-a1.m3u8", filename: "index-s1080p-v1-a1.m3u8" };
  assertEq(runIn(ctxA, "defaultFilenameFor(dl)"), undefined, "the engine names it instead of saving the stream stem");
  setDocA("The Mongoose | LookMovie");
  assertEq(runIn(ctxA, "defaultFilenameFor(dl)"), "The Mongoose", "a page title still wins");
});

// ---------- Context B: video no-measure dedupe (item 3) ----------
const ctxB = makeContext(
  {},
  [extractMany(contentSrc, ["sortFormatsForPopup"], 2)]
);

test("Item 3: sortFormatsForPopup dedupes measurement-less VIDEO rows by label+kind+height", () => {
  ctxB.formats = [
    { format_id: "v144a", label: "144p · mp4", kind: "combined", height: 144 },
    { format_id: "v144b", label: "144p · mp4", kind: "combined", height: 144 },
    { format_id: "v144c", label: "144p · mp4", kind: "combined", height: 144 },
    { format_id: "v240", label: "240p · mp4", kind: "combined", height: 240 },
    { format_id: "v1080", label: "1080p · mp4", kind: "combined", height: 1080 },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 3, "the 144p triple collapses to one: " + out.map((f) => f.format_id).join(","));
  assertEq(out[0].format_id, "v1080", "ladder order kept");
  assert(out.some((f) => f.format_id === "v144a"), "first occurrence wins");
});

test("Item 3: same label at different heights stays, and 233/234 still survive", () => {
  ctxB.formats = [
    { format_id: "v144", label: "Video · mp4", kind: "combined", height: 144 },
    { format_id: "v240", label: "Video · mp4", kind: "combined", height: 240 },
    { format_id: "233", label: "Audio only · mp4", kind: "audio_only" },
    { format_id: "234", label: "Audio only · mp4", kind: "audio_only" },
  ];
  const out = runIn(ctxB, "sortFormatsForPopup(formats)");
  assertEq(out.length, 4, "different heights are different streams; audio pair exempt");
  assert(out.some((f) => f.format_id === "233") && out.some((f) => f.format_id === "234"), "233/234 both present");
});

// ---------- Context C: openFormatsPopup (items 1, 6, 7, 11, 14) ----------
let formatsReplyC = null;
const sendCallsC = [];
const pendingTimersC = [];
function makeFloatOverlayC() {
  return {
    popup: { style: { display: "none" } },
    popupTitle: { textContent: "" },
    popupSubtitle: { textContent: "", style: { display: "none" } },
    popupNote: { textContent: "", style: { display: "none" } },
    popupBody: (() => {
      // Setting textContent = "" removes the children in the real
      // DOM (the loading row, the old rows) — mirror that, or row
      // counts include the "Loading qualities…" div.
      const b = {
        children: [],
        appendChild(c) { this.children.push(c); },
        querySelector() { return null; },
      };
      let tc = "";
      Object.defineProperty(b, "textContent", {
        get() { return tc; },
        set(v) { tc = v; if (v === "") b.children.length = 0; },
      });
      return b;
    })(),
    filenameInput: { value: "" },
    langRow: { style: { display: "none" } },
    langSelect: { value: "", textContent: "", appendChild() {} },
    subRow: { style: { display: "none" } },
    subList: { textContent: "", appendChild() {}, querySelectorAll() { return []; } },
    fmtRow: { style: { display: "none" } },
    fmtSelect: { value: "m4a" },
  };
}
const ctxC = makeContext(
  {
    document: {
      title: "The Mongoose | LookMovie",
      createElement: () => ({ dataset: {}, style: {}, textContent: "", title: "", appendChild: () => {}, addEventListener: () => {} }),
      querySelector: function () { return null; },
    },
    window: {
      setTimeout: (fn) => { pendingTimersC.push({ fn: fn }); return pendingTimersC.length; },
      clearTimeout: (id) => { if (pendingTimersC[id - 1]) pendingTimersC[id - 1].fn = null; },
    },
    sendRuntimeMessage: (msg) => {
      assertEq(msg.type, "getFormats");
      return Promise.resolve(formatsReplyC);
    },
  },
  [
    "let popupOpen = false;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet popupFilenameDirty = false;\n"
    + "const FORMATS_TIMEOUT_MS = 15000;\nlet floatOverlay = null;\n"
    + "globalThis.__popupRows = function () { return popupRows; };\n"
    + "globalThis.__popupOpen = function () { return popupOpen; };\n"
    + "function pickAndSend(btn, dl, opts) { opts = opts || {}; if (!opts.filename) opts.filename = currentPopupFilename(); globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); return Promise.resolve({ ok: true }); }\n",
    extractMany(contentSrc, ["openFormatsPopup", "openQualityPopup", "closeQualityPopup", "addQualityRow", "configurePopupOptions", "currentPopupOptions", "popupDefaultName", "currentPopupFilename", "sanitizeFilenameStem", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "isStreamLikeName", "stemOfFilename", "sortFormatsForPopup", "isDashUrl", "resolutionFromFormat", "singleFormatRowLabel", "legalAlternativeNote", "isAccessRestrictedReason", "setPopupNote"], 2),
  ]
);
ctxC.__sendCalls = sendCallsC;
async function openFormatsC(dl, reply) {
  formatsReplyC = reply;
  sendCallsC.length = 0;
  pendingTimersC.length = 0;
  const overlay = makeFloatOverlayC();
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxC.overlay = overlay;
  ctxC.btn = btn;
  ctxC.dl = dl;
  runIn(ctxC, "floatOverlay = overlay; openFormatsPopup(btn, dl);");
  await tick();
  return { overlay: overlay, btn: btn };
}
function rowLabelsC(overlay) { return overlay.popupBody.children.map((c) => c.textContent); }

test("Item 1: a video-only listing gains a synthesized Audio only row", async () => {
  // The novahd / NOXX / 123Movies / ThugTV / MoviesJoy shape: the
  // bridge lists video rows and no audio-only format.
  const dl = { url: "https://cdn.example.com/film/master.m3u8", filename: "master.m3u8" };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    title: "The Mongoose (2025)",
    formats: [
      { format_id: "v1080", label: "1080p · ts", kind: "combined", height: 1080 },
      { format_id: "v720", label: "720p · ts", kind: "combined", height: 720 },
    ],
    default_format_id: "v1080",
  });
  const labels = rowLabelsC(overlay);
  assertEq(labels.length, 3, "two video rows + synthesized audio row: " + labels.join(" | "));
  assertEq(labels[2], "Audio only", "the synthesized row is last");
  // Picking it sends the audio-only shape (no format id, no URL —
  // the engine extracts the audio from the stream).
  const rows = runIn(ctxC, "__popupRows()");
  rows[2].pick();
  assertEq(sendCallsC.length, 1, "one send");
  assertEq(sendCallsC[0].opts.audioOnly, true, "audio-only flag set");
  assertEq(sendCallsC[0].opts.formatId, undefined, "no format id on the synthesized row");
  assertEq(sendCallsC[0].opts.url, undefined, "no variant URL on the synthesized row");
});

test("Item 1: a listing that already has an audio row gains no second one", async () => {
  const dl = { url: "https://example.com/watch?v=x", filename: undefined };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    formats: [
      { format_id: "v1080", label: "1080p", kind: "combined", height: 1080 },
      { format_id: "a140", label: "Audio only · m4a", kind: "audio_only", abr: 128 },
    ],
    default_format_id: "v1080",
  });
  const audioRows = rowLabelsC(overlay).filter((l) => l === "Audio only · m4a");
  assertEq(audioRows.length, 1, "exactly one audio row");
  assertEq(rowLabelsC(overlay).length, 2, "no synthesized duplicate");
});

test("Item 7: a lone format shows its resolution from the variant name", async () => {
  const dl = { url: "https://cdn.example.com/index-s1080p-v1-a1.m3u8", filename: "index-s1080p-v1-a1.m3u8" };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    formats: [{ format_id: "https://cdn.example.com/index-s1080p-v1-a1.m3u8", label: "Video · ts", kind: "combined", ext: "ts" }],
    default_format_id: "https://cdn.example.com/index-s1080p-v1-a1.m3u8",
  });
  assertEq(rowLabelsC(overlay)[0], "1080p · ts", "resolution from the variant name, not a bare Video · ts");
  assertEq(overlay.popupNote.style.display, "none", "no unknown-quality note when the resolution is known");
});

test("Item 7: a lone format with a height shows it; one with neither says unknown + why", async () => {
  const dl = { url: "https://cdn.example.com/v0.m3u8", filename: "v0.m3u8" };
  const withHeight = await openFormatsC(dl, {
    ok: true,
    formats: [{ format_id: "v0", label: "Video · ts", kind: "combined", height: 720 }],
    default_format_id: "v0",
  });
  assertEq(rowLabelsC(withHeight.overlay)[0], "720p · ts", "resolution from height");
  const unknown = await openFormatsC(dl, {
    ok: true,
    formats: [{ format_id: "https://cdn.example.com/index_266", label: "Video · ts", kind: "combined", ext: "ts" }],
    default_format_id: "https://cdn.example.com/index_266",
  });
  assertEq(rowLabelsC(unknown.overlay)[0], "Video · ts · quality unknown", "genuinely unknown quality is labelled so");
  assertEq(unknown.overlay.popupNote.style.display, "block", "the WHY is shown in the note line");
  assert(unknown.overlay.popupNote.textContent.indexOf("no resolution") !== -1, "the note says why: " + unknown.overlay.popupNote.textContent);
});

test("Item 11: FlixTor's single unlabeled Video row is labelled by the same rule", async () => {
  const dl = { url: "https://cdn.example.com/stream/v0.m3u8", filename: "v0.m3u8" };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    formats: [{ format_id: "https://cdn.example.com/stream/v0.m3u8", label: "Video", kind: "combined" }],
    default_format_id: "https://cdn.example.com/stream/v0.m3u8",
  });
  assertEq(rowLabelsC(overlay)[0], "Video · quality unknown", "a bare Video label gains the unknown marker");
  assert(overlay.popupNote.textContent.indexOf("no resolution") !== -1, "and the WHY note");
});

test("Item 6: a stream-like server title does not overwrite the filename pre-fill", async () => {
  const dl = { url: "https://cdn.example.com/mongoose/v0.m3u8", filename: "index_web.m3u8" };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    title: "index_web",
    formats: [{ format_id: "v1080", label: "1080p", kind: "combined", height: 1080 }],
    default_format_id: "v1080",
  });
  assertEq(overlay.filenameInput.value, "The Mongoose", "the page title survives the stream-like server title");
  assertEq(overlay.popupSubtitle.style.display, "none", "the stream name is not shown as the subtitle either");
});

test("Item 14: a DRM refusal shows the legal-alternative note in the degraded popup", async () => {
  const dl = { url: "https://plex.example.com/watch/123", filename: undefined };
  const { overlay } = await openFormatsC(dl, {
    ok: false,
    reason: "This video is DRM-protected. FetchForge does not bypass DRM or encryption.",
  });
  assert(overlay.popupSubtitle.textContent.indexOf("DRM-protected") !== -1, "the refusal itself is shown");
  assertEq(overlay.popupNote.style.display, "block", "the alternative note is shown alongside");
  assert(overlay.popupNote.textContent.indexOf("legal streaming service") !== -1, "generic pointer, no service named: " + overlay.popupNote.textContent);
  assert(overlay.popupNote.textContent.indexOf("Netflix") === -1, "no specific service is claimed");
});

test("Item 14: an ordinary failure shows no alternative note", async () => {
  const dl = { url: "https://example.com/watch", filename: undefined };
  const { overlay } = await openFormatsC(dl, {
    ok: false,
    reason: "The app took too long to list the qualities — it may be busy. Try again in a moment.",
  });
  assertEq(overlay.popupNote.style.display, "none", "a timeout is not an access restriction");
  const { overlay: overlay2 } = await openFormatsC(dl, { ok: false, reason: "Access denied (HTTP 403)." });
  assertEq(overlay2.popupNote.style.display, "none", "a 403 fetch failure is not an access restriction");
});

test("Item 14: legalAlternativeNote triggers on DRM / sign-in / private only", () => {
  assert(runIn(ctxC, "legalAlternativeNote('This video is DRM-protected. FetchForge does not bypass DRM.')").indexOf("legal streaming service") !== -1, "DRM");
  assert(runIn(ctxC, "legalAlternativeNote('Sign in to confirm you are not a bot')").indexOf("legal streaming service") !== -1, "sign-in");
  assert(runIn(ctxC, "legalAlternativeNote('This video is private')").indexOf("legal streaming service") !== -1, "private");
  assertEq(runIn(ctxC, "legalAlternativeNote('The app took too long')"), "", "timeout: no note");
  assertEq(runIn(ctxC, "legalAlternativeNote('Access denied (HTTP 403)')"), "", "403: no note");
  assertEq(runIn(ctxC, "legalAlternativeNote('')"), "", "empty: no note");
});

// ---------- Context D: bestDownloadSource (item 9a, item 10 verdict) ----------
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
function setElectionD(frames, observed) {
  const map = new Map();
  for (const f of frames) {
    map.set(f.frameId, { candidates: f.candidates, frameUrl: f.frameUrl, at: Date.now() });
  }
  const obs = new Map();
  for (const o of observed || []) {
    obs.set(o.url, { url: o.url, kind: o.kind, initiator: o.initiator || "", at: Date.now() });
  }
  ctxD.frames = map;
  ctxD.observed = obs;
  runIn(ctxD, "tabOverlayCandidates.clear(); tabOverlayCandidates.set(7, frames); tabObserved.clear(); tabObserved.set(7, observed);");
  return runIn(ctxD, "bestDownloadSource(7, null)");
}
const MONGOOSE_VARIANT = "https://cdn.lookmovie.example/mongoose/v0.m3u8";
const MOVIESJOY_MASTER = "https://vidrift.example.com/pl/abc123/master.m3u8";

test("Item 9: an unattributed stale observation must not beat the current page's stream", () => {
  // The LookMovie shape: frames report the Mongoose variant, while
  // tabObserved still holds the MoviesJoy master from the previous
  // film — with NO initiator, so the old code's origin filter
  // could not attribute (or reject) it, and the master's rank
  // beat the variant's. The verdict must be the Mongoose stream.
  const v = setElectionD(
    [{ frameId: 0, frameUrl: "https://lookmovie2.uk/movies/view/the-mongoose-2025", candidates: [
      { key: "a", source: { url: MONGOOSE_VARIANT, filename: undefined }, playing: true, duration: 0, area: 100000 },
    ]}],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: "" }]
  );
  assert(v, "a verdict must be returned");
  assertEq(v.url, MONGOOSE_VARIANT, "the current page's stream wins, not the stale observation");
});

test("Item 9: an attributed stale observation is still filtered by origin (control)", () => {
  const v = setElectionD(
    [{ frameId: 0, frameUrl: "https://lookmovie2.uk/movies/view/the-mongoose-2025", candidates: [
      { key: "a", source: { url: MONGOOSE_VARIANT, filename: undefined }, playing: true, duration: 0, area: 100000 },
    ]}],
    [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: "https://moviesjoy.st/watch/film" }]
  );
  assertEq(v.url, MONGOOSE_VARIANT, "the origin filter already rejected this one");
});

test("Item 9: observed findings alone still elect when no frame has reported (round 9 preserved)", () => {
  const v = setElectionD([], [{ url: MOVIESJOY_MASTER, kind: "playlist", initiator: "https://moviesjoy.st/watch/film" }]);
  assert(v, "observed-alone election still works");
  assertEq(v.url, MOVIESJOY_MASTER, "the observed playlist is the verdict");
});

test("Item 10: the verdict carries noStream through for a failed player", () => {
  const v = setElectionD(
    [{ frameId: 0, frameUrl: "https://primewire.example/film/123", candidates: [
      { key: "a", source: { url: "https://primewire.example/film/123", viaPage: true, noStream: true }, playing: false, duration: 0, area: 100000 },
    ]}],
    []
  );
  assert(v, "a verdict must be returned");
  assertEq(v.url, "https://primewire.example/film/123");
  assertEq(v.noStream, true, "the flag survives the election");
});

// ---------- Context E: navigation clears the election (item 9, whole background) ----------
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
  assert(chrome.__onUpdated, "background must listen for tab navigations (round 12, item 9)");
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

test("Item 9: navigating to a new film clears the previous film's observed stream", () => {
  const { chrome } = loadBackgroundE();
  // On MoviesJoy: the vidrift master is observed and elected.
  chrome.__onResponseStarted({
    url: MOVIESJOY_MASTER, tabId: 7, initiator: "https://moviesjoy.st/watch/film", type: "media",
    responseHeaders: [{ name: "Content-Type", value: "application/vnd.apple.mpegurl" }],
  });
  const before = lastVerdictE(chrome);
  assert(before && before.url === MOVIESJOY_MASTER, "pre-navigation verdict is the MoviesJoy stream: " + JSON.stringify(before));
  // Same tab navigates to LookMovie / The Mongoose.
  chrome.__onUpdated(7, { url: "https://lookmovie2.uk/movies/view/the-mongoose-2025" }, { id: 7, url: "https://lookmovie2.uk/movies/view/the-mongoose-2025" });
  // The new page's player fetches its variant — the election must
  // now return the Mongoose stream, never the MoviesJoy master
  // (before the fix the stale 30-minute observation won, because
  // no frame had reported yet and the origin filter was skipped).
  chrome.__onResponseStarted({
    url: MONGOOSE_VARIANT, tabId: 7, initiator: "https://lookmovie2.uk/movies/view/the-mongoose-2025", type: "media",
    responseHeaders: [{ name: "Content-Type", value: "application/vnd.apple.mpegurl" }],
  });
  const after = lastVerdictE(chrome);
  assert(after && after.url === MONGOOSE_VARIANT, "post-navigation verdict is the Mongoose stream: " + JSON.stringify(after));
});

// ---------- Context F: pickAndSend re-validates the verdict (item 9) ----------
const sendCallsF = [];
const closeCallsF = [];
const ctxF = makeContext(
  {
    document: { title: "The Mongoose | LookMovie" },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
  },
  [
    "let popupOpen = true;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet floatOverlay = null;\nlet currentDownload = null;\n"
    + "globalThis.__sendCalls = [];\n"
    + "function sendCurrentDownload(btn, dl, opts) { globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); return Promise.resolve({ ok: true }); }\n"
    + "function closeQualityPopup() { globalThis.__closeCalls.push(1); popupOpen = false; }\n"
    + "function currentPopupOptions() { return {}; }\n"
    + "function currentPopupFilename() { return 'The Mongoose'; }\n"
    + "function showPopupSendError(reason) { globalThis.__sendError = reason; }\n",
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

test("Item 9: a pick for a stale verdict sends nothing and closes the popup", async () => {
  // The popup was opened for the MoviesJoy verdict; the election
  // has since moved to the Mongoose stream (or the verdict
  // message was missed). The click must not send MoviesJoy.
  const { btn, p } = pickF(
    { url: MOVIESJOY_MASTER, filename: "master.m3u8" },
    { url: MONGOOSE_VARIANT }
  );
  const r = await p;
  assertEq(sendCallsF.length, 0, "no send for a stale verdict");
  assertEq(closeCallsF.length, 1, "the stale popup closes");
  assertEq(btn.textContent, "Stream changed — try again", "the button says why");
  assertEq(r.ok, false, "the pick resolves as a refusal, not a send");
});

test("Item 9: a pick for the current verdict still sends", async () => {
  const dl = { url: MONGOOSE_VARIANT, filename: "v0.m3u8" };
  const { p } = pickF(dl, { url: MONGOOSE_VARIANT });
  await p;
  await tick();
  assertEq(sendCallsF.length, 1, "the fresh pick sends");
  assertEq(sendCallsF[0].dl.url, MONGOOSE_VARIANT);
});

test("Item 9: a pick when the stream is gone sends nothing", async () => {
  const { p } = pickF({ url: MONGOOSE_VARIANT, filename: "v0.m3u8" }, null);
  await p;
  assertEq(sendCallsF.length, 0, "no stream, no send");
});

// ---------- Context G: failed-player detection (item 10) ----------
const ctxG = makeContext(
  {
    document: { querySelectorAll: function () { return []; } },
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
    extractMany(contentSrc, ["extractHtmlMedia", "isVisible", "mediaSource", "mediaTitle", "filenameForSource", "fileNameFromUrl", "sameDirectoryEncodes", "qualityHintScore", "resolveUrl"], 2),
  ]
);
function makeVideoG(currentSrc, error) {
  return {
    currentSrc: currentSrc || "",
    error: error || null,
    isConnected: true,
    getAttribute: function () { return null; },
    querySelectorAll: function () { return []; },
    getBoundingClientRect: function () { return { width: 640, height: 360, left: 0, top: 0, right: 640, bottom: 360 }; },
  };
}

test("Item 10: a failed blob player reports noStream instead of a usable viaPage source", () => {
  ctxG.el = makeVideoG("blob:https://primewire.example/abc", { code: 4 });
  runIn(ctxG, "__playlistStub = null;");
  const out = runIn(ctxG, "mediaSource(el)");
  assertEq(out.url, null);
  assertEq(out.viaPage, true);
  assertEq(out.noStream, true, "the failed player is flagged, not offered as a download");
});

test("Item 10: a healthy blob player still returns the plain viaPage source", () => {
  ctxG.el = makeVideoG("blob:https://example.com/abc", null);
  runIn(ctxG, "__playlistStub = null;");
  const out = runIn(ctxG, "mediaSource(el)");
  assertEq(out.viaPage, true);
  assert(!out.noStream, "no error, no flag");
});

test("Item 10: a failed player with an observed stream still returns the stream", () => {
  ctxG.el = makeVideoG("blob:https://example.com/abc", { code: 4 });
  runIn(ctxG, "__playlistStub = { url: 'https://cdn.example.com/master.m3u8', manifestText: undefined, masterText: undefined };");
  const out = runIn(ctxG, "mediaSource(el)");
  assertEq(out.url, "https://cdn.example.com/master.m3u8", "the observed stream wins over the element error");
  assert(!out.noStream, "a real stream is not flagged");
});

test("Item 10: the Media Detector does not list a failed player as downloadable", () => {
  ctxG.el = makeVideoG("blob:https://primewire.example/abc", { code: 4 });
  runIn(ctxG, "__playlistStub = null;");
  const extract = runIn(ctxG, "extractHtmlMedia('video')");
  assertEq(extract(ctxG.el), null, "no toolbar-popup row for the failed player");
  ctxG.el2 = makeVideoG("blob:https://example.com/abc", null);
  const item = extract(ctxG.el2);
  assert(item, "a healthy player is still listed");
  assertEq(item.url, "https://primewire.example/film/123", "viaPage item carries the page URL");
});

// ---------- Context H: overlay click on a noStream verdict (item 10) ----------
const ctxH = makeContext(
  {
    document: { title: "PrimeWire | Watch Film" },
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
function clickH(dl) {
  runIn(ctxH, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.simple.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxH.btn = btn;
  ctxH.dl = dl;
  runIn(ctxH, "currentDownload = dl; onOverlayClick(btn);");
  return { calls: runIn(ctxH, "__calls"), btn: btn };
}

test("Item 10: clicking the button on a failed player shows No stream found, never sends", async () => {
  const { calls, btn } = clickH({ url: "https://primewire.example/film/123", viaPage: true, noStream: true });
  await tick();
  assertEq(calls.formats.length, 0, "no formats popup for a dead player");
  assertEq(calls.send.length, 0, "the home address is never sent");
  assertEq(btn.textContent, "No stream found", "the button says so");
  assert(btn.title.indexOf("did not load a video") !== -1, "tooltip explains: " + btn.title);
});

test("Item 10: a healthy viaPage verdict still opens the formats popup (control)", async () => {
  const { calls } = clickH({ url: "https://primewire.example/film/123", viaPage: true });
  await tick();
  assertEq(calls.formats.length, 1, "the working path is untouched");
});

run("round12.test.js");
