# Install, upgrade, uninstall

## Install (Windows 10 22H2+ / Windows 11 x64)

1. Run `FetchForge-Setup-0.1.0-x64.exe`. It installs per-user to
   `%LOCALAPPDATA%\Programs\FetchForge` — no administrator rights.
2. The installer creates a Start Menu shortcut and a desktop shortcut,
   and registers the native-messaging host for Chrome, Edge, and
   Firefox (per-user registry keys under HKCU).
3. Launch FetchForge. First run creates `%LOCALAPPDATA%\FetchForge`
   (settings, download history, bridge token) and the app starts the
   bridge on `127.0.0.1:18443` automatically. The bridge stops when
   you close the app; it does not keep running after the app closes.
   If port 18443 is already taken, the bridge exits with a clear
   "could not bind" error (see `bridge.log` in
   `%LOCALAPPDATA%\FetchForge`, and the app status bar) and there is
   no silent fallback to another port. Close the other program using
   18443, then relaunch FetchForge.
   Note (F08, NOT VERIFIED): the Tauri autostart code has not yet
   been compiled or run on Windows; until the Windows build runs,
   treat this step as unproven and, if the extension says the bridge
   is not reachable, run `fetchforge-bridge.exe serve` manually from
   the install folder as a workaround.
4. Downloads go to `%USERPROFILE%\Downloads\<Category>\` by default.

If Windows SmartScreen warns, that is because v0.1.0 is unsigned
(see BUILD.md). Do not disable SmartScreen globally; choose "Run
anyway" only if you obtained the installer from a source you trust.

## Upgrade

Run the newer installer over the existing install. It replaces the
binaries and re-registers the native host. Download history, settings,
and in-progress `.part` files in `%LOCALAPPDATA%\FetchForge` are kept.
Interrupted downloads are marked Paused (if resumable) or Failed on
next launch, never silently lost.

## Uninstall

Use Windows Settings > Apps > FetchForge > Uninstall, or
`%LOCALAPPDATA%\Programs\FetchForge\Uninstall.exe`. The uninstaller
removes binaries, shortcuts, and registry keys, and then asks whether
to also delete user data in `%LOCALAPPDATA%\FetchForge`. Downloaded
files in Downloads are never deleted by the uninstaller.

## Portable build

Unzip `FetchForge-portable-x64.zip` anywhere and run `FetchForge.exe`.
No registry changes, no shortcuts, no native-host registration — so
browser extensions on a portable install must use the bridge-token
path (see EXTENSION_SETUP.md), not native messaging.

## Troubleshooting install

- "Bridge not reachable" in the extension: is FetchForge running?
  Check that no other program occupies port 18443, and that the token
  in the extension popup matches Settings in the app.
- WebView2 missing (old Windows 10): install the Evergreen WebView2
  Runtime from Microsoft, then relaunch.
- Previous install left a bridge running: only one bridge may bind
  127.0.0.1:18443; close the old copy first.
