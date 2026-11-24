// Round 7 content-script harness: Fix 4 (viaBrowser label),
// Fix 5 (title strips), Fix 6 (clip guard), Fix 7 (DASH popup).
// Round 8: always-popup (single MP4 / single-variant HLS open the
// popup with 1 quality + audio; master ladder highest-first;
// clip verdict refuses, no popup; bridge unreachable shows an
// error state, never a silent send).
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
  [extractMany(contentSrc, ["pageTitleStem", "isJunkTitle", "sanitizeFilenameStem", "parseMp4Duration"], 2)]
);

test("Fix 5: strips a leading (N) notification count", () => {
  // Round 10, item 11: the badge is stripped AND the remaining
  // fake notification text is junk, so the stem is now empty
  // (the caller falls through to the video title / h1 / og:title).
  ctx1.document.title = "(1) New Message!";
  assertEq(runIn(ctx1, "pageTitleStem()"), "");
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
    // Round 10: sendCurrentDownload reads/writes the module-level
    // revert timer; the harness provides it as a context global.
    sendRevertTimer: null,
  },
  [extractMany(contentSrc, ["sendCurrentDownload", "defaultFilenameFor", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "stemOfFilename", "sanitizeFilenameStem", "isManifestUrl", "shortFailureReason"], 2)]
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
function clickOverlay(dl, behaviour) {
  fetchBehaviour = behaviour || "short";
  fetchCalls.length = 0;
  runIn(ctx3, "__calls.hls.length = 0; __calls.formats.length = 0; __calls.simple.length = 0; __calls.send.length = 0; __calls.close = 0; popupOpen = false;");
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
  assertEq(calls3().simple.length, 0, "a refused clip shows the refusal, never the popup");
  assertEq(fetchCalls.length, 1);
});

test("Round 8: long file opens the popup after the probe (no silent send)", async () => {
  const btn = clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "long");
  await tick();
  assertEq(calls3().simple.length, 1, "the popup opens with 1 quality + audio");
  assertEq(calls3().send.length, 0, "nothing is sent before the user picks");
  assertEq(fetchCalls.length, 1, "the duration was probed before the popup");
  assertEq(btn.textContent, "↓ FetchForge", "the probe restores the button before the popup opens");
  assertEq(btn.disabled, false);
});

test("Round 8: probe failure fails OPEN to the popup", async () => {
  clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "throw");
  await tick();
  assertEq(calls3().simple.length, 1);
  assertEq(calls3().send.length, 0);
  clickOverlay({ url: "https://example.com/movie.mp4", filename: "movie.mp4", pageVideoDuration: 10356 }, "404");
  await tick();
  assertEq(calls3().simple.length, 1);
  assertEq(calls3().send.length, 0);
});

test("Round 8: no probe without a long video; video files open the popup, not a send", async () => {
  clickOverlay({ url: "https://example.com/clip.mp4", filename: "clip.mp4" }, "short");
  await tick();
  assertEq(calls3().simple.length, 1, "short page video: popup without probing");
  assertEq(calls3().send.length, 0);
  assertEq(fetchCalls.length, 0);
  clickOverlay({ url: "https://example.com/movie.mkv", filename: "movie.mkv", pageVideoDuration: 10356 }, "short");
  await tick();
  assertEq(calls3().simple.length, 1, "mkv is a video file: popup without probing (the guard covers mp4-family only)");
  assertEq(calls3().send.length, 0);
  assertEq(fetchCalls.length, 0);
  clickOverlay({ url: "https://example.com/v.m3u8", filename: "v.m3u8", pageVideoDuration: 10356 }, "short");
  await tick();
  assertEq(calls3().simple.length, 1, "single-variant HLS: popup without probing");
  assertEq(calls3().send.length, 0);
  assertEq(fetchCalls.length, 0);
});

