// Round 7 content-script harness: Fix 4 (viaBrowser label),
// Fix 5 (title strips), Fix 6 (clip guard), Fix 7 (DASH popup).
// Every function under test is the exact source extracted from
// extensions/chromium/content.js — see extract.js.
"use strict";
const { readSource, extractMany, makeContext, runIn, test, assert, assertEq, run } = require("./extract");

const contentSrc = readSource("extensions/chromium/content.js");
const tick = () => new Promise((r) => setTimeout(r, 10));

// ---------- mp4 fixtures (Fix 6) ----------
function box(type, payload) {
  const out = new Uint8Array(8 + payload.length);
  const dv = new DataView(out.buffer);
  dv.setUint32(0, out.length);
  for (let i = 0; i < 4; i++) out[4 + i] = type.charCodeAt(i);
  out.set(payload, 8);
  return out;
}
function concatBytes(...arrs) {
  const total = arrs.reduce((n, a) => n + a.length, 0);
  const out = new Uint8Array(total);
  let off = 0;
  for (const a of arrs) { out.set(a, off); off += a.length; }
  return out;
}
function mvhdV0(timescale, duration) {
  const payload = new Uint8Array(20);
  const dv = new DataView(payload.buffer);
  dv.setUint32(12, timescale);
  dv.setUint32(16, duration);
  return box("mvhd", payload);
}
function mvhdV1(timescale, duration) {
  const payload = new Uint8Array(32);
  payload[0] = 1;
  const dv = new DataView(payload.buffer);
  dv.setUint32(20, timescale);
  dv.setBigUint64(24, BigInt(duration));
  return box("mvhd", payload);
}
function mp4WithMoov(...children) {
  return concatBytes(
    box("ftyp", new Uint8Array([105, 115, 111, 109, 0, 0, 0, 0])),
    box("moov", concatBytes(...children))
  );
}
const SHORT_CLIP = mp4WithMoov(mvhdV0(1000, 15000)); // 15 s, the novahd ad clip
const LONG_FILE = mp4WithMoov(mvhdV0(1000, 10356000)); // 2h52m36s, the film

// ---------- Context 1: pure helpers (Fix 5 + Fix 6 parser) ----------
const ctx1 = makeContext(
  { document: { title: "" }, window: {} },
  [extractMany(contentSrc, ["pageTitleStem", "sanitizeFilenameStem", "parseMp4Duration"], 2)]
);

test("Fix 5: strips a leading (N) notification count", () => {
  ctx1.document.title = "(1) New Message!";
  assertEq(runIn(ctx1, "pageTitleStem()"), "New Message!");
  ctx1.document.title = "(12) The Odyssey";
  assertEq(runIn(ctx1, "pageTitleStem()"), "The Odyssey");
  ctx1.document.title = "(1) (2) Stacked";
  assertEq(runIn(ctx1, "pageTitleStem()"), "Stacked");
});

test("Fix 5: strips a leading Watching prefix", () => {
  ctx1.document.title = "Watching The Odyssey";
  assertEq(runIn(ctx1, "pageTitleStem()"), "The Odyssey");
  ctx1.document.title = "WATCHING Dune";
  assertEq(runIn(ctx1, "pageTitleStem()"), "Dune");
  ctx1.document.title = "(3) Watching The Odyssey";
  assertEq(runIn(ctx1, "pageTitleStem()"), "The Odyssey");
});

test("Fix 5: keeps existing strips and a 4-digit year", () => {
  ctx1.document.title = "Watch The Odyssey (2026) | NovaHD";
  assertEq(runIn(ctx1, "pageTitleStem()"), "The Odyssey (2026)");
  ctx1.document.title = "(2026) Dune";
  assertEq(runIn(ctx1, "pageTitleStem()"), "(2026) Dune");
  ctx1.document.title = "(1)";
  assertEq(runIn(ctx1, "pageTitleStem()"), "");
  ctx1.document.title = "";
  assertEq(runIn(ctx1, "pageTitleStem()"), "");
  ctx1.document.title = "Spider-Man - Homecoming";
  assertEq(runIn(ctx1, "pageTitleStem()"), "Spider-Man - Homecoming");
});

