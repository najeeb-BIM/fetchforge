// FetchForge desktop UI. Two modes, one UI:
//  - Tauri mode (window.__TAURI__): calls Rust commands directly.
//  - Bridge mode (dev / web preview): talks to http://127.0.0.1:18443
//    with the per-install token stored in localStorage on this machine.
const BRIDGE = "http://127.0.0.1:18443";
const isTauri = !!(window.__TAURI__ && window.__TAURI__.core);
const tokenBox = document.getElementById("tokenBox");
if (!isTauri) tokenBox.hidden = false;

// ---- Status: sticky errors survive the 2s refresh ----
// refresh() updates the "N shown" line only when no error is sticky.
// A sticky error stays visible until the condition clears (refresh
// errors) or the next user action succeeds (action errors), so a
// failed Pair/Download is never wiped before the user reads it.
let stickyError = null; // { text, source: "refresh" | "action" } | null
function setStatus(t) { document.getElementById("status").textContent = t; }
function setError(t, source) { stickyError = { text: t, source }; setStatus(t); }
function clearError() { stickyError = null; }

// ---- Startup diagnostic ----
// This line proves the frontend script actually ran. If the status
// line stays empty, app.js never executed. If it sticks on "Loading…",
// the script ran but the first Rust call never answered.
setStatus("Loading…");

// Any uncaught script error is shown in the status line, never silent.
window.onerror = (msg) => { setError("UI error: " + msg, "action"); };

function token() { return localStorage.getItem("fetchforge-token") || ""; }
document.getElementById("saveTokenBtn").onclick = () => {
  localStorage.setItem("fetchforge-token", document.getElementById("token").value.trim());
  clearError();
  setStatus("Token saved locally.");
};
function fmtBytes(n) {
  if (n == null) return "-";
  const u = ["B", "KB", "MB", "GB"]; let i = 0; let v = n;
  while (v >= 1024 && i < u.length - 1) { v /= 1024; i++; }
  return v.toFixed(v >= 100 || i === 0 ? 0 : 1) + " " + u[i];
}

// ---- Busy state: every button shows it is working ----
// While an invoke is in flight the button is disabled and labelled,
// so a click is always visibly answered (with the :active/:disabled
// styles in styles.css).
function setBusy(btn, busy, busyText) {
  btn.disabled = busy;
  if (busy) {
    if (!btn.dataset.label) btn.dataset.label = btn.textContent;
    btn.textContent = busyText;
  } else if (btn.dataset.label) {
    btn.textContent = btn.dataset.label;
    delete btn.dataset.label;
  }
}