test("Round 8: images, 3D models and documents still send one-click (no popup)", async () => {
  clickOverlay({ url: "https://example.com/photo.jpg", filename: "photo.jpg" });
  await tick();
  assertEq(calls3().send.length, 1, "image: one-click send");
  assertEq(calls3().simple.length, 0);
  clickOverlay({ url: "https://example.com/model.glb", filename: "model.glb" });
  await tick();
  assertEq(calls3().send.length, 1, "3D model: one-click send");
  clickOverlay({ url: "https://example.com/doc.pdf", filename: "doc.pdf" });
  await tick();
  assertEq(calls3().send.length, 1, "PDF: one-click send");
});

test("Round 8: single-variant master opens the HLS popup (audio choice alone is worth showing)", async () => {
  const master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\nmid.m3u8\n";
  clickOverlay({ url: "https://example.com/master.m3u8", filename: "master.m3u8", masterText: master });
  await tick();
  assertEq(calls3().hls.length, 1);
  assertEq(calls3().send.length, 0);
  assertEq(calls3().hls[0].parsed.rows.length, 1, "one variant row (the popup adds the audio row itself)");
});

test("Round 8: media-playlist text (no master) opens the simple popup", async () => {
  const media = "#EXTM3U\n#EXTINF:10,\nseg0.ts\n#EXTINF:10,\nseg1.ts\n#EXT-X-ENDLIST\n";
  clickOverlay({ url: "https://example.com/v.m3u8", filename: "v.m3u8", manifestText: media });
  await tick();
  assertEq(calls3().simple.length, 1);
  assertEq(calls3().hls.length, 0, "a media playlist is not a master");
  assertEq(calls3().send.length, 0);
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

// ---------- Context 4: openFormatsPopup (Fix 7 + Round 8) ----------
let formatsReply = null;
const sendCalls4 = [];
// The popup's 15 s timeout, captured (not fired) so the timeout
// test can fire it on demand; clearTimeout disarms an entry.
const pendingTimers = [];
function fireTimers() {
  for (const t of pendingTimers.splice(0)) {
    if (t.fn) t.fn();
  }
}
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
    window: {
      setTimeout: (fn) => { pendingTimers.push({ fn: fn }); return pendingTimers.length; },
      clearTimeout: (id) => { if (pendingTimers[id - 1]) pendingTimers[id - 1].fn = null; },
    },
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
    + "function pickAndSend(btn, dl, opts) { opts = opts || {}; if (!opts.filename) opts.filename = currentPopupFilename(); globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); return Promise.resolve({ ok: true }); }\n",
    extractMany(contentSrc, ["openFormatsPopup", "openQualityPopup", "closeQualityPopup", "addQualityRow", "popupDefaultName", "currentPopupFilename", "sanitizeFilenameStem", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "stemOfFilename", "sortFormatsForPopup", "isDashUrl"], 2),
  ]
);
ctx4.__sendCalls = sendCalls4;
async function openFormats(dl, reply) {
  formatsReply = reply;
  sendCalls4.length = 0;
  pendingTimers.length = 0;
  const overlay = makeFloatOverlay();
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctx4.overlay = overlay;
  ctx4.btn = btn;
  ctx4.dl = dl;
  runIn(ctx4, "floatOverlay = overlay; openFormatsPopup(btn, dl);");
  await tick();
  return { overlay: overlay, btn: btn };
}

test("Round 8: single-format response still shows the popup (no silent send)", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  const { overlay } = await openFormats(dl, { ok: true, formats: [{ format_id: "v1", label: "1080p", height: 1080, kind: "combined" }], default_format_id: "v1" });
  assertEq(sendCalls4.length, 0, "no send before the user picks");
  assertEq(overlay.popup.style.display, "flex");
  assertEq(overlay.popupBody.children.length, 1);
  assertEq(overlay.popupBody.children[0].textContent, "1080p");
});

