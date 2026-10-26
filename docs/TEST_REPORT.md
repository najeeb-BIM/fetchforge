# Test report — v0.1.0 (run 2026-10-26, verification round 2 same day)

Environment: Ubuntu 24.04 x64, Rust stable 1.99.1 (installed during
this build; initial 1.82 could not build current crates.io deps),
Node 24, Python 3.12. No Windows, no NSIS, no Wine, no browser.

## Verification round 3 (2026-10-26, after v0.1.0 acceptance)

- Git: v0.1.0 committed as `4b17a7b` on main, annotated tag
  `v0.1.0-engine`. Original backup kept:
  `~/workspace/backups/fetchforge-2026-10-26-preverify.tar.gz`
  (SHA-256 `f22723bcef810362d21025f2c57d61927dd2723d7aff8d87b2569ca84ea8a175`).
  Round-3 work is committed separately and does NOT move the tag.
- Windows workflow: `.github/workflows/windows-build.yml` now runs on
  `windows-latest`, runs `cargo test`, installs NSIS + Tauri CLI,
  builds the Tauri app, NSIS installer and portable zip via
  `scripts/build-windows.ps1`, prints SHA-256 of all four outputs
  (installer, portable zip, app exe, bridge exe) and uploads the
  installer + portable zip as artifacts. Validated with actionlint
  1.7.7: exit 0, no findings. The workflow itself has NOT run on
  GitHub yet — that needs the user's repo + Run workflow click.
  Non-developer guide: `docs/WINDOWS_BUILD_GUIDE.md`, exactly 10 lines
  (verified with `wc -l`). Note: the guide tells the user to unzip
  first and upload the files, not the zip itself, because GitHub
  Actions cannot run from a zip stored as a single file.
- Build-script bug fixed in round 3: the old script copied patched
  host manifests back over the source manifests in `installer/`,
  corrupting them with an absolute build path. Now it stages only
  under `installer/stage/`, NSIS reads from `stage/`, and makensis
  runs from inside `installer/` so paths resolve deterministically.
  Also: the script now fails explicitly if the Tauri CLI or app exe
  is missing, instead of warning and continuing.
