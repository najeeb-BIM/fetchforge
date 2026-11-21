# FetchForge fixes — 2026-11-21 (for build #5)

Apply to **GitHub main** via web-UI edits (one commit per fix below, or
fewer — order does not matter for compilation, the build runs after all
are applied). Every fix below is already applied to the local tree
(branch `audit-round`, uncommitted), so `git diff` shows the same change.

What each fix addresses is in `AUDIT.md` (§2 defect ids). Nothing here
changes version numbers, signing, or unrelated behaviour.

**Verification done locally (2026-11-21):**
- `node --check` passes on the new `app.js`.
- Engine tests: **24 passed, 0 failed** (`cargo test -p fetchforge-engine --lib`),
  including the new `reload_from_disk` test.
- `cargo check -p fetchforge-bridge` (Linux host) passes.
- `cargo check --target x86_64-pc-windows-gnu` on the desktop crate passes
  (mingw cross-compiler installed on the VM for this check). This caught a
  real error in the first draft of Fix 5c/5d (async Tauri commands need
  `State<'_, …>` — E0726); the hunks below are the corrected versions.

**Not verifiable by code reading alone:** whether the dead buttons are
"app.js never runs" or "every invoke hangs" (AUDIT.md §1). Fix 1 makes
build #5 report which one it is — see the retest table at the end.

---

## Fix 1 — `apps/desktop/src/app.js` (FULL FILE REPLACEMENT)

Fixes D1 (silent failures), D2 (status wiped by refresh), m1 (busy state),
m2 (pairing-code logic). Replace the whole file with:

```js
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
```

What changed vs the old file:
- Status line writes "Loading…" the moment the script runs (startup
  diagnostic: empty status = script never ran; stuck on "Loading…" =
  invoke hangs; "0 shown" = frontend works).
