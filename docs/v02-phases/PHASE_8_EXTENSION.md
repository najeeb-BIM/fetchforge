# Phase 8 — Browser Extension Integration (popup media list)

Date: 2026-11-21. Implemented by the extension worker; verified independently
by the coordinator. (Phase 8's desktop half — native-messaging forwarding —
is with the desktop worker and is NOT covered here.)

## What was implemented
- **Popup media list**: `popup.html` gains a "Media on this page" section
  (status line + list); `popup.js` queries the active tab and asks the
  content script (`tabs.sendMessage`, `getMedia`) for detected media. Each
  item gets a Download button through the existing `sendUrl` → `sendDownload`
  path. Graceful states: restricted pages ("does not allow media
  detection"), unreachable content script ("Reload the page and try again"),
  no media ("No media found on this page"). `viaPage` items are labelled
  "(via page link)". Existing paste-URL, pairing, and advanced-token
  sections untouched. `textContent` only, no `innerHTML`.
- **`activeTab` permission** added to both manifests — needed for the popup
  to read the active tab's URL and message it. Low-trust-cost (only the tab
  the user invokes the extension on).

## Files modified and created
- Modified: `extensions/{chromium,firefox}/popup.html`,
  `extensions/{chromium,firefox}/popup.js`, `extensions/{chromium,firefox}/
  manifest.json` (+`activeTab`, version 0.2.0).
- Created: none in this phase.
- Commits: `b7f526d` (Chromium), `a1b0073` (Firefox mirror).

## Dependencies added
None.

## Tests run (with results)
- `node --check`: passes on both popup.js files (coordinator re-ran).
- `python3 -m json.tool`: both manifests valid JSON (coordinator re-ran).
- Worker re-read popup.js end-to-end.

## Remaining limitations
- Popup list shows only the first responding frame's media (top frame in
  practice) — iframe media gets overlay buttons but may not appear in the
  popup list.
- Multi-select batch download and the Media Detector panel are Phase 9
  (out of scope, not built).
- Pages loaded before the extension was installed/reloaded need a reload
  before the content script exists there.
- NOTHING was run in a real browser (see Phase 4 report for the test plan).
- `docs/EXTENSION_SETUP.md` is now stale (says "No content script exists in
  this extension") — docs update is a later step; the worker owned only
  `extensions/`.
- Native-messaging survival fix (the other half of Phase 8) is pending with
  the desktop worker.
