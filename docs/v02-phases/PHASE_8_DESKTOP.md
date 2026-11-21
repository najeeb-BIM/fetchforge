# Phase 8 (desktop half) — Native-messaging survival + /media/detected

Date: 2026-11-21. Implemented by the desktop worker; verified independently
by the coordinator (`tests/native_host_forwarding.sh` re-run below).
(Phase 8's extension half — popup media list — is in PHASE_8_EXTENSION.md.)

## What was implemented
- **Native-host forwarding (the download-survival fix):** the native host
  now forwards `addDownload` to the long-running bridge over loopback HTTP
  (127.0.0.1:18443, token re-read from `bridge-token.txt`) instead of
  downloading in-process. If no bridge listens, it spawns a detached bridge
  (CREATE_NO_WINDOW + DETACHED_PROCESS on Windows), waits ~5 s for /health,
  then forwards. Last resort is in-process `add_download` with
  `"via": "in-process"` in the reply — refusing outright would hand a
  YouTube URL to the browser's own downloader, saving a useless `.html`
  file; the in-process record is recoverable as Paused on next start.
  This closes the open question from 2026-11-21 (Chrome killing the host
  after the sendNativeMessage reply can no longer stall a download that
  the bridge owns).
- **`/media/detected` (bridge only):** `POST /media/detected {items:[…]}`
  → `{stored: n}`; `GET /media/detected` → `{items:[…]}`, both behind the
  existing token auth. `DetectedItem` carries only confirmed metadata (all
  fields except `url` optional, skipped when unknown). Dedupe by URL
  (replace + move to end), cap 200 dropping oldest, `detected_unix`
  stamped when absent, persisted to `detected.json` (tmp+rename,
  best-effort; corrupt file starts empty with a warning). No Tauri invoke
  yet and no Phase 9 panel yet — this is the data source Phase 9 reads.

## Files modified and created
- Modified: `crates/bridge/src/main.rs` only.
- Created: `tests/native_host_forwarding.sh`.
- Commits: `6cce113` (forwarding), `da65798` (test files),
  `b4ae551` (`/media/detected`).

## Dependencies added
None.

## Tests run (with results)
- `tests/native_host_forwarding.sh` — COORDINATOR RE-RAN, exit 0:
  ping→pong; bridge running → `"via":"bridge"`, file completed, SHA-256
  matches; bridge stopped → host spawns detached bridge, `/health`
  answers, second file completed, SHA-256 matches.
- 3 new bridge unit tests (dedupe/persistence, 200-item cap,
  unknown-field omission + corrupt-file tolerance) — in the 94-test
  workspace run (coordinator re-ran, 0 failed).
- Live HTTP check (worker): no token → 401; POST 2 → `{"stored":2}`; GET
  returns both with unknown fields omitted; duplicate POST replaces with
  new title and moves to end; `detected.json` on disk matches.

## Remaining limitations
- Real-browser native messaging end-to-end for the *forwarding* fix is NOT
  verified — only the length-prefixed protocol test on this VM (the user's
  2026-11-21 real-browser check exercised the old in-process host).
- NSIS installer execution on Windows is NOT verified (bundled yt-dlp
  staging, native-messaging manifest registration) — GitHub Actions is
  the gate.
- Tauri crate compile on Windows is NOT verified — GitHub Actions is the
  gate (the worker did a stub-tauri type check of main.rs against the real
  engine: 0 errors).
