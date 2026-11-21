# FetchForge audit — 2026-11-21 (build #4, run 44880635437)

Scope: why the installed Windows app's buttons do nothing (Pair/Unpair/Download),
why no status text ever appears, why a black console window opens, and what
will break the browser-extension goal next. All findings are from reading the
local tree (`~/workspace/fetchforge`, branch `audit-round`), which matches the
shipped GitHub main tree after the two build fixes (`move` in main.rs, NSIS
`GITHUB_PATH` in the workflow).

Evidence standard: every claim cites file:line or is labelled NOT VERIFIED.
Tauri-framework claims were checked against the pinned Tauri 2.12.5 /
tauri-build 2.7.4 / tauri-codegen 2.7.4 source in the local cargo registry,
not from memory. Engine unit tests were run locally: **23 passed, 0 failed**
(`cargo test -p fetchforge-engine --lib`, 2026-11-21).

---

## 1. Root cause: NOT PROVEN from code — two hypotheses, ranked

The single most important symptom is the **empty status line** next to the
Refresh button (confirmed by zooming the user's screenshot: the `#status`
span is completely empty — not "0 shown", not "Bridge not reachable").

Why that matters: `apps/desktop/src/app.js:89` calls `refresh()` at script
load, and `refresh()` (`app.js:39-77`) **always** settles the status span —
either `setStatus(rows.length + " shown")` on success (`app.js:76`) or
`setStatus("Bridge not reachable…")` on failure (`app.js:77`). There is no
code path where `refresh()` runs and the span stays empty. So exactly one of
these is true:

### Hypothesis 1 (most likely): `app.js` never executes in the installed app
- If the script never runs: no listeners are attached (all buttons dead),
  no status is ever written (span stays empty), and the token box stays
  hidden (it is `hidden` in the HTML at `index.html:33` and only JS unhides
  it in bridge mode, `app.js:8-9`). This explains **every** observed symptom
  with one cause, including why the static HTML (dark theme, buttons, the
  "Feat 1, NOT VERIFIED…" note) renders perfectly.
- What was RULED OUT as the mechanism (checked, not assumed):
  - JS syntax: `node --check apps/desktop/src/app.js` passes.
  - Asset embedding: tauri-codegen 2.7.4 `embedded_assets.rs` walks **all**
    files under `frontendDist` with no extension filter, so `app.js` is
    embedded alongside `index.html` and `styles.css` (and the CSS provably
    loads — the dark theme renders).
  - CSP: `tauri.conf.json` sets `script-src 'self'`; `app.js` is same-origin.
  - Early JS exception: every `getElementById` in `app.js` matches an id
    that exists exactly once in `index.html` (verified by reading both
    files); no top-level statement can throw before `refresh()` is called.
  - `window.__TAURI__` shape: the pinned `bundle.global.js` (tauri 2.12.5)
    defines `window.__TAURI__.core.invoke` as a standalone function, so
    `app.js:6` (`isTauri`) and `app.js:24` (destructured `invoke`) are both
    correct.
- The mechanism by which the script fails to execute in the installed build
  is therefore **NOT VERIFIED** — it cannot be established by code reading.
  It needs the WebView2 console on the user's machine, which the current
  build cannot show (see defect D1).

### Hypothesis 2: `app.js` runs, but every Tauri invoke hangs forever
- Tauri 2.12's JS `invoke` (`scripts/core.js` in the tauri crate) returns a
  promise with **no timeout**: if Rust never answers, the promise never
  settles, no error is raised, and `refresh()` never reaches `setStatus`.
  Every later invoke (Pair, Unpair, Download) would hang the same way.
