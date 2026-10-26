// FetchForge extension background script (shared source; see docs/EXTENSION_SETUP.md).
// NOT VERIFIED in a real browser: pairing and challenge code below was
// written in this round and has not run in Chrome/Edge/Firefox yet.
// Token hygiene: the token lives ONLY in storage.local, read only here
// in the background script. It is never put in a content script, page
// context, URL, or log. No content script exists in this extension.
// Minimal permissions, no browsing history is read or sent anywhere.
// Flow: context-menu click -> try native messaging -> try loopback bridge
// with the stored token -> fall back to the browser's own downloader.
// Service-worker safe: no persistent in-memory state, no DOM access, all
// state lives in storage. Works after a service-worker restart because
// every listener is registered at the top level on each start.
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
async function sendViaBridge(url, filename) {
  const token = await getToken();
  if (!token) throw new Error("no-token");
  if (!(await verifyServer(token))) throw new Error("challenge-failed");
  const r = await fetch(BRIDGE + "/downloads", {
    method: "POST",
    headers: { "Content-Type": "application/json", "X-FetchForge-Token": token },
    body: JSON.stringify({ url, filename }),
  });
  if (!r.ok) throw new Error("bridge-" + r.status);
  return true;
}
async function sendViaNative(url, filename) {
  const resp = await callApi(
    (...a) => api.runtime.sendNativeMessage(...a),
    HOST,
    { type: "addDownload", url, filename }
  );
  if (resp && resp.type === "error") throw new Error(resp.error);
  return true;
}
async function sendDownload(url, filename) {
  if (!/^https?:\/\//i.test(url)) return false;
  try { return await sendViaNative(url, filename); } catch (_) { /* host not installed */ }
  try { return await sendViaBridge(url, filename); } catch (_) { /* bridge not running / no token */ }
  // Documented fallback: let the browser handle it normally, and
  // record it so the popup can say so (not a silent fallback).
  try { await callApi((...a) => api.downloads.download(...a), { url, saveAs: false }); try { await callApi((...a) => api.storage.local.set(...a), { lastFallback: Date.now() }); } catch (_) {} return true; }
  catch (_) { return false; }
}
api.runtime.onInstalled.addListener(() => {
  try {
    api.contextMenus.create({ id: "fetchforge-download", title: "Download with FetchForge", contexts: ["link", "video", "audio"] });
  } catch (_) { /* already created on a service-worker restart */ }
});
api.contextMenus.onClicked.addListener((info) => {
  const url = info.linkUrl || info.srcUrl;
  if (url) sendDownload(url);
});
// Popup asks the background to send a pasted URL (keeps token handling here).
api.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  if (msg && msg.type === "sendUrl") {
    sendDownload(msg.url, msg.filename).then((ok) => sendResponse({ ok }));
    return true;
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
