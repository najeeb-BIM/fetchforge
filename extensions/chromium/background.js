// FetchForge extension background script (shared source; see docs/EXTENSION_SETUP.md).
// NOT VERIFIED in a real browser: pairing and challenge code below was
// written in this round and has not run in Chrome/Edge/Firefox yet.
// Token hygiene: the token lives ONLY in storage.local, read only here
// in the background script. It is never put in a content script, page
// context, URL, or log. The content script (content.js) detects media
// and sends only the clicked media/page URL here; it never sees the token.
// Minimal permissions, no browsing history is read or sent anywhere.
// Flow: context-menu click / overlay click / popup -> try native messaging
// -> try loopback bridge with the stored token -> fall back to the browser's
// own downloader. Detected media (content script reports) is POSTed to
// POST /media/detected best-effort and shown as the toolbar badge count.
// Service-worker safe: no DOM access, every listener is registered at
// the top level on each start. Download/pairing state lives in storage.
// Detected-media reporting keeps per-tab frame state in memory (lost on
// a service-worker restart, rebuilt from the next content-script report);
// only the badge counts are mirrored to storage so the badge survives
// a restart. Reporting is best-effort, so losing it is never fatal.
const api = typeof browser !== "undefined" ? browser : chrome;
const isFirefox = typeof browser !== "undefined";
const BRIDGE = "http://127.0.0.1:18443";
const HOST = "com.fetchforge.bridge";

// Call a browser API in both styles: Firefox returns a Promise, Chrome
// (MV3) also returns a Promise for most APIs, but sendNativeMessage /
// sendMessage historically need a callback in Chrome. This helper tries
// the Promise form first when running under `browser`, and the callback
// form under `chrome`, so a missing native host can never hang forever.
function callApi(fn, ...args) {
  if (isFirefox) return fn(...args);
  return new Promise((resolve, reject) => {
    fn(...args, (result) => {
      const err = api.runtime.lastError || api.lastError;
      if (err) reject(new Error(err.message));
      else resolve(result);
    });
  });
}

