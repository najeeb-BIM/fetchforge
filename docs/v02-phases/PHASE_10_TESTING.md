# Phase 10 — Full-system testing + docs + GitHub landing plan

Date: 2026-11-21 (evening, PKT). Tree: `audit-round` at the Phase 10
commits below. Baseline before Phase 10: `cargo test --workspace`
103 passed, 0 failed; working tree clean; Phases 1–9 committed.

## 1. What was tested (suite by suite)

All suites were re-run on the final tree by hand (sequentially —
see the parallel-run note below), not taken on trust from earlier
phases.

### Rust unit / integration tests

| Suite | Result |
|---|---|
| `cargo build --workspace` | OK (re-run twice: baseline and after the fixes) |
| `cargo test --workspace` | **103 passed, 0 failed** (baseline and after the fixes; no test weakened or deleted) |

### Shell e2e suites (all `tests/*.sh`, run from the repo root)

| Suite | Result | Evidence |
|---|---|---|
| `e2e.sh` | PASS | 5 MiB segmented download, SHA-256 match; unauthenticated list 401; invalid scheme rejected; native-host ping |
| `stream_download.sh` | PASS | HLS 3 variants/default 720p, muxed 360p byte-identical, 1080p merged .mp4; DASH v720 default + merge, audio .m4a byte-identical; DASH auto-pick records v720; DRM manifest refused |
| `media_extraction.sh` | PASS | Drive file format 22 → 961,726-byte file; YouTube Big Buck Bunny 49 formats/default 234; 144p+audio merged to a real 14,877,817-byte .mp4 (1 video + 1 audio stream); auto-pick records 234 |
| `native_host_forwarding.sh` | PASS | `"via":"bridge"`; detached-bridge spawn when none listening; SHA-256 matches |
| `image_3d.sh` | PASS | 6/6: direct image, extension-less image (content-type classification), single-file GLB, glTF bundle with folder preserved, broken glTF fails honestly, unreachable dependency fails honestly naming the file |
| `pairing_e2e.sh` | PASS | Token never appears in the bridge log |
| `pause_resume.sh` | PASS | Slow download paused/resumed, SHA-256 match |
| `changing_resume.sh` | PASS | File changed mid-download → resumed from the start with the new file |
| `segmented_pause_and_xss_filename.sh` | PASS | Segmented pause clean; hostile filename sanitised (`_img src=x onerror=alert(1)_.txt`) |
| `symlink_part.sh` | PASS | Symlinked `.part` path handled, SHA-256 match |
| `token_hygiene.sh` | PASS after fix (see §2) | All 8 checks: content script never touches the token, token header only in background.js, popup never shows the token, no token in logs/URLs/cookies, Advanced toggle + red warning, Chrome ID `hkmnehbdedolcaaikibkadblmbfkkija` matches host manifest, Firefox gecko id matches, no private key/token file tracked |
| `two_process_store.sh` | PASS | Both processes' records on disk and after restart |
| `settings_per_field.sh` | PASS | Per-field settings round-trip |
| `bridge_api_failures.sh` | PASS | Token never appears in the bridge log |
| `build_manifest_check.sh` | PASS | Installer + host-manifest paths resolve |
| `build_no_bundle_check.sh` | PASS after fix (see §2) | `--no-bundle` in the build script; installer + portable artifacts in the workflow |
| `probe_error_surfaces.sh` (new) | FAIL before fix, PASS after (see §2) | Fixture 403s the probe UA, serves a plain page to yt-dlp |
| `no_range_server.sh` (new) | PASS | No-Range server: record says `resumable=false`, bytes match (SHA-256) |

### Audit suites (`tests/audit/*.sh`)

| Suite | Result |
|---|---|
| `audit_concurrency_bridge.sh` | PASS (exit 0): 200-request flood all OK, CRLF/NUL/colon filenames sanitised, CORS preflight 403, pairing race exactly one success, 200-input fuzz leaves the bridge healthy |
| `audit_hostile.sh` | PASS (exit 0): redirect-loop/file/ftp fail honestly, chunked + reset-mid fail honestly, huge-length fails honestly, traversal + `CON` filenames sanitised; see the redirect-private observation in §5 |
| `audit_scale_state.sh` | PASS (exit 0) |
| `audit_state_concurrency.sh` | PASS (exit 0) |

### Static verification (all re-run in Phase 10)

- `node --check`: all 6 extension JS files OK; both `index.html`
  script blocks OK.
- Both manifests valid JSON, version 0.2.0.
- Chromium `key` byte-identical to the GitHub-main sync point
  (commit `41954a0`) and to the local GitHub-main fetch copy
  (`~/workspace/tailscale-ssh/ff-extension/manifest.json`) — the
  stable extension ID `hkmnehbdedolcaaikibkadblmbfkkija` is preserved.
- No `const isTauri` in any source file (one hit only, in the
  historical doc `audit-2026-11-21/FIXES.md`, which quotes the old
  buggy line).
- No example media URLs in production Rust: every `youtube.com` /
  `youtu.be` URL sits inside `#[cfg(test)]` fixtures; the single
  production host check is `drive.google.com` (the Drive fallback,
  `extract.rs` `drive_file_id` / `drive_direct_url`).

