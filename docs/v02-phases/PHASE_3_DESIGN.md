# Phase 3 — Integration Design (v0.2.0)

Date: 2026-11-21. Governing spec: UNIVERSAL_MEDIA_SPEC.md (repo root).
Design principle: build on what exists; add modules; do not rewrite working
paths. Direct-link downloading must keep working exactly as it does now.

## Module map (spec §11 → real code)

| Spec module | Implementation | Status |
|---|---|---|
| MediaDetectionEngine | Extension content script (DOM + MutationObserver) + background reporting | NEW (Phase 4/8) |
| NetworkMonitor | Extension background, webRequest observation (only if the permissions tradeoff is accepted; DOM-first otherwise) | NEW, optional (Phase 4) |
| StreamAnalyzer, HLSParser, DASHParser | crates/engine/src/media.rs — wire in + extend | EXISTS, unwired/stub (Phase 5) |
| MediaMetadataExtractor | crates/engine/src/extract.rs (NEW) — yt-dlp wrapper: formats, title, duration, Drive handling | NEW (Phase 6) |
| VideoDownloadEngine | engine download.rs (exists) + extract.rs + ffmpeg.rs | EXTEND (Phase 6) |
| FFmpegProcessor | crates/engine/src/ffmpeg.rs (NEW) — locate ffmpeg, merge A/V | NEW (Phase 6) |
| ImageDownloadEngine | crates/engine/src/image.rs (NEW) — srcset, bulk, resolution choice | NEW (Phase 7) |
| ThreeDAssetDownloader | crates/engine/src/threed.rs (NEW) — GLB/GLTF/OBJ/FBX/STL/USDZ + glTF deps | NEW (Phase 7) |
| DownloadQueueManager | state.rs + workers | EXISTS |
| BrowserIntegration | extensions/* + native_host forwarding | EXTEND (Phase 8) |
| DownloadStorageManager | state.rs, filename.rs, category.rs | EXISTS (extend categories for Images/3D) |

## Data flow

```
Browser page ──content script──► background.js ──sendDownload / report──►
  (a) native messaging ──► bridge --native-host ──FORWARDS──► bridge :18443
  (b) bridge HTTP :18443 (token + HMAC challenge, as today)
Bridge/Tauri add path:
  URL ──► validate ──► classify ──► direct file → existing downloader
                         ├─► .m3u8/.mpd → media.rs (HLS/DASH, quality choice)
                         ├─► HTML page / YouTube / Drive → extract.rs (yt-dlp)
                         │     → formats listed → user picks (or sensible
                         │       default for extension-sent URLs) → download
                         │       (+ ffmpeg merge when streams are split)
                         └─► image / 3D URL → image.rs / threed.rs (Phase 7)
Detected media (no download yet):
  content script ──► background ──► POST /media/detected ──► bridge stores
  ──► GET /media/detected ──► app Media Detector panel (Phase 9)
```

## Bridge API additions (all under the existing token auth)
- `POST /media/formats` { url } → { title, duration, formats: [{id, label, resolution, ext, vcodec, acodec, filesize, kind}] } — error when the page has no extractable media, with the reason.
- `POST /downloads` gains optional `format_id` and `kind` ("video" | "audio" | "image" | "model") fields. Without them, behavior is unchanged.
- `POST /media/detected` { items: [...] } → stores detected media (dedupe by URL). `GET /media/detected` → list for the UI. Items carry only confirmed metadata (spec §8); unknown fields are omitted, never guessed.
- Native host (`--native-host`) forwards `addDownload` to the loopback bridge over HTTP (reading the token from the data dir) instead of downloading in-process, so the download survives the host process exiting. Fallback when the bridge is unreachable: decided by the implementer, must be justified and must never silently drop the request.

## Tauri invoke additions
- `media_formats(url)` → same shape as POST /media/formats.
- `add_download` gains the same optional format/kind behavior as the bridge (shared engine code — one implementation in the engine crate, two thin call sites).

## Extension design
- Content script (NEW): DOM detection of <video>/<audio> (+ <img> and 3D assets in Phase 7) via query + MutationObserver; IDM-style overlay button on videos; reports detected items to background; never touches the stored token (token stays in background storage only, as today).
- Background: new `runtime.onMessage` handler for content-script reports + downloads; badge count of detected media on the current tab (spec §7 toolbar count).
- Popup: media list for the current tab with per-item Download (Phase 8), multi-select batch download (Phase 9).
- Permissions: DOM-first. Any new host permission (e.g. <all_urls> for webRequest) must be justified in the implementer's report — it affects user trust.

## UI (Phase 9)
- Keep the current single-page structure and style. Add panels progressively: Media Detector panel (auto-list from GET /media/detected), format picker (dialog/section in the add flow), batch multi-select on detected items. No fake buttons: every control must work or not exist.
- The inlined script in apps/desktop/src/index.html is the source of truth; app.js is deleted once verified unreferenced.

## yt-dlp / FFmpeg provisioning
- Decided by the desktop implementer with real numbers (binary sizes, installer impact, hash verification). Options: bundle in the installer (NSIS + workflow changes) or download-on-first-use from GitHub releases with pinned SHA-256 verification. No network at install time is required either way for direct-link downloads; extraction features must fail with a clear message when their tool is missing.

## Security (spec §10, binding)
- No DRM/auth/paywall/encryption bypass, ever. No session credentials or cookies taken from the browser. No sensitive headers/tokens in logs. Protected/inaccessible media gets a clear explanation, never a fake success. yt-dlp is used only for supported/authorized sources; it must not be passed user credentials.

## Phase sequencing
- Phases 1–3: this design (done 2026-11-21).
- Phases 4–6 + 8 (detection, HLS/DASH, quality/merge, extension + native messaging): in progress with two workers.
- Phase 7 (image + 3D): second wave, after the extraction/quality foundation lands.
- Phase 9 (UI panels): second wave, after Phases 4–8 land.
- Phase 10 (full-system testing): final wave, authorized test sources only (spec §13).

## Files
- Modified: none (design only).
- Created: docs/v02-phases/PHASE_3_DESIGN.md (this file).
- Dependencies added: none yet (yt-dlp/ffmpeg provisioning decision is pending with the desktop implementer; any new Rust crates must be justified in that phase's report).
- Tests run: none (no code changed).
- Remaining limitations: Phases 4–10 not yet implemented; the design will be revised if implementation proves any part of it wrong.
