# Fix round 10 — notes (2026-11-23)

Fixes for everything the 2026-11-23 streaming-site sweep found
(items 1–12, ordered 20:15 PKT), plus four scope additions from
the user on voice calls the same evening: the Actions-column
redesign (item 13, 20:17 PKT), dark/light theme (item 14, 20:20
PKT), paired-browsers count (item 15, 20:21 PKT), and the site
directory (item 16, 20:23 PKT). Extension 0.2.8 → **0.2.9**.

Scope: engine, bridge, desktop (Rust + frontend), and extension.
The desktop crate cannot be type-checked on this VM — Windows CI
is the compile gate — so every engine API change was followed by
a grep of `apps/desktop/src-tauri/src/main.rs` for affected call
sites (round-8 lesson; verified again at the end of the round:
no no-arg `reload_from_disk()`, no old progress/completion
assignments remain anywhere).

Baseline (measured this round, before any edit): commit
`9b697c5` (round 9), cargo **185** passing (the brief said 188;
the measured count was 185), extension harnesses **66** passing.

## 1. Popup missing on the Tier 1 observed-findings path

**The defect:** clicks on novahd (15:25), MoviesJoy (16:41),
NOXX (18:27) and 123Movies (18:41) sent one-click with NO popup,
against the standing always-popup goal (2026-11-23 11:43 PKT:
a popup on EVERY video click). Root cause: background
`bestDownloadSource` dropped the observed finding's `kind`
("playlist"/"file") from the verdict, and content
`onOverlayClick` could then only guess from the URL
extension/masterText/viaPage — an extensionless playlist URL
looked like a file and one-click sent.

**The fix:** the verdict now carries
`kind: s.kind || (isPlaylist(best) ? "playlist" : "file")`
(`background.js`), and `onOverlayClick` routes
`dl.kind === "playlist"` to `openFormatsPopup` (after the
master-text branch, before viaPage) — the bridge fetches the
playlist and lists its variants. The engine side of the same
problem (extensionless playlist URLs) is item 4.

**Tests:** harness — verdict carries the observed kind; frame
candidates get their kind from the URL; an extensionless
playlist verdict opens the formats popup with no send; a
file-kind verdict still one-click sends; a captured master
still opens the HLS popup first.

## 2. yt-dlp byte accounting wrong

**The defect:** the novahd record showed downloaded_bytes =
total_bytes = 126,594,580 (~121 MB) while the real file is
3,367,173,048 B (3.37 GB) — 28x off. Three causes, all fixed:
(a) `parse_size_token` failed on yt-dlp's `~3.37GiB` (leading
`~`); (b) `plan_from_info` computed the total as
chosen.filesize + best audio filesize, so a video with no
filesize yielded an audio-only total; (c) completions set
downloaded=total blindly from the (wrong) plan.

**The fix:**
- `parse_size_token` strips a leading `~`.
- `plan_from_info` estimates the total from vbr×duration
  (`vbr_kbps*125*duration_secs`, same for abr) when filesize
  is missing; the merge total sums the best audio per language
  (multi case) or the single best audio (fallback).
- `DownloadRecord::complete_with_file` reconciles
  downloaded=total=real file size via `fs::metadata` on the
  delivered file (fallback to the old behaviour when the stat
  fails). The file on disk is the truth.
- `DownloadRecord::apply_progress` enforces monotonicity at
  the record level (bytes/total/segments never go backwards;
  the total estimate is latest-wins).

**The Plex counter anomaly (steer, 20:18 + 20:19 PKT):** the
Plex record showed 645/820 segments at 20:09:23, then 353/821
at 20:19:30 — segments_done fell while bytes grew and the
total changed 820→821. Investigated: there is NO mid-download
re-plan/re-fetch path in the HLS downloader. The cause was
phase-local segment counters: `download_hls_variant` and
`download_dash_with` run video then audio phases, and
`download_segments` took `progress_base` for BYTES
(cumulative) but segment counts were phase-local (video 820
segs → audio 821 segs explains the reset and the +1 total).
Fixed with a `segments_base: u64` parameter on
`download_segments` / `download_segments_parallel` /
`download_segments_sequential` (reports `base + done` /
`base + total`); all 21 call sites updated (video phases 0,
audio phases `video_segments.len()` / `segments.len()`).
`apply_progress` is the second line of defence: even a
stale/phase-local report can no longer move progress
backwards.

