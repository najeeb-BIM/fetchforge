# Phase 4 — Network-Level Media Detection (extension side)

Date: 2026-11-21. Implemented by the extension worker; verified independently
by the coordinator (see "Verification" below).

## What was implemented
DOM-based media detection in the browser extension (the network-monitor half
of spec §1 is the browser's DOM + page context; see the webRequest decision
below for why observation was not added).

- **New content script** `extensions/{chromium,firefox}/content.js` (280 lines
  each, byte-identical): detects `<video>`/`<audio>` via querySelectorAll +
  MutationObserver (dynamic players, `src` changes), reading `currentSrc`,
  `src`, `<source>` children. Skips hidden/tiny elements (<400 px²) and
  `data:` URLs. http/https only; manifest matches mean it never runs on
  `chrome://`, `about:`, or extension pages.
- **Detector registry (Phase 7 plug-in point)**: detectors are
  `{ kind, selector, overlay, extract }` entries via `registerDetector()`.
  Video and audio are registered now; Phase 7 adds image/3D by registering
  new detectors — collect/overlay/report/popup need no changes.
- **IDM-style overlay button**: Shadow-DOM-isolated "↓ FetchForge" button at
  the top-right of each video ≥120×68 px, repositioned on scroll/resize
  (rAF-throttled), hidden off-screen, removed when the video leaves the DOM.
  Click re-resolves the source at click time and sends it via a new
  `downloadMedia` message → existing `sendDownload` path. Button shows
  Sending… / Sent / Failed feedback.
- **blob: handling**: blob: sources (e.g. YouTube MSE) cannot be downloaded
  directly, so the overlay/popup send the page URL with `viaPage: true`;
  the desktop extraction endpoint (Phases 5–6) resolves it server-side.
- **Reporting path (per Phase 3 design)**: content script sends `reportMedia`
  to background only when the detected set changes. Background merges
  per-frame reports per tab (dedupe by kind|URL), stamps `pageUrl`, POSTs
  `{ items }` to `POST /media/detected` — debounced 1s per tab, only when
  changed, using the existing token + HMAC-challenge auth. Best-effort and
  silent: no token, no bridge, or a missing endpoint (404) never breaks
  detection or downloading.
- **Badge count (spec §7)**: per-tab detected-media count via
  `action.setBadgeText`, counts mirrored to storage.local so the badge
  survives a service-worker restart; restored on tab activation, cleaned up
  on tab close.
- **Manifest**: `content_scripts` added (`http://*/*`, `https://*/*`,
  `all_frames: true`, `run_at: document_idle`); version 0.1.0 → 0.2.0.

## webRequest / host-permissions decision
DOM-only detection. No `webRequest` observation, no `<all_urls>` host
permission; `host_permissions` remains exactly
`["http://127.0.0.1:18443/*"]`. Rationale: DOM detection covers the v0.2.0
scope (direct media + blob:/MSE via the page-URL extraction flow);
`<all_urls>` would trigger Chrome's broad "read your data on all sites"
warning and undermine the extension's minimal-permission trust story; the
Phase 3 design marks NetworkMonitor optional and DOM-first. Revisit only
with the user's explicit consent if a later wave needs it.
Caveat (stated loudly): `content_scripts` with `http://*/*` + `https://*/*`
matches inherently grants read/change of page content on all sites — the
minimum needed for DOM detection, but users reviewing permissions will see
the broad grant. It is not the same as `<all_urls>` host permission (no
network observation, no fetch/XHR to arbitrary sites).

## Files modified and created
- Created: `extensions/chromium/content.js`, `extensions/firefox/content.js`.
- Modified: `extensions/{chromium,firefox}/background.js` (+`downloadMedia` /
  `reportMedia` handlers, reporting + badge), `extensions/{chromium,firefox}/
  manifest.json` (+`content_scripts`, +`activeTab`, version 0.2.0; Chromium
  `key` byte-identical — stable ID preserved).
- Commits: `e66b86e` (Chromium), `a1b0073` (Firefox mirror).

## Dependencies added
None.

## Tests run (with results)
- `node --check`: passes on all 6 extension JS files (coordinator re-ran).
- `python3 -m json.tool`: both manifests valid JSON, version 0.2.0, MV3
  (coordinator re-ran).
- Coordinator verified: Chromium `key` identical to the GitHub main copy;
  only `extensions/` paths touched in the 3 commits (+926/−18); no hardcoded
  example media URLs in production logic; Firefox JS byte-identical to
  Chromium; `reportMedia`/`downloadMedia` handlers + `setBadgeText` present.
- Worker re-read background.js end-to-end (found and fixed one self-caught
  mistake: `refreshMedia()` misplaced inside the pairing handler).

## Remaining limitations
- No webRequest observation (deliberate): XHR/fetch-loaded streams with no
  `<video>` tag are not detected; media inside shadow DOMs is not detected.
- Overlay does not appear over fullscreen video (top layer).
- `POST /media/detected` round-trip UNVERIFIED — the desktop endpoint does
  not exist yet (desktop worker building it); the extension tolerates 404
  silently by design.
- On Chrome a service-worker restart loses in-memory per-frame state until
  the next content-script report (badge counts survive via storage).
- NOTHING was run in a real browser: injection, overlay rendering,
  MutationObserver behavior, badge, and message flows are code-reviewed and
  syntax-checked only. Real-browser test plan is in the worker's report
  (10 steps: load unpacked, direct video page, overlay click, popup list,
  YouTube via-page, no-media/restricted states, dynamic video, Firefox
  repeat, bridge reporting check, context-menu regression).