async function getToken() {
  const s = await callApi((...a) => api.storage.local.get(...a), "token");
  return (s && s.token) || "";
}
// Round 10, item 15: this browser's stable instance id, generated
// once and kept in background storage. It identifies the browser
// install (for the app's paired-browsers count), not the user —
// and it is sent only to the local bridge at pair time, never
// with downloads. Never rejects: storage failure yields a fresh
// ephemeral id instead of breaking pairing.
async function getBrowserId() {
  try {
    const s = await callApi((...a) => api.storage.local.get(...a), "browserId");
    if (s && s.browserId) return s.browserId;
  } catch (_) {}
  const id = (typeof crypto !== "undefined" && crypto.randomUUID)
    ? crypto.randomUUID()
    : ("ff-" + Date.now().toString(36) + "-" + Math.random().toString(36).slice(2, 10));
  try { await callApi((...a) => api.storage.local.set(...a), { browserId: id }); } catch (_) {}
  return id;
}
// Browser name for the paired-browsers list, from the user agent
// (Edge first — its UA contains "Chrome" too).
function browserName() {
  const ua = (typeof navigator !== "undefined" && navigator.userAgent) || "";
  if (/Edg\//.test(ua)) return "Edge";
  if (/Firefox\//.test(ua)) return "Firefox";
  if (/OPR\//.test(ua)) return "Opera";
  if (/Chrome\//.test(ua)) return "Chrome";
  return "Browser";
}
// Challenge-response: before sending the stored token to a server,
// prove that server knows it. Generate a random nonce, ask the server
// for HMAC(token, nonce), and verify it locally with the stored token.
// If verification fails (or crypto is unavailable), do NOT send the token.
async function verifyServer(token) {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  const nonce = [...bytes].map((b) => b.toString(16).padStart(2, "0")).join("");
  const r = await fetch(BRIDGE + "/challenge", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ nonce }),
  });
  if (!r.ok) return false;
  const j = await r.json();
  if (!j.hmac) return false;
  const key = await crypto.subtle.importKey("raw", new TextEncoder().encode(token), { name: "HMAC", hash: "SHA-256" }, false, ["sign"]);
  const sig = await crypto.subtle.sign("HMAC", key, new TextEncoder().encode(nonce));
  const expected = [...new Uint8Array(sig)].map((b) => b.toString(16).padStart(2, "0")).join("");
  if (expected.length !== j.hmac.length) return false;
  let diff = 0;
  for (let i = 0; i < expected.length; i++) diff |= expected.charCodeAt(i) ^ j.hmac.charCodeAt(i);
  return diff === 0;
}
async function sendViaBridge(url, filename, referer, audioUrl, formatId, manifestText, audioOnly, audioLang, subtitleLangs, audioFormat) {
  const token = await getToken();
  if (!token) throw new Error("no-token");
  if (!(await verifyServer(token))) throw new Error("challenge-failed");
  const r = await fetch(BRIDGE + "/downloads", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
    body: JSON.stringify({ url, filename, referer, audio_url: audioUrl, format_id: formatId, manifest_text: manifestText, audio_only: audioOnly, audio_lang: audioLang, subtitle_langs: subtitleLangs, audio_format: audioFormat }),
  });
  if (!r.ok) throw new Error("bridge-" + r.status);
  // The bridge answers 202 once the download is queued;
  // extraction and the download itself run in the worker, so a
  // failure surfaces later as a Failed row in the app, not here.
  // The record is still checked: a "failed" status is reported
  // rather than claimed as success.
  const rec = await r.json().catch(() => null);
  if (rec && rec.status === "failed") return { ok: false, error: rec.error || "Download failed" };
  // A duplicate add queues nothing: the file is already in the
  // app's downloads. Report it honestly instead of claiming a
  // fresh download was sent (the bridge flags it; see
  // DownloadRecord.duplicate).
  if (rec && rec.duplicate) return { ok: true, duplicate: true };
  return { ok: true };
}
async function sendViaNative(url, filename, referer, audioUrl, formatId, manifestText, audioOnly, audioLang, subtitleLangs, audioFormat) {
  const resp = await callApi(
    (...a) => api.runtime.sendNativeMessage(...a),
    HOST,
    { type: "addDownload", url, filename, referer, audio_url: audioUrl, format_id: formatId, manifest_text: manifestText, audio_only: audioOnly, audio_lang: audioLang, subtitle_langs: subtitleLangs, audio_format: audioFormat }
  );
  if (resp && resp.type === "error") throw new Error(resp.error);
  // Same as the bridge path: the native host forwards the bridge's
  // record, which may already say the download failed — or that
  // it is a duplicate (nothing new was queued).
  if (resp && resp.download && resp.download.status === "failed") {
    return { ok: false, error: resp.download.error || "Download failed" };
  }
  if (resp && resp.download && resp.download.duplicate) {
    return { ok: true, duplicate: true };
  }
  return { ok: true };
}
// Returns { ok, error? }: ok=false with a reason when the app
// accepted the request but the download itself failed at once, or
// when no path could take the URL at all. Callers surface `error`
// to the user (overlay button, popup status) instead of a silent
// revert or a bare "could not send". referer is the page the media
// came from (sent as the Referer header on segment requests —
// signed CDNs refuse requests without it); audioUrl is a sibling
// audio variant to download and merge with a video variant.
// formatId is the quality the user picked in the popup (a yt-dlp
// format id for a page URL, undefined for a default send).
// manifestText is playlist text the extension captured from the
// player (round 6): the engine uses it instead of re-fetching a
// possibly single-use signed playlist link. Over the 256 KB cap
// it is dropped here (defence in depth — the content script
// already caps at capture) and the engine fetches normally.
// audioOnly is the popup's "Audio only" choice (round 8): the app
// delivers an audio file instead of the video. audioLang /
// subtitleLangs / audioFormat are the quality popup's option
// selects (round 11 extension, features 2/4/13): the chosen
// audio language, the subtitle languages to mux in, and the
// audio container for an audio-only send. Undefined values are
// dropped by JSON.stringify, so a plain send carries none of
// them.
async function sendDownload(url, filename, referer, audioUrl, formatId, manifestText, audioOnly, audioLang, subtitleLangs, audioFormat) {
  if (!/^https?:\/\//i.test(url)) return { ok: false, error: "Not a web link" };
  if (typeof manifestText === "string" && manifestText.length > 262144) manifestText = undefined;
  try {
    const r = await sendViaNative(url, filename, referer, audioUrl, formatId, manifestText, audioOnly, audioLang, subtitleLangs, audioFormat);
    // A real failure reason from the app (not a transport miss):
    // the native host already asked the bridge, so retrying the
    // bridge directly would only fail the same way again.
    // Otherwise return the result as-is: it may carry
    // `duplicate` (nothing new was queued), which the caller
    // reports honestly instead of claiming a fresh send.
    if (r && r.ok === false) return r;
    return r;
  } catch (_) { /* host not installed */ }
  try { return await sendViaBridge(url, filename, referer, audioUrl, formatId, manifestText, audioOnly, audioLang, subtitleLangs, audioFormat); } catch (_) { /* bridge not running / no token */ }
  // Documented fallback: let the browser handle it normally, and
  // record it so the popup can say so (not a silent fallback).
  // Round 11 extension, feature 14: register the URL first, so
  // download interception does not re-grab a file the extension
  // itself just handed to the browser (an infinite
  // pause/cancel loop). The set is capped: a stale entry (the
  // download never started) only ever lets one later download
  // of the same URL slip through un-intercepted — fail-open.
  try {
    browserFallbackUrls.add(url);
    if (browserFallbackUrls.size > 100) browserFallbackUrls.delete(browserFallbackUrls.values().next().value);
    await callApi((...a) => api.downloads.download(...a), { url, saveAs: false }); try { await callApi((...a) => api.storage.local.set(...a), { lastFallback: Date.now() }); } catch (_) {} return { ok: true, viaBrowser: true }; }
  catch (_) { return { ok: false, error: "Could not reach the FetchForge app" }; }
}
// Format list for the IDM-style quality popup (round 6, study C1):
// POST the page URL to the bridge's POST /media/formats with the
// stored token + HMAC challenge (same pattern as sendViaBridge).
// The bridge probes the page and returns { title, extractor,
// duration_secs, formats: [{ format_id, label, ext, kind, height,
// width, abr, vbr, fps, filesize, filesize_approx, needs_merge }],
// default_format_id, resolved_url? }. Only the page URL is sent —
// no cookies, no credentials, no page content. Failures are
// classified for the popup (round 8): a throw with
// `unreachable: true` means the bridge could not be reached at
// all (no token, challenge failed, network failure) and the popup
// shows an error state; a plain throw (bridge-<status>) means the
// bridge answered but the listing failed, and the popup shows its
// degraded single-quality rows instead.
async function getFormats(url, referer) {
  const token = await getToken();
  if (!token) { const e = new Error("no-token"); e.unreachable = true; throw e; }
  let verified;
  try {
    verified = await verifyServer(token);
  } catch (_) {
    const e = new Error("bridge unreachable"); e.unreachable = true; throw e;
  }
  if (!verified) { const e = new Error("challenge-failed"); e.unreachable = true; throw e; }
  let r;
  try {
    // Round 11, item 5: the page referer goes with the request —
    // the bridge forwards it when it fetches the manifest, and
    // vidrift-class hosts 403 a manifest fetch without it.
    const body = { url };
    if (referer) body.referer = referer;
    r = await fetch(BRIDGE + "/media/formats", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
      body: JSON.stringify(body),
    });
  } catch (_) {
    const e = new Error("bridge unreachable"); e.unreachable = true; throw e;
  }
  if (!r.ok) {
    // Round 10, item 12: the bridge returns {"error": <friendly
    // text>} with the failure — carry it through instead of
    // discarding it, so the degraded popup can say WHY the
    // qualities are unavailable.
    let reason = "";
    try { const j = await r.json(); if (j && j.error) reason = String(j.error); } catch (_) {}
    const e = new Error("bridge-" + r.status);
    e.reason = reason || ("The bridge could not list this page's qualities (HTTP " + r.status + ").");
    throw e;
  }
  return await r.json();
}
// ---- Browser download interception (round 11 extension, feature 14) ----
// Opt-in, default OFF (the popup's toggle persists
// `interceptDownloads` in storage.local): when enabled, a
// browser-initiated download of a media file — video/audio by
// extension or MIME type — is paused, sent to FetchForge, and
// cancelled in the browser once the app takes it (IDM/XDM-style).
// Any failure resumes the browser download (fail-open): the user
// never loses a download because the app was unreachable.
// Non-media downloads are never touched. browserFallbackUrls
// holds URLs the sendDownload fallback handed to the browser, so
// interception does not re-grab them (an infinite pause/cancel
// loop); each entry is consumed by the next onCreated for it.
const browserFallbackUrls = new Set();
const INTERCEPT_EXTENSIONS = ["mp4", "mkv", "webm", "mov", "avi", "m4v", "3gp", "wmv", "flv", "ts", "m3u8", "mpd", "mp3", "m4a", "opus", "ogg", "flac", "wav", "aac"];
// Pure: is this download item a media file worth intercepting?
// MIME type first (a download's extension can lie), then the URL
// path's extension. Unit-tested by the harness.
function isInterceptableDownload(item) {
  if (!item || !item.url || !/^https?:\/\//i.test(item.url)) return false;
  const mime = String(item.mime || "").toLowerCase();
  if (mime.indexOf("video/") === 0 || mime.indexOf("audio/") === 0) return true;
  let path = "";
  try { path = new URL(item.url).pathname.toLowerCase(); } catch (_) { return false; }
  const ext = path.split(".").pop();
  return INTERCEPT_EXTENSIONS.indexOf(ext) !== -1;
}
// The download item's own filename (a full path on disk) reduced
// to its basename — the name the app saves the file under.
function downloadItemBasename(item) {
  const raw = String((item && item.filename) || (item && item.url) || "").replace(/\\/g, "/");
  const base = raw.split("/").pop() || "download";
  try { return decodeURIComponent(base); } catch (_) { return base; }
}
if (api.downloads && api.downloads.onCreated) {
  api.downloads.onCreated.addListener((item) => {
    if (!item || !item.url) return;
    if (browserFallbackUrls.has(item.url)) { browserFallbackUrls.delete(item.url); return; }
    if (!isInterceptableDownload(item)) return;
    (async () => {
      let enabled = false;
      try {
        const s = await callApi((...a) => api.storage.local.get(...a), "interceptDownloads");
        enabled = !!(s && s.interceptDownloads);
      } catch (_) { enabled = false; }
      if (!enabled) return;
      // Pause first: a small file could finish before the app
      // answers. A pause that fails (already finished) means
      // there is nothing left to intercept — fail open.
      try { await callApi((...a) => api.downloads.pause(...a), item.id); } catch (_) { return; }
      const r = await sendDownload(item.url, downloadItemBasename(item)).catch(() => ({ ok: false }));
      try {
        if (r && r.ok) {
          // The app took it — or the send fell back to the
          // browser's own downloader, which is already
          // downloading it: either way the paused original is
          // cancelled, never resumed (resuming would download
          // the file twice).
          await callApi((...a) => api.downloads.cancel(...a), item.id);
        } else {
          // Nothing took it: let the browser finish the download.
          await callApi((...a) => api.downloads.resume(...a), item.id);
        }
      } catch (_) { /* the download finished or vanished meanwhile */ }
    })();
  });
}
api.runtime.onInstalled.addListener(() => {
  try {
    api.contextMenus.create({ id: "fetchforge-download", title: "Download with FetchForge", contexts: ["link", "video", "audio"] });
  } catch (_) { /* already created on a service-worker restart */ }
});

// ---- Detected-media reporting (Phase 4 data flow) ----
// Content scripts report their detected set per frame; the background
// merges frames per tab, shows the count as the toolbar badge, and
// POSTs the merged set to the bridge (POST /media/detected) so the
// app's Media Detector panel (Phase 9) can list it. Reporting is
// best-effort and silent: no token, no bridge, or a missing endpoint
// (the desktop agent is building it in parallel) never breaks
// detection or downloading. Only confirmed metadata is sent —
// no cookies, no credentials, no page content. The token is used
// here exactly as for downloads (HMAC challenge first) and never
// leaves the background script.
const tabFrames = new Map(); // tabId -> Map(frameId -> items[])
const tabPostTimers = new Map(); // tabId -> timeout id
const tabLastPosted = new Map(); // tabId -> JSON key of last POSTed set
let mediaCounts = {}; // tabId -> count, mirrored to storage.local for SW restarts

async function loadMediaCounts() {
  try {
    const s = await callApi((...a) => api.storage.local.get(...a), "mediaCounts");
    if (s && s.mediaCounts) mediaCounts = s.mediaCounts;
  } catch (_) {}
}
async function saveMediaCounts() {
  try { await callApi((...a) => api.storage.local.set(...a), { mediaCounts }); } catch (_) {}
}
function setBadge(tabId, count) {
  try {
    api.action.setBadgeText({ tabId, text: count > 0 ? String(count) : "" });
    api.action.setBadgeBackgroundColor({ tabId, color: "#22c55e" });
  } catch (_) { /* action API unavailable */ }
}
function mergedItems(tabId) {
  const frames = tabFrames.get(tabId);
  const items = [];
  const seen = new Set();
  if (!frames) return items;
  for (const list of frames.values()) {
    for (const item of list) {
      const key = item.kind + "|" + item.url;
      if (seen.has(key)) continue;
      seen.add(key);
      items.push(item);
    }
  }
  return items;
}
async function postDetected(tabId, pageUrl) {
  const items = mergedItems(tabId).map((it) => Object.assign({}, it, { pageUrl }));
  const key = JSON.stringify(items);
  if (tabLastPosted.get(tabId) === key) return;
  tabLastPosted.set(tabId, key);
  let token = "";
  try { token = await getToken(); } catch (_) { return; }
  if (!token) return;
  try {
    if (!(await verifyServer(token))) return;
    await fetch(BRIDGE + "/media/detected", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
      body: JSON.stringify({ items }),
    });
    // Any status is fine: a 404 just means the endpoint has not landed yet.
  } catch (_) { /* bridge unreachable; reporting is best-effort */ }
}
function handleReport(tabId, frameId, items, pageUrl) {
  if (!tabFrames.has(tabId)) tabFrames.set(tabId, new Map());
  tabFrames.get(tabId).set(frameId, Array.isArray(items) ? items : []);
  const count = mergedItems(tabId).length;
  mediaCounts[tabId] = count;
  setBadge(tabId, count);
  saveMediaCounts();
  if (tabPostTimers.has(tabId)) clearTimeout(tabPostTimers.get(tabId));
  tabPostTimers.set(tabId, setTimeout(() => {
    tabPostTimers.delete(tabId);
    postDetected(tabId, pageUrl);
  }, 1000));
}
// ---- Clipboard monitoring (round 11 extension, feature 6) ----
// Opt-in, default OFF (the popup's toggle persists
// `clipboardWatch` in storage.local): when enabled, a 1-minute
// alarm reads the clipboard and OFFERS any media URL it finds —
// it never starts a download. The offer lands in the app's Media
// Detector panel (POST /media/detected) like any detected item,
// and the user downloads it from there. Dedupe: the
// last-offered URL is kept in storage.local, so the same
// clipboard text is offered once, not once a minute.
// Chromium MV3: a service worker has no document focus, so
// navigator.clipboard.readText() fails there — the read goes
// through a short-lived offscreen document (offscreen.html,
// chromium-only) that reads the clipboard on load and replies.
// Firefox's background page is a real document and reads
// directly. The clipboardRead permission is OPTIONAL (manifest
// optional_permissions); the popup requests it when the user
// enables the toggle, and the check below verifies it is still
// granted before every read.
const CLIPBOARD_ALARM = "fetchforge-clipboard";
const CLIPBOARD_MEDIA_HOSTS = ["youtube.com", "youtu.be", "vimeo.com", "dailymotion.com", "twitch.tv", "tiktok.com", "instagram.com", "facebook.com", "fb.watch", "x.com", "twitter.com", "soundcloud.com", "reddit.com", "drive.google.com"];
const CLIPBOARD_MEDIA_EXTENSIONS = ["mp4", "mkv", "webm", "mov", "avi", "m4v", "3gp", "wmv", "flv", "ts", "m3u8", "mpd", "mp3", "m4a", "opus", "ogg", "flac", "wav", "aac"];
// Finds the first media URL in pasted text: the first http(s)
// URL whose host is a known media page or whose path names a
// media file or manifest. Trailing sentence punctuation is
// stripped (a period after a pasted link is not part of the
// URL). Returns null when the text carries no media URL.
// Pure — unit-tested by the harness.
function extractMediaUrlFromText(text) {
  const matches = String(text || "").match(/https?:\/\/[^\s"'<>]+/gi) || [];
  for (const raw of matches) {
    const url = raw.replace(/[.,;:!?)]+$/, "");
    let host = "", path = "";
    try { const u = new URL(url); host = u.hostname.toLowerCase(); path = u.pathname.toLowerCase(); } catch (_) { continue; }
    if (CLIPBOARD_MEDIA_HOSTS.some((h) => host === h || host.endsWith("." + h))) return url;
    const ext = path.split(".").pop();
    if (CLIPBOARD_MEDIA_EXTENSIONS.indexOf(ext) !== -1) return url;
  }
  return null;
}
async function clipboardPermissionGranted() {
  try {
    if (!api.permissions || !api.permissions.contains) return false;
    if (isFirefox) return await api.permissions.contains({ permissions: ["clipboardRead"] });
    return await new Promise((resolve) => api.permissions.contains({ permissions: ["clipboardRead"] }, resolve));
  } catch (_) { return false; }
}
// Chromium only: the offscreen document reads the clipboard on
// load and replies with { type: "clipboardText", text }. The
// listener is registered BEFORE the document is created, so a
// fast reply is never missed; the document is closed after the
// reply (or after 5 s with no reply).
async function readClipboardTextViaOffscreen() {
  if (!api.offscreen || !api.offscreen.createDocument) return null;
  let onMsg = null;
  try {
    const text = await new Promise((resolve) => {
      const timer = setTimeout(() => { cleanup(); resolve(null); }, 5000);
      function cleanup() {
        clearTimeout(timer);
        if (onMsg) { try { api.runtime.onMessage.removeListener(onMsg); } catch (_) {} onMsg = null; }
      }
      onMsg = function (msg) {
        if (msg && msg.type === "clipboardText") { cleanup(); resolve(typeof msg.text === "string" ? msg.text : null); }
      };
      api.runtime.onMessage.addListener(onMsg);
      // Close a stale document from a previous poll first (its
      // one read already went to that poll's listener), then
      // create a fresh one — it reads on load and replies here.
      (async () => {
        try { await api.offscreen.closeDocument(); } catch (_) {}
        try {
          await api.offscreen.createDocument({
            url: "offscreen.html",
            reasons: ["CLIPBOARD"],
            justification: "Read the clipboard for media URLs when clipboard monitoring is enabled.",
          });
        } catch (_) { cleanup(); resolve(null); }
      })();
    });
    return text;
  } catch (_) { return null; }
  finally {
    try { await api.offscreen.closeDocument(); } catch (_) {}
  }
}
async function readClipboardText() {
  // Firefox background page: a real document, so the Clipboard
  // API works directly. (In a Chromium service worker
  // navigator.clipboard is undefined and this falls through.)
  if (typeof navigator !== "undefined" && navigator.clipboard && navigator.clipboard.readText) {
    try { return await navigator.clipboard.readText(); } catch (_) { return null; }
  }
  return readClipboardTextViaOffscreen();
}
async function offerClipboardUrl(url) {
  let token = "";
  try { token = await getToken(); } catch (_) { return; }
  if (!token) return;
  try {
    if (!(await verifyServer(token))) return;
    await fetch(BRIDGE + "/media/detected", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
      body: JSON.stringify({ items: [{ kind: "video", url: url, title: "From clipboard", pageUrl: "Clipboard" }] }),
    });
  } catch (_) { /* bridge unreachable; the offer is best-effort */ }
}
async function checkClipboard() {
  try {
    const s = await callApi((...a) => api.storage.local.get(...a), ["clipboardWatch", "lastClipboardOffer"]);
    if (!s || !s.clipboardWatch) return;
    if (!(await clipboardPermissionGranted())) return;
    const text = await readClipboardText();
    if (!text) return;
    const url = extractMediaUrlFromText(text);
    if (!url) return;
    if (s.lastClipboardOffer === url) return; // already offered
    await offerClipboardUrl(url);
    await callApi((...a) => api.storage.local.set(...a), { lastClipboardOffer: url });
  } catch (_) { /* clipboard reads are best-effort; never break the background */ }
}
async function applyClipboardWatchPref() {
  try {
    const s = await callApi((...a) => api.storage.local.get(...a), "clipboardWatch");
    const enabled = !!(s && s.clipboardWatch);
    if (!api.alarms) return;
    if (enabled) {
      await api.alarms.create(CLIPBOARD_ALARM, { periodInMinutes: 1 });
    } else {
      await api.alarms.clear(CLIPBOARD_ALARM);
    }
  } catch (_) { /* alarms unavailable */ }
}
if (api.alarms && api.alarms.onAlarm) {
  api.alarms.onAlarm.addListener((alarm) => {
    if (alarm && alarm.name === CLIPBOARD_ALARM) checkClipboard();
  });
}
if (api.storage && api.storage.onChanged) {
  api.storage.onChanged.addListener((changes, area) => {
    if (area === "local" && changes.clipboardWatch) applyClipboardWatchPref();
  });
}
applyClipboardWatchPref();
// Ad blocker: apply the stored on/off pref at startup (the ruleset
// is enabled by default in the manifest; the popup toggle persists
// the choice in storage.local). Chrome also persists the enabled
// state itself, so this is belt-and-braces after an update/reload.
async function applyStoredAdBlockPref() {
  try {
    const s = await callApi((...a) => api.storage.local.get(...a), "adBlockEnabled");
    const enabled = !s || typeof s.adBlockEnabled !== "boolean" ? true : s.adBlockEnabled;
    if (api.declarativeNetRequest && api.declarativeNetRequest.updateEnabledRulesets) {
      await api.declarativeNetRequest.updateEnabledRulesets(
        enabled ? { enableRulesetIds: ["ff-ads"] } : { disableRulesetIds: ["ff-ads"] }
      );
    }
  } catch (_) { /* DNR unavailable; blocking stays as the manifest left it */ }
}
applyStoredAdBlockPref();
loadMediaCounts();
if (api.tabs && api.tabs.onActivated) {
  api.tabs.onActivated.addListener((info) => {
    if (typeof mediaCounts[info.tabId] === "number") setBadge(info.tabId, mediaCounts[info.tabId]);
  });
}
// ---- One button per tab: cross-frame overlay election ----
// Content scripts report their overlay representatives per frame
// (each with its frame's resolved stream source — only the frame
// that fetched a manifest can see it in its timing log); the
// background elects exactly ONE winner per tab (playing first,
// then largest area, then the lowest frameId, then the lowest key)
// and tells the top frame which stream its button downloads.
// Frames re-report every 10 s, so a service-worker restart
// rebuilds the state from the next report; frames that stopped
// reporting are pruned after 12 s. Election state is in memory
// only — losing it just means buttons show until the next verdict.
const tabOverlayCandidates = new Map(); // tabId -> Map(frameId -> { candidates, frameUrl, at })
const OVERLAY_CANDIDATE_TTL_MS = 12000;
function sendToFrame(tabId, frameId, msg) {
  try {
    const p = api.tabs.sendMessage(tabId, msg, { frameId: frameId });
    if (p && typeof p.catch === "function") p.catch(function () { /* frame gone */ });
  } catch (_) { /* tabs API unavailable */ }
}
// Stream ranking, mirroring content.js: a playlist is the stream
// that is playing; a master playlist is the whole stream (video
// AND audio), a video variant is one track, an audio variant is
// audio only.
// Round 9: match .m3u8/.mpd ANYWHERE in the URL, as content.js
// does — proxy-wrapped playlists carry the extension mid-URL.
const BG_STREAM_EXT_RE = /\.(m3u8|mpd)/i;
const BG_MEDIA_FILE_EXT_RE = /\.(mp4|webm|mov|mkv)/i;
const BG_PLAYLIST_MIME_RE = /(?:vnd\.apple\.mpegurl|x-mpegurl|dash\+xml)/i;
const BG_MASTER_STEM_RE = /^(master|index|playlist|manifest)$/i;
const BG_AUDIO_STEM_RE = /audio/i;
function bgManifestKind(url) {
  const name = url.split(/[?#]/)[0].split("/").pop() || "";
  const stem = name.replace(/\.(m3u8|mpd)$/i, "");
  if (BG_MASTER_STEM_RE.test(stem)) return "master";
  if (BG_AUDIO_STEM_RE.test(stem)) return "audio";
  return "video";
}
// The stream the top frame's button downloads, picked from ALL
// frames' reported sources. Ranking order (round 6, study B1/B4):
//   0. POOL: a playlist anywhere beats a direct file anywhere: the
//      playlist is the stream that is playing, while a direct file
//      is often a preview/ad clip (novahd 2026-11-22: the top
//      frame's clip won the election while the movie's playlist
//      sat in the player iframe's timing log, and the click sent
//      the clip).
//   1. DURATION of the PLAYING video, descending — the primary
//      signal when no playlist was observed anywhere: a 15-second
//      clip vs a 2h52m film separates cleanly on duration, while
//      size does not (the novahd clip is 3.1 MB). Only PLAYING
//      videos count (a paused/unknown-duration video scores 0),
//      so a long paused video never beats the stream that is
//      actually playing.
//   2. Election winner (playing first, then largest area).
//   3. Playing.
//   4. Playlist kind: master > video > audio.
//   5. Largest area.
// Duration deliberately outranks the winner: the winner is picked
// by playing + area alone, and a big autoplaying preview can win
// the election while the film plays in another frame.
// Returns { url, filename?, audioUrl?, referer?, viaPage?,
// manifestText?, masterText?, pageVideoDuration } (referer is the frame the stream
// came from, sent as the Referer header on segment requests;
// viaPage marks a page URL for the formats popup; manifestText is
// the captured text for the download URL, masterText the captured
// text of the frame's master playlist — see content.js;
// pageVideoDuration is the longest video duration seen in ANY
// frame, playing or not, for the content script's clip guard) or
// null when no frame has a source.
function bestDownloadSource(tabId, winner) {
  const frames = tabOverlayCandidates.get(tabId);
  const all = [];
  if (frames) {
    for (const [fid, entry] of frames.entries()) {
      for (const c of entry.candidates) {
        if (!c.source || !c.source.url) continue;
        all.push({ frameId: fid, frameUrl: entry.frameUrl, cand: c });
      }
    }
  }
  // Round 9 (Tier 1): merge webRequest observations as
  // additional pool entries. An observed playlist carries its
  // kind (playlist vs file) from classification, so it does not
  // depend on the URL matching BG_STREAM_EXT_RE (extensionless
  // playlists would fail that test). playing: true — the
  // browser requested it, so the stream is live; duration 0 —
  // unknown, so a real playing video still outranks it on
  // duration inside the playlist pool. Stale findings from a
  // previous page on the same tab are filtered out: an entry
  // only counts when its initiator origin matches a frame the
  // tab currently has (when no frame has reported yet, nothing
  // is filtered — the observation may precede the first report).
  const observed = tabObserved.get(tabId);
  if (observed && observed.size) {
    const now = Date.now();
    const frameOrigins = new Set();
    if (frames) {
      for (const entry of frames.values()) {
        try { frameOrigins.add(new URL(entry.frameUrl || "").origin); } catch (_) {}
      }
    }
    for (const rec of observed.values()) {
      if (now - rec.at > OBSERVED_TTL_MS) continue;
      // Round 12, item 9: an observed finding with NO initiator
      // cannot be attributed to any page — it may be a stale
      // entry from a previous navigation (or a previous tab, on
      // tab-id reuse). Once frames have reported for the current
      // page, the frame layers cover the stream, so an
      // unattributable observation must not win the election and
      // send a previous film's URL. Before any frame reports,
      // observed findings are the only signal (round 9) and stay.
      if (frameOrigins.size && !rec.initiator) continue;
      if (frameOrigins.size && rec.initiator) {
        let origin = "";
        try { origin = new URL(rec.initiator).origin; } catch (_) { origin = ""; }
        if (origin && !frameOrigins.has(origin)) continue;
      }
      all.push({
        frameId: -1,
        frameUrl: rec.initiator || "",
        cand: {
          key: "observed",
          playing: true,
          area: 0,
          duration: 0,
          source: { url: rec.url, viaPage: false, filename: undefined, audioUrl: undefined, manifestText: undefined, masterText: undefined, kind: rec.kind },
        },
      });
    }
  }
  if (!all.length) return null;
  const isWinner = function (e) {
    return !!winner && e.frameId === winner.frameId && e.cand.key === winner.key;
  };
  const isPlaylist = function (e) {
    // Observed findings carry their kind from classification
    // (an extensionless playlist has no .m3u8/.mpd in its URL);
    // frame candidates are classified by URL.
    if (e.cand.source.kind === "playlist") return true;
    if (e.cand.source.kind === "file") return false;
    return BG_STREAM_EXT_RE.test(e.cand.source.url);
  };
  const playlistRank = function (e) {
    if (!isPlaylist(e)) return 0;
    const k = bgManifestKind(e.cand.source.url);
    return k === "master" ? 2 : k === "video" ? 1 : 0;
  };
  // Playing duration in seconds, else 0 (paused, live, or no
  // metadata yet — see the ranking comment above).
  const playingDuration = function (e) {
    if (!e.cand.playing) return 0;
    const d = e.cand.duration;
    return (typeof d === "number" && isFinite(d) && d > 0) ? d : 0;
  };
  const playlists = all.filter(isPlaylist);
  const pool = playlists.length ? playlists : all;
  pool.sort(function (a, b) {
    const ad = playingDuration(a), bd = playingDuration(b);
    if (ad !== bd) return bd - ad;
    const aw = isWinner(a) ? 1 : 0, bw = isWinner(b) ? 1 : 0;
    if (aw !== bw) return bw - aw;
    const ap = a.cand.playing ? 1 : 0, bp = b.cand.playing ? 1 : 0;
    if (ap !== bp) return bp - ap;
    const ak = playlistRank(a), bk = playlistRank(b);
    if (ak !== bk) return bk - ak;
    return (b.cand.area || 0) - (a.cand.area || 0);
  });
  const best = pool[0];
  const s = best.cand.source;
  // Round 7 (clip guard): the longest video duration seen in ANY
  // frame, playing or not — the content script uses it to tell a
  // long-form watch page (a film playing) from a page whose only
  // video is a short clip. Unlike the election's playing-only
  // ranking, a paused film still proves the page is long-form.
  let pageVideoDuration = 0;
  for (const e of all) {
    const d = e.cand.duration;
    if (typeof d === "number" && isFinite(d) && d > pageVideoDuration) pageVideoDuration = d;
  }
  // Round 10, item 1: carry the kind in the verdict, so the content
  // script can route an observed playlist to the formats popup
  // instead of one-click sending it (the kind was dropped here,
  // and the click handler could only guess from the URL).
  // Round 12, item 10: carry noStream — a failed player the
  // content script flagged, so the button can say "No stream
  // found" instead of sending the page URL.
  return { url: s.url, filename: s.filename, audioUrl: s.audioUrl, referer: best.frameUrl, viaPage: !!s.viaPage, manifestText: s.manifestText, masterText: s.masterText, pageVideoDuration: pageVideoDuration, kind: s.kind || (isPlaylist(best) ? "playlist" : "file"), noStream: !!s.noStream };
}
function electOverlay(tabId) {
  const frames = tabOverlayCandidates.get(tabId);
  const observed = tabObserved.get(tabId);
  // Round 9: run the election on observed findings alone too —
  // a tab whose frames never reported a candidate (no <video>
  // element anywhere) can still have a stream the observer saw.
  if (!frames && !(observed && observed.size)) return;
  const now = Date.now();
  if (frames) {
    for (const [fid, entry] of Array.from(frames.entries())) {
      if (now - entry.at > OVERLAY_CANDIDATE_TTL_MS) frames.delete(fid);
    }
  }
  let winner = null; // { frameId, key, playing, area }
  const frameEntries = frames ? Array.from(frames.entries()) : [];
  for (const [fid, entry] of frameEntries) {
    for (const c of entry.candidates) {
      const cand = { frameId: fid, key: c.key, playing: !!c.playing, area: c.area || 0 };
      if (!winner) { winner = cand; continue; }
      if (cand.playing !== winner.playing) { if (cand.playing) winner = cand; continue; }
      if (cand.area !== winner.area) { if (cand.area > winner.area) winner = cand; continue; }
      if (cand.frameId !== winner.frameId) { if (cand.frameId < winner.frameId) winner = cand; continue; }
      if (cand.key < winner.key) winner = cand;
    }
  }
  // The download source is picked across ALL frames (see
  // bestDownloadSource): only the top frame shows the button, and
  // it downloads this stream. Every frame gets the verdict; only
  // the top frame acts on `download`.
  const download = bestDownloadSource(tabId, winner);
  // Every known frame gets the verdict; when no frame has
  // reported yet, frame 0 (the top frame) still gets it — it is
  // the one that shows the button, and its content script is
  // running even before its first report.
  const targets = frames ? Array.from(frames.keys()) : [];
  if (!targets.length) targets.push(0);
  for (const fid of targets) {
    const winnerKey = winner && winner.frameId === fid ? winner.key : null;
    sendToFrame(tabId, fid, { type: "overlayVerdict", winnerKey: winnerKey, download: download });
  }
}

// ---- Tier 1: observe-only webRequest stream sniffing (round 9) ----
//
// The page-side layers (hook, timing log, embedded scan) cannot
// see streams fetched by a worker or a service worker, or by a
// player that assembles its requests off the main thread. The
// webRequest observer sees EVERY request the browser makes, in
// every world, and classifies it by URL + response Content-Type —
// the same technique cat-catch uses (GPL-3.0, techniques only,
// reimplemented here). OBSERVE-ONLY: the listener never blocks,
// modifies, or redirects anything, never reads request/response
// bodies, never touches cookies or auth headers, and nothing
// leaves the machine — findings feed only the local election,
// consistent with "Download anything. Share nothing."
//
// The observer only sees origins the extension has host
// permission for. The <all_urls> host permission is OPTIONAL
// (optional_host_permissions in the manifest) and is requested
// at runtime from the popup, with an explanation — without it,
// the listeners simply never fire for CDN origins, and detection
// falls back to the Tier 0 layers. No error, no nagging.
//
// Findings live in memory only (tabObserved): a service-worker
// restart loses them until the next matching request, and the
// Tier 0 layers cover the same streams in most cases. No
// keep-alive hack: waking the worker on a timer drains battery
// for an insurance layer.
const tabObserved = new Map(); // tabId -> Map(url -> { url, kind, initiator, at })
const OBSERVED_PER_TAB_MAX = 100;
const OBSERVED_TTL_MS = 30 * 60 * 1000; // 30 min: a film's playlist stays valid for the watch session
// Classify one observed response: { kind: "playlist"|"file", url }
// or null. Playlist by URL or Content-Type (extensionless
// endpoints like /api/gs-hls); file by URL, by a media
// resourceType, or by a video/audio Content-Type. video/mp2t is
// a TS segment, never a downloadable file.
function classifyObservedRequest(url, contentType, resourceType) {
  if (!url || !/^https?:\/\//i.test(url)) return null;
  const ct = String(contentType || "").toLowerCase().split(";")[0].trim();
  if (BG_STREAM_EXT_RE.test(url) || BG_PLAYLIST_MIME_RE.test(ct)) {
    return { kind: "playlist", url: url };
  }
  if (ct === "video/mp2t") return null; // segment, not a file
  if (BG_MEDIA_FILE_EXT_RE.test(url) || resourceType === "media"
      || ct.indexOf("video/") === 0 || ct.indexOf("audio/") === 0) {
    return { kind: "file", url: url };
  }
  return null;
}
function storeObserved(tabId, url, kind, initiator) {
  let perTab = tabObserved.get(tabId);
  if (!perTab) { perTab = new Map(); tabObserved.set(tabId, perTab); }
  const now = Date.now();
  // Prune stale entries first (keeps the cap meaningful).
  for (const [u, rec] of Array.from(perTab.entries())) {
    if (now - rec.at > OBSERVED_TTL_MS) perTab.delete(u);
  }
  if (perTab.has(url)) {
    perTab.get(url).at = now;
    return;
  }
  // Cap: drop the oldest entry (Map iteration is insertion order).
  if (perTab.size >= OBSERVED_PER_TAB_MAX) {
    const oldest = perTab.keys().next().value;
    if (oldest !== undefined) perTab.delete(oldest);
  }
  perTab.set(url, { url: url, kind: kind, initiator: initiator || "", at: now });
  // A new finding may change the election (a playlist observed
  // here beats any direct file): re-elect promptly so the
  // button appears without waiting for the next frame report.
  electOverlay(tabId);
}
// A request from a service worker (or the extension itself) has
// tabId -1: attribute it to a tab whose URL shares the request's
// initiator origin, else the active tab, else drop it.
function resolveObservedTab(initiator) {
  return new Promise(function (resolve) {
    if (!api.tabs || !api.tabs.query) { resolve(null); return; }
    let origin = "";
    try { origin = new URL(initiator || "").origin; } catch (_) { origin = ""; }
    const done = function (tabs) {
      if (!tabs || !tabs.length) { resolve(null); return; }
      if (origin) {
        for (const t of tabs) {
          try {
            if (t.url && new URL(t.url).origin === origin) { resolve(t.id); return; }
          } catch (_) {}
        }
      }
      for (const t of tabs) {
        if (t.active) { resolve(t.id); return; }
      }
      resolve(null);
    };
    try {
      const p = api.tabs.query({});
      if (p && typeof p.then === "function") p.then(done).catch(function () { resolve(null); });
      else api.tabs.query({}, done);
    } catch (_) { resolve(null); }
  });
}
function handleObservedResponse(details) {
  if (!details || !details.url) return;
  let contentType = "";
  const headers = details.responseHeaders || [];
  for (const h of headers) {
    if (h && h.name && h.name.toLowerCase() === "content-type") { contentType = h.value || ""; break; }
  }
  const found = classifyObservedRequest(details.url, contentType, details.type);
  if (!found) return;
  const tabId = details.tabId;
  if (typeof tabId === "number" && tabId >= 0) {
    storeObserved(tabId, found.url, found.kind, details.initiator || "");
    return;
  }
  resolveObservedTab(details.initiator || "").then(function (resolved) {
    if (resolved !== null && resolved !== undefined) {
      storeObserved(resolved, found.url, found.kind, details.initiator || "");
    }
  }).catch(function () {});
}
// Observe-only: no "blocking" extraInfoSpec (removed for standard
// MV3 extensions anyway), no request headers, no bodies — the
// URL and the response Content-Type are all classification needs.
// onResponseStarted fires for redirects too, so a playlist URL
// that 302s is still classified by its own URL.
if (api.webRequest && api.webRequest.onResponseStarted) {
  try {
    api.webRequest.onResponseStarted.addListener(
      handleObservedResponse,
      { urls: ["http://*/*", "https://*/*"] },
      ["responseHeaders"]
    );
  } catch (_) { /* webRequest unavailable (old browser): Tier 0 only */ }
}

if (api.tabs && api.tabs.onRemoved) {
  api.tabs.onRemoved.addListener((tabId) => {
    tabFrames.delete(tabId);
    tabLastPosted.delete(tabId);
    tabOverlayCandidates.delete(tabId);
    tabObserved.delete(tabId);
    if (tabPostTimers.has(tabId)) { clearTimeout(tabPostTimers.get(tabId)); tabPostTimers.delete(tabId); }
    delete mediaCounts[tabId];
    saveMediaCounts();
  });
}
// Round 12, item 9 (LookMovie wrong-film send): the navigation
// boundary for the election. Observed findings live 30 minutes
// and candidate reports live 12 seconds — both outlive a
// same-tab navigation, so a playlist observed on the previous
// film's page could still win the election on the new film's
// page and send the old film's URL (the MoviesJoy link
// re-downloaded from the LookMovie page, 2026-11-25). Clearing
// both maps when the tab commits to a new URL ends that: the new
// page's frames re-report within a scan, and any observation
// stored after the clear belongs to the new page. Frames that
// report before the clear (same-document navigations do not fire
// changeInfo.url) are unaffected.
function clearTabElectionState(tabId) {
  tabObserved.delete(tabId);
  tabOverlayCandidates.delete(tabId);
}
if (api.tabs && api.tabs.onUpdated) {
  api.tabs.onUpdated.addListener((tabId, changeInfo) => {
    if (changeInfo && typeof changeInfo.url === "string") clearTabElectionState(tabId);
  });
}
api.contextMenus.onClicked.addListener((info) => {
  const url = info.linkUrl || info.srcUrl;
  if (url) sendDownload(url, undefined, info.pageUrl);
});
// Popup asks the background to send a pasted URL (keeps token handling here).
api.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === "sendUrl") {
    sendDownload(msg.url, msg.filename).then((r) => sendResponse({ ok: r.ok, error: r.error, duplicate: r.duplicate, viaBrowser: r.viaBrowser }));
    return true;
  }
  if (msg && msg.type === "downloadMedia") {
    // Content-script overlay click: same sendDownload path as the popup
    // and context menu. Only accept from a real tab's content script.
    // The referer is the frame the media came from (the button lives
    // in the top frame, but the stream may come from a subframe, so
    // the verdict carries the source frame's URL); fall back to the
    // sending frame's URL when no referer was given. The audio URL
    // is a sibling audio variant to merge. formatId is the quality
    // the user picked in the popup; manifestText is playlist text
    // the content script captured from the player (round 6).
    if (!_sender || !_sender.tab) { sendResponse({ ok: false, error: "No tab" }); return false; }
    const referer = msg.referer || (_sender && _sender.url) || (_sender.tab && _sender.tab.url) || undefined;
    sendDownload(msg.url, msg.filename, referer, msg.audioUrl, msg.formatId, msg.manifestText, msg.audioOnly, msg.audioLang, msg.subtitleLangs, msg.audioFormat).then((r) => sendResponse({ ok: r.ok, error: r.error, duplicate: r.duplicate, viaBrowser: r.viaBrowser }));
    return true;
  }
  if (msg && msg.type === "getFormats") {
    // Quality popup asking for a page's formats (round 6). Only
    // accept from a real tab's content script. Failures carry the
    // unreachable classification (round 8): the popup shows an
    // error state only when the bridge could not be reached at
    // all, and degraded rows otherwise — never a silent send.
    if (!_sender || !_sender.tab) { sendResponse({ ok: false, error: "No tab" }); return false; }
    // Round 11, item 5: the content script sends the verdict's
    // referer (the page the stream came from); the sender's tab
    // URL is the fallback when it does not. The bridge forwards
    // it as the Referer header on the manifest fetch.
    const fmtReferer = msg.referer || (_sender && _sender.url) || (_sender.tab && _sender.tab.url) || undefined;
    getFormats(msg.url, fmtReferer).then((j) => sendResponse(Object.assign({ ok: true }, j)))
      .catch((e) => sendResponse({ ok: false, error: (e && e.message) || "formats failed", unreachable: !!(e && e.unreachable), reason: (e && e.reason) || "" }));
    return true;
  }
  if (msg && msg.type === "overlayCandidates") {
    // Content-script overlay election report (fire-and-forget).
    if (_sender && _sender.tab && typeof _sender.tab.id === "number") {
      const tabId = _sender.tab.id;
      const frameId = _sender.frameId || 0;
      const frameUrl = _sender.url || "";
      const candidates = Array.isArray(msg.candidates) ? msg.candidates : [];
      // A viaPage source has no URL of its own: the frame's page
      // URL is the extraction target (and the Referer for the
      // stream's requests).
      for (const c of candidates) {
        if (c.source && c.source.viaPage && !c.source.url) c.source.url = frameUrl;
      }
      if (!tabOverlayCandidates.has(tabId)) tabOverlayCandidates.set(tabId, new Map());
      tabOverlayCandidates.get(tabId).set(frameId, {
        candidates: candidates,
        frameUrl: frameUrl,
        at: Date.now(),
      });
      electOverlay(tabId);
    }
    return false;
  }
  if (msg && msg.type === "reportMedia") {
    // Content-script detected-media report (fire-and-forget, no response).
    if (_sender && _sender.tab && typeof _sender.tab.id === "number") {
      handleReport(_sender.tab.id, _sender.frameId || 0, msg.items, _sender.tab.url || "");
    }
    return false;
  }
  if (msg && msg.type === "pairCode") {
    // Pairing: send only the one-time code (never a token) to the bridge.
    // On success the bridge returns the token, which is stored here in
    // background storage only. Then challenge the server with that token
    // before trusting it; if the challenge fails, discard the token.
    // Round 10, item 15: the verify request also carries this
    // browser's stable instance id + name, so the app can count
    // paired browsers.
    getBrowserId().then((browser_id) => fetch(BRIDGE + "/pair/verify", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: msg.code, browser_id: browser_id, browser_name: browserName() }),
    }).then(async (r) => {
      if (!r.ok) { const j = await r.json().catch(() => ({})); sendResponse({ ok: false, error: j.error || "pairing failed" }); return; }
      const j = await r.json();
      if (!j.token) { sendResponse({ ok: false, error: "pairing failed" }); return; }
      const ok = await verifyServer(j.token).catch(() => false);
      if (!ok) { sendResponse({ ok: false, error: "server challenge failed" }); return; }
      await callApi((...a) => api.storage.local.set(...a), { token: j.token });
      sendResponse({ ok: true });
    }).catch(() => sendResponse({ ok: false, error: "bridge not reachable" })));
    return true;
  }
  if (msg && msg.type === "checkBridge") {
    fetch(BRIDGE + "/health").then((r) => sendResponse({ bridge: r.ok })).catch(() => sendResponse({ bridge: false }));
    return true;
  }
});