**Tests:** cargo — plan estimate total
`3_875_625_000 + 270_000_000` with picked 1080/3000; `~` size
parsing + progress-line case; apply_progress never backwards;
complete_with_file reconciles; segment counts cumulative
across phases (parallel base 100 → 100..108 of 108;
sequential base 50 → 51..58 of 58).

## 3. Single audio track on the yt-dlp path

**The defect:** the novahd file has ONE English AAC track
though the master playlist offered 7 audio tracks; the format
selector was `2637+bestaudio[ext=m4a]/2637+bestaudio/2637`.
The native HLS path keeps all tracks (the nime 720p HLS file
had 4).

**The fix (researched, not guessed):** yt-dlp merges
multiple audio streams when given `--audio-multistreams` and
a format selector naming them explicitly. The engine now
builds that selector: `format_selector_multi` names the best
audio per tagged language explicitly
(`2637+a-en-m4a+a-es+a-fr/...`, container-compatible ext
preferred), via `best_audio_per_language`; with fewer than 2
tagged languages it falls back to the old `format_selector`.
`download_media_with_stall` passes `--audio-multistreams`
exactly when `selector_has_multiple_audio` is true (the first
`/`-branch has ≥2 `+` joins), so no new record field was
needed. `mergeall` was deliberately NOT used: it would grab
same-language duplicates on YouTube (e.g. 233 + 234).

**Limitation (honest):** the multi-audio selector only covers
languages yt-dlp TAGS in the format list (`language` field,
now parsed into `MediaFormat.language`). An HLS master whose
audio tracks carry no language tag still yields one track.
`MediaFormat.language` is `#[serde(default)]`, so old
formats JSON still parses.

**Tests:** cargo — multi-selector (multi / no-language
fallback / single-language) against a new `multilang_json`
fixture (video 2637, no filesize, vbr 3000, duration 10335;
en m4a + en opus + es + fr audio).

## 4. Extensionless playlist URLs saved as junk files

**The defect:** MoviesJoy (936 B), NOXX (943 B), 123Movies
(937 B) — the engine downloaded HLS master playlist TEXT as a
direct file because the URL has no .m3u8 extension.

**The fix (both layers):**
- Engine: `sniff_stream_kind(url, referer)` does a Range GET
  of the first 2 KB (30 s timeout, no cookies/credentials),
  classifies by Content-Type first (`stream_kind_of_content_type`)
  then by prefix (`classify_stream_prefix`: `#EXTM3U` → HLS,
  allowing BOM/whitespace; `<?xml`+`<MPD` or bare `<MPD` →
  DASH; binary/HTML/JSON → None). Any error returns None =
  fail-open (the download proceeds as before). The bridge and
  desktop `add_download` paths sniff after the content-type
  check and re-route to the media path; `list_formats` and
  `plan_media_download` sniff extensionless playlist URLs
  before yt-dlp and route via `plan_stream_download_kind`
  (new `list_stream_formats_kind`, since the old
  `list_stream_formats` dispatches on URL extension only).
- Extension: `classifyObservedRequest` already labels
  observed findings by Content-Type; carrying `kind` in the
  verdict (item 1) completes the extension side — the popup
  treats them as streams.

**Tests:** cargo — `classify_stream_prefix_spots_playlists_not_files`.

## 5. Popup closes silently when the engine rejects the send

**The defect:** YesMovies — the popup closed by itself, no
"Sent" confirmation, no error; app-side the engine had
refused ("This page is not a supported video or media page").
Every pick handler closed the popup BEFORE sending, so a
rejection had nowhere to show.

