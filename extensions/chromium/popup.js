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
    const batch = document.getElementById("mediaBatch");
    if (batch) batch.style.display = items.length ? "block" : "none";
    if (!items.length) { mediaStatus.textContent = "No media found on this page."; return; }
    mediaStatus.textContent = items.length + (items.length === 1 ? " media item found:" : " media items found:");
    for (const item of items) {
      const row = document.createElement("div");
      row.className = "media-row";
      const check = document.createElement("input");
      check.type = "checkbox";
      check.className = "media-check";
      check.dataset.url = item.url;
      if (item.filename) check.dataset.filename = item.filename;
      const label = document.createElement("span");
      label.className = "media-label";
      label.textContent = (item.kind === "audio" ? "Audio: " : "Video: ") + item.title + (item.viaPage ? " (via page link)" : "");
      const btn = document.createElement("button");
      btn.className = "media-btn";
      btn.textContent = "Download";
      btn.onclick = async () => {
        btn.disabled = true;
        const res = await sendMessage({ type: "sendUrl", url: item.url, filename: item.filename });
        status.textContent = res && res.ok ? "Sent (or fell back to browser download)." : "Could not send: " + (res && res.error ? res.error : "unknown error");
        btn.disabled = false;
      };
      row.appendChild(check);
      row.appendChild(label);
      row.appendChild(btn);
      listEl.appendChild(row);
    }
  } catch (_) {
    mediaStatus.textContent = "Could not reach this page. Reload the page and try again.";
  }
}
document.getElementById("mediaSelectAll").onclick = () => {
  const boxes = document.querySelectorAll(".media-check");
  const allChecked = boxes.length > 0 && [...boxes].every((b) => b.checked);
  boxes.forEach((b) => { b.checked = !allChecked; });
  document.getElementById("mediaSelectAll").checked = !allChecked;
};
document.getElementById("mediaDownloadSelected").onclick = async () => {
  const boxes = [...document.querySelectorAll(".media-check:checked")];
  if (!boxes.length) { status.textContent = "Select at least one media item first."; return; }
  const btn = document.getElementById("mediaDownloadSelected");
  btn.disabled = true;
  let sent = 0;
  for (const box of boxes) {
    // Same sendUrl path as a per-item Download; a viaPage item sends
    // the page URL and the app extracts it with its default quality.
    const res = await sendMessage({ type: "sendUrl", url: box.dataset.url, filename: box.dataset.filename || undefined });
    if (res && res.ok) sent += 1;
  }
  btn.disabled = false;
  status.textContent = sent === boxes.length
    ? "Sent " + sent + " item" + (sent === 1 ? "" : "s") + "."
    : "Sent " + sent + " of " + boxes.length + " selected.";
};
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
  status.textContent = r && r.ok ? "Sent (or fell back to browser download)." : "Could not send: " + (r && r.error ? r.error : "is the link http/https?");
};
// Ad blocker toggle: the static ruleset (rules/ads.json) is enabled
// by default in the manifest; this checkbox persists the choice in
// storage.local and enables/disables the ruleset. Default ON.
const AD_BLOCK_RULESET_ID = "ff-ads";
function applyAdBlock(enabled) {
  if (!api.declarativeNetRequest || !api.declarativeNetRequest.updateEnabledRulesets) return;
  try {
    const p = api.declarativeNetRequest.updateEnabledRulesets(
      enabled ? { enableRulesetIds: [AD_BLOCK_RULESET_ID] } : { disableRulesetIds: [AD_BLOCK_RULESET_ID] }
    );
    if (p && p.catch) p.catch(function () {});
  } catch (_) { /* DNR unavailable; the manifest default stands */ }
}
async function refreshAdBlock() {
  const box = document.getElementById("adBlock");
  if (!box) return;
  let enabled = true;
  try {
    const s = await getStorage("adBlockEnabled");
    if (typeof s.adBlockEnabled === "boolean") enabled = s.adBlockEnabled;
  } catch (_) {}
  box.checked = enabled;
  applyAdBlock(enabled);
}
document.getElementById("adBlock").onchange = async (e) => {
  const enabled = e.target.checked;
  await setStorage({ adBlockEnabled: enabled });
  applyAdBlock(enabled);
};
refresh();
refreshMedia();
refreshAdBlock();
