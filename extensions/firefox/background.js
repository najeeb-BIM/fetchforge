// FetchForge extension background script (shared source; see docs/EXTENSION_SETUP.md).
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
async function sendViaBridge(url, filename) {
  const token = await getToken();
  if (!token) throw new Error("no-token");
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
  // Documented fallback: let the browser handle it normally.
  try { await callApi((...a) => api.downloads.download(...a), { url, saveAs: false }); return true; }
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
  if (msg && msg.type === "checkBridge") {
    fetch(BRIDGE + "/health").then((r) => sendResponse({ bridge: r.ok })).catch(() => sendResponse({ bridge: false }));
    return true;
  }
});