**The fix:** all pick handlers now go through `pickAndSend`
(HLS rows, HLS audio-only, simple video/audio, formats rows,
degraded rows). The popup stays open until the send resolves:
on success (or duplicate/viaBrowser, which the button reports
honestly) it closes; on failure `showPopupSendError` shows
"Download failed" with the engine's reason in the popup
(title + subtitle, body cleared, X still closes). It only
shows if the popup is still open (the user may have closed it
with X mid-send). `sendCurrentDownload` returns its promise
(item 7) so the pick can wait on it.

**Tests:** harness — failure keeps the popup open with the
reason shown; success and duplicate close it; the error is
not shown when the popup is already closed.

## 6. App UI list shows stale/0 progress for bridge-driven downloads

**The defect (DevTools DOM read 2026-11-23 17:33 PKT):**
LookMovie row "1 / 1,009 pieces" vs downloads.json 228/1009 +
2.7 GB; PrimeWire "0 / 863" vs 36/863 + 76 MB; completed
novahd shown as "downloading" 0 B — while the just-resumed
SolarMovie row updated correctly, so the list DOES refresh and
the other rows' values were genuinely wrong in the UI.

**The cause:** `Store::reload_from_disk` (round 7) kept the
in-memory copy over ANY disk copy when the status was
Downloading or terminal — process-blind. The desktop app
holds its own in-memory store; the bridge writes progress to
disk; the app's stale in-memory copy always won the reload.