test("Fix 7: multi-format response shows rows with filename field", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd", manifestText: "<MPD>captured</MPD>" };
  const { overlay } = await openFormats(dl, {
    ok: true,
    title: "The Odyssey (2026)",
    // Deliberately unsorted: the popup sorts the ladder itself.
    formats: [
      { format_id: "v720", label: "720p", height: 720, kind: "combined" },
      { format_id: "a1", label: "Audio only", kind: "audio_only" },
      { format_id: "v1080", label: "1080p", height: 1080, kind: "combined" },
    ],
    default_format_id: "v1080",
  });
  assertEq(sendCalls4.length, 0, "no send before the user picks");
  assertEq(overlay.popup.style.display, "flex");
  assertEq(overlay.popupBody.children.length, 3);
  assertEq(overlay.popupBody.children[0].textContent, "1080p", "ladder highest-first");
  assertEq(overlay.popupBody.children[1].textContent, "720p");
  assertEq(overlay.popupBody.children[2].textContent, "Audio only", "audio last");
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

test("Round 8: formats error shows the degraded popup for DASH (video + audio)", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  const { overlay } = await openFormats(dl, { ok: false, error: "bridge-500" });
  assertEq(sendCalls4.length, 0, "no silent send on a formats failure");
  assertEq(overlay.popup.style.display, "flex", "the popup stays open");
  assertEq(overlay.popupSubtitle.textContent, "Quality info unavailable");
  assertEq(overlay.popupBody.children.length, 2, "video row + audio row for DASH");
  assertEq(overlay.popupBody.children[0].textContent, "Video · default quality");
  assertEq(overlay.popupBody.children[1].textContent, "Audio only");
  // Pick the video row: default quality, no format id.
  runIn(ctx4, "__popupRows()[0].pick()");
  await tick();
  assertEq(sendCalls4.length, 1);
  assertEq(sendCalls4[0].opts.formatId, undefined);
  assert(!sendCalls4[0].opts.audioOnly, "the video row is not audio-only");
});

test("Round 8: degraded DASH audio row sends audio-only", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  await openFormats(dl, { ok: false, error: "bridge-500" });
  runIn(ctx4, "__popupRows()[1].pick()");
  await tick();
  assertEq(sendCalls4.length, 1);
  assertEq(sendCalls4[0].opts.audioOnly, true);
  assertEq(sendCalls4[0].opts.formatId, undefined);
});

test("Round 8: formats error shows video-only degraded rows for a page URL", async () => {
  const dl = { url: "https://www.youtube.com/watch?v=aqz-KE-bpKQ", viaPage: true };
  const { overlay } = await openFormats(dl, { ok: false, error: "bridge-500" });
  assertEq(sendCalls4.length, 0);
  assertEq(overlay.popup.style.display, "flex");
  assertEq(overlay.popupBody.children.length, 1, "page URLs get no audio row without the quality list");
  assertEq(overlay.popupBody.children[0].textContent, "Video · default quality");
});

test("Round 8: bridge unreachable shows the error state, no rows, no send", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  const { overlay } = await openFormats(dl, { ok: false, error: "no-token", unreachable: true });
  assertEq(sendCalls4.length, 0);
  assertEq(overlay.popup.style.display, "flex", "the popup stays open with the error");
  assertEq(overlay.popupTitle.textContent, "FetchForge app not reachable");
  assert(overlay.popupSubtitle.textContent.indexOf("paired") !== -1, "the subtitle must say what to do: " + overlay.popupSubtitle.textContent);
  assertEq(overlay.popupBody.children.length, 0, "no rows in the error state");
});

test("Round 8: timeout shows the degraded popup and a late reply never overwrites it", async () => {
  const dl = { url: "https://example.com/index_web.mpd", filename: "index_web.mpd" };
  let resolveReply;
  const deferred = new Promise((resolve) => { resolveReply = resolve; });
  const { overlay } = await openFormats(dl, deferred);
  // The formats reply has not arrived: fire the 15 s timeout.
  assertEq(pendingTimers.length, 1);
  fireTimers();
  await tick();
  assertEq(sendCalls4.length, 0);
  assertEq(overlay.popup.style.display, "flex");
  // Round 10, item 12: the timeout path names its own reason.
  assertEq(overlay.popupSubtitle.textContent, "The app took too long to list the qualities — it may be busy. Try again in a moment.");
  assertEq(overlay.popupBody.children.length, 2, "degraded DASH rows");
  // The late reply arrives after the timeout: it must not
  // overwrite the degraded popup (settled guard).
  resolveReply({
    ok: true,
    formats: [
      { format_id: "v1080", label: "1080p", height: 1080, kind: "combined" },
      { format_id: "v720", label: "720p", height: 720, kind: "combined" },
    ],
    default_format_id: "v1080",
  });
  await tick();
  await tick();
  assertEq(overlay.popupBody.children.length, 2, "the late reply must not replace the degraded rows");
  assertEq(overlay.popupBody.children[0].textContent, "Video · default quality");
  assertEq(sendCalls4.length, 0);
});

