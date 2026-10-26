# Feature matrix — v0.1.0

Status key: DONE = implemented and tested as noted in TEST_REPORT.md.
PARTIAL = implemented but with a stated limit. NOT YET = not in v0.1.0.

## Desktop app

| Feature | Status | Note |
|---|---|---|
| Dashboard: active / queued / completed / failed | DONE | Tauri UI + bridge list API |
| Progress, % , size, speed | PARTIAL | Engine reports bytes/sec; UI shows progress/size, speed column is a placeholder in the Tauri UI (bridge progress API carries bytes, UI refreshes every 2 s) |
| Pause / resume / cancel / retry | DONE* | *Retry = re-add; in-app retry button is not yet wired. Pause/resume proven for single-stream (see TEST_REPORT) |
| Remove, search, filter, sort | PARTIAL | Search + status filter in UI; remove/sort not yet in UI |
| Queue + max simultaneous | DONE | Bridge gate, default 3; FIFO |
| Scheduling (timed start/stop) | NOT YET | Next milestone |
| Bandwidth limit | DONE | Engine limiter, per-download setting in bridge settings |
| Default download directory | DONE | Settings, default Downloads |
| Categories + auto-organisation | DONE | Extension-based folders |
| Notifications on finish/fail | NOT YET | Planned via Tauri notification plugin |
| Settings, startup prefs, log viewer | PARTIAL | Settings file; no log-viewer UI yet |
| Persistent state + crash recovery | DONE | JSON store, atomic save, interrupted -> Paused/Failed |
| Startup on login | NOT YET | Manual for now |

## Engine

| Feature | Status | Note |
|---|---|---|
| HTTP/HTTPS, redirects, error mapping | DONE | 10 redirects, friendly 401/403/404/429/5xx messages |
| Large files | DONE | 5 MiB segmented test passed; streaming, no whole-file buffering |
| Range resume (single-stream) | DONE | Proven by pause/resume test, hash-verified |
| Segmented downloads | DONE | 4 segments, Range-checked (206 required) |
| Segmented pause/resume | PARTIAL | Paused segmented download restarts cleanly; never corrupt, but re-downloads |
| Retries with backoff | DONE | Configurable, exponential up to 16 s |
| Timeouts, interrupted recovery | DONE | Connect 30 s, total 120 s |
| Integrity check | PARTIAL | Length verified against Content-Length; no checksum unless server supplies one |
| Safe temp files, atomic finish | DONE | `.part` + rename |
| Filename from headers/URL, Unicode, Windows rules | DONE | Unit-tested |
| Non-Range server fallback | DONE | Restarts honestly, never claims resume |

## Browser integration

| Feature | Status | Note |
|---|---|---|
| Chrome / Edge / Chromium extension | PARTIAL | Source complete, MV3; not tested in a real browser in this env (no browser installed) |
| Firefox extension | PARTIAL | Source complete, MV3 (background.scripts, Firefox 109+); not tested in a real browser in this env (no browser installed) |
| Context menu | DONE (source) | Link / video / audio contexts |
| File-type filters | PARTIAL | Extension sends any link the user chooses; automatic filtering by type is not yet enforced |
| Connected indicator | DONE (source) | Popup checks /health + token state |
| Native messaging | PARTIAL | Host code + manifests + installer registration complete; end-to-end in a real browser NOT tested here |
| Loopback bridge + token auth | DONE | Tested: health, 401 without token, add/list/pause/resume, native ping |
| Fallback to browser downloader | DONE (source) | On any bridge/native failure |

## Media

| Feature | Status | Note |
|---|---|---|
| Direct MP4/WebM/etc. links | DONE | Ordinary engine downloads |
| HLS clear playlists | PARTIAL | Engine function works; not yet exposed via bridge/UI |
| DASH | NOT YET | Refuses DRM/unsupported shapes honestly; no segment assembly |
| DRM / paywall bypass | NEVER | Refused by design |

## Packaging

| Feature | Status | Note |
|---|---|---|
| Windows x64 app binary | NOT TESTED HERE | Built by Windows CI / build-windows.ps1; cannot be built on Linux |
| NSIS installer | PARTIAL | Script complete, per-user, uninstall + data choice; not executed here (no NSIS/Wine/Windows) |
| Portable build | PARTIAL | Script complete; same Windows-only caveat |
| Signed installer | NO | No certificate; SmartScreen warning expected |
| Windows 10 22H2 / 11 compat | NOT TESTED HERE | Targeted, not verified — see TEST_REPORT |