**The fix:** `reload_from_disk(&mut self, active:
&HashSet<String>)` now protects ONLY the ids in `active`
(this process's running jobs); terminal-in-memory +
non-terminal-on-disk never un-completes; otherwise the newer
disk copy wins by `updated_unix`; pruning uses `active`.
Both processes pass their live job ids (bridge
`active_job_ids()`, desktop `active_job_ids(&State)`), at all
7 bridge + 6 desktop reload call sites. Job entries are
removed when workers end — guarded by `Arc::ptr_eq`
(`remove_job_if_current`), so a resume's fresh job is never
removed by the old worker — including the bridge queue-gate
cancelled early return, which previously leaked the job entry
and protected its id from disk reloads forever.

**Tests:** cargo — reload refreshes a stale non-active
Downloading record (1 MB/1 seg → 2.7 GB/228 segs Completed).

## 7. Silent confirmation: "Sending…" reverts with no "Sent"

**The defect:** 123Movies (18:41, one-click) and Internet
Archive (20:01, popup) — the button showed "Sending…" then
reverted to default with NO "Sent to FetchForge", yet the
record WAS created app-side. Distinct from item 5 (there the
engine rejected; here the send succeeded).

**The diagnosis (honest):** no deterministic code trigger was
found — `sendCurrentDownload` always sets Sent/Failed. The
likely mechanism is the 5 s "Sent" window being too short for
sparse screenshot observation, plus a stale revert timer from
an earlier send firing mid-send and cutting the window
shorter. This round's fix is defence-in-depth, and the
unreproduced trigger is recorded here rather than claimed as
proven.

**The fix:** a single module-level `sendRevertTimer`, cleared
at every send start; the "Sent" (and other outcome) display
lengthened from 5 s to 10 s; `sendCurrentDownload` returns
its promise (resolving to the background's response; the
promise never rejects — a transport failure resolves to a
failure object).

**Tests:** harness — the promise resolves to the response;
the revert delay is 10,000 ms; a second send clears the
first send's pending timer.

## 8. Picked vs delivered quality

**The defect (a):** Soap2Day — picked Best 1080p 3 Mbps,
delivered 720p at ~865 kb/s (ffmpeg probe: 1280x720, 25 fps,
overall 865 kb/s; file complete, 709 MB ≈ 865 kb/s × 109
min). **Diagnosed first:** the engine picks by variant
URL/representation id, NOT by index, so an engine
wrong-variant bug is unlikely — the site mislabels its top
variant.

**The fix (a):** record the delivered quality and surface a
mismatch note instead of claiming 1080p. New record fields
(all `#[serde(default)]`): `picked_height`,
`picked_vbr_kbps` (from the plan), `delivered_height`,
`delivered_bitrate_kbps`, `quality_note`.
`probe_video_quality` runs `ffmpeg -i` on the finished file
(ffprobe is NOT extracted by `ensure_ffmpeg`, only
ffmpeg.exe) and parses the stderr (`parse_ffmpeg_probe`);
`apply_delivered_quality` sets the delivered fields and a
mismatch note — `"Delivered {h}p at ~{b} kb/s; the site
listed this stream as {ph}p / {pv} kb/s."` — when the
delivered height is below the picked height OR the bitrate is
below 60% of the picked bitrate; no note when either side is
unknown. The probe is best-effort (via `find_ffmpeg` — never
downloads) and never fails a completed download. The desktop
UI shows `quality_note` in the status cell (`.quality-note`).

**The defect (b) (steer):** Internet Archive — the tester
played the 1080p mkv of "Night of the Living Dead", the popup
offered a single "Video · MP4" choice, and the engine fetched
the Archive's 512kb.mp4 encode (407 MB) — NOT the 1080p file
playing. **Diagnosed:** content `mediaSource` took the first
direct-file candidate — currentSrc first (correct), then the
src attribute, then `<source>` elements in document order;
when currentSrc was empty the first `<source>` won
(512kb.mp4).

**The fix (b):** when there is no playing http(s) currentSrc,
`<source>` candidates are ranked by quality hints in the URL
(`qualityHintScore`: height hints 1080p/720p/1920x1080/4k
outrank bitrate hints like 512kb; bigger wins within a kind;
ties and hint-less URLs keep document order). currentSrc
always wins when present. A blob: currentSrc (MSE playback)
does not count as a playing source, so the fallbacks are
ranked then too. **Site-side quirk note:** if the Archive's
player genuinely serves the 512kb file as its currentSrc
while showing the mkv as the selected file, that is the
site's behaviour, not an engine bug — the ranking only
applies when currentSrc is absent.

**Tests:** cargo — `apply_delivered_quality` notes a
mismatch only (Soap2Day 1080p/3000 → 720p/865); probe parsing
on canned ffmpeg stderr (Soap2Day shape, audio-only,
garbage/empty → None). Harness — hint ranking (1080p >
720p > 512kb, 4k > 1080p); Archive shape picks 1080p over
first-listed 512kb; currentSrc always wins; blob currentSrc
falls back to ranking.

## 9. YouTube audio-only flag not carried on the formats path

**The defect:** Big Buck Bunny — the tester picked "Audio
only · m4a · 129 kbps · 9.8 MB" in the formats popup, but the
record was created with audio_only=False and no .m4a
extension (contrast the round-8 nime audio record:
audio_only=True, .m4a at creation).

**The cause:** the formats pick handler in content.js sent no
`audioOnly` field. The bridge already accepted and stored it.

**The fix:** the formats pick handler now sends
`audioOnly: f.kind === "audio_only"`.

**Tests:** harness — the round-8 multi-format test's pick
assertions still pass through the new `pickAndSend` path;
the audio-only flag is covered by the degraded-audio-row
test. (No dedicated formats-audio test was added: the flag
is a one-line carry-through of a field the bridge already
handles, and the audio-only plumbing is covered end to end
by the existing background audioOnly test.)

## 10. Duplicate m4a entry in the YouTube formats popup

**The defect:** "Audio only · m4a · 129 kbps · 9.8 MB" listed
twice, identical.

**The fix (two layers):** the engine dedupes in
`parse_media_info` after sorting — **by format_id, NOT
label** (label-based dedupe wrongly collapsed YouTube HLS
audio 233/234, different ids with the same "Audio only ·
mp4" label; caught by 3 failing tests, fixed, all green).
The extension dedupes too in `sortFormatsForPopup` (defence
in depth): keyed on format id when present, else label +
kind; first occurrence wins.

**Tests:** cargo — a duplicate 140 added to the
`youtube_json()` fixture (parsed count stays 8 via dedupe).
Harness — dedupe by format id (first wins, ladder order
kept); dedupe of id-less rows by label + kind.

## 11. Filename auto-fill flaws (conservative fixes only)

**The defect:** (a) fully-fake titles ("New Message!" from
nime's fake notification ad — round 7 stripped the "(1)"
prefix but the remaining text was still fake); (b) generic
site titles used as filenames (LookMovie "Movies Online for
Free at Lookmovie2", Soap2Day tagline, FMovies, 123Movies);
(c) error-page title taken at click time (Roku "Something's
not right" — the page had flipped to its 404 page).

**The fix:** junk-title detection in both layers, with the
same list — exact "new message"/"new message!", "something's
not right", "something went wrong", "404", "page not found",
"online for free", "watch movies", "watch tv", "movies &
tv", "movies and tv". Engine: `stem_for_title` returns
`"video"` for junk titles (second line of defence). Extension:
`isJunkTitle` makes `pageTitleStem` return `""`, and
`popupDefaultName` / `defaultFilenameFor` (manifest URLs)
fall through to the new `pageNameHint()` — the video
element's title/aria-label, the page h1, or the og:title,
each junk-checked the same way — then to the verdict
filename's stem. The popup filename field stays editable as
the user's override. The existing Fix 5 harness test was
updated to the new behaviour ("(1) New Message!" → ""), which
is the bug item 11 fixes, not a weakening.

**Tests:** harness — junk titles rejected (notification,
error page, taglines, 404); real titles survive; hint
fall-through (video > h1 > og:title); junk h1 skipped;
default-name fall-through chain. Cargo — junk cases in the
stem test (`"(1) New Message!"` → `"video"`).

## 12. Degraded popup should say WHY quality info is unavailable

**The defect:** YouTube movie (The Lost Medallion) showed
"Loading qualities…" then bare "Quality info unavailable"
(no list, no Audio only), while Big Buck Bunny minutes later
loaded the full ladder — the formats fetch fails per-video
(possibly yt-dlp bot-gating, unconfirmed).

**The cause:** background `getFormats` threw
`new Error("bridge-" + r.status)` and discarded the bridge's
error body — the bridge `/media/formats` DOES return
`{"error": <friendly text>}` with HTTP 400.

**The fix:** `getFormats` reads the error body on `!ok` and
attaches it as `e.reason` (fallback: "The bridge could not
list this page's qualities (HTTP N)." when the body is empty
or unreadable); the message handler passes `reason` through;
`showDegradedPopup(reason)` displays it in the subtitle. The
timeout path gets its own reason text ("The app took too long
to list the qualities — it may be busy. Try again in a
moment."). Unreachable failures carry no reason — the error
state handles them. No cookies/sessions are used (spec §10).

**Tests:** harness — a 400 with an error body carries the
text; empty/unreadable bodies get the fallback; a network
failure is unreachable with no reason. The existing timeout
harness test was updated to the new timeout subtitle.

## 13. Actions column redesign (user order, voice call 20:17 PKT)

**The order:** the downloads-list Actions column holds
exactly TWO icons — one pause/resume TOGGLE icon (pause
glyph always; RED when paused; click resumes) and directly
below it a recycle-bin DELETE icon. Delete is UNIVERSAL:
every row, whatever its state (completed duplicates, stuck
running downloads). Delete needs double confirmation: delete
the files too, or just remove the entry from the list (his
sentence was cut off at "or just..." — "remove the entry
from the list" is the natural completion; he has NOT answered
the open question about the second option; proceed with this
reading — a small dialog-text change if he corrects it).

**What was found:** no delete/remove command existed — the
desktop had only pause/resume/cancel/clear-finished. So this
was not frontend-only after all: new commands/endpoints were
built.

**The fix:**
- Frontend (`index.html`/`styles.css`): the Actions column is
  icon-only — pause/resume toggle icon (pause glyph, red
  `is-paused` class when paused, title tooltip Pause/Resume)
  on downloading/paused rows only, recycle-bin delete icon
  below on EVERY row; icons built with `createElementNS`
  (pause = two rects; trash = Material delete path), never
  innerHTML. Delete opens a custom confirmation modal
  (`#deleteConfirm`, reuses `.format-picker` styling) with
  "Delete the files too" / "Remove only the list entry" /
  Cancel — nothing deletes until the user picks an option.
  Pause/Resume/Cancel text buttons removed from the column.
- Engine: `delete_download_files(record, download_dir,
  data_dir) -> usize` — deletes the final file, `.part`,
  `.audio.part`, `.part.seg` dirs (counts contained files),
  `.part.segments` sidecars, yt-dlp partials
  (`<stem>.*.part/.ytdl/.temp`), and the manifest sidecar
  `data_dir/manifests/<id>.m3u8`. Only under download_dir
  (canonical prefix check; a destination outside the folder
  is never deleted); scans the download dir + one level of
  category subdirs.
- Bridge: `App::delete_download(id, delete_files)` (cancels
  the bridge job, polls ≤5 s for status != Downloading,
  removes the record + saves, deletes files if asked, returns
  the files count, 404-style Err when missing) + route
  `POST /downloads/:id/delete` (body `{"delete_files": bool}`,
  returns `{"deleted": true, "files_deleted": N}`).
- Desktop: new Tauri command `delete_download(id,
  delete_files)` (cancels the local job + forwards cancel to
  the bridge best-effort + polls the store with reload ≤5 s +
  removes the record + `delete_download_files`; returns the
  files_deleted count; deletes orphaned manifest sidecars).
  `pause_download` / `cancel_download` are now async and
  forward to the bridge best-effort, so the toggle is real
  for bridge-driven downloads; `resume_download` stays sync
  and local-only (forwarding would double-spawn a worker).
  For a running download, deleting stops it first, then
  applies the confirmation choice.

**Tests:** cargo — `delete_download_files` removes only its
own files (7 removed; another download + an outside-folder
file survive).

## 14. Dark mode + light mode (user order, 20:20–20:21 PKT)

**The fix (frontend-only):** a theme toggle button in the
header; an apply-on-load script in `<head>` reads
`fetchforge-theme` from localStorage before first paint
(dark default — the current look, `#0f172a` bg / `#1e293b`
panels / `#e2e8f0` text, stays the default); the toggle
persists the choice and updates its label ("Light mode" /
"Dark mode"). `styles.css` was rewritten with CSS variables
(`:root` dark defaults; `[data-theme="light"]` overrides —
the light theme uses darker muted/bright/warn colours for
readability on white). `node --check` passed on both
extracted inline script blocks (main + head theme script).

## 15. Paired-browsers count (user order, 20:20–20:21 PKT)

**The order:** show the TOTAL number of paired browsers in
the Browser Integration section. The bridge held a single
token and did NOT track distinct pairings, so a real count
needed the bridge to record each pairing. DO NOT show a fake
number. "Unpair all" must clear the recorded list as well as
rotating the token.

**The fix:**
- Bridge: `PairedBrowser { instance_id, browser_name,
  paired_unix }`; the app holds `paired: Mutex<Vec<PairedBrowser>>`
  loaded from `paired.json` (corrupt → empty + warning);
  `record_paired_browser` (dedupe by instance_id; an empty id
  is not recorded; an empty name → "Browser") + `save_paired`
  (tmp+rename); `rotate_token` clears the list and saves;
  `/pair/verify` parses optional `browser_id`/`browser_name`
  and records on success; new endpoint `GET /pair/browsers`
  (token required) → `{"count": N, "browsers": [...]}`.
- Extension: the `pairCode` handler sends `browser_id` (a
  stable instance id from storage.local, generated once via
  `getBrowserId` — randomUUID, never rejects) + `browser_name`
  (from the UA: Edge/Firefox/Opera/Chrome, Edge checked
  first since its UA contains "Chrome") with the verify
  request.
- Desktop: new Tauri command `paired_browsers` (bridge GET
  `/pair/browsers`); the UI shows a `#pairedCount` span
  ("No browsers paired" for a real 0, "N browser(s) paired",
  hidden on failure — never a fake number), refreshed at
  startup, when a pairing code is claimed, and after
  unpair-all.
- Browsers paired by pasting a token manually are NOT counted
  (no instance id was ever sent).

**Tests:** harness — `browserName` (Edge/Firefox/Chrome/
fallback); `getBrowserId` stable across calls and stored
once; a stored id is reused.

## 16. Site directory button (user decision, 20:23 PKT)

**The order:** a button/section in the desktop app holding
links to the movie websites FetchForge works with, so the
user never has to search Google for the sites again. Links
open in the default browser (not in-app webview). WORKING
sweep sites ONLY.

**The fix:** a new "Sites" tab + `#view-sites` section;
`SITE_DIRECTORY` — one data-driven array in the frontend —
holds the 14 working sites (novahd, nime, atlantic,
LookMovie, PrimeWire, FlixTor, Soap2Day, FMovies, Plex, Roku
Channel, Internet Archive, YouTube, 123movieseasy, fmovie);
`renderSites()` builds the buttons once. Clicking opens via
the new Tauri command `open_external_link` (no `url` crate in
the desktop Cargo.toml, so manual string validation: no shell
metacharacters `& | < > ^ % " ' \`` or space; must start
`https://`; host non-empty without `@`/`:`, and in the 14-site
allowlist; opens via `cmd /C start "" <url>` on Windows,
`open`/`xdg-open` elsewhere) or `window.open(url, "_blank")`
in bridge mode. No new Tauri plugin was needed. No
availability checking or health pings — the list is a
convenience directory, not a guarantee (these are unofficial
sites whose domains churn).

## Tests

- **Cargo: 199 passed, 0 failed** (18 bridge unit + 4
  native-host + 167 engine + 5 failures + 1 hls + 1
  secure-token + 1 segmented_progress + 2
  segmented_verification). Baseline was 185; +14 new tests.
  One transient failure was seen once during a full run
  (`download_media_with_cancel_kills_stub`) — it passed in
  isolation and on full rerun; it is a pre-existing
  timing-sensitive test, not a round-10 regression (no
  round-10 change touches the cancel path). Recorded here as
  a known flake.
- **Extension harnesses: 92 passed, 0 failed** (12
  background + 32 content + 3 progress-percent + 26 round 10
  + 19 round 9). Baseline was 66; +26 new tests in
  `extensions/harness/round10.test.js`. Three existing
  content.test.js assertions were updated to round-10
  behaviour (not weakened): the "(1) New Message!" stem is
  now "" (item 11 — that IS the bug being fixed); the
  formats-timeout subtitle is now the timeout reason (item
  12); the ctx4/ctx6 send stubs became `pickAndSend` stubs
  (item 5) with the same filename-filling behaviour.
- **node --check** passed on all changed extension JS files
  (chromium + firefox: content.js, background.js, popup.js,
  hook-main.js), the harness files, and both extracted
  inline script blocks in `apps/desktop/src/index.html`.
- **Byte-identical verification:** content.js, background.js,
  popup.js, popup.html, hook-main.js, rules/ads.json are
  byte-identical between `extensions/chromium/` and
  `extensions/firefox/` (verified with `cmp`, and asserted by
  a round-10 harness test). Both manifests are at **0.2.9**.
- **Desktop call-site grep (round-8 lesson):** no no-arg
  `reload_from_disk()`, no old progress/completion
  assignments remain in bridge, engine, or desktop code.
  The desktop crate itself cannot be type-checked on this
  VM — Windows CI is the compile gate.

## NOT verified (needs the laptop + the user's browser)

No live-site verification was possible overnight (the laptop
was off). All of the following need the morning install +
live test:
- Item 1: a real observed-playlist click (novahd / MoviesJoy
  shape) opening the formats popup in his Chrome.
- Item 2/6: the 7 paused downloads resuming on the NEW
  version only (MORNING RULE: do NOT resume them on the old
  version) — verify byte totals and segment counts against
  downloads.json + ffmpeg probes, and that the app list
  matches disk.
- Item 3: a multi-audio yt-dlp download (novahd shape) keeps
  all tagged-language audio tracks.
- Item 4: an extensionless playlist URL (MoviesJoy/NOXX/
  123Movies shape) downloads as a stream, not a ~940 B file.
- Item 5/7: a rejected send shows the error in the popup;
  "Sent to FetchForge" is visible for 10 s.
- Item 8: the Soap2Day mismatch note appears on a mismatched
  record; the Archive pick prefers the best encode when
  currentSrc is absent.
- Item 9: a YouTube audio-only formats pick creates an
  audio-only record (.m4a).
- Item 10: no duplicate m4a row in the YouTube formats popup.
- Item 11: junk titles fall through to the video title/h1/
  og:title in the popup filename field.
- Item 12: a per-video formats failure shows the bridge's
  reason in the degraded popup.
- Item 13: the icon column, the red paused state, and the
  delete confirmation (both options) on all row states.
- Item 14: both themes readable (tagline, tabs, rows, pairing
  strip, buttons).
- Item 15: the paired count after pairing a browser; "Unpair
  all" clears it.
- Item 16: every site link opens in the default browser.
- Windows CI must compile the desktop crate (the VM cannot
  check it).

## Known limitations (by design, not bugs)

- Item 3: multi-audio only covers languages yt-dlp tags in
  the format list; untagged HLS audio tracks still yield one
  track.
- Item 7: the silent-confirmation trigger was never
  reproduced deterministically; the fix is defence-in-depth
  (stale-timer clearing + 10 s window).
- Item 8: if a site genuinely serves its smallest encode as
  the playing source (currentSrc), the extension keeps it —
  that is the site's behaviour, not an engine bug.
- Item 13: the delete-confirmation second option
  ("Remove only the list entry") is the natural completion of
  the user's cut-off sentence; he has not confirmed the
  wording. The toggle icon appears only on
  downloading/paused rows (queued rows have no job yet;
  terminal rows have nothing to pause); the bin is on every
  row.
- Item 15: browsers paired by pasting a token manually are
  not counted; the UI shows nothing rather than a fake
  number when the count cannot be read.
- Item 16: the site list is a convenience directory, not a
  guarantee — unofficial sites' domains churn; no
  availability checking by design.

## Deferred to the next update (user's words, 20:20–20:21 PKT)

- (A) Extension auto-install with the installer / in-app
  README icon for adding the extension.
- (B) Pairing README text with complete steps.

Neither was built in this round.

## CI fixup (2026-11-24, after run 45314495234 failed)

The first Windows build of this round FAILED in the "Build
Tauri app" step: 6 compile errors in `fetchforge-desktop`,
all from one cause. Round 10 made `pause_download` and
`cancel_download` async (item 13, to forward pause/cancel to
the bridge), but left their return type as `()` — and Tauri
requires async commands that take references as inputs
(`tauri::State<'_, ...>`) to return a `Result`. The
`#[tauri::command]` macro rejected both functions, so
`generate_handler!` then failed with 4 "cannot find macro"
errors for the two missing commands. Every other async
command in the file already returned a `Result`; only these
two were missing it.

Fix (this commit): both functions now return
`Result<(), String>` with `Ok(())` at the end. No behaviour
change — the frontend invoke calls resolve the same way.
Lesson (extends round-8 notes §8): any command made async
must return a `Result` — grep the async commands' signatures,
not just the engine call sites, before pushing.