// Tauri invoke with a timeout: a call that never answers must surface
// as an error instead of hanging the UI silently forever. The timeout
// error carries .code = "TIMEOUT" so callers can word it honestly.
const INVOKE_TIMEOUT_MS = 10000;
const ADD_TIMEOUT_MS = 130000; // covers the engine's 120s probe timeout
function invokeTimed(cmd, args, timeoutMs = INVOKE_TIMEOUT_MS) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      const e = new Error("The app did not respond in time. Try again, or restart FetchForge.");
      e.code = "TIMEOUT";
      reject(e);
    }, timeoutMs);
    window.__TAURI__.core.invoke(cmd, args).then(
      (v) => { clearTimeout(timer); resolve(v); },
      (e) => { clearTimeout(timer); reject(e instanceof Error ? e : new Error(String(e))); },
    );
  });
}
async function api(path, method = "GET", body) {
  if (isTauri) {
    if (path === "/downloads" && method === "GET") return invokeTimed("list_downloads");
    if (path === "/downloads" && method === "POST") return invokeTimed("add_download", body, ADD_TIMEOUT_MS);
    const m = path.match(/^\/downloads\/(.+)\/(pause|resume|cancel)$/);
    if (m) return invokeTimed(m[2] + "_download", { id: m[1] });
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
        b.onclick = async () => {
          try { await api(`/downloads/${d.id}/${action}`, "POST"); }
          catch (e) { setError("Could not " + action + " download: " + e.message, "action"); }
          refresh();
        };
        td.appendChild(b); td.appendChild(document.createTextNode(" "));
      }
      tbody.appendChild(tr);
    }
    // A refresh error clears itself once refresh works again; an action
    // error (failed Pair/Download) stays until the next action succeeds.
    if (stickyError && stickyError.source === "refresh") clearError();
    if (!stickyError) setStatus(rows.length + " shown");
  } catch (e) {
    setError(isTauri
      ? "Could not load the download list: " + e.message
      : "Bridge not reachable. Start FetchForge, or paste its token above.", "refresh");
  }
}
document.getElementById("refreshBtn").onclick = async () => {
  const btn = document.getElementById("refreshBtn");
  setBusy(btn, true, "Refreshing…");
  try { await refresh(); } finally { setBusy(btn, false); }
};
document.getElementById("search").oninput = refresh;
document.getElementById("filter").onchange = refresh;
document.getElementById("addBtn").onclick = async () => {
  const url = document.getElementById("url").value.trim();
  const filename = document.getElementById("filename").value.trim() || undefined;
  if (!url) return;
  const btn = document.getElementById("addBtn");
  setBusy(btn, true, "Adding…");
  try {
    await api("/downloads", "POST", { url, filename });
    clearError();
    setStatus("Added.");
    refresh();
  } catch (e) {
    if (e.code === "TIMEOUT") {
      // The Rust side keeps working after a timeout: the download may
      // still appear in the list on the next refresh.
      setStatus("Still adding — if the download appears in the list, it was added.");
      setTimeout(refresh, 5000);
    } else {
      setError("Could not add download: " + e.message, "action");
    }
  } finally { setBusy(btn, false); }
};
refresh(); setInterval(refresh, 2000);
// Feat 1: Pair a browser. Code is shown as text only, never logged,
// with a 60-second countdown. Tauri mode calls the Rust command
// (which talks to the local bridge); bridge mode calls the bridge
// API directly with the stored token.
let pairTimer = null;
function showPairCode(code) {
  document.getElementById("pairCode").textContent = code;
  document.getElementById("pairCodeBox").hidden = false;
  let left = 60;
  document.getElementById("pairCountdown").textContent = "Active for " + left + " seconds";
  if (pairTimer) clearInterval(pairTimer);
  pairTimer = setInterval(() => {
    left -= 1;
    if (left <= 0) {
      clearInterval(pairTimer);
      pairTimer = null;
      hidePairCode();
      setStatus("Pairing code expired. Click Pair a browser again.");
    } else {
      document.getElementById("pairCountdown").textContent = "Active for " + left + " seconds";
    }
  }, 1000);
}
function hidePairCode() {
  if (pairTimer) { clearInterval(pairTimer); pairTimer = null; }
  document.getElementById("pairCode").textContent = "";
  document.getElementById("pairCountdown").textContent = "";
  document.getElementById("pairCodeBox").hidden = true;
}
document.getElementById("pairBtn").onclick = async () => {
  const btn = document.getElementById("pairBtn");
  setBusy(btn, true, "Working…");
  try {
    let code;
    if (isTauri) {
      code = await invokeTimed("pair_browser");
    } else {
      const r = await fetch(BRIDGE + "/pair/generate", { method: "POST", headers: { "X-FetchForge-Token": token() } });
      if (!r.ok) throw new Error(await r.text());
      code = (await r.json()).code;
    }
    showPairCode(code);
    clearError();
    setStatus("Pairing code ready. Type it into the extension popup.");
  } catch (e) { setError("Could not generate a pairing code: " + e.message, "action"); }
  finally { setBusy(btn, false); }
};
document.getElementById("unpairBtn").onclick = async () => {
  const btn = document.getElementById("unpairBtn");
  setBusy(btn, true, "Working…");
  try {
    if (isTauri) {
      await invokeTimed("unpair_all_browsers");
    } else {
      const r = await fetch(BRIDGE + "/unpair-all", { method: "POST", headers: { "X-FetchForge-Token": token() } });
      if (!r.ok) throw new Error(await r.text());
    }
    hidePairCode();
    clearError();
    setStatus("All browsers unpaired. They must pair again.");
  } catch (e) { setError("Could not unpair browsers: " + e.message, "action"); }
  finally { setBusy(btn, false); }
};
// F08: in Tauri mode, show a clear error if the bridge did not start
// (for example, port 18443 already taken). No silent port fallback.
if (isTauri) {
  invokeTimed("bridge_status").then((s) => {
    if (s !== "running") setError("Bridge: " + s, "refresh");
  }).catch((e) => { setError("Could not check the bridge: " + e.message, "refresh"); });
}
