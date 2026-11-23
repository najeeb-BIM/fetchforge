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
async function sendViaBridge(url, filename, referer, audioUrl, formatId, manifestText, audioOnly) {
  const token = await getToken();
  if (!token) throw new Error("no-token");
  if (!(await verifyServer(token))) throw new Error("challenge-failed");
  const r = await fetch(BRIDGE + "/downloads", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
    body: JSON.stringify({ url, filename, referer, audio_url: audioUrl, format_id: formatId, manifest_text: manifestText, audio_only: audioOnly }),
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
async function sendViaNative(url, filename, referer, audioUrl, formatId, manifestText, audioOnly) {
  const resp = await callApi(
    (...a) => api.runtime.sendNativeMessage(...a),
    HOST,
    { type: "addDownload", url, filename, referer, audio_url: audioUrl, format_id: formatId, manifest_text: manifestText, audio_only: audioOnly }
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
// delivers an audio file instead of the video.
async function sendDownload(url, filename, referer, audioUrl, formatId, manifestText, audioOnly) {
  if (!/^https?:\/\//i.test(url)) return { ok: false, error: "Not a web link" };
  if (typeof manifestText === "string" && manifestText.length > 262144) manifestText = undefined;
  try {
    const r = await sendViaNative(url, filename, referer, audioUrl, formatId, manifestText, audioOnly);
    // A real failure reason from the app (not a transport miss):
    // the native host already asked the bridge, so retrying the
    // bridge directly would only fail the same way again.
    // Otherwise return the result as-is: it may carry
    // `duplicate` (nothing new was queued), which the caller
    // reports honestly instead of claiming a fresh send.
    if (r && r.ok === false) return r;
    return r;
  } catch (_) { /* host not installed */ }
  try { return await sendViaBridge(url, filename, referer, audioUrl, formatId, manifestText, audioOnly); } catch (_) { /* bridge not running / no token */ }
  // Documented fallback: let the browser handle it normally, and
  // record it so the popup can say so (not a silent fallback).
  try { await callApi((...a) => api.downloads.download(...a), { url, saveAs: false }); try { await callApi((...a) => api.storage.local.set(...a), { lastFallback: Date.now() }); } catch (_) {} return { ok: true, viaBrowser: true }; }
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
async function getFormats(url) {
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
    r = await fetch(BRIDGE + "/media/formats", {
      method: "POST",
      headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
      body: JSON.stringify({ url }),
    });
  } catch (_) {
    const e = new Error("bridge unreachable"); e.unreachable = true; throw e;
  }
  if (!r.ok) throw new Error("bridge-" + r.status);
  return await r.json();
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
const BG_STREAM_EXT_RE = /\.(m3u8|mpd)([?#]|$)/i;
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
  if (!frames) return null;
  const all = [];
  for (const [fid, entry] of frames.entries()) {
    for (const c of entry.candidates) {
      if (!c.source || !c.source.url) continue;
      all.push({ frameId: fid, frameUrl: entry.frameUrl, cand: c });
    }
  }
  if (!all.length) return null;
  const isWinner = function (e) {
    return !!winner && e.frameId === winner.frameId && e.cand.key === winner.key;
  };
  const isPlaylist = function (e) { return BG_STREAM_EXT_RE.test(e.cand.source.url); };
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
  return { url: s.url, filename: s.filename, audioUrl: s.audioUrl, referer: best.frameUrl, viaPage: !!s.viaPage, manifestText: s.manifestText, masterText: s.masterText, pageVideoDuration: pageVideoDuration };
}
function electOverlay(tabId) {
  const frames = tabOverlayCandidates.get(tabId);
  if (!frames) return;
  const now = Date.now();
  for (const [fid, entry] of Array.from(frames.entries())) {
    if (now - entry.at > OVERLAY_CANDIDATE_TTL_MS) frames.delete(fid);
  }
  let winner = null; // { frameId, key, playing, area }
  for (const [fid, entry] of frames.entries()) {
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
  for (const fid of frames.keys()) {
    const winnerKey = winner && winner.frameId === fid ? winner.key : null;
    sendToFrame(tabId, fid, { type: "overlayVerdict", winnerKey: winnerKey, download: download });
  }
}

if (api.tabs && api.tabs.onRemoved) {
  api.tabs.onRemoved.addListener((tabId) => {
    tabFrames.delete(tabId);
    tabLastPosted.delete(tabId);
    tabOverlayCandidates.delete(tabId);
    if (tabPostTimers.has(tabId)) { clearTimeout(tabPostTimers.get(tabId)); tabPostTimers.delete(tabId); }
    delete mediaCounts[tabId];
    saveMediaCounts();
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
    sendDownload(msg.url, msg.filename, referer, msg.audioUrl, msg.formatId, msg.manifestText, msg.audioOnly).then((r) => sendResponse({ ok: r.ok, error: r.error, duplicate: r.duplicate, viaBrowser: r.viaBrowser }));
    return true;
  }
  if (msg && msg.type === "getFormats") {
    // Quality popup asking for a page's formats (round 6). Only
    // accept from a real tab's content script. Failures carry the
    // unreachable classification (round 8): the popup shows an
    // error state only when the bridge could not be reached at
    // all, and degraded rows otherwise — never a silent send.
    if (!_sender || !_sender.tab) { sendResponse({ ok: false, error: "No tab" }); return false; }
    getFormats(msg.url).then((j) => sendResponse(Object.assign({ ok: true }, j)))
      .catch((e) => sendResponse({ ok: false, error: (e && e.message) || "formats failed", unreachable: !!(e && e.unreachable) }));
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
    fetch(BRIDGE + "/pair/verify", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ code: msg.code }),
    }).then(async (r) => {
      if (!r.ok) { const j = await r.json().catch(() => ({})); sendResponse({ ok: false, error: j.error || "pairing failed" }); return; }
      const j = await r.json();
      if (!j.token) { sendResponse({ ok: false, error: "pairing failed" }); return; }
      const ok = await verifyServer(j.token).catch(() => false);
      if (!ok) { sendResponse({ ok: false, error: "server challenge failed" }); return; }
      await callApi((...a) => api.storage.local.set(...a), { token: j.token });
      sendResponse({ ok: true });
    }).catch(() => sendResponse({ ok: false, error: "bridge not reachable" }));
    return true;
  }
  if (msg && msg.type === "checkBridge") {
    fetch(BRIDGE + "/health").then((r) => sendResponse({ bridge: r.ok })).catch(() => sendResponse({ bridge: false }));
    return true;
  }
});