// ---------- Context 5: quality ladder (Round 8, pure) ----------
const ctx5 = makeContext(
  { document: { title: "" }, window: {} },
  [extractMany(contentSrc, ["parseHlsMaster", "parseHlsAttrs", "resolveAgainst", "friendlyCodec", "formatBandwidth", "hlsVariantHeight", "sortFormatsForPopup"], 2)]
);

test("Round 8: HLS ladder is height-first, not bandwidth-first", () => {
  const master = "#EXTM3U\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1280x720,CODECS=\"avc1.64001f\"\n720.m3u8\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=3000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028\"\n1080.m3u8\n";
  ctx5.text = master;
  const parsed = runIn(ctx5, "parseHlsMaster(text, 'https://example.com/master.m3u8')");
  assertEq(parsed.rows.length, 2);
  assert(parsed.rows[0].label.indexOf("1080p") !== -1, "1080p outranks fatter 720p: " + parsed.rows[0].label);
  assertEq(parsed.rows[0].selected, true, "the top row is pre-selected");
  assert(parsed.rows[1].label.indexOf("720p") !== -1, "second: " + parsed.rows[1].label);
  assertEq(parsed.hasAudioRows, false, "no audio groups: muxed");
});

test("Round 8: HLS ladder orders 2160, 1080, 720, 480 from scrambled input", () => {
  const master = "#EXTM3U\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=854x480\n480.m3u8\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=12000000,RESOLUTION=3840x2160\n2160.m3u8\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720\n720.m3u8\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080\n1080.m3u8\n";
  ctx5.text = master;
  const parsed = runIn(ctx5, "parseHlsMaster(text, 'https://example.com/master.m3u8')");
  const labels = parsed.rows.map((r) => r.label);
  assert(labels[0].indexOf("2160p") !== -1, "first: " + labels[0]);
  assert(labels[1].indexOf("1080p") !== -1, "second: " + labels[1]);
  assert(labels[2].indexOf("720p") !== -1, "third: " + labels[2]);
  assert(labels[3].indexOf("480p") !== -1, "fourth: " + labels[3]);
});

test("Round 8: HLS audio groups become audio-only rows flagged audioOnly", () => {
  const master = "#EXTM3U\n"
    + "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",URI=\"audio.m3u8\"\n"
    + "#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,AUDIO=\"aac\"\n720.m3u8\n";
  ctx5.text = master;
  const parsed = runIn(ctx5, "parseHlsMaster(text, 'https://example.com/master.m3u8')");
  assertEq(parsed.hasAudioRows, true);
  assertEq(parsed.rows.length, 2);
  assertEq(parsed.rows[1].label, "Audio only · English");
  assertEq(parsed.rows[1].audioOnly, true, "the audio row must carry the flag");
  assertEq(parsed.rows[1].url, "https://example.com/audio.m3u8");
  assertEq(parsed.rows[0].audioOnly, undefined, "video rows carry no flag");
});

test("Round 8: sortFormatsForPopup puts video height-first and audio last", () => {
  ctx5.formats = [
    { format_id: "a1", label: "Audio", kind: "audio_only" },
    { format_id: "v480", label: "480p", height: 480, vbr: 800, kind: "combined" },
    { format_id: "v720", label: "720p", height: 720, vbr: 2500, kind: "combined" },
    { format_id: "v1080", label: "1080p", height: 1080, vbr: 4500, kind: "combined" },
    { format_id: "v1080b", label: "1080p high-bitrate", height: 1080, vbr: 8000, kind: "video_only" },
  ];
  const sorted = runIn(ctx5, "sortFormatsForPopup(formats)");
  assertEq(sorted.map((f) => f.format_id).join(","), "v1080b,v1080,v720,v480,a1");
  // The input is not mutated.
  assertEq(ctx5.formats[0].format_id, "a1");
});