- Filename tests (Linux, pure function): reserved CON/NUL/AUX/PRN and
  COM1-9/LPT1-9 with and without extension, trailing dot/space,
  colon/ADS, backslash, drive letters, `../` and `..\` traversal,
  empty/dot-only names, and a 500-char name truncated so a realistic
  total path stays under 260 — all pass (`cargo test filename`: 7 tests).
- Bridge Host tests (Linux): evil Host 403, suffix-trick Host
  (`127.0.0.1.evil.example`) 403, `localhost` Host 200,
  `127.0.0.1` Host 200 — all pass in `tests/bridge_api_failures.sh`.
- Segmented progress (Linux): reporter thread polls the shared
  counter every 100 ms; new test `segmented_progress` (2 MiB slow
  server, 4 segments) asserts an intermediate value strictly between
  0 and total and a final value equal to total — passes in ~5 s.
  Progress callback type changed to `Send + Sync` to allow this;
  bridge builds cleanly with the new type.
- Full suite after round 3: 12 unit + 5 failure + 1 HLS + 1 segmented
  progress = 19 tests, 0 failures; `cargo fmt --check` clean,
  `cargo clippy --all-targets` clean; e2e and pause/resume re-run
  and still hash-match.
- NOT built in round 3, per user order: scheduling, notifications,
  DASH, retry/remove/sort. Token UX was NOT implemented — design
  only, awaiting user approval (see final report).

## Verification round 2 (user's proof process, 2026-10-26)

Backup before changes: `~/workspace/backups/fetchforge-2026-10-26-preverify.tar.gz`
(76 KB, target/ excluded). No user data was deleted; all test data
went to fresh /tmp directories.

- Static checks: `cargo fmt --check` clean, `cargo clippy --all-targets`
  clean (0 warnings after fixing 4 clippy warnings + 2 compiler
  warnings), `node --check` passes for app.js, background.js, popup.js
  (both browsers), both manifests parse as Manifest V3 and reference
  files that exist (icons now present — see Bugs).
- Unit + integration: `cargo test` now 16 tests, 0 failures:
  10 unit (incl. empty/long/emoji/non-English filenames, duplicate
  unique paths, empty/very-long/non-English URLs) + 5 failure tests
  + 1 HLS test.
- Failure cases (all fail with a clear message, no crash/hang):
  401 / 403 / 429 / 500 / 404 via probe, closed-port connection
  ("Could not connect"), empty URL, destination-is-a-file
  ("Cannot create download folder"), zero segments + zero bandwidth
  (degrades to single-stream/unlimited, downloads correctly),
  bridge bad JSON / wrong-type / missing URL -> 400, wrong/no token
  -> 401, bad Origin -> 403, corrupt settings JSON -> defaults used,
  bridge started and answered /settings normally.
- Clean-state run: `rm -rf target`, full `cargo test` + `cargo build`
  from scratch, then e2e / pause-resume / API-failure scripts on fresh
  /tmp data dirs — all passed (see below).
- Duplicate-add (idempotency): see Bugs — this test FOUND a real
  corruption bug, which was fixed and re-verified (same ID returned,
  one completed file, hash matches).
- Segmented pause: 2 MiB, 4 segments, slow server, paused mid-way:
  only `slow.bin.part` existed while paused (no final file), resumed,
  completed, SHA-256 `0611f89f5461d1feee6f971ce761750b19f344368910c187fad8c1a64ea6432b`
  identical to source. Note: segmented progress shows 0 until segments
  finish — a known UI limit, not corruption.
- Hostile filename: `<img src=x onerror=alert(1)>.txt` was sanitised
  by the engine to `_img src=x onerror=alert(1)_.txt` before the UI,
  and the UI now uses textContent only (see Bugs).
- Full-disk: NOT simulated by filling a disk (would risk the VM).
  The write-failure path is covered by the destination-is-a-file test
  and by the atomic `.part` + rename design; a genuinely full disk
  will surface as a write error, not silent success.
- Secrets: grep found no hardcoded keys/tokens/passwords, no `eval`,
  no `innerHTML` in code (only a comment describing the old bug).
  Bridge log verified not to contain the token.

## Passed

- `cargo test` (workspace): 6 engine unit tests + 1 HLS integration
  test, 0 failures.
  - URL validation: http/https accepted; file/javascript/ftp and
    embedded credentials rejected.
  - Filename sanitising: traversal, forbidden chars, reserved names,
    trailing dots; Content-Disposition incl. RFC 5987.
  - Categories, JSON store round-trip + interrupted-download recovery.
  - HLS: clear 2-segment playlist concatenated byte-exact.
- `tests/e2e.sh` (bridge + loopback server):
  - `/health` returns app/version.
  - `GET /downloads` without token: HTTP 401.
  - 5 MiB download, 4 segments, Range server: completed,
    SHA-256 of downloaded file =
    `5a3cd5a86adfe8e88b10a2e04398cfcfe2b2e9fd9e9ecf901ee7cdf771ad97d4`,
    identical to the source file.
  - `file:///etc/passwd` rejected with "Unsupported URL scheme: file".
  - Native-messaging framing: `ping` -> `pong` with version.
- `tests/pause_resume.sh` (single-stream, slow server):
  - Download paused at 245,760 / 524,288 bytes, status `paused`,
    resumed, completed, SHA-256 identical to source
    (`0dd0efde79ff06ae918e6cf05feeefba040103e4409979a7ccc8f374b52dfe99`).
- Syntax checks: `node --check` passes for `app.js`, extension
  `background.js`, `popup.js`; `python -m json.tool` passes for both
  extension manifests and `tauri.conf.json`; `rustfmt` parses all
  Rust files (formatting applied).
- Build errors fixed during development (not hidden): reqwest
  blocking `chunk()` does not exist (switched to `Read`), raw
  pointers are not `Send` (switched to scoped threads), edition-2024
  deps need current stable Rust.

