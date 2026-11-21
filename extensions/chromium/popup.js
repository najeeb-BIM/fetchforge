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
function queryActiveTab() {
  if (isFirefox) return api.tabs.query({ active: true, currentWindow: true });
  return new Promise((resolve) => api.tabs.query({ active: true, currentWindow: true }, resolve));
}
// Returns null when the tab has no content script listening (restricted
// page, or the page was loaded before the extension was installed).
function sendTabMessage(tabId, msg) {
  if (isFirefox) return api.tabs.sendMessage(tabId, msg);
  return new Promise((resolve) => api.tabs.sendMessage(tabId, msg, (r) => {
    if (api.runtime.lastError) resolve(null);
    else resolve(r);
  }));
}
async function refreshMedia() {
  const listEl = document.getElementById("mediaList");
  const mediaStatus = document.getElementById("mediaStatus");
  if (!listEl || !mediaStatus) return;
  listEl.textContent = "";
  try {
    const tabs = await queryActiveTab();
    const tab = tabs && tabs[0];
    if (!tab || !tab.id) { mediaStatus.textContent = "Could not reach this page."; return; }
    if (tab.url && !/^https?:\/\//i.test(tab.url)) {
      mediaStatus.textContent = "This page does not allow media detection (browser or extension page).";
      return;
    }
    const r = await sendTabMessage(tab.id, { type: "getMedia" });
    if (r === null || r === undefined) {
      mediaStatus.textContent = "Could not reach this page. Reload the page and try again.";
      return;
    }
    const items = r.items || [];
    if (!items.length) { mediaStatus.textContent = "No media found on this page."; return; }
    mediaStatus.textContent = items.length + (items.length === 1 ? " media item found:" : " media items found:");
    for (const item of items) {
      const row = document.createElement("div");
      row.className = "media-row";
      const label = document.createElement("span");
      label.className = "media-label";
      label.textContent = (item.kind === "audio" ? "Audio: " : "Video: ") + item.title + (item.viaPage ? " (via page link)" : "");
      const btn = document.createElement("button");
      btn.className = "media-btn";
      btn.textContent = "Download";
      btn.onclick = async () => {
        btn.disabled = true;
        const res = await sendMessage({ type: "sendUrl", url: item.url, filename: item.filename });
        status.textContent = res && res.ok ? "Sent (or fell back to browser download)." : "Could not send.";
        btn.disabled = false;
      };
      row.appendChild(label);
      row.appendChild(btn);
      listEl.appendChild(row);
    }
  } catch (_) {
    mediaStatus.textContent = "Could not reach this page. Reload the page and try again.";
  }
}
async function refresh() {
  const s = await getStorage("token");
  const token = s.token;
  const fb = (await getStorage("lastFallback")).lastFallback;
  const note = document.getElementById("fallbackNote");
  if (note) note.textContent = fb ? "Last link fell back to browser download because FetchForge was not reachable." : "";
  status.textContent = token ? "Paired. Checking app…" : "Not paired yet. Use a pairing code from FetchForge, or Advanced for manual token. Native messaging is tried first.";
  try {
    const r = await sendMessage({ type: "checkBridge" });
    if (r && r.bridge) status.textContent = "FetchForge app is reachable on this machine.";
    else if (!token) status.textContent = "App not reachable. Install the desktop app, then save its bridge token above.";
  } catch (_) {
    if (!token) status.textContent = "App not reachable. Install the desktop app, then save its bridge token above.";
  }
}
document.getElementById("advancedToggle").onclick = () => {
  const d = document.getElementById("advanced");
  d.style.display = d.style.display === "none" ? "block" : "none";
};
document.getElementById("pair").onclick = async () => {
  const input = document.getElementById("pairCode");
  const r = await sendMessage({ type: "pairCode", code: input.value.trim() });
  input.value = "";
  status.textContent = r && r.ok ? "Paired with FetchForge." : "Pairing failed: " + (r && r.error ? r.error : "bridge not reachable");
  refresh();
};
document.getElementById("saveToken").onclick = async () => {
  const input = document.getElementById("token");
  await setStorage({ token: input.value.trim() });
  input.value = "";
  refresh();
};
document.getElementById("send").onclick = async () => {
  const url = document.getElementById("url").value.trim();
  const filename = document.getElementById("filename").value.trim() || undefined;
  const r = await sendMessage({ type: "sendUrl", url, filename });
  status.textContent = r && r.ok ? "Sent (or fell back to browser download)." : "Could not send. Is the link http/https?";
};
refresh();
refreshMedia();
