# Fix round 7 — notes (2026-11-23)

Round 6 (app v0.2.0 + extension 0.2.5) was installed and live-tested
on 2026-11-23 against novahd.cc, nime.pro and atlantic.bz (all The
Odyssey, 2026). The re-test exposed nine bugs plus two further
findings from the same morning's live evidence. Every fix below is
grounded in that evidence, not guessed. Extension version 0.2.6.

Baseline before round 7: `cargo test --workspace` 152 passed,
0 failed. After round 7: 165 passed, 0 failed (see §12).

## 1. Fix 1 — the native host never worked (highest priority)

**Evidence.** Chrome launches the native host with NO arguments (the
host manifest's path is just `../fetchforge-bridge.exe`), but
`crates/bridge/src/main.rs` only entered native-host mode when an
argument equalled `"native-host"`. With no args it went to serve
mode, failed to bind 127.0.0.1:18443 (the bridge was already
running), and exited immediately without replying — so the
extension's `sendViaNative` always threw. All successful sends ever
went via the bridge HTTP path.

**Fix.** Mode selection in `main()`, in precedence order:
1. `print-token` (explicit user action; must work piped).
2. `native-host` arg (explicit native-messaging mode).
3. `serve` arg (explicit serve mode — checked BEFORE the stdin
   test, because both the desktop app and `spawn_detached_bridge`
   spawn the bridge with `serve` and a null stdin, and null stdin
   is not a terminal).
4. Piped stdin (not a terminal), no mode argument — this IS a
   Chrome native-host launch.
5. Anything else (a terminal, a plain double-click): serve.

Detection uses `std::io::IsTerminal` (`stdin().is_terminal()`,
stable since Rust 1.70, no new dependency).

**Tests.** `crates/bridge/tests/native_host_mode.rs` (new
integration-test file — the behaviour under test IS process
startup, so it spawns the real binary): no-args + piped stdin
answers a framed `{"type":"ping"}` with a framed `{"type":"pong"}`;
the explicit `native-host` arg still works; `serve` with piped
stdin still serves and answers `/health`.

## 2. Fix 2 — every click stomped in-progress downloads to Paused

**Evidence.** `App::new` ran `store.recover_interrupted()` + save
BEFORE `main()` decided the mode — so every native-host launch
(every extension button click!) marked ALL Downloading records
Paused ("Interrupted by app close or crash") on disk. Proven live:
the nime DASH download flipped Downloading→Paused between 08:21
and 08:26 with NO process restart, while the download itself kept
progressing (967 MB → 1.17 GB).

**Fix.** Recovery moved out of `App::new` into
`App::recover_interrupted()`, called in `main()` only after the TCP
bind succeeds — only the process that actually becomes the server
recovers. A short-lived native-host probe never writes
`downloads.json`. The desktop app's own recovery (a real startup)
is untouched.

**Test.** Same integration file: a Downloading record on disk is
untouched by a native-host probe (ping/pong, exit), then recovered
to Paused by a real `serve` start over the same data dir.

## 3. Fix 3 — reload_from_disk must not clobber active downloads

Round 6 added `reload_from_disk()` before duplicate checks (8
sites). It propagated the stomped Paused status from disk into the
live bridge's memory, so a progressing download showed Paused
forever.

**Fix** (`crates/engine/src/state.rs`). Merge rule: a record this
process is actively downloading (in-memory status Downloading)
keeps its in-memory status and progress counters over whatever
disk says; terminal records (Completed/Failed/Cancelled) never
un-complete; a Paused in-memory record still refreshes from disk
by timestamp (resume transfers ownership via the Paused state).
Downloading records are also excluded from the prune step.
`save()`'s timestamp merge is unchanged — it is fine once Fix 2
removes the stomping writer and Fix 3 stops memory infection.

**Tests.** Two unit tests in `state.rs`: in-memory Downloading +
newer Paused on disk stays Downloading (counters intact),
Completed + newer Paused stays Completed, Paused + newer Failed
takes Failed; a Downloading record absent from disk is never
pruned.

## 4. Fix 4 — the "Sent" lie

**Evidence.** `sendCurrentDownload` showed "Sent to FetchForge"
whenever `resp.ok`, even when `resp.viaBrowser` was true (the
browser-download fallback fired because both app paths failed).
Files landed in Chrome's Downloads folder while the button claimed
Sent. Worse: `background.js` DROPPED `viaBrowser` in both message
handlers' replies, so the content script could not have known even
if it had checked.

**Fix.** Both handlers (`downloadMedia`, `sendUrl`) now pass
`viaBrowser` through; the button shows "Downloaded by browser"
with a tooltip explaining the app wasn't reached, same 5s revert.
"Already downloaded" and failure paths are unchanged.

**Tests.** Harness `background.test.js` evaluates the whole
`background.js` with a stubbed `chrome` API (no token, no native
host) and asserts the reply carries `viaBrowser: true` and the
browser download fired; `content.test.js` asserts the label,
tooltip, and the unchanged ok/duplicate/failure labels.

## 5. Fix 5 — title cleaning

**Evidence.** A spam notification overlay rewrote a tab title to
"(1) New Message!" and the file was literally saved as
"(1) New Message!.mp4"; atlantic produced "Watching The Odyssey —
Atlantic.mp4" ("Watch " was stripped, "Watching " was not).

**Fix** (both layers, same rules). JS `pageTitleStem` and engine
`stem_for_title` now strip: a leading "(N)" notification count
(1–3 digits only, looped for stacked badges — a year like "(2026)"
survives) and a leading "Watching " prefix (checked before
"Watch "). Existing strips, Windows sanitisation and the 100-char
stem cap are unchanged. The engine deliberately does NOT gain the
trailing " | site" strip (that would change YouTube filename
behaviour — out of scope).

**Tests.** `stem_for_title_strips_notification_counts_and_watch_prefixes`
(engine) and harness cases for both new strips plus the existing
ones.

## 6. Fix 6 — novahd clip guard

**Evidence.** The extension verdict still sent the 15-second ad
clip (`novahd.cc/assets/p/*.mp4`) when the movie playlist was
invisible — it completed as a 3.1 MB file again on 2026-11-23.
Round 6's suppression only helps when a playlist IS seen, and the
planner's <60s rule only covers page-URL fallback, not direct clip
URLs.

**Fix.** The election verdict now carries `pageVideoDuration` —
the longest video duration seen in ANY frame, playing or not (a
paused film still proves a long-form page). On a one-click send of
a direct mp4-family file where a video over 10 minutes is playing
somewhere on the page, the content script Range-probes the first
64 KB (5s timeout, button shows "Checking…") and parses its
moov/mvhd duration (mirroring the engine's `mp4_duration_secs`).
A known-short clip (<60s) is refused on the button ("Only a short
clip was found" + duration tooltip). Any probe failure or
inconclusive result FAILS OPEN (the send proceeds) — the button
must never break a legitimate download. Same-origin only in
practice (cross-origin fetches fail CORS and return null); no host
permissions were added. WebM/MKV duration parsing (EBML) is out of
scope — those files are never probed.

**Tests.** Harness: synthetic mp4 prefixes (v0/v1 mvhd, no-moov,
truncated, garbage, box-skipping); refusal with no send on a 15s
clip; send on a long file; fail-open on fetch throw and on 404;
no probe without a long video, for non-mp4 files, or for
manifests; verdict `pageVideoDuration` across frames (incl. a
paused long video) and with a playlist verdict.

## 7. Fix 7 — DASH in the quality popup

**Evidence.** The popup only triggered for HLS masters (≥2
variants) or viaPage URLs. Nime's VidLink source serves DASH
(`index_web.mpd`), so it went one-click with no choice.

**Finding during implementation.** The engine chain already exists
end to end: `list_formats` routes `.mpd` URLs to
`list_dash_formats`, representation ids flow as `StreamFormat.id`,
and `download_dash` honours `representation_id`. So this fix is
extension-routing only: `.mpd` verdicts now open the existing
formats popup (the bridge fetches the `.mpd` itself). Two more
rules: a response with fewer than 2 formats falls through to the
one-click send (a single representation is not a choice — same
rule as the HLS master path; this also applies to viaPage), and
the pick handler passes captured manifest text along
(`dl.manifestText || dl.masterText`) so a signed `.mpd` is never
re-fetched.

**Tests.** Harness: `.mpd` verdict routes to the formats popup
(not one-click), including with captured mpd text; HLS-master and
viaPage routing unchanged; single-format response falls through
to one-click; multi-format response shows rows with the filename
field (server title upgrades it); a row pick sends the format id
+ captured manifest text; a formats error falls back to
one-click.

## 8. Fix 8 — atlantic sequential fallback

**Evidence.** totallyacdn still timed out with 4 parallel workers
+ browser headers + HTTP/1.1 ("Segment 2 of 1730 could not be
downloaded after 1 try. (error decoding response body: operation
timed out)"). Hypothesis: the CDN throttles concurrent connections
from one client.

**Fix** (`crates/engine/src/media.rs`). `download_segments`
retries a parallel run ONCE with 1 worker when it fails a segment
on a timeout (`should_fallback_sequential`: segment error + looks
like a timeout; never for pause/cancel, stalls, or fast refusals).
The retry reuses the same segment dir (completed pieces are never
re-fetched) and the same fail-fast + stall rules, so it stays
bounded. If the retry also fails, the ORIGINAL error stands: the
sequential pass was a rescue attempt and must never rewrite the
failure's diagnosis. `download_segments_parallel` is parameterised
by worker count; the fallback covers HLS and DASH (both route
through `download_segments`).

**Tests.** A stub CDN that stalls its first 4 requests past the
client timeout: the parallel pass times out, the sequential retry
completes the download (hit count 4 + 8 proves both passes ran).
A stub that stalls every request: the record fails with the
original timeout error, quickly (hit count 4 + 1 proves the
fallback ran exactly once). Plus a unit test for the gate.

## 9. Fix 9 — HLS hook capture investigation

**Evidence.** The round-6 MAIN-world fetch/XHR hook captured NO
master playlist text on any of the three sites (no `manifests/`
sidecars, no popup).

**Findings.**
1. **Proxy-wrapped playlist URLs (the main cause — FIXED).** The
   hook's URL pre-filter required `.m3u8`/`.mpd` followed by
   `?`/`#`/end. Proxy-wrapped URLs
   (`frappe.hybrows.workers.dev/?url=https://cdn/...m3u8&...`)
   carry the extension mid-URL, followed by `&` — the old test
   missed them entirely. The content-type fallback only catches
   `mpegurl`/`dash+xml`, but many CDNs serve playlists as
   `text/plain` or `octet-stream`. **Fix:** match `.m3u8`/`.mpd`
   ANYWHERE in the URL (in both `isPlaylistUrl` and
   `looksLikePlaylist`); the text-shape gate still guards against
   false positives. A Content-Length guard was added alongside:
   a body known to exceed 256 KB is never read, so a segment URL
   that happens to contain ".m3u8" cannot trigger a multi-MB text
   read.
2. **Strict page CSP.** A `script-src` without `'unsafe-inline'`
   blocks the injected inline script, so the hook never runs.
   Best-effort by design; the timing-log fallback remains.
3. **document_start race.** The player's first manifest fetch can
   happen before the MutationObserver retry installs the hook.
   Mitigated, not eliminated (injection is attempted immediately
   and retried on DOM mutation + DOMContentLoaded).
4. **Worker-loaded playlists are invisible by design** (workers
   have their own fetch). The timing-log fallback covers those.
5. **XHR with `responseType` arraybuffer/blob** yields empty
   `responseText`; hls.js uses text for manifests, so this is
   unlikely to matter.
6. **NOT implemented:** `chrome.scripting.executeScript` with
   world "MAIN" would bypass CSP, but it needs the "scripting"
   permission + host_permissions (currently only 127.0.0.1:18443)
   — a big permission expansion for a best-effort hook. Documented
   as a future option, not built.

The detection pipeline was NOT rewritten.

## 10. Finding A — DASH resume re-downloaded completed phases

**Evidence.** The nime DASH download (video 1768 segments + audio
349) failed at audio segment 121 with HTTP 429 after the video
phase had FULLY completed (2,299,009,402 B `.part` on disk). On
Resume, the engine started the VIDEO phase over from scratch into
a fresh `.part.seg` dir (209/1768 segments re-fetched in the first
minute, `downloaded_bytes` reset), instead of resuming only the
audio phase.

**Root cause (code).** `download_segments_parallel` deleted the
segment dir after a successful assembly. On resume, the video
phase re-entered with no seg dir and re-fetched everything; the
complete `.part` file was never consulted. Same flaw affected HLS
variant + pair downloads.

**Fix.** A phase that assembles successfully KEEPS its segment
dir; the caller removes it only when the final file is produced
(`remove_seg_dir` after rename/merge in `download_hls_variant`,
`download_hls_pair`, `download_dash_with` — and on a between-phase
cancel). A resume re-enters the completed phase, finds all pieces
on disk, skips every fetch, and re-assembles locally (a disk copy,
no network) before resuming the failed phase. If the merge fails,
the pieces stay, so its resume also re-assembles locally instead
of re-downloading. Disk cost: pieces + assembled `.part` coexist
until the final file is produced (previously the pieces were freed
at assembly); "Clear cache" already sweeps orphaned segment dirs.

**Tests.** Segments level: a second run over the same part path
makes ZERO HTTP requests and returns the same bytes. DASH level
(real ffmpeg, skipped where absent): a manifest whose last audio
segment 404s on run 1 (video completes, audio fails) then becomes
servable — run 2 fetches ONLY that segment, merges, and cleans
up both segment dirs.

## 11. Finding B — progress percent over 100%

**Evidence.** The UI showed "1324% (2.2 GB / ?)" for the nime DASH
download: `downloaded_bytes` is cumulative across phases but
`estimated_total_bytes` was the CURRENT phase's estimate (~166 MB
audio estimate vs 2.3 GB downloaded).

**Fix (two layers).** Engine: progress reports now carry a
COMBINED estimate — completed phases contribute their exact bytes
(`progress_base`) and the current phase its measured estimate
(both the parallel and the legacy sequential path). Frontend
(`apps/desktop/src/index.html`): the percent computation is now a
named pure function `progressPercent`, capped at 100% as a safety
net for any estimate that still runs low.

**Tests.** Engine: a second-phase run with `progress_base` =
1,000,000 reports `downloaded` 1,000,400 and `estimated_total`
1,000,400, and no report ever falls below the completed phases.
JS harness (`progress-percent.test.js`, extracts the function from
the shipped file): the live nime numbers cap at 100, the combined
estimate gives 93%, normal cases unchanged.

## 12. Tests

- `cargo test --workspace`: 165 passed, 0 failed (baseline 152).
  New (13): Fix 1+2 integration tests (4), Fix 3 (2), Fix 5 engine
  (1), Fix 8 (3), Finding A (2), Finding B engine (1). Full suite
  runs in ~31s.
- Test-harness bug found and fixed during this round (not a product
  bug): `assert_eq!(hits.lock().unwrap().len(), N, "hits: {:?}",
  hits.lock().unwrap())` deadlocks when the assertion fails — the
  first MutexGuard lives until the end of the statement, so the
  message's second lock blocks forever on the same thread. This was
  the cause of the full-suite stall during round 7 (the failing
  count was also wrong: 5 expected, 2 actual — with one segment
  only one worker fetches per pass). All four occurrences in
  `crates/engine/src/media.rs` now bind the guard once before
  asserting. Lesson: never lock the same Mutex twice in one assert.
- `node --check` on all 6 extension JS files: pass.
- New persistent extension harnesses
  (`extensions/harness/`, run with
  `node extensions/harness/run-all.js`): 29 tests, all passing —
  `content.test.js` 19, `background.test.js` 7,
  `progress-percent.test.js` 3. The round-6 harnesses lived in
  `/tmp` and were lost; these live in the repo.
- chromium/ and firefox/ copies of content.js, background.js,
  popup.js, rules/ads.json verified byte-identical; both
  manifests bumped to 0.2.6.

## 13. NOT verified (needs the parent's post-install checks)

- Desktop crate compile (no GTK/WebKit dev libs on this VM) —
  Windows CI is the compile gate. Note: this round changes
  `apps/desktop/src/index.html` (the `progressPercent` helper) but
  no Rust in the desktop crate.
- Live-site behaviour: the native-host path in real Chrome, the
  clip guard against the real novahd clip, the DASH popup against
  nime's VidLink mpd, the sequential fallback against totallyacdn,
  the widened hook capture on all three sites, and the DASH resume
  against a real multi-phase failure.
- The atlantic hypothesis (CDN throttling concurrency) is still a
  hypothesis — the fallback is the test, not the proof.
