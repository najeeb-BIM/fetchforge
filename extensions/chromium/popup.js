// Popup: textContent only, no innerHTML, no eval. Storage and messaging
// are called in Promise form under Firefox and callback form under Chrome
// (see background.js) so the popup never hangs waiting for a callback
// that a Promise-style API never calls.
const api = typeof browser !== "undefined" ? browser : chrome;
const isFirefox = typeof browser !== "undefined";
const status = document.getElementById("status");
function getStorage(key) {
  if (isFirefox) return api.storage.local.get(key);
  return new Promise((resolve) => api.storage.local.get(key, resolve));
}
function setStorage(obj) {
  if (isFirefox) return api.storage.local.set(obj);
  return new Promise((resolve) => api.storage.local.set(obj, resolve));
}
function sendMessage(msg) {
  if (isFirefox) return api.runtime.sendMessage(msg);
  return new Promise((resolve) => api.runtime.sendMessage(msg, resolve));
}
async function refresh() {
  const token = (await getStorage("token")).token;
  status.textContent = token ? "Token saved. Checking app…" : "No bridge token saved yet. Native messaging is tried first.";
  try {
    const r = await sendMessage({ type: "checkBridge" });
    if (r && r.bridge) status.textContent = "FetchForge app is reachable on this machine.";
    else if (!token) status.textContent = "App not reachable. Install the desktop app, then save its bridge token above.";
  } catch (_) {
    if (!token) status.textContent = "App not reachable. Install the desktop app, then save its bridge token above.";
  }
}
document.getElementById("saveToken").onclick = async () => {
  await setStorage({ token: document.getElementById("token").value.trim() });
  refresh();
};
document.getElementById("send").onclick = async () => {
  const url = document.getElementById("url").value.trim();
  const filename = document.getElementById("filename").value.trim() || undefined;
  const r = await sendMessage({ type: "sendUrl", url, filename });
  status.textContent = r && r.ok ? "Sent (or fell back to browser download)." : "Could not send. Is the link http/https?";
};
getStorage("token").then((s) => { if (s.token) document.getElementById("token").value = s.token; });
refresh();