## Bugs found in verification round 2 (all fixed, full suite re-run)

1. **Duplicate simultaneous downloads corrupted each other** (critical).
   Two POSTs for the same URL derived the same `.part` path; one record
   reported downloaded=total with "got 0 bytes". Root cause: `unique_path`
   checks only the final file, not an in-progress download. Fix: bridge
   and Tauri now return the existing Queued/Downloading job for the same
   URL + filename (check + insert under one lock). Re-test: same ID,
   one file, hash matches. Full suite re-run after the fix: 16/16 +
   e2e + pause/resume + API failures, all pass.
2. **Desktop UI used innerHTML for filenames/errors** (XSS). A filename
   containing HTML would have been parsed. Fix: rows built with
   textContent only; engine sanitising is defence in depth.
3. **Firefox Promise-style APIs would hang the old extension code.**
   `sendNativeMessage`/`sendMessage` with a Chrome-style callback are
   never called back under Firefox's Promise APIs, so a missing native
   host could hang instead of falling back. Fix: `callApi` branches on
   Firefox vs Chrome; popup uses the same branching. Not testable here
   without a browser — see manual test below.
4. **Missing icon files.** Both manifests and Tauri config referenced
   `icon128.png` / `icons/icon.png` that did not exist. Fix: generated
   128 px extension icons and a 512 px Tauri icon (simple teal "F").
5. **Firefox was Manifest V2** while the verification rule requires V3.
   Fix: Firefox manifest converted to MV3 (`action`, `host_permissions`,
   `background.scripts` — Firefox supports scripts in MV3, keeping
   109+ compatibility; the background code is restart-safe anyway).
6. **Compiler/clippy warnings.** Fixed: needless assignment, dead
   `data_dir` field, `suspicious_open_options`, `next_back`, unused
   media options, `&PathBuf` -> `&Path`, needless return. Clippy now clean.

## Partially tested

- Tauri desktop shell: Rust code parses (rustfmt) and mirrors the
  bridge's tested engine calls, but `cargo check` fails on Linux at
  the `gdk-sys` build script — the GTK/WebKit system libraries are
  absent on this VM. This is an environment limit, not a code error
  found in `main.rs`. It must be checked on Windows.
- Extension JS: syntax-checked only. No browser was installed, so
  context-menu, popup, native-messaging, and fallback flows were not
  exercised in a real browser.
- Bandwidth limiter, queue gate, retry/backoff: implemented and
  code-reviewed, not exercised by an automated test in this run.
- HLS via bridge/UI: engine-tested only; no bridge endpoint yet.

## Not tested (requires Windows)

- Windows 10 22H2 and Windows 11: app launch, WebView2, installer
  execution, Start Menu/desktop shortcuts, native-host registration,
  upgrade, uninstall, user-data choice.
- NSIS script execution (`makensis` not available here).
- Signed installer: no certificate, not signed, not attempted.
- Real browsers: Chrome, Edge, Firefox extension install and
  end-to-end browser-to-desktop download.
- DASH: deliberately refused; no positive DASH test exists.
- Scheduling, notifications, in-app retry button, remove/sort:
  not implemented (see FEATURE_MATRIX.md).

## Compatibility matrix

| Target | Status |
|---|---|
| Engine + bridge on Linux (dev) | Tested, passing |
| Windows 10 22H2 x64 | Targeted, NOT tested |
| Windows 11 x64 | Targeted, NOT tested |
| Chrome / Edge / Chromium | Source complete, NOT tested in browser |
| Firefox | Source complete, NOT tested in browser |
| NSIS installer | Script complete, NOT executed |

## Next reproducible action (to complete acceptance)

On a Windows 10 22H2+ or Windows 11 x64 machine (or via the GitHub
Actions workflow):

```powershell
powershell -ExecutionPolicy Bypass -File scripts/build-windows.ps1
```

Then: install the produced exe, launch, add a download in the UI,
install the extension, right-click a link, confirm it lands in the
dashboard, pause/resume it, uninstall, and confirm the data-choice
prompt. Record those results in this file before calling the Windows
build "tested".
