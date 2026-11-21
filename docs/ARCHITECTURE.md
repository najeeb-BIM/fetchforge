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
  tokio runtime in the download path. The `system-proxy` feature is
  enabled (it is a reqwest default, dropped by `default-features =
  false` until v0.2.0), so downloads honour the OS proxy settings;
  TLS roots stay the pinned Mozilla set (webpki), so a TLS-intercepting
  proxy is refused rather than trusted — see `docs/SECURITY.md`.
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
  category.rs   extension -> Video/Audio/Documents/Images/3D/Archives/Software/Other
  download.rs   probe, single-stream resume, segmented, retry, limiter
  media.rs      HLS + DASH (clear only): manifest parsing, variant /
                representation listing, segment assembly, resume;
                refuses DRM/live/multi-period honestly
  extract.rs    yt-dlp wrapper: format listing, quality choice, media
                download (+ ffmpeg merge); routes .m3u8/.mpd URLs to
                media.rs (never to yt-dlp)
  image.rs      image classification (extension + image/* content
                type), srcset full-resolution pick; original format
                only, never upscaled
  threed.rs     3D models (GLB/GLTF/OBJ/FBX/STL/USDZ); glTF bundles
                resolve + download dependencies, folder preserved
  ffmpeg.rs     ffmpeg lookup, pinned first-use download (SHA-256),
                stream-copy merge of separate video+audio tracks
                (extract.rs keeps the yt-dlp provisioning side)
  state.rs      JSON store, atomic save, recover_interrupted()
crates/bridge/
  main.rs       loopback HTTP API, token auth, origin check, native host
                (forwards addDownload to the bridge, spawns one if
                needed), queue gate (max_simultaneous), worker threads,
                detected-media store (POST/GET /media/detected,
                detected.json, capped at 200 items)
apps/desktop/
  src/          index.html (inlined script is the source of truth),
                styles.css (Tauri or bridge mode); IDM-style tabs
                (Dashboard, Media Detector, Video/Images/3D views,
                Queue, History) + batch download
  src-tauri/    Tauri commands -> engine (built on Windows only)
extensions/
  chromium/     Manifest V3 (Chrome, Edge, other Chromium)
  firefox/      Manifest V3 (Firefox 109+, background.scripts), same
                background/popup source as Chromium
  content.js    (both) DOM media detection: video/audio via query +
                MutationObserver, image srcset, 3D model elements;
                overlay download button on videos, per-tab badge,
                reports to the background script
installer/      fetchforge.nsi, native-host manifests (paths patched at build)
scripts/        build-windows.ps1
.github/workflows/windows-build.yml
tests/          server.py (loopback Range server), e2e.sh, pause_resume.sh
```

## Data flow

Browser extension -> (1) native messaging to `fetchforge-bridge
native-host`, which forwards the request to the long-running bridge
over loopback HTTP (spawning a detached bridge if none is listening),
or (2) `POST http://127.0.0.1:18443/downloads` with
`X-FetchForge-Token` -> bridge probes the URL, records a Queued job,
spawns a worker (gated by max_simultaneous) -> engine downloads to
`<Downloads>/<Category>/<name>.part` -> atomic rename -> record
Completed. The desktop app reads the same `downloads.json` store.
A probe returning text/html (a video page) goes through media
extraction instead: yt-dlp lists formats, the user picks a quality
(or the extension path takes the recorded default), yt-dlp downloads
it, merging separate streams with ffmpeg when needed. Image and 3D
URLs are classified in the same add path (by extension and, for
extension-less image URLs, by probed image/* content type) and take
the direct path with category Images / 3D; a .gltf URL is planned
first (its .bin + texture dependencies are resolved and probed)
and downloaded as a bundle with its folder structure preserved.
The extension's content script reports detected media to the
background script, which POSTs it to /media/detected; the desktop
app's Media Detector panel lists it (GET /media/detected) and batch
download sends each selected item through the same add path.

## Known design limits (v0.2.0)

- Segmented downloads are fresh-download only. A paused segmented
  download restarts cleanly (its part file is discarded); a paused
  single-stream download resumes via Range. See FEATURE_MATRIX.md.
- Segmented progress is reported at segment completion, not per chunk.
- No scheduler yet (no timed start/stop). Queue priority is FIFO.
- DASH covers clear single-period manifests with SegmentTemplate
  (Number or SegmentTimeline) or SegmentList; multi-period, live
  (dynamic) and DRM-protected manifests are refused honestly. HLS
  covers clear master/media playlists; encrypted (EXT-X-KEY) and
  live playlists are refused (a live playlist is downloaded only as
  the finite snapshot listed at fetch time, never re-fetched).
- yt-dlp is pinned (2026.10.26) and bundled; a newer yt-dlp ships with
  the next app release. ffmpeg (Windows) downloads once on first merge
  (~114 MB); on Linux/macOS a system ffmpeg is required for merges.
- Live streams are refused; DRM content is refused, never bypassed.
- The extension's on-page behaviour (overlay button, badge, popup
  media list) is code-reviewed and syntax-checked, not yet verified
  in a real browser; neither is the Windows Tauri compile or the
  NSIS installer run. See docs/v02-phases/PHASE_10_TESTING.md.
