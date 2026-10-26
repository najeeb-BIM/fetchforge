# Architecture

## Decision (Milestone 1, verified 2026-10-26)

Environment found on the dev VM: Ubuntu 24.04, Node 24, Python 3.12,
gcc, **no Rust, no NSIS, no Wine, no browser**. Rust stable was
installed (1.99.1 at test time; the 1.82 toolchain initially installed
could not build current crates.io deps that require edition 2024, so
the project requires a current stable Rust — see BUILD.md).

Stack chosen, after checking Windows 10 compatibility rather than
popularity:

- **Rust + Tauri 2** for the desktop app. Tauri 2 / WebView2 supports
  Windows 10 1803+ (baseline here is 10 22H2), and the app is a single
  small binary with no Electron runtime to ship. No Windows-11-only
  APIs are used for essential features.
- **Engine crate with no UI dependency** (`crates/engine`). This is
  what makes the project testable on Linux at all: the same code that
  runs in the Tauri app also runs in the bridge, and its unit +
  loopback tests pass here. The Tauri shell is a thin command layer.
- **Vanilla HTML/CSS/JS frontend** (no npm build step). One less build
  fragility on Windows, and the same files serve both Tauri and the
  bridge preview mode.
- **reqwest (blocking, rustls)** — rustls avoids an OpenSSL dependency
  on Windows. Blocking + threads keeps the engine simple and avoids a
  tokio runtime in the download path.
- **NSIS 3** per-user installer, no admin. The Tauri bundle config also
  targets NSIS, but `installer/fetchforge.nsi` is the canonical
  installer because it also registers the native-messaging hosts and
  ships the bridge binary — things a plain Tauri bundle would not do.
- **Electron was evaluated and rejected**: larger download, a second
  runtime to keep patched, and no compatibility advantage for this
  feature set on Windows 10 22H2+.

## Module map

```
crates/engine/
  validate.rs   only http/https, no host, no embedded credentials
  filename.rs   Windows sanitising, Content-Disposition, unique paths
  category.rs   extension -> Video/Audio/Documents/Images/Archives/Software/Other
  download.rs   probe, single-stream resume, segmented, retry, limiter
  media.rs      HLS (clear only), DASH (refuses DRM/unsupported shapes)
  state.rs      JSON store, atomic save, recover_interrupted()
crates/bridge/
  main.rs       loopback HTTP API, token auth, origin check, native host,
                queue gate (max_simultaneous), worker threads
apps/desktop/
  src/          index.html, styles.css, app.js (Tauri or bridge mode)
  src-tauri/    Tauri commands -> engine (built on Windows only)
extensions/
  chromium/     Manifest V3 (Chrome, Edge, other Chromium)
  firefox/      Manifest V2 (Firefox 109+), same background/popup source
installer/      fetchforge.nsi, native-host manifests (paths patched at build)
scripts/        build-windows.ps1
.github/workflows/windows-build.yml
tests/          server.py (loopback Range server), e2e.sh, pause_resume.sh
```

## Data flow

Browser extension -> (1) native messaging to `fetchforge-bridge
native-host`, or (2) `POST http://127.0.0.1:18443/downloads` with
`X-FetchForge-Token` -> bridge probes the URL, records a Queued job,
spawns a worker (gated by max_simultaneous) -> engine downloads to
`<Downloads>/<Category>/<name>.part` -> atomic rename -> record
Completed. The desktop app reads the same `downloads.json` store.

## Known design limits (v0.1.0)

- Segmented downloads are fresh-download only. A paused segmented
  download restarts cleanly (its part file is discarded); a paused
  single-stream download resumes via Range. See FEATURE_MATRIX.md.
- Segmented progress is reported at segment completion, not per chunk.
- No scheduler yet (no timed start/stop). Queue priority is FIFO.
- DASH segment assembly is not implemented; it refuses instead of
  producing a corrupt file. HLS clear playlists work in the engine but
  are not yet exposed through the bridge/UI.
