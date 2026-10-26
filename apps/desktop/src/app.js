// FetchForge desktop UI. Two modes, one UI:
//  - Tauri mode (window.__TAURI__): calls Rust commands directly.
//  - Bridge mode (dev / web preview): talks to http://127.0.0.1:18443
//    with the per-install token stored in localStorage on this machine.
const BRIDGE = "http://127.0.0.1:18443";
const isTauri = !!(window.__TAURI__ && window.__TAURI__.core);
const tokenBox = document.getElementById("tokenBox");
if (!isTauri) tokenBox.hidden = false;

function token() { return localStorage.getItem("fetchforge-token") || ""; }
document.getElementById("saveTokenBtn").onclick = () => {
  localStorage.setItem("fetchforge-token", document.getElementById("token").value.trim());
  setStatus("Token saved locally.");
};
function setStatus(t) { document.getElementById("status").textContent = t; }
function fmtBytes(n) {
  if (n == null) return "-";
  const u = ["B", "KB", "MB", "GB"]; let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
}
async function api(path, method = "GET", body) {
  if (isTauri) {
    const invoke = window.__TAURI__.core.invoke;
    if (path === "/downloads" && method === "GET") return invoke("list_downloads");
    if (path === "/downloads" && method === "POST") return invoke("add_download", body);
    const m = path.match(/^\/downloads\/(.+)\/(pause|resume|cancel)$/);
    if (m) return invoke(m[2] + "_download", { id: m[1] });
    throw new Error("Unsupported Tauri call " + path);
  }
  const r = await fetch(BRIDGE + path, {
    method,
    headers: { "Content-Type": "application/json", "X-FetchForge-Token": token() },
    body: body ? JSON.stringify(body) : undefined,
  });
  if (!r.ok) throw new Error(await r.text());
  return r.json();
}
async function refresh() {
  try {
    const all = await api("/downloads");
    const q = document.getElementById("search").value.toLowerCase();
    const f = document.getElementById("filter").value;
    const rows = all.filter(d =>
      (f === "all" || d.status === f) &&
      (d.filename.toLowerCase().includes(q) || d.url.toLowerCase().includes(q)));
    const tbody = document.getElementById("list");
    tbody.textContent = "";
    document.getElementById("empty").hidden = rows.length > 0;
    for (const d of rows) {
      const tr = document.createElement("tr");
      const pct = d.total_bytes ? Math.round(d.downloaded_bytes / d.total_bytes * 100) : 0;
      // textContent only: filenames and server error strings are untrusted
      // and must never be parsed as HTML (a file named <img src=x onerror=…>
      // used to be injected via innerHTML before this fix).
      const tdName = document.createElement("td"); tdName.textContent = d.filename;
      const tdCat = document.createElement("td"); tdCat.textContent = d.category;
      const tdStatus = document.createElement("td");
      tdStatus.textContent = d.status + (d.error ? " - " + d.error : "");
      const tdProg = document.createElement("td");
      const prog = document.createElement("progress");
      prog.value = d.downloaded_bytes; prog.max = d.total_bytes || 1;
      tdProg.appendChild(prog);
      tdProg.appendChild(document.createTextNode(
        ` ${pct}% (${fmtBytes(d.downloaded_bytes)} / ${d.total_bytes ? fmtBytes(d.total_bytes) : "?"})`));
      const tdSpeed = document.createElement("td"); tdSpeed.textContent = "-";
      const td = document.createElement("td");
      tr.append(tdName, tdCat, tdStatus, tdProg, tdSpeed, td);
      for (const [label, action] of [["Pause", "pause"], ["Resume", "resume"], ["Cancel", "cancel"]]) {
        const b = document.createElement("button"); b.className = "secondary"; b.textContent = label;
        b.onclick = async () => { await api(`/downloads/${d.id}/${action}`, "POST"); refresh(); };
        td.appendChild(b); td.appendChild(document.createTextNode(" "));
      }
      tbody.appendChild(tr);
    }
    setStatus(rows.length + " shown");
  } catch (e) { setStatus("Bridge not reachable. Start FetchForge, or paste its token above."); }
}
document.getElementById("refreshBtn").onclick = refresh;
document.getElementById("search").oninput = refresh;
document.getElementById("filter").onchange = refresh;
document.getElementById("addBtn").onclick = async () => {
  const url = document.getElementById("url").value.trim();
  const filename = document.getElementById("filename").value.trim() || undefined;
  if (!url) return;
  try { await api("/downloads", "POST", { url, filename }); setStatus("Added."); refresh(); }
  catch (e) { setStatus("Could not add download: " + e.message); }
};
refresh(); setInterval(refresh, 2000);
// Feat 1: Pair a browser. Code is shown as text only, never logged,
// with a 60-second countdown. Tauri mode calls the Rust command
// (which talks to the local bridge); bridge mode calls the bridge
// API directly with the stored token. NOT VERIFIED in a real browser.
let pairTimer = null;
document.getElementById("pairBtn").onclick = async () => {
  try {
    let code;
    if (isTauri) {
      code = await window.__TAURI__.core.invoke("pair_browser");
    } else {
      const r = await fetch(BRIDGE + "/pair/generate", { method: "POST", headers: { "X-FetchForge-Token": token() } });
      if (!r.ok) throw new Error(await r.text());
      code = (await r.json()).code;
    }
    document.getElementById("pairCode").textContent = code;
    let left = 60;
    document.getElementById("pairCountdown").textContent = left + "s left";
    if (pairTimer) clearInterval(pairTimer);
    pairTimer = setInterval(() => {
      left -= 1;
      if (left <= 0) {
        clearInterval(pairTimer);
        document.getElementById("pairCode").textContent = "";
        document.getElementById("pairCountdown").textContent = "Code expired. Click Pair a browser again.";
      } else {
        document.getElementById("pairCountdown").textContent = left + "s left";
      }
    }, 1000);
    setStatus("Pairing code ready. Type it into the extension popup.");
  } catch (e) { setStatus("Could not generate a pairing code: " + e.message); }
};
document.getElementById("unpairBtn").onclick = async () => {
  try {
    if (isTauri) {
      await window.__TAURI__.core.invoke("unpair_all_browsers");
    } else {
      const r = await fetch(BRIDGE + "/unpair-all", { method: "POST", headers: { "X-FetchForge-Token": token() } });
      if (!r.ok) throw new Error(await r.text());
    }
    document.getElementById("pairCode").textContent = "";
    setStatus("All browsers unpaired. They must pair again.");
  } catch (e) { setStatus("Could not unpair browsers: " + e.message); }
};
// F08: in Tauri mode, show a clear error if the bridge did not start
// (for example, port 18443 already taken). No silent port fallback.
if (isTauri) {
  window.__TAURI__.core.invoke("bridge_status").then((s) => {
    if (s !== "running") setStatus("Bridge: " + s);
  }).catch(() => {});
}
