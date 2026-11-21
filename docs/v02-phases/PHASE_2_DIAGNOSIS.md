# Phase 2 — Diagnosis: why direct links work but embedded media does not

Date: 2026-11-21. Every cause below was verified by reading the code cited.

## 1. There is no detection layer at all
The only URL sources are (a) paste-link in the app, (b) right-click context menu in the browser. Embedded media — players, galleries, 3D viewers — never surfaces a URL to the user or to the extension. The extension has no content script and no network-request monitoring (extensions/chromium/manifest.json has no `content_scripts`; background.js header says "No content script exists in this extension").

## 2. blob: URLs are rejected on both sides
HTML5 players using Media Source Extensions expose `blob:` URLs. `validate_url` (crates/engine/src/validate.rs) accepts http/https only, and extension `sendDownload` (background.js) returns false for non-http(s) URLs. The actual media bytes live behind MediaSource / XHR / fetch inside the page — invisible to the app. No code anywhere resolves a blob: URL to its underlying resource.

## 3. HTML pages are refused instead of extracted
Both `add_download` implementations (crates/bridge/src/main.rs ~line 274 and apps/desktop/src-tauri/src/main.rs ~line 156) probe the URL and fail any `text/html` response with "video pages such as YouTube are not supported yet". There is no extraction step: no yt-dlp, no manifest discovery, no format listing. Proven live on 2026-11-21: a YouTube watch URL right-clicked in Chrome reached the app and became a Failed row with exactly this error.

## 4. HLS is dead code; DASH is a stub
`download_hls` (crates/engine/src/media.rs) exists but nothing calls it — the only reference in the tree is the re-export in lib.rs. A pasted .m3u8 URL goes through the generic file downloader, which saves the manifest text as a file. `download_dash` always errors ("DASH segment assembly is not yet implemented"). No manifest parsing, no variant/quality choice anywhere in the add path.

## 5. Split streams cannot be merged
YouTube and other adaptive services serve separate video-only and audio-only streams. Nothing lists formats, nothing picks a combination, and nothing merges (no FFmpeg integration anywhere in the tree).

## 6. Extension-sent downloads run in a short-lived process
`native_host()` (crates/bridge/src/main.rs ~line 802) calls `app.add_download` in-process with an async worker thread and replies immediately. Chrome may terminate the host after the sendNativeMessage reply, which would kill the worker mid-download. Even direct links sent from the extension are at risk — the long-running bridge child (which survives) is not in this path.

## 7. Images and 3D have no handling at all
No srcset parsing, no lazy-image handling, no bulk image mode, no 3D format detection. Categories are assigned by filename extension only (crates/engine/src/category.rs).

## 8. The UI has nowhere to show detected media
Single list view (apps/desktop/src/index.html). No media-detection panel, no format/quality picker, no batch multi-select, no per-resource info panel.

## Files
- Modified: none (diagnosis only).
- Created: docs/v02-phases/PHASE_2_DIAGNOSIS.md (this file).
- Dependencies added: none.
- Tests run: none (no code changed); the YouTube failure was verified live on the user's laptop on 2026-11-21 (Failed row, error text quoted above).
- Remaining limitations: all eight causes above are open; Phase 3 designs the fix for each.