- `window.onerror` shows script errors in the status line.
- All Tauri invokes go through `invokeTimed` (10 s timeout; 130 s for
  add, covering the engine's 120 s probe timeout). A timeout rejects
  with `.code = "TIMEOUT"` instead of hanging forever.
- Sticky errors: `setError(text, source)`; refresh clears only
  refresh-sourced errors, action errors stay until the next action
  succeeds. Refresh failure message is mode-aware (Tauri vs bridge).
- Busy state on Pair / Unpair / Download / Refresh (disabled + label
  change while the invoke is in flight).
- Pairing code shown via `showPairCode`/`hidePairCode` in the new
  `#pairCodeBox`; countdown reads "Active for NN seconds"; expiry hides
  the box. `bridge_status` failures are now shown, not swallowed.
- Row Pause/Resume/Cancel failures now show an error instead of failing
  silently. Everything else (rendering, filtering, bridge-mode fetch
  branch, token handling) is unchanged.

## Fix 2 — `apps/desktop/src/index.html` (FULL FILE REPLACEMENT)

Fixes m2 (pairing-code box, dev note) and m3 (token label). Replace the
whole file with:

```html
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>FetchForge</title>
<link rel="stylesheet" href="styles.css">
</head>
<body>
<header>
  <h1>FetchForge</h1>
  <p class="tag">Local download manager. Nothing leaves this machine.</p>
</header>
<main>
  <section class="add">
    <input id="url" type="url" placeholder="Paste a download link (http / https)">
    <input id="filename" type="text" placeholder="Filename (optional)">
    <button id="addBtn">Download</button>
  </section>
  <section class="controls">
    <input id="search" type="search" placeholder="Search downloads">
    <select id="filter">
      <option value="all">All</option>
      <option value="downloading">Active</option>
      <option value="queued">Queued</option>
      <option value="completed">Completed</option>
      <option value="failed">Failed</option>
      <option value="paused">Paused</option>
    </select>
    <button id="refreshBtn">Refresh</button>
    <span id="status"></span>
  </section>
  <section id="tokenBox" class="token-box" hidden>
    <label>Bridge token (for browser extension setup):
      <input id="token" type="password" placeholder="Paste the bridge token (bridge-token.txt)">
    </label>
    <button id="saveTokenBtn">Save token locally</button>
    <small>Used only to talk to the loopback bridge on this machine. In the Tauri app this box is hidden.</small>
  </section>
  <section id="pairBox" class="pair-box">
    <button id="pairBtn">Pair a browser</button>
    <button id="unpairBtn" class="secondary">Unpair all browsers</button>
    <small>Pairing code is valid for 60 seconds and can be used once. Type it into the browser extension popup, then click Pair there.</small>
    <div id="pairCodeBox" class="pair-code-box" hidden>
      <span class="pair-code-label">Current pairing code</span>
      <span id="pairCode"></span>
      <span id="pairCountdown"></span>
    </div>
  </section>
  <div class="table-wrap">
  <table>
    <thead><tr><th>File</th><th>Category</th><th>Status</th><th>Progress</th><th>Speed</th><th>Actions</th></tr></thead>
    <tbody id="list"></tbody>
  </table>
  </div>
  <p id="empty">No downloads yet. Paste a link above.</p>
</main>
<script src="app.js"></script>
</body>
</html>
```

What changed vs the old file:
- `#pairCode` / `#pairCountdown` moved into a dedicated `#pairCodeBox`
  div directly under the Pair/Unpair buttons (same element ids, so
  Fix 1's JS works unchanged).
- Removed the user-facing text "Feat 1, NOT VERIFIED in a real browser
  yet." (dev note, not UI copy).
- Token input placeholder no longer references a "Settings" page that
  does not exist; it names the actual file (`bridge-token.txt`).

## Fix 3 — `apps/desktop/src/styles.css` (FULL FILE REPLACEMENT)

Fixes m1 (click effect) and m2 (pair-code box styling). Replace the
whole file with:

```css
body{font-family:system-ui,-apple-system,Segoe UI,Roboto,sans-serif;margin:0;background:#0f172a;color:#e2e8f0}
header{padding:16px 24px;background:#1e293b}h1{margin:0;font-size:22px}.tag{margin:4px 0 0;color:#94a3b8}
main{padding:16px 24px}.add,.controls{display:flex;gap:8px;margin-bottom:12px;flex-wrap:wrap}
input,select{background:#1e293b;color:#e2e8f0;border:1px solid #334155;border-radius:6px;padding:8px}
#url{flex:2;min-width:260px}#filename{flex:1}
button{background:#22c55e;color:#0f172a;border:0;border-radius:6px;padding:8px 14px;font-weight:600;cursor:pointer}
button.secondary{background:#334155;color:#e2e8f0}
button:active{filter:brightness(1.3);transform:translateY(1px)}
button.secondary:active{filter:brightness(1.6)}
button:disabled{opacity:.55;cursor:wait}
#status{align-self:center;color:#cbd5e1}
.pair-box{display:flex;gap:8px;align-items:center;flex-wrap:wrap;margin-bottom:12px}
.pair-code-box{display:flex;gap:12px;align-items:baseline;flex-basis:100%;background:#1e293b;border:1px solid #22c55e;border-radius:8px;padding:10px 14px}
.pair-code-box[hidden]{display:none}
.pair-code-label{color:#94a3b8;font-size:13px}
#pairCode{font-size:22px;font-weight:700;letter-spacing:2px;color:#4ade80}
#pairCountdown{color:#facc15;font-size:14px}
.table-wrap{overflow-x:auto}
table{width:100%;border-collapse:collapse}th,td{padding:8px;border-bottom:1px solid #334155;text-align:left;font-size:14px}
@media (max-width:600px){.add,.controls{flex-direction:column}#url{flex:1}main{padding:12px}th,td{font-size:13px}}
progress{width:140px}.token-box{background:#1e293b;border:1px solid #334155;border-radius:8px;padding:12px;margin-bottom:12px}
```

What changed vs the old file (all existing rules kept):
- `button:active` brightens + presses down 1px; `button:disabled` dims
  and shows a wait cursor — every click is visibly answered.
- `.pair-box` flex layout; `.pair-code-box` (green-bordered box, full
  row under the buttons), `#pairCode` large green monospace-spaced,
  `#pairCountdown` amber; `[hidden]` still hides the box.
- `#status` vertically centred in the controls row.

## Fix 4 — `crates/engine/src/state.rs` (ADD METHOD + TEST)

Fixes D7 (list staleness). Add `reload_from_disk` at the end of
`impl Store`, immediately before `pub fn upsert`:

OLD:
```rust
    pub fn upsert(&mut self, rec: DownloadRecord) {
```
NEW:
```rust
    /// Re-read the state file and merge any records this process does
    /// not know about (for example, downloads added by the bridge while
    /// the app was running). Same rule as `save`: a record wins only if
    /// it is newer than the one already held. A missing or corrupt file
    /// is ignored here; `save` still refuses to overwrite corrupt state.
    pub fn reload_from_disk(&mut self) {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let Ok(disk_records): Result<BTreeMap<String, DownloadRecord>, _> =
            serde_json::from_str(&text)
        else {
            return;
        };
        for (id, disk_rec) in disk_records {
            match self.records.get(&id) {
                None => {
                    self.records.insert(id, disk_rec);
                }
                Some(ours) if disk_rec.updated_unix > ours.updated_unix => {
                    self.records.insert(id, disk_rec);
                }
                Some(_) => {}
            }
        }
    }
    pub fn upsert(&mut self, rec: DownloadRecord) {
```

Add this test in `mod tests`, immediately before
`fn corrupt_state_without_backup_starts_empty_with_warning`:

```rust
    #[test]
    fn reload_from_disk_picks_up_other_process_records() {
        let dir = std::env::temp_dir().join("fetchforge-test-store-reload");
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("downloads.json");
        let mut a = Store::load(path.clone());
        a.upsert(rec("one"));
        a.save().unwrap();
        // B loads when only "one" exists.
        let mut b = Store::load(path.clone());
        // A adds "two" and saves. B does not see it on its own...
        a.upsert(rec("two"));
        a.save().unwrap();
        assert!(b.get("two").is_none(), "B must not see two before reload");
        // ...but reload_from_disk picks it up without losing "one".
        b.reload_from_disk();
        assert!(b.get("one").is_some(), "one must survive reload");
        assert!(b.get("two").is_some(), "two must appear after reload");
    }
```

## Fix 5 — `apps/desktop/src-tauri/src/main.rs` (6 HUNKS)

### 5a. Spawn the bridge with no console window (fixes D3)

OLD:
```rust
    let log_path = data_dir.join("bridge.log");
    let log_file = std::fs::File::create(&log_path).ok();
    let mut cmd = std::process::Command::new(&bridge_path);
    cmd.arg("serve")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("18443");
    if let Some(f) = log_file {
        cmd.stderr(f);
    }
```
NEW:
```rust
    let log_path = data_dir.join("bridge.log");
    let log_file = std::fs::File::create(&log_path).ok();
    let mut cmd = std::process::Command::new(&bridge_path);
    cmd.arg("serve")
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg("18443");
    // F08b: on Windows, start the bridge with no console window. The
    // bridge used to open a blank black console next to the app, and
    // closing it killed the bridge (and Pair with it). Windows-only;
    // other platforms are unaffected.
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW (0x08000000): no console window is created.
        cmd.creation_flags(0x08000000);
    }
    // The bridge logs to bridge.log (stderr); its stdout is not used in
    // serve mode, so discard it instead of inheriting the app's.
    cmd.stdout(std::process::Stdio::null());
    if let Some(f) = log_file {
        cmd.stderr(f);
    }
```

### 5b. `list_downloads` picks up bridge-added downloads (fixes D7)

OLD:
```rust
#[tauri::command]
fn list_downloads(state: tauri::State<Arc<State>>) -> Vec<DownloadRecord> {
    state
        .store
        .lock()
        .unwrap()
        .all()
        .into_iter()
        .cloned()
        .collect()
}
```
NEW:
```rust
#[tauri::command]
fn list_downloads(state: tauri::State<Arc<State>>) -> Vec<DownloadRecord> {
    let mut store = state.store.lock().unwrap();
    // Pick up downloads added by the bridge (browser extension) since
    // launch; without this the list never shows them until restart.
    store.reload_from_disk();
    store.all().into_iter().cloned().collect()
}
```

### 5c. `add_download` becomes async (fixes D5)

OLD:
```rust
#[tauri::command]
fn add_download(
    state: tauri::State<Arc<State>>,
    url: String,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
```
NEW:
```rust
// Async so the blocking probe runs on Tauri's async runtime instead of
// the IPC thread: a slow probe used to freeze the whole UI.
#[tauri::command]
async fn add_download(
    state: tauri::State<'_, Arc<State>>,
    url: String,
    filename: Option<String>,
) -> Result<DownloadRecord, String> {
```
Note the `'_` lifetime on `State`: async Tauri commands require it
(E0726 otherwise — caught by the Windows-target check).

### 5d. `pair_browser` / `unpair_all_browsers` become async (fixes D5)

OLD:
```rust
#[tauri::command]
fn pair_browser(state: tauri::State<Arc<State>>) -> Result<String, String> {
```
NEW:
```rust
// Async so the blocking loopback call runs off the IPC thread (see
// add_download).
#[tauri::command]
async fn pair_browser(state: tauri::State<'_, Arc<State>>) -> Result<String, String> {
```

OLD:
```rust
#[tauri::command]
fn unpair_all_browsers(state: tauri::State<Arc<State>>) -> Result<(), String> {
```
NEW:
```rust
#[tauri::command]
async fn unpair_all_browsers(state: tauri::State<'_, Arc<State>>) -> Result<(), String> {
```

### 5e. `bridge_post` never blocks forever (fixes D5)

OLD:
```rust
fn bridge_post(_data_dir: &PathBuf, token: &str, path: &str) -> Result<String, String> {
    use std::net::TcpStream;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
```
NEW:
```rust
fn bridge_post(_data_dir: &PathBuf, token: &str, path: &str) -> Result<String, String> {
    use std::net::TcpStream;
    use std::time::Duration;
    let mut stream = TcpStream::connect("127.0.0.1:18443")
        .map_err(|e| format!("Bridge not reachable ({e}). Is FetchForge running?"))?;
    // Never block forever: if the bridge accepts but never answers,
    // fail visibly instead of hanging the UI.
    let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
```

### 5f. Refuse web-page links honestly (fixes D6) — RECOMMENDED, skippable

This changes behaviour: a YouTube watch page (or any HTML page) now
produces a Failed row with a clear reason instead of downloading the
page HTML as a file named after the URL path. Skip this hunk (and 6b)
only if the old behaviour is preferred; everything else stands alone.

OLD:
```rust
    let (filename, total, resumable) = match probe(&probe_opts) {
```
NEW:
```rust
    // A web page is not a downloadable file: a YouTube watch page (or
    // any HTML page) would otherwise be saved as a useless file named
    // after the URL path. Refuse it here so the probe-error branch
    // below records a Failed row with this reason.
    let probe_result = probe(&probe_opts).and_then(|p| {
        if p.content_type
            .as_deref()
            .is_some_and(|ct| ct.starts_with("text/html"))
        {
            Err("This link is a web page, not a downloadable file. FetchForge downloads direct file links; video pages such as YouTube are not supported yet.".to_string())
        } else {
            Ok(p)
        }
    });
    let (filename, total, resumable) = match probe_result {
```

## Fix 6 — `crates/bridge/src/main.rs` (2 HUNKS)

### 6a. `GET /downloads` picks up app-added downloads (fixes D7)

OLD:
```rust
    if method == "GET" && path == "/downloads" {
        let list: Vec<DownloadRecord> = app
            .store
            .lock()
            .unwrap()
            .all()
            .into_iter()
            .cloned()
            .collect();
```
NEW:
```rust
    if method == "GET" && path == "/downloads" {
        let list: Vec<DownloadRecord> = {
            let mut store = app.store.lock().unwrap();
            // Pick up downloads added by the desktop app since launch.
            store.reload_from_disk();
            store.all().into_iter().cloned().collect()
        };
```

### 6b. Refuse web-page links honestly (fixes D6) — same note as 5f

OLD:
```rust
        let (filename, total, resumable) = match probe(&probe_opts) {
```
NEW:
```rust
        // A web page is not a downloadable file: a YouTube watch page (or
        // any HTML page) would otherwise be saved as a useless file named
        // after the URL path. Refuse it here so the probe-error branch
        // below records a Failed row with this reason.
        let probe_result = probe(&probe_opts).and_then(|p| {
            if p.content_type
                .as_deref()
                .is_some_and(|ct| ct.starts_with("text/html"))
            {
                Err("This link is a web page, not a downloadable file. FetchForge downloads direct file links; video pages such as YouTube are not supported yet.".to_string())
            } else {
                Ok(p)
            }
        });
        let (filename, total, resumable) = match probe_result {
```

## Fix 7 — Extension manifests: add `nativeMessaging` (fixes D4)

Apply the identical one-word change to BOTH
`extensions/chromium/manifest.json` and
`extensions/firefox/manifest.json` (each file is a single line; change
only the permissions value):

OLD:
```
"permissions":["contextMenus","downloads","storage"]
```
NEW:
```
"permissions":["contextMenus","downloads","nativeMessaging","storage"]
```
Without this, `runtime.sendNativeMessage` always fails and the native
path is dead. Do not touch the `key` (Chrome) or `gecko.id` (Firefox):
they match the installed host manifests.

## Fix 8 — Extension popups: correct the pairing instruction (fixes m3)

Apply the identical one-line change to BOTH
`extensions/chromium/popup.html` and `extensions/firefox/popup.html`:

OLD:
```html
<hr><label>Pairing code (from FetchForge Settings &gt; Pair a browser, 60 seconds)</label>
```
NEW:
```html
<hr><label>Pairing code (from the FetchForge app: click Pair a browser, 60 seconds)</label>
```

---

## After applying: build #5 retest (what proves what)

| What the user sees on launch | Meaning |
|---|---|
| No black console window | Fix 5a worked |
| Status shows "Loading…" then "0 shown" | Frontend runs, invoke works — dead-buttons bug is gone |
| Status stays **empty** | `app.js` never executes in the installed build (hypothesis 1) — needs the WebView2 console to go further |
| Status sticks on **"Loading…"** | Script runs but the first invoke never answers (hypothesis 2) |
| Status shows a persistent error | The error text names the failing layer — report it verbatim |

Then: Download a direct file link (row appears, button shows "Adding…");
Pair (code box appears under the button, "Active for NN seconds");
paste the YouTube link (Failed row with the web-page reason, if 5f/6b
applied); load the extension, pair, right-click a direct link (download
appears in the app list without restarting — Fixes 7 + 4/5b/6a).