test("Fix 6: parseMp4Duration reads v0 and v1 mvhd", () => {
  ctx1.bytes = SHORT_CLIP;
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), 15);
  ctx1.bytes = LONG_FILE;
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), 10356);
  ctx1.bytes = mp4WithMoov(mvhdV1(1000, 7200));
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), 7.2);
});

test("Fix 6: parseMp4Duration returns null when unknown, never short", () => {
  ctx1.bytes = box("ftyp", new Uint8Array([105, 115, 111, 109, 0, 0, 0, 0]));
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), null);
  ctx1.bytes = mp4WithMoov(box("trak", new Uint8Array([1, 2, 3, 4])));
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), null);
  ctx1.bytes = mp4WithMoov(box("mvhd", new Uint8Array(10))); // truncated mvhd
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), null);
  ctx1.bytes = new Uint8Array([0, 1, 2, 3, 4, 5, 6, 7, 8, 9]);
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), null);
  ctx1.bytes = new Uint8Array(0);
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), null);
});

test("Fix 6: parseMp4Duration skips boxes before moov", () => {
  const free = box("free", new Uint8Array(64));
  ctx1.bytes = concatBytes(
    box("ftyp", new Uint8Array([105, 115, 111, 109, 0, 0, 0, 0])),
    free,
    box("moov", mvhdV0(1000, 15000))
  );
  assertEq(runIn(ctx1, "parseMp4Duration(bytes)"), 15);
});

// ---------- Context 2: sendCurrentDownload (Fix 4) ----------
let nextReply = { ok: true };
const sendCalls2 = [];
const ctx2 = makeContext(
  {
    document: { title: "The Odyssey | NovaHD" },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
    sendRuntimeMessage: (msg) => { sendCalls2.push(msg); return Promise.resolve(nextReply); },
  },
  [extractMany(contentSrc, ["sendCurrentDownload", "defaultFilenameFor", "pageTitleStem", "stemOfFilename", "sanitizeFilenameStem", "isManifestUrl", "shortFailureReason"], 2)]
);
function clickSend(reply, dl) {
  nextReply = reply;
  sendCalls2.length = 0;
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctx2.btn = btn;
  ctx2.dl = dl || { url: "https://example.com/v.mp4", filename: "v.mp4" };
  runIn(ctx2, "sendCurrentDownload(btn, dl, {})");
  return btn;
}

test("Fix 4: browser fallback shows Downloaded by browser, not Sent", async () => {
  const btn = clickSend({ ok: true, viaBrowser: true });
  await tick();
  assertEq(btn.textContent, "Downloaded by browser");
  assert(btn.title.indexOf("browser") !== -1, "tooltip must explain: " + btn.title);
});

test("Fix 4: app send still shows Sent to FetchForge", async () => {
  const btn = clickSend({ ok: true });
  await tick();
  assertEq(btn.textContent, "Sent to FetchForge");
});

test("Fix 4: duplicate and failure paths unchanged", async () => {
  const dup = clickSend({ ok: true, duplicate: true });
  await tick();
  assertEq(dup.textContent, "Already downloaded");
  const fail = clickSend({ ok: false, error: "bridge-500" });
  await tick();
  assert(fail.textContent.indexOf("Failed") === 0, "failure label: " + fail.textContent);
});

