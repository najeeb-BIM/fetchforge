// Round 11 harness: popup fallthrough (item 1), hash-like titles
// (items 2/13), visible-identity dedupe (item 3), same-directory
// best-encode scan (item 4), formats referer (item 5), and the
// desktop helpers fmtSpeed / displayTotal (items 8/10/11).
// Extension version 0.2.10.
"use strict";
const vm = require("vm");
const nodeCrypto = require("crypto");
const { readSource, extractMany, extractConst, extractFunction, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const bgSrc = readSource("extensions/chromium/background.js");
const html = readSource("apps/desktop/src/index.html");
const tick = () => new Promise((r) => setTimeout(r, 10));

// ---------- Context A: onOverlayClick fallthrough (item 1) ----------
const ctxA = makeContext(
  {
    document: { title: "The Odyssey | AZMovies" },
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
function clickA(dl) {
  runIn(ctxA, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.simple.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctxA.btn = btn;
  ctxA.dl = dl;
  runIn(ctxA, "currentDownload = dl; onOverlayClick(btn);");
  return runIn(ctxA, "__calls");
}

test("Item 1: an extensionless file-kind verdict opens the formats popup, never one-click", async () => {
  // The NOXX / 123Movies shape: an extensionless playlist URL
  // whose Content-Type did not classify as a playlist MIME, so
  // the election carried kind "file" and the URL failed every
  // popup branch.
  const calls = clickA({ url: "https://firandfable.space/pl/IG6fqf9x2k?token=abc", filename: "IG6fqf9x2k", kind: "file", pageVideoDuration: 10355 });
  await tick();
  assertEq(calls.formats.length, 1, "a video verdict always opens a popup");
  assertEq(calls.send.length, 0, "no silent one-click send");
  assertEq(calls.simple.length, 0);
});

test("Item 1: named image, 3D, document and audio files still one-click send", async () => {
  for (const [url, name] of [
    ["https://example.com/photo.jpg", "image"],
    ["https://example.com/model.glb", "3D model"],
    ["https://example.com/doc.pdf", "PDF"],
    ["https://example.com/track.mp3", "audio file"],
  ]) {
    const calls = clickA({ url: url, filename: url.split("/").pop(), kind: "file" });
    await tick();
    assertEq(calls.send.length, 1, name + ": one-click send");
    assertEq(calls.formats.length, 0, name + ": no popup");
  }
});

// ---------- Context B: hash-like title guards (items 2/13) ----------
const ctxB = makeContext(
  {
    document: {
      title: "The Odyssey · NOVA",
      querySelector: function () { return null; },
    },
  },
  [extractMany(contentSrc, ["isHashLikeTitle", "isJunkTitle", "stemOfFilename", "popupDefaultName", "pageTitleStem", "pageNameHint", "sanitizeFilenameStem"], 2)]
);

test("Items 2/13: isHashLikeTitle spots URL tokens, not real titles", () => {
  assertEq(runIn(ctxB, "isHashLikeTitle('0BGHIHfw0FTI-dNbLxEad3TYSrs97iR2W7IUCXXIJQ55hG38')"), true, "novahd hash token");
  assertEq(runIn(ctxB, "isHashLikeTitle('The Odyssey')"), false, "real title has spaces");
  assertEq(runIn(ctxB, "isHashLikeTitle('Inception')"), false, "short single word");
  assertEq(runIn(ctxB, "isHashLikeTitle('The.Odyssey.2026.1080p')"), false, "dotted release name");
  assertEq(runIn(ctxB, "isHashLikeTitle('short-token')"), false, "under 20 chars");
  assertEq(runIn(ctxB, "isHashLikeTitle('')"), false);
});

test("Items 2/13: a hash-like verdict stem falls through to 'download'", () => {
  assertEq(runIn(ctxB, "stemOfFilename('0BGHIHfw0FTI-dNbLxEad3TYSrs97iR2W7IUCXXIJQ55hG38')"), "", "hash stem is not a name");
  assertEq(runIn(ctxB, "stemOfFilename('movie.mp4')"), "movie");
  assertEq(runIn(ctxB, "stemOfFilename('index_287.ts')"), "index_287");
});

test("Item 2: popupDefaultName skips a hash-like server title for the page title", () => {
  ctxB.dl = { url: "https://example.com/stream", filename: "stream" };
  const name = runIn(ctxB, "popupDefaultName(dl, '0BGHIHfw0FTI-dNbLxEad3TYSrs97iR2W7IUCXXIJQ55hG38')");
  assertEq(name, "The Odyssey · NOVA", "page title beats the hash");
  const clean = runIn(ctxB, "popupDefaultName(dl, 'The Odyssey (2026)')");
  assertEq(clean, "The Odyssey (2026)", "a real server title still wins");
});

// ---------- Context C: openFormatsPopup keeps the pre-fill (item 2) ----------
let formatsReplyC = null;
const sendCallsC = [];
const pendingTimersC = [];
function makeFloatOverlayC() {
  return {
    popup: { style: { display: "none" } },
    popupTitle: { textContent: "" },
    popupSubtitle: { textContent: "", style: { display: "none" } },
    popupBody: { textContent: "", children: [], appendChild(c) { this.children.push(c); } },
    filenameInput: { value: "" },
  };
}
const ctxC = makeContext(
  {
    document: {
      title: "The Odyssey · NOVA",
      createElement: () => ({ dataset: {}, style: {}, textContent: "", title: "", addEventListener: () => {} }),
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
    extractMany(contentSrc, ["openFormatsPopup", "openQualityPopup", "closeQualityPopup", "addQualityRow", "popupDefaultName", "currentPopupFilename", "sanitizeFilenameStem", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "stemOfFilename", "sortFormatsForPopup", "isDashUrl"], 2),
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

test("Item 2: a hash-like server title does not overwrite the filename pre-fill", async () => {
  // The novahd retest shape: the field starts pre-filled from the
  // page title, then the formats reply arrives with a hash title.
  const dl = { url: "https://nova-edge.example.dev/mpd/0BGHIHfw0FTI/v0.m3u8", filename: "v0.m3u8" };
  const { overlay } = await openFormatsC(dl, {
    ok: true,
    title: "0BGHIHfw0FTI-dNbLxEad3TYSrs97iR2W7IUCXXIJQ55hG38",
    formats: [{ format_id: "v1080", label: "1080p", height: 1080, kind: "combined" }],
    default_format_id: "v1080",
  });
  assertEq(overlay.filenameInput.value, "The Odyssey · NOVA", "the good pre-fill survives the hash reply");
  assertEq(overlay.popupSubtitle.textContent, "", "the hash is not shown as the subtitle either");
  // And a clean server title still upgrades the field.
  const { overlay: overlay2 } = await openFormatsC(dl, {
    ok: true,
    title: "The Odyssey (2026)",
    formats: [{ format_id: "v1080", label: "1080p", height: 1080, kind: "combined" }],
    default_format_id: "v1080",
  });
  assertEq(overlay2.filenameInput.value, "The Odyssey (2026)", "a real server title still upgrades the field");
});

// ---------- Context D: sortFormatsForPopup visible identity (item 3) ----------
const ctxD = makeContext(
  {},
  [extractMany(contentSrc, ["sortFormatsForPopup"], 2)]
);

test("Item 3: sortFormatsForPopup dedupes 140/140-drc, keeps 233/234", () => {
  ctxD.formats = [
    { format_id: "140", label: "Audio only · m4a · 129 kbps · 9.8 MB", kind: "audio_only", abr: 129, filesize: 10276000 },
    { format_id: "140-drc", label: "Audio only · m4a · 129 kbps · 9.8 MB", kind: "audio_only", abr: 129, filesize: 10276000 },
    { format_id: "233", label: "Audio only · mp4", kind: "audio_only" },
    { format_id: "234", label: "Audio only · mp4", kind: "audio_only" },
    { format_id: "251", label: "Audio only · webm · 160 kbps · 12.0 MB", kind: "audio_only", abr: 160, filesize: 12000000 },
  ];
  const out = runIn(ctxD, "sortFormatsForPopup(formats)");
  const ids = out.map((f) => f.format_id);
  assertEq(out.length, 4, "140-drc deduped, 233+234 kept: " + ids.join(","));
  assert(ids.includes("140") && !ids.includes("140-drc"), "first occurrence wins");
  assert(ids.includes("233") && ids.includes("234"), "measurement-less pair survives");
});

// ---------- Context E: mediaSource same-directory scan (item 4) ----------
function makeDocE(links) {
  return {
    title: "Night of the Living Dead (1968) : Free Download",
    baseURI: "https://archive.org/details/Night.Of.The.Living.Dead_1080p",
    querySelector: function () { return null; },
    querySelectorAll: function (sel) {
      if (sel === "a[href]") {
        return links.map((href) => ({
          getAttribute: function (name) { return name === "href" ? href : null; },
          href: href,
        }));
      }
      return [];
    },
  };
}
function makeVideoE(currentSrc, sources) {
  return {
    currentSrc: currentSrc || "",
    getAttribute: function (name) { return name === "src" ? null : null; },
    querySelectorAll: function () {
      return sources.map((u) => ({
        getAttribute: function () { return u; },
        src: u,
      }));
    },
  };
}
const ctxE = makeContext(
  { document: makeDocE([]), location: { href: "https://archive.org/details/Night.Of.The.Living.Dead_1080p" } },
  [
    "const capturedManifests = new Map();\n"
    + "function findPlaylistFromResources() { return null; }\n"
    + "function findStreamUrlFromResources() { return null; }\n",
    extractConst(contentSrc, "STREAM_EXT_RE", 2) + "\n",
    extractMany(contentSrc, ["mediaSource", "sameDirectoryEncodes", "qualityHintScore", "resolveUrl"], 2),
  ]
);

test("Item 4: qualityHintScore reads the filename, not a hint in the directory", () => {
  const dir = "https://archive.org/download/Night.Of.The.Living.Dead_1080p/";
  assertEq(runIn(ctxE, "qualityHintScore('" + dir + "NightOfTheLivingDead.mp4')"), 0, "the 1080p in the directory name must not inflate the standard mp4");
  assert(runIn(ctxE, "qualityHintScore('" + dir + "NightOfTheLivingDead_1080p.mkv')") > 10000, "the 1080p in the filename scores");
});

test("Item 4: with no playing source, a same-directory 1080p mkv link wins", () => {
  // The Archive shape: the <source> elements list the standard
  // mp4, but the file list beside it links the 1080p mkv. (Both
  // the 1080p mp4 and the 1080p mkv are 1080p — the test lists
  // only the mkv so the mechanism is unambiguous.)
  const dir = "https://archive.org/download/Night.Of.The.Living.Dead_1080p/";
  ctxE.document = makeDocE([
    dir + "NightOfTheLivingDead_1080p.mkv",
    "https://archive.org/download/Other.Item/unrelated.mp4",
  ]);
  ctxE.el = makeVideoE("", [dir + "NightOfTheLivingDead.mp4"]);
  const out = runIn(ctxE, "mediaSource(el)");
  assertEq(out.url, dir + "NightOfTheLivingDead_1080p.mkv", "the 1080p mkv beats the standard mp4");
});

test("Item 4: a playing currentSrc still wins over same-directory links", () => {
  const dir = "https://archive.org/download/Night.Of.The.Living.Dead_1080p/";
  ctxE.document = makeDocE([dir + "NightOfTheLivingDead_1080p.mkv"]);
  ctxE.el = makeVideoE(dir + "NightOfTheLivingDead_512kb.mp4", [dir + "NightOfTheLivingDead.mp4"]);
  const out = runIn(ctxE, "mediaSource(el)");
  assertEq(out.url, dir + "NightOfTheLivingDead_512kb.mp4", "round 10, item 8: the playing source is the site's choice");
});

// ---------- Context F: getFormats carries the referer (item 5) ----------
let tokenF = "test-token-123";
const fetchCallsF = [];
const ctxF = makeContext(
  {
    chrome: {
      runtime: { lastError: undefined },
      storage: {
        local: {
          get: (key, cb) => { cb(tokenF ? { token: tokenF } : {}); },
          set: (items, cb) => { if (cb) cb(); },
        },
      },
    },
    crypto: nodeCrypto.webcrypto,
    fetch: async (url, opts) => {
      fetchCallsF.push({ url: url, body: opts && opts.body });
      if (url.endsWith("/challenge")) {
        const nonce = JSON.parse(opts.body).nonce;
        const hmac = nodeCrypto.createHmac("sha256", tokenF).update(nonce).digest("hex");
        return { ok: true, json: async () => ({ hmac: hmac }) };
      }
      if (url.endsWith("/media/formats")) {
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

test("Item 5: getFormats sends the page referer in the formats POST body", async () => {
  fetchCallsF.length = 0;
  await runIn(ctxF, "getFormats('https://vidrift.example.com/stream/abc', 'https://flixmovie.st/watch/movie-the-odyssey')");
  const post = fetchCallsF.find((c) => c.url.endsWith("/media/formats"));
  assert(post, "the formats POST must have happened");
  assertEq(JSON.parse(post.body).referer, "https://flixmovie.st/watch/movie-the-odyssey");
  // Without a referer the body carries no referer key at all.
  fetchCallsF.length = 0;
  await runIn(ctxF, "getFormats('https://example.com/watch')");
  const post2 = fetchCallsF.find((c) => c.url.endsWith("/media/formats"));
  assertEq(JSON.parse(post2.body).referer, undefined);
});

// ---------- Context G: desktop helpers (items 8/10/11) ----------
const ctxG = makeContext(
  {},
  [extractMany(html, ["fmtBytes", "fmtSpeed", "displayTotal", "progressPercent"], 0)]
);

test("Item 8: fmtSpeed renders B/s, KB/s, MB/s", () => {
  assertEq(runIn(ctxG, "fmtSpeed(0)"), "-");
  assertEq(runIn(ctxG, "fmtSpeed(null)"), "-");
  assertEq(runIn(ctxG, "fmtSpeed(512)"), "512 B/s");
  assertEq(runIn(ctxG, "fmtSpeed(1536)"), "1.5 KB/s");
  assertEq(runIn(ctxG, "fmtSpeed(2500000)"), "2.4 MB/s");
});

test("Items 10/11: displayTotal marks the ladder kind, falls back to the estimate", () => {
  const t = (d) => { ctxG.d = d; return runIn(ctxG, "displayTotal(d)"); };
  assertEq(t({ total_bytes: 4900000000, size_kind: "exact" }).text, "4.6 GB", "exact: plain");
  assertEq(t({ total_bytes: 4900000000, size_kind: "average" }).text, "≈4.6 GB", "average: ≈");
  assertEq(t({ total_bytes: 4900000000, size_kind: "peak" }).text, "~4.6 GB", "peak: ~");
  assertEq(t({ total_bytes: 4900000000, size_kind: "measured" }).text, "~4.6 GB", "measured: ~");
  assertEq(t({ total_bytes: 4900000000 }).text, "4.6 GB", "no kind recorded: plain");
  const est = t({ total_bytes: null, estimated_total_bytes: 1200000000 });
  assertEq(est.text, "~1.1 GB", "estimate stands in with ~");
  assertEq(est.bytes, 1200000000);
  assertEq(t({ total_bytes: null, estimated_total_bytes: null }), null, "no total at all");
});

run("round11.test.js");