// ---------- Context 6: openHlsPopup audio rows (Round 8) ----------
const sendCalls6 = [];
const ctx6 = makeContext(
  {
    document: {
      title: "The Odyssey | NovaHD",
      createElement: () => ({ dataset: {}, style: {}, textContent: "", title: "", addEventListener: () => {} }),
    },
    window: { setTimeout: () => 1, clearTimeout: () => {} },
  },
  [
    "let popupOpen = false;\nlet popupRows = [];\nlet popupRequestId = 0;\nlet popupFilenameDirty = false;\nlet floatOverlay = null;\n"
    + "globalThis.__popupRows = function () { return popupRows; };\n"
    + "function capturedTextFor(url) { return undefined; }\n"
    + "function pickAndSend(btn, dl, opts) { opts = opts || {}; if (!opts.filename) opts.filename = currentPopupFilename(); globalThis.__sendCalls.push({ btn: btn, dl: dl, opts: opts }); return Promise.resolve({ ok: true }); }\n",
    extractMany(contentSrc, ["openHlsPopup", "openQualityPopup", "closeQualityPopup", "addQualityRow", "popupDefaultName", "currentPopupFilename", "sanitizeFilenameStem", "pageTitleStem", "pageNameHint", "isJunkTitle", "isHashLikeTitle", "stemOfFilename"], 2),
  ]
);
ctx6.__sendCalls = sendCalls6;
function openHls(dl, parsed) {
  sendCalls6.length = 0;
  const overlay = makeFloatOverlay();
  const btn = { disabled: false, textContent: "↓ FetchForge", title: "" };
  ctx6.overlay = overlay;
  ctx6.btn = btn;
  ctx6.dl = dl;
  ctx6.parsed = parsed;
  runIn(ctx6, "floatOverlay = overlay; openHlsPopup(btn, dl, parsed);");
  return { overlay: overlay, btn: btn };
}

test("Round 8: muxed master gets an Audio only row for the best variant", () => {
  const dl = { url: "https://example.com/master.m3u8", filename: "master.m3u8" };
  const parsed = {
    rows: [
      { label: "Best · 1080p", url: "https://example.com/1080.m3u8", selected: true },
      { label: "720p", url: "https://example.com/720.m3u8", selected: false },
    ],
    hasAudioRows: false,
  };
  const { overlay } = openHls(dl, parsed);
  assertEq(overlay.popupBody.children.length, 3, "two video rows + the muxed audio row");
  assertEq(overlay.popupBody.children[2].textContent, "Audio only");
  runIn(ctx6, "__popupRows()[2].pick()");
  assertEq(sendCalls6.length, 1);
  assertEq(sendCalls6[0].opts.url, "https://example.com/1080.m3u8", "audio of the BEST variant");
  assertEq(sendCalls6[0].opts.audioOnly, true);
});

test("Round 8: master with audio groups shows the group row, no extra muxed row", () => {
  const dl = { url: "https://example.com/master.m3u8", filename: "master.m3u8" };
  const parsed = {
    rows: [
      { label: "Best · 720p", url: "https://example.com/720.m3u8", audioUrl: "https://example.com/audio.m3u8", selected: true },
      { label: "Audio only · English", url: "https://example.com/audio.m3u8", audioUrl: undefined, audioOnly: true, selected: false },
    ],
    hasAudioRows: true,
  };
  const { overlay } = openHls(dl, parsed);
  assertEq(overlay.popupBody.children.length, 2, "no extra muxed row when a group row exists");
  runIn(ctx6, "__popupRows()[1].pick()");
  assertEq(sendCalls6.length, 1);
  assertEq(sendCalls6[0].opts.url, "https://example.com/audio.m3u8");
  assertEq(sendCalls6[0].opts.audioOnly, true, "audio-group rows send audio-only");
  assertEq(sendCalls6[0].opts.audioUrl, undefined);
});

run("content.test.js");