// ---------- Context 3: onOverlayClick routing (Fix 6 + Fix 7) ----------
let fetchBehaviour = "short"; // short | long | throw | 404
const fetchCalls = [];
const ctx3 = makeContext(
  {
    document: { title: "The Odyssey | NovaHD" },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
    fetch: async (url) => {
      fetchCalls.push(url);
      if (fetchBehaviour === "throw") throw new Error("CORS blocked");
      if (fetchBehaviour === "404") return { ok: false, status: 404, arrayBuffer: async () => new ArrayBuffer(0) };
      const bytes = fetchBehaviour === "long" ? LONG_FILE : SHORT_CLIP;
      return { ok: true, status: 206, arrayBuffer: async () => bytes.buffer.slice(0) };
    },
  },
  [
    "let currentDownload = null;\nlet popupOpen = false;\n"
    + "const __calls = { hls: [], formats: [], send: [], close: 0 };\n"
    + "globalThis.__calls = __calls;\n"
    + "function closeQualityPopup() { __calls.close++; popupOpen = false; }\n"
    + "function openHlsPopup(btn, dl, parsed) { __calls.hls.push({ btn: btn, dl: dl, parsed: parsed }); popupOpen = true; }\n"
    + "function openFormatsPopup(btn, dl) { __calls.formats.push({ btn: btn, dl: dl }); popupOpen = true; }\n"
    + "function sendCurrentDownload(btn, dl, opts) { __calls.send.push({ btn: btn, dl: dl, opts: opts }); }\n",
    extractMany(contentSrc, ["onOverlayClick", "parseHlsMaster", "parseHlsAttrs", "resolveAgainst", "friendlyCodec", "formatBandwidth", "hlsVariantHeight", "probeDirectFileDuration", "parseMp4Duration", "isManifestUrl", "isDashUrl", "isMp4FamilyUrl"], 2),
  ]
);
function clickOverlay(dl, behaviour) {
  fetchBehaviour = behaviour || "short";
  fetchCalls.length = 0;
  runIn(ctx3, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctx3.btn = btn;
  ctx3.dl = dl;
  runIn(ctx3, "currentDownload = dl; onOverlayClick(btn);");
  return btn;
}
const calls3 = () => runIn(ctx3, "__calls");

test("Fix 6: short clip on a long-form page is refused, never sent", async () => {
  const btn = clickOverlay({ url: "https://novahd.cc/assets/p/41876ddc05627e720d53f4a2.mp4", filename: "41876ddc05627e720d53f4a2.mp4", pageVideoDuration: 10356 }, "short");
  await tick();
  assertEq(btn.textContent, "Only a short clip was found");
  assertEq(calls3().send.length, 0);
  assertEq(fetchCalls.length, 1);
});

test("Fix 6: long file is sent after the probe", async () => {
  const btn = clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "long");
  await tick();
  assertEq(calls3().send.length, 1);
  assertEq(fetchCalls.length, 1, "the duration was probed before sending");
  assertEq(btn.textContent, "Checking…", "the stubbed send leaves the label alone; the real send sets Sending…");
});

test("Fix 6: probe failure fails OPEN (send proceeds)", async () => {
  clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "throw");
  await tick();
  assertEq(calls3().send.length, 1);
  clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "404");
  await tick();
  assertEq(calls3().send.length, 1);
});

test("Fix 6: no probe without a long video, a non-mp4 file, or a manifest", async () => {
  clickOverlay({ url: "https://example.com/clip.mp4", filename: "clip.mp4" }, "short");
  await tick();
  assertEq(calls3().send.length, 1, "short page video: send without probing");
  assertEq(fetchCalls.length, 0);
  clickOverlay({ url: "https://example.com/movie.mkv", filename: "movie.mkv", pageVideoDuration: 10356 }, "short");
  await tick();
  assertEq(calls3().send.length, 1, "non-mp4: send without probing");
  assertEq(fetchCalls.length, 0);
  clickOverlay({ url: "https://example.com/v.m3u8", filename: "v.m3u8", pageVideoDuration: 10356 }, "short");
  await tick();
  assertEq(calls3().send.length, 1, "manifest: send without probing");
  assertEq(fetchCalls.length, 0);
});

test("Fix 7: DASH manifest opens the formats popup", async () => {
  clickOverlay({ url: "https://surge.flooderrr.store/sacdn/dash/2413592258760133680_0_0_1080_h265_894/index_web.mpd", filename: "index_web.mpd" });
  await tick();
  assertEq(calls3().formats.length, 1);
  assertEq(calls3().send.length, 0);
});

test("Fix 7: DASH with captured mpd text still opens the formats popup", async () => {
  clickOverlay({ url: "https://example.com/index_web.mpd", filename: "index_web.mpd", manifestText: "<MPD></MPD>" });
  await tick();
  assertEq(calls3().formats.length, 1);
  assertEq(calls3().hls.length, 0, "mpd text is not an HLS master");
});

