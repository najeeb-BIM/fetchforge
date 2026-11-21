# Phase 1 — Codebase Inspection (v0.2.0)

Date: 2026-11-21. Baseline: audit-round @ 41954a0 (synced with GitHub main build #8).
Verified by reading the code on this VM; no Windows verification claimed.

## Current download architecture

```
User pastes link in app ──┐
Extension right-click ────┼──► Tauri app (in-process engine) ──► downloads.json ──► disk
Extension popup paste ────┘    + bridge child (HTTP :18443) ──► same store file
Native messaging ─────────► bridge --native-host (short-lived process)
```

### crates/engine (shared by app + bridge)
- `validate.rs` — accepts http/https only; rejects blob:, data:, file:, ftp:, embedded credentials, >8 KiB.
- `download.rs` — `probe()` (HEAD, falls back to ranged GET bytes=0-0), segmented multi-connection Range download, pause/resume/cancel, `.part` + atomic rename, retry, speed/progress. No content-type routing: every URL goes through the same file path.
- `media.rs` — `download_hls()` implemented (master playlist follows FIRST variant only, segments concatenated to one .ts; refuses EXT-X-KEY). `download_dash()` is a STUB: refuses DRM/multi-period honestly, then always errors "DASH segment assembly is not yet implemented". NEITHER function is called by any code path (only re-exported from lib.rs).
- `state.rs` — `Store` over downloads.json, `reload_from_disk()`, lock-file guard for multi-process saves, `recover_interrupted()`. Categories in `category.rs` (by filename extension only).
- `pairing.rs`, `token.rs`, `filename.rs`, `id.rs` — pairing codes, secure token, Windows-safe filenames, IDs.

### crates/bridge (single file, main.rs ~905 lines)
- HTTP server on 127.0.0.1:18443: GET /downloads, POST /downloads, /pair/verify, /challenge, /health. Token auth (bridge-token.txt in the data dir).
- `App::add_download`: probe → refuse text/html → record → `spawn_worker` (background thread). Duplicates the Tauri add logic.
- `native_host()`: stdio length-prefixed JSON, handles `ping` and `addDownload` IN-PROCESS with an async worker thread, replies immediately. Chrome may kill the host after the reply (see Phase 2).
- Data dir: %LOCALAPPDATA%\FetchForge (Windows), ~/.local/share/fetchforge (Linux).

### apps/desktop/src-tauri (Tauri app)
- Own in-process store + invoke handlers: list/add/pause/resume/cancel/pair/unpair. `list_downloads` reloads from disk each call. Spawns the bridge child on launch (CREATE_NO_WINDOW). Frontend: `apps/desktop/src/index.html` with the UI script INLINED (app.js is dead weight, unreferenced). No CSP block in tauri.conf.json. Never declare `isTauri` in frontend code (Tauri defines window.isTauri; a const with that name kills the whole script — build #8 root cause).

### extensions/chromium + extensions/firefox (MV3)
- background.js: context menu (link/video/audio) → sendDownload: native messaging first → bridge HTTP (token + HMAC challenge) → browser fallback (records lastFallback). sendDownload rejects non-http(s) URLs. Popup: paste URL, pairing code, manual token. NO content script, NO network monitoring, NO media detection. Chromium manifest has a fixed `key` → stable ID hkmnehbdedolcaaikibkadblmbfkkija (must not change).

### installer + CI
- installer/fetchforge.nsi: NSIS per-user install; registers native-messaging host manifests (Chrome/Edge/Firefox). .github/workflows/windows-build.yml: builds Tauri + NSIS on windows-latest.

## What already satisfies the spec
- Spec §4 (download engine): largely EXISTS — multithreaded Range downloads, pause/resume, retry, queue, history, integrity checks on segments.
- Spec §5 (HLS): implemented but UNWIRED. DASH: stub only.
- Spec §7 (browser integration): partial — context menu + native messaging + pairing exist; detection does not.

## Files
- Modified: none (inspection only).
- Created: docs/v02-phases/PHASE_1_INSPECTION.md (this file).
- Dependencies added: none.
- Tests run: baseline `cargo build --workspace` + `cargo test --workspace` — all pass (see status file).
- Remaining limitations: see Phase 2.