- What was RULED OUT as the mechanism:
  - Missing command registration: all 8 commands are registered in
    `apps/desktop/src-tauri/src/main.rs:419-428` (`list_downloads`,
    `add_download`, `pause/resume/cancel_download`, `bridge_status`,
    `pair_browser`, `unpair_all_browsers`). An unregistered command would
    REJECT ("Command … not found", tauri `webview/mod.rs`), not hang — and
    the frontend catch would show "Bridge not reachable".
  - Capability/ACL: tauri `webview/mod.rs:2080` only ACL-checks plugin
    commands, apps with an ACL manifest, or remote origins. FetchForge's
    commands are app commands from a local origin with no app ACL manifest,
    so `capabilities/default.json` (`core:default`) cannot block them. An
    ACL denial would also REJECT, not hang.
  - Panic in `list_downloads`: its body (`main.rs:120-129`) is a pure
    in-memory read; the store mutex cannot be pre-poisoned (it is first
    locked by the first command itself). Tauri has no `catch_unwind`
    (verified: zero occurrences in tauri 2.12.5 `src/`), so a panic in a
    command WOULD hang that invoke — but no panic path exists in the
    first-invoked command.
  - Invoke-key mismatch: tauri `webview/mod.rs:2003-2025` silently drops
    invokes with a wrong key — the exact hang signature — but the key is
    generated once per run and injected from the same `Builder`
    (`app.rs:1653,1753,2436`), so no mismatch mechanism exists in this app.
- So hypothesis 2 also has **no code-level mechanism**: NOT VERIFIED.

### How build #5 settles it
Fix 1 (FIXES.md) adds a startup diagnostic: `app.js` writes "Loading…" to
the status line the moment it executes. On retest:
- Status stays **empty** → hypothesis 1 (script never runs).
- Status sticks on **"Loading…"** → hypothesis 2 (script runs, invoke hangs).
- Status shows **"0 shown"** → the frontend works and the bug is elsewhere.
No further guessing: the next build reports its own state. The 10-second
invoke timeout in the same fix also converts any future hang into a visible
error instead of a dead UI.

---

## 2. Defects found (with severity)

### Critical
None proven as the root cause (see §1). The closest to Critical is D1: the
app is a black box — every failure mode is silent, which is why this audit
cannot pin the root cause from code alone.

### Major
**D1 — No diagnostics anywhere; every failure is silent.**
- No `window.onerror` handler in `app.js`; no startup self-test; no
  invoke timeout (Tauri's JS `invoke` never times out).
- `app.js:137-140`: the `bridge_status` check swallows failures with
  `.catch(() => {})`.
- Rust `eprintln!` output (e.g. the invoke-key mismatch message, the
  "bridge binary not found" warning at `main.rs:57`) goes nowhere:
  the app is built with `windows_subsystem = "windows"` (`main.rs:9`),
  so there is no console to read it from.
- Net effect: the user, and this audit, get zero signal from the app.

**D2 — `refresh()` wipes error messages within 2 seconds.**
- `app.js:76`: every successful 2-second refresh overwrites `#status`
  with "N shown". A Pair/Download error (`app.js:87,120,132`) is visible
  for at most one refresh cycle. (Pre-existing finding, confirmed.)

**D3 — Bridge console window: the bridge is spawned with a visible console.**
- `crates/bridge/Cargo.toml` has no `windows_subsystem`, and
  `spawn_bridge` (`main.rs:44-76`) sets no `CREATE_NO_WINDOW` flag, so
  Windows shows a blank black console next to the app. Closing it kills
  the bridge, which kills Pair/Unpair (they POST to 127.0.0.1:18443).
  The black window the user sees IS the bridge — confirmed by the spawn
  args (`serve --data-dir <data> --port 18443`, `main.rs:64-68`) and by
  the installer placing `fetchforge-bridge.exe` next to `FetchForge.exe`
  (`installer/fetchforge.nsi:35-36`, `scripts/build-windows.ps1:31-35`).

**D4 — Extension cannot use native messaging: `nativeMessaging` permission missing.**
- `extensions/chromium/manifest.json` and `extensions/firefox/manifest.json`
  both declare `"permissions":["contextMenus","downloads","storage"]` —
  no `"nativeMessaging"`. Chrome and Firefox both require that permission
  for `runtime.sendNativeMessage`, so `background.js:74-82` (`sendViaNative`,
  documented as "tried first") always fails and the extension silently
  falls back to the bridge path. (Repo-wide grep for `nativeMessaging`
  returns zero matches outside this audit.)

**D5 — Blocking commands run on the UI/IPC thread; a slow probe freezes the app.**
- `add_download` (`main.rs:132-224`) is a sync command: it runs `probe()`,
  a blocking network call with a 120 s timeout + 30 s connect timeout
  (`crates/engine/src/download.rs:152-153`), inline on the Tauri IPC thread.
  Same for `pair_browser`/`unpair_all_browsers` via `bridge_post`
  (`main.rs:337-368`): `TcpStream::connect` + `read_to_string` with **no**
  read/write timeout — if the bridge accepts and never answers, the call
  blocks forever. While any of these run, no other invoke is processed.

**D6 — A YouTube link can never download a video, and fails silently into garbage.**
- `validate_url` (`crates/engine/src/validate.rs:13-36`) accepts any
  http(s) URL with a host, so a YouTube watch page passes. `probe()`
  (`download.rs:161-190`) HEADs the page, gets 200 HTML, and
  `filename_from_url` (`filename.rs:89-96`) derives the filename `watch`
  from the URL path. The engine has no watch-page/media extraction
  (`media.rs` supports only direct files, open HLS and (refused) DASH;
  no yt-dlp, no page parsing). Result once the buttons work: the app
  downloads the watch-page HTML as a file named `watch`. The user's
  actual test link hits exactly this.

**D7 — App list never shows extension/bridge downloads until restart.**
- The Tauri app and the bridge each hold their own in-memory `Store` over
  the same `%LOCALAPPDATA%\FetchForge\downloads.json`. `list_downloads`
  (`main.rs:120-129`) reads only the app's copy, and the bridge's
  `GET /downloads` reads only its copy, so a download added from the
  extension never appears in the app UI (and vice versa) until restart.
  (`Store::save` does reload-before-save merging, `state.rs:170-232`, so
  no records are LOST — they are just invisible to the other process.)

### Minor
**m1 — No button click feedback or busy state.** `styles.css` has no
`:active` or `:disabled` rules; handlers give no in-flight indication.
(User request, 2026-11-21.)

**m2 — Pairing-code UI is confusing and ships a dev note.**
- The code/countdown live in two bare inline spans (`index.html:42-43`)
  between the Pair and Unpair buttons, with no label.
- `index.html:45` shows users the text "Feat 1, NOT VERIFIED in a real
  browser yet." — a development note, not UI copy. (This is the text the
  user read out on the call; its source is that line.)

