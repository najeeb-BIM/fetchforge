# FetchForge — a local download manager for Windows 10 / 11

Original implementation, inspired by the practical features of IDM. No
browsing history, download history, or user data leaves the machine.

## What is in v0.1.0

- **Download engine (Rust)** — HTTP/HTTPS, redirects, Range resume,
  segmented downloads for large files, retries with backoff, bandwidth
  limiting, safe Windows filenames, atomic `.part` → final rename,
  category folders, persistent JSON state with crash recovery.
- **Desktop app shell (Tauri 2 + vanilla HTML/JS)** — dashboard with
  active / queued / completed / failed, progress, pause / resume /
  cancel, search and filtering. The Tauri shell shares the same engine
  as the bridge.
- **Browser extensions (Chrome / Edge / Chromium, Firefox)** — context
  menu "Download with FetchForge", popup with paste-a-link and bridge
  token setup. Native messaging first, loopback bridge second, browser
  downloader as fallback.
- **Localhost bridge** — loopback-only HTTP API on 127.0.0.1:18443 with
  a per-install token, plus a native-messaging host mode.
- **Installer** — per-user NSIS installer (no admin), Start Menu and
  desktop shortcuts, native-host registration, clean uninstall with an
  explicit choice about user data. Portable zip too.

## Quick start (developer, any OS for engine/bridge)

```bash
cargo test -p fetchforge-engine
cargo build -p fetchforge-bridge
./target/debug/fetchforge-bridge serve --data-dir /tmp/ff-data
# token is printed in the log and stored in <data-dir>/bridge-token.txt
bash tests/e2e.sh            # full loopback proof (needs python3)
bash tests/pause_resume.sh   # pause/resume proof
```

## Windows build (the real installer)

On Windows 10 22H2+ / Windows 11 x64:

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

Or push to GitHub and let `.github/workflows/windows-build.yml` build it
on `windows-2022`. A Windows binary/installer cannot be produced or
verified on Linux — see `docs/TEST_REPORT.md` for exactly what was and
was not tested here.

## Docs

- `docs/ARCHITECTURE.md` — stack choice, module map, data flow
- `docs/BUILD.md` — reproducible builds, versions, signing status
- `docs/INSTALL.md` — install / upgrade / uninstall
- `docs/EXTENSION_SETUP.md` — extension install + troubleshooting
- `docs/SECURITY.md` — threat model and controls
- `docs/FEATURE_MATRIX.md` — implemented vs not-yet, honestly
- `docs/TEST_REPORT.md` — actual test results from this environment