## 2. What was fixed (with the evidence each fix was needed)

### Fix 1 — the probe-failure branch swallowed yt-dlp's real error

**Evidence (reproduced before fixing).** New test
`tests/probe_error_surfaces.sh`: a fixture server 403s the bridge
probe (UA `FetchForge/0.2`) and serves a plain HTML page to yt-dlp.
Pre-fix the Failed record carried the probe error:
`Access denied by the server (401/403). The link may need sign-in
or may have expired.` — which says nothing about why the page cannot
be downloaded. The test failed, correctly, with
"FAIL: record carries the probe error, not yt-dlp's error".

**Fix** (commits `34076d2`, bridge + Tauri, mirrored): in the
probe-`Err` branch, when yt-dlp is installed and extraction is tried
and fails, the extraction error replaces the probe error in the
Failed record (`Err(mut e)` + `e = extract_err`). Post-fix the same
test passes: the record carries yt-dlp's error —
`This page is not a supported video or media page. FetchForge
downloads video pages such as YouTube, Vimeo and public Google
Drive files, plus direct file links.` (yt-dlp's "Unsupported URL"
through `friendly_media_error`). In the real YouTube bot-gate case
this is the difference between a bare 403 and yt-dlp's "Sign in to
confirm you're not a bot" message. The "only if yt-dlp is already
installed" guard is unchanged: a failed probe on a dead direct link
still never triggers a first-use yt-dlp download.

The fix was proven to discriminate by stash: with the bridge fix
stashed the test FAILs (probe error recorded); with it applied the
test PASSes (yt-dlp error recorded).

### Fix 2 — Media Detector 3D view vs Phase 7's "3D" category (confirmed, no code change needed)