**m3 — Popup and docs point at a "Settings" page that does not exist.**
- `extensions/chromium/popup.html` (identical in Firefox): "Pairing code
  (from FetchForge Settings > Pair a browser…)". There is no Settings UI;
  the button is on the main window. Same wrong reference in
  `docs/EXTENSION_SETUP.md:14`, `index.html:35` ("Paste token from
  FetchForge Settings"), and the bridge doc comment
  (`crates/bridge/src/main.rs:5`, "shown in the desktop app's Settings").

**m4 — Startup race: bridge may not be listening when the UI first calls it.**
- `main()` spawns the bridge (`main.rs:408`) and immediately builds the
  window; `pair_browser` reads the token file (written by the bridge at
  startup, before it binds) and then connects. If the bridge is slow to
  bind, Pair fails with "Bridge not reachable". No retry. (Mitigated by
  Fix 1's visible error + retry-by-clicking-again; no code change for a
  retry loop — out of scope.)

**m5 — `refresh()` re-renders the whole table every 2 s**, rebuilding all
row buttons each cycle (focus/hover state on row buttons is lost).
Cosmetic; not fixed in this round.

**m6 — Extension has no one-click download for the current page.**
- The only in-browser triggers are the right-click context menu on
  link/video/audio (`background.js:94-100`) and pasting a URL into the
  popup. There is no toolbar-button "download this page's media".
  On YouTube, right-clicking the player yields a `blob:` URL, which
  `sendDownload` rejects (`background.js:84`, `^https?://` check) —
  silently (returns `false`, no user message). Noted, not redesigned
  (per task scope).

---

## 3. Verified CORRECT (do not "fix")

- **All 8 Tauri commands registered** (`main.rs:419-428`), names match
  `app.js` invoke calls exactly.
- **Tauri config**: `withGlobalTauri: true`, `frontendDist: "../src"`,
  CSP allows same-origin scripts, capability `core:default` for window
  `main` (default label). Verified against the pinned Tauri 2.12.5 source:
  none of these can block app-command invoke.
- **Bridge spawn exists and runs on startup** (`main.rs:408`, `spawn_bridge`
  `main.rs:44-76`) — the 2026-10-26 F08 note ("docs claimed autostart but
  no code spawned it") is STALE and wrong for current code. The bridge is
  also killed on app exit (`main.rs:432-437`).
- **Token path matches**: bridge writes `bridge-token.txt` into the
  `--data-dir` it was given (`crates/bridge/src/main.rs:183`);
  the app passes `%LOCALAPPDATA%\FetchForge` (`main.rs:64-68,103-109`)
  and reads the token from the same dir (`main.rs:370-379`). Same file.
- **Pairing/challenge protocol is consistent end to end**: bridge
  endpoints `/pair/generate`, `/pair/verify`, `/challenge`, `/unpair-all`,
  `/health` (`crates/bridge/src/main.rs:596-727`) match `app.js`
  (Tauri + bridge mode), `background.js` (challenge HMAC-SHA256 verified
  against RFC 4231 test case 1 in `pairing.rs` tests), and `popup.js`.
  One active code, 60 s TTL, single use, 5-wrong-attempt invalidation —
  all unit-tested (23 engine tests pass).
- **Extension ID matches the native-messaging host manifest**: the ID
  computed from the `key` in `extensions/chromium/manifest.json` is
  `hkmnehbdedolcaaikibkadblmbfkkija`, exactly the `allowed_origins` entry
  in `installer/host-manifest-chrome.json`. Firefox `gecko.id`
  `fetchforge@example.local` matches `host-manifest-firefox.json`
  `allowed_extensions`. (Native messaging still fails in practice — see
  D4 — but the registration side is correct.)
- **Installer layout**: `fetchforge-bridge.exe` is installed next to
  `FetchForge.exe` (`fetchforge.nsi:35-36`); manifests go to
  `$INSTDIR\native-host\` with the relative path `../fetchforge-bridge.exe`
  patched in by the build script (`build-windows.ps1:48`); registry
  keys for Chrome/Edge/Firefox are written per-user (`fetchforge.nsi:46-48`).
- **Store crash-safety**: atomic temp-file+rename saves, `.bak` fallback,
  corrupt-file quarantine, two-process merge on save (`state.rs:170-232`) — all
  unit-tested, including the two-store merge test.
- **`add_download` always creates a record** (Queued, or Failed with the
  probe error, `main.rs:141-171`): a clicked Download producing no row
  cannot be a URL or probe problem — it means the click never reached
  Rust or the list never refreshed. (Pre-existing finding, confirmed.)
- **Frontend JS/JSON syntax**: `node --check` passes on `app.js`,
  `background.js`, `popup.js`; all JSON manifests/configs parse.
- **No duplicate element IDs** in `index.html`; every id used by `app.js`
  exists exactly once.

---

## 4. Retest plan for build #5 (what proves what)

After the fixes in FIXES.md are applied to GitHub main and build #5 passes:
1. Install build #5, launch. **No black console window** → D3 fixed.
   (If a console still appears, the spawn flags did not take effect.)
2. Status line at launch: **"Loading…" then "0 shown"** → frontend runs
   and invoke works (both hypotheses dead). **Empty** → hypothesis 1.
   **Stuck on "Loading…"** → hypothesis 2. **"Bridge not reachable…"**
   persisting → list_downloads invoke failing (new information).
3. Click Download with a direct file link (e.g. a small .zip/.pdf URL):
   button shows "Adding…", a row appears. Click Pair: button shows
   "Working…", the code box appears under the button with a live
   "Active for NN seconds" countdown.
4. Click Pair with the same YouTube link the user tried: expect the honest
   refusal row ("This link is a web page, not a downloadable file…") —
   NOT a file named `watch`. (Only if Fix 9 is applied.)
5. Load `extensions/chromium/` unpacked in Chrome, pair with the code,
   right-click a direct download link → "Download with FetchForge": the
   download must appear in the app list without restarting the app
   (Fixes 7 + 10). If it falls back to the browser downloader, the popup
   says so (existing fallback note).
6. Close the app: no `fetchforge-bridge.exe` left running (Task Manager);
   port 18443 free.

## 5. Not verified (needs the Windows machine)
- The root-cause mechanism itself (§1) — needs the build #5 diagnostic.
- Whether WebView2 on the user's machine is current and unblocked by
  policy (cannot be checked from here).
- The extension flow in a real Chrome (pairing, challenge, native
  messaging) — code-consistent but never run in a browser.
- YouTube's exact probe response (HEAD 200 vs 403) — Fix 9 covers both:
  a 403 already produces a Failed record; a 200 HTML page is now refused
  with a clear message instead of being downloaded.
