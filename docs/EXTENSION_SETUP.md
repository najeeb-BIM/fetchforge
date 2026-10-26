# Browser extension setup

## Install the desktop app first

Extensions do nothing useful without FetchForge running. Install it
(see INSTALL.md) and launch it once so the bridge and its token exist.

## Chrome / Edge / other Chromium browsers

1. Open `chrome://extensions` (Edge: `edge://extensions`), turn on
   Developer mode.
2. Load unpacked > select `extensions/chromium/`.
3. Right-click any download link > "Download with FetchForge".
4. Pairing (preferred, replaces manual token entry): in FetchForge
   Settings click "Pair a browser" to show a code for 60 seconds,
   then in the extension popup paste that code and click "Pair with
   code". Native messaging (installed by the Windows installer) is
   tried first; pairing is the fallback for portable installs or
   when native messaging is not registered.
5. Manual token entry is now behind the Advanced button in the
   popup, with the warning "Never paste this into a website". It
   will be removed once pairing is verified in a real browser.
   The stable Chrome ID is documented in docs/EXTENSION_ID.md:
   `hkmnehbdedolcaaikibkadblmbfkkija`.
   NOTE: pairing, challenge-response, and the popup changes in this
   round are written but NOT VERIFIED in a real browser yet.

## Firefox (Manifest V3, Firefox 109+)

1. Open `about:debugging#/runtime/this-firefox` > Load Temporary
   Add-on > select `extensions/firefox/manifest.json`. (Temporary
   add-ons unload on browser restart; a signed AMO listing is needed
   for permanent install — not provided in v0.1.0.)
   Note: Firefox MV3 here uses `background.scripts` (supported in
   Firefox MV3), not a service worker, for 109+ compatibility. The
   background code keeps no in-memory state, so a browser or
   service-worker restart is safe — it re-registers its listeners
   and reads the token from storage.
2. Same context-menu and token flow as Chromium.

## What the extension does and does not do

- It sends only the link you right-click (or paste in the popup) to
  the app on this machine. It does not read, store, or send browsing
  history, cookies, or passwords.
- If FetchForge is not running, not registered, and no token is
  saved, the link falls back to the browser's own downloader — it is
  never silently dropped.
- If a site requires sign-in, uses DRM, or serves expiring signed
  links, the desktop app will report the server's error (often
  401/403) rather than bypassing the restriction. That is by design.

## Troubleshooting

- "App not reachable" but FetchForge is running: check the token,
  check that port 18443 is not taken by another program, and check
  that you loaded the extension from the matching folder (Chromium
  vs Firefox manifests differ).
- Context menu missing: reload the extension on its extensions page.
- Native messaging fails after moving the install: reinstall with
  the installer so the registry paths point at the new location.