The 3D view filter (`index.html` line 212) matches
`d.category === "3D"`, and Phase 7 records category "3D" (proven by
`image_3d.sh`: `"category":"3D"` for robot.glb and model.gltf). It
matches. Only a stale Phase 9 comment was fixed ("3D files have no
category of their own yet (Phase 7 may add one)" → "3D downloads
carry category '3D' (Phase 7); the extension match stays as a
safety net").

### Fix 3 — two stale shell-suite checks (commit `b813ed7`)

- `tests/token_hygiene.sh` check 1 forbade `content_scripts` in the
  manifests, but Phase 4 deliberately added a content script (DOM
  detection). The check now asserts the real invariant instead:
  content script present, never touches the token (no storage API,
  no token header). Verified: `content.js` has no `chrome.storage` /
  `browser.storage` and no `X-FetchForge-Token`; it only sends media
  metadata via `runtime.sendMessage`.
- `tests/token_hygiene.sh` check 4 tripped on a Phase 4 comment
  line ("no cookies ... the token is used"). It now skips comment
  lines; on code lines no file has a token near location/href/
  cookie/search (verified by grep).
- `tests/build_no_bundle_check.sh` still expected the 0.1.0
  installer name; the workflow ships
  `FetchForge-Setup-0.2.0-x64.exe` since the Phase 6 version bump.
  Updated to 0.2.0. (Not a weakening: the artifact name is the
  thing being checked, and it changed with the version.)

Both suites pass fully after the updates (`token_hygiene.sh` 8/8,
`build_no_bundle_check.sh` PASS).

### New test — no-Range servers (commit `52b0212`)

§13 gap found by inspection: no suite covered a server without Range
support (`tests/server.py` always advertises `Accept-Ranges`).
New `tests/no_range_server.sh`: a fixture that ignores Range headers
(always 200, no `Accept-Ranges`) serves a 3 MiB file; the test
asserts the record says `resumable=false` and the bytes match
(SHA-256). Passes on first run.

## 3. Spec §13 matrix — coverage

| Matrix item | Covered by | Notes |
|---|---|---|
| Direct MP4 URLs | `e2e.sh`, `media_extraction.sh` (Drive file) | SHA-256 verified |
| HTML5 video players | `content.js` video detector (`video` selector + MutationObserver) | Code-reviewed only — no real browser here |
| HLS/DASH test streams | `stream_download.sh` (local fixtures) | Public HLS/DASH streams NOT tested on this VM (sandbox MITM vs rustls roots; fixture-verified instead) |
| Audio players | `content.js` audio detector; DASH audio-only `.m4a` byte-identical; yt-dlp best-audio merge | Detection code-reviewed only |
| Image galleries | `content.js` img/picture/srcset detectors; `image_3d.sh` | Detection code-reviewed only |
| Lazy-loaded images | Phase 7 unit tests (srcset/data-src); `image_3d.sh` extension-less case | ✓ |
| WebGL 3D viewers | `content.js` `model-viewer` + model-link detectors | DOM detection only — no real browser here |
| Multiple simultaneous downloads | `audit_concurrency_bridge.sh` (200-request flood); `settings_per_field.sh` (max_simultaneous) | ✓ |
| Interrupted / resumed downloads | `pause_resume.sh`, `changing_resume.sh`, `segmented_pause_and_xss_filename.sh` | ✓ |
| Servers without Range support | `no_range_server.sh` (new) | ✓ |
| Separate audio/video tracks | HLS 1080p merge, DASH v720 merge, yt-dlp 160+bestaudio merge (real 14.8 MB mp4, 1 video + 1 audio stream) | ✓ |
| Detected media downloadable before success is shown | Detector Download buttons go through the add path, which probes/extracts and records Failed honestly (proven by `probe_error_surfaces.sh`); the panel never claims success before the add path verifies | ✓ |

## 4. Docs updated

- `docs/EXTENSION_SETUP.md`: added the v0.2.0 detection section
  (content script, overlay button, badge, popup media list, Media
  Detector, auto-picked quality for extension downloads); "not
  provided in v0.1.0" → v0.2.0 (signed AMO listing); quality-picker
  note in "What it does".
- `docs/ARCHITECTURE.md`: module map gains `image.rs`, `threed.rs`,
  the "3D" category, the detected-media store, the content script,
  and the IDM-style tabs; Firefox corrected Manifest V2 → V3;
  data flow gains image/3D routing and the detected-media flow;
  known limits gain the no-real-browser-verification note.
- `docs/FEATURE_MATRIX.md`: status key now cites this report;
  new rows for IDM-style tabs, batch download, on-page detection,
  overlay button, badge, popup media list, Images, 3D models;
  Settings row now says the panel was omitted (no working backend);
  Non-Range row now cites `no_range_server.sh`.
- `README.md`: new Images + 3D bullet; desktop bullet gains tabs /
  Media Detector / batch download; extensions bullet gains
  detection, overlay button, popup media list.
- `docs/TEST_REPORT.md`: v0.2.0 addendum pointer at the top; the
  v0.1.0 record below it is unmodified.
- `docs/v02-phases/PHASE_7_IMAGES_3D.md` and
  `PHASE_9_UI_PANELS.md`: written from the committed diffs and the
  test evidence re-run here (the workers' final reports were not
  filed in the repo — both reports say so).

## 5. Observations (pre-existing, NOT fixed — out of scope)

- `audit_hostile.sh` redirect-private case: a redirect to a
  loopback-private URL completes (15-byte file). Pre-existing
  v0.1.0 behaviour; the audit suite exits 0 and treats results as
  informational. Changing redirect policy is a design decision
  (it interacts with the Drive fallback and legitimate redirects),
  not a Phase 10 targeted fix. Noted here for the user.
- The shell suites share ports (e.g. 8766) and `/tmp` dirs and are
  NOT safe to run in parallel: Phase 10 proved this by accident —
  `image_3d.sh` run in parallel with `native_host_forwarding.sh`
  failed with a 404 because it hit the other suite's fixture
  server. Run suites sequentially.
- `media_extraction.sh` (YouTube) did NOT flap in this run — it
  passed first try. The YouTube bot-gate flapping reported in the
  Phase 7 run remains an environmental limitation: it comes and
  goes by IP/time, is not a code regression, and must not be
  "fixed" with cookies or browser sessions (spec §10 forbids
  session credentials).

## 6. What Phase 10 could NOT verify

- **Real-browser behaviour** (extension overlay button, badge,
  popup list, pairing, native messaging end-to-end): no browser on
  this VM. Code-reviewed + syntax-checked only; the 10-step
  real-browser test plan in the Phase 4 report is still open.
- **Windows Tauri compile, NSIS installer execution, ffmpeg
  first-use download on Windows, bundled yt-dlp staging**: GitHub
  Actions is the gate (the workflow only runs on push to `main`
  or manual dispatch — see `GITHUB_LANDING_PLAN.md`).
- **Public HLS/DASH streams end-to-end on this VM**: sandbox MITM
  vs rustls roots; fixture-verified instead.
- **Drive large-file virus-scan page**: no large Drive file tested.
- **First-use yt-dlp download through the Rust code on this VM**:
  same MITM limitation; unit test + curl + hash instead — TLS
  roots deliberately not weakened.

## 7. Landing plan

`docs/v02-phases/GITHUB_LANDING_PLAN.md` — the exact steps to land
`audit-round` on GitHub `main`, the options for the unrelated-history
problem, what each destroys or preserves, and the recommendation
(squash the v0.2.0 tree onto GitHub `main` via PR; do NOT force-push
without the user's explicit approval). The force-push decision
belongs to the user and was NOT made here.

## Phase 10 commits

- `34076d2` Phase 10: surface yt-dlp error when the probe fails (bridge + Tauri)
- `b813ed7` Phase 10: update stale shell-suite checks
- `52b0212` Phase 10: add no-Range-server test (§13 matrix gap)
- docs + reports + landing plan: committed together with this report
  (docs/EXTENSION_SETUP.md, ARCHITECTURE.md, FEATURE_MATRIX.md,
  README.md, TEST_REPORT.md, docs/v02-phases/PHASE_7_IMAGES_3D.md,
  PHASE_9_UI_PANELS.md, PHASE_10_TESTING.md, GITHUB_LANDING_PLAN.md)