test("Fix 7: HLS master and viaPage routing unchanged", async () => {
  const master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360\nlow.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\nmid.m3u8\n";
  clickOverlay({ url: "https://example.com/master.m3u8", filename: "master.m3u8", masterText: master });
  await tick();
  assertEq(calls3().hls.length, 1, "multi-variant master opens the HLS popup");
  assertEq(calls3().formats.length, 0);
  clickOverlay({ url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ", viaPage: true });
  await tick();
  assertEq(calls3().formats.length, 1, "page URL opens the formats popup");
});

// ---------- Context 4: openFormatsPopup (Fix 7) ----------
let formatsReply = null;
const sendCalls4 = [];
function makeFloatOverlay() {
  const children = [];
  const body = {
    children: children,
    appendChild(c) { children.push(c); return c; },
    querySelector() { return children[0] || null; },
  };
  Object.defineProperty(body, "textContent", { set() { children.length = 0; }, get() { return ""; } });
  return {
    popup: { style: { display: "none" } },
    popupTitle: { textContent: "" },
    popupSubtitle: { textContent: "", style: { display: "none" } },
    popupBody: body,
    filenameInput: { value: "" },
  };
}
const ctx4 = makeContext(
  {
    document: {
      title: "The Odyssey | NovaHD",
      createElement: () => ({ dataset: {}, style: {}, textContent: "", title: "", addEventListener: () => {} }),
    },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
    sendRuntimeMessage: (msg) => {
      assertEq(msg.type, "getFormats");
      return Promise.resolve(formatsReply);
    },
  },
  [
    "let popupOpen = false;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet popupFilenameDirty = false;\n"
    + "const FORMATS_TIMEOUT_MS = 15000;\nlet floatOverlay = null;\n"
    + "globalThis.__popupRows = function () { return popupRows; };\n"
    + "globalThis.__popupOpen = function () { return popupOpen; };\n"
    + "function sendCurrentDownload(btn, dl, opts) { globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); }\n",
    extractMany(contentSrc, ["openFormatsPopup", "openQualityPopup", "closeQualityPopup", "addQualityRow", "popupDefaultName", "currentPopupFilename", "sanitizeFilenameStem", "pageTitleStem", "stemOfFilename"], 2),
  ]
);
ctx4.__sendCalls = sendCalls4;
async function openFormats(dl, reply) {
  formatsReply = reply;
  sendCalls4.length = 0;
  const overlay = makeFloatOverlay();
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctx4.overlay = overlay;
  ctx4.btn = btn;
  ctx4.dl = dl;
  runIn(ctx4, "floatOverlay = overlay; openFormatsPopup(btn, dl);");
  await tick();
  return { overlay: overlay, btn: btn };
}

test("Fix 7: single-format response falls through to one-click", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  const { overlay } = await openFormats(dl, { ok: true, formats: [{ format_id: "v1", label: "1080p" }], default_format_id: "v1" });
  assertEq(sendCalls4.length, 1, "one-click fallback must send");
  assertEq(overlay.popup.style.display, "none", "popup closes on fallback");
});

test("Fix 7: multi-format response shows rows with filename field", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd", manifestText: "<MPD>captured</MPD>" };
  const { overlay } = await openFormats(dl, {
    ok: true,
    title: "The Odyssey (2026)",
    formats: [
      { format_id: "v1080", label: "1080p" },
      { format_id: "v720", label: "720p" },
      { format_id: "a1", label: "Audio only" },
    ],
    default_format_id: "v1080",
  });
  assertEq(sendCalls4.length, 0, "no send before the user picks");
  assertEq(overlay.popup.style.display, "flex");
  assertEq(overlay.popupBody.children.length, 3);
  assertEq(overlay.filenameInput.value, "The Odyssey (2026)", "server title upgrades the filename field");
  // Pick the second row: the send carries the format id AND the
  // captured manifest text (signed links may fail on re-fetch).
  runIn(ctx4, "__popupRows()[1].pick()");
  await tick();
  assertEq(sendCalls4.length, 1);
  assertEq(sendCalls4[0].opts.formatId, "v720");
  assertEq(sendCalls4[0].opts.manifestText, "<MPD>captured</MPD>");
  assertEq(sendCalls4[0].opts.filename, "The Odyssey (2026)");
});

test("Fix 7: formats error falls back to one-click", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  const { overlay } = await openFormats(dl, { ok: false, error: "bridge down" });
  assertEq(sendCalls4.length, 1);
  assertEq(overlay.popup.style.display, "none");
});

run("content.test.js");
