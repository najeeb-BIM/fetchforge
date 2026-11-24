# Fix round 11, extension — notes (2026-11-24)

The 14 downloader-study features the user ordered on the ~18:07
PKT voice call, added INTO round 11 (not a later update), on top
of commit `1190671`. Source for the features:
`docs/v02-phases/downloader-size-and-features-study.md` §4 (the
20-item missing-functions list); the user picked 14 and said no
to the rest (see Deferred). Extension stays **0.2.10**.

Scope: engine, bridge, desktop (Rust + frontend), and extension.
The desktop crate cannot be type-checked on this VM — Windows CI
is the compile gate — so every engine API change was followed by
a grep of `apps/desktop/src-tauri/src/main.rs` for affected call
sites (round-8 lesson) and a check that every async Tauri command
returns a `Result` (round-10 lesson; verified at the end of the
round: all 5 new async commands return `Result` and are
registered in `generate_handler!`, and the modified `add_download`
keeps its `Result` return).

Baseline (measured this round, before any edit): commit
`1190671` (round 11 first 13 items), cargo **210** passing,
extension harnesses **105** passing.

## Shared groundwork

- **`DownloadRecord` (state.rs):** 8 new fields, all
  `#[serde(default)]` so old `downloads.json` files load:
  `audio_lang`, `subtitle_langs`, `audio_format`,
  `format_kind` ("audio"/"video"/"combined"), `playlist`,
  `playlist_index`, `downgraded_from`, `downgrade_attempted`.
  Helpers: `audio_extension()` (mp3/opus/m4a, default m4a) and
  `reset_for_downgrade(new_format_id, old_label)`.
- **`Settings` (bridge + desktop, mirrored field-for-field):**
  `proxy_url`, `shutdown_when_finished` (default false),
  `notifications_enabled` (default true). `settings.json` on
  disk is the single source of truth; both processes load fresh
  per operation (`App::current_settings()` /
  `current_settings(data_dir)`). Each worker loads once at
  start — a settings change mid-download applies to the NEXT
  download.
- **No new dependencies.** reqwest gained the `socks` feature
  (needed for socks5 proxies); no new crate.

## 1. Auto-start queued downloads

**What:** when the bridge starts, every Queued record starts
automatically, oldest first (`auto_start_queued()`, called in
`main()` serve mode after `recover_interrupted()` and after the
bind succeeds). Paused records stay paused. Mid-run auto-start
already worked via `spawn_worker`'s queue gate; only the restart
gap was missing — queued rows used to sit forever after an app
restart until resumed by hand.

**Deliberately bridge-only.** The desktop must NOT duplicate it:
two workers on one record derive the same `.part` path and
corrupt each other. If no bridge runs, queued records still sit
(status quo — noted honestly, not fixed).

**Tests:** bridge `auto_start_queued_spawns_queued_only` (job
map gains the Queued id only; worker fails fast on the fake URL;
test cancels the job).

## 2. Subtitle download and muxing

**What:** subtitles were ignored entirely; now the chosen
language(s) are downloaded and muxed into the output file.

- **yt-dlp path:** `--embed-subs --sub-langs <csv>` — WITHOUT
  `--write-subs`, so no sidecar `.vtt` files are left beside the
  video. Subtitle languages come from yt-dlp's `subtitles` keys
  only (human subtitles, NOT `automatic_captions`).
- **HLS/DASH:** the subtitle rendition/representation is
  downloaded as segments, the VTT cues are merged with
  EXTINF / X-TIMESTAMP-MAP / start-time offsets, and ffmpeg
  muxes the result (`merge_tracks`, mov_text with a language
  tag). A muxed `.ts` variant re-containerises to `.mp4` at
  merge time (the record's completion step reconciles the
  extension).
- **Subtitle failure never fails the video:** a subtitle
  download error warns to stderr and the video continues
  without subtitles.
- **UI:** subtitle checkboxes in the content-script quality
  popup (popup chrome, shown only when the stream offers any)
  and in the desktop format picker.

**Not done, honestly:** DASH subtitles are WebVTT only — TTML
is explicitly unsupported (never listed or matched). Direct
file downloads get no subtitles (there is no track to fetch).
An ffmpeg-direct subtitle fetch was rejected: signed single-use
links fail on a second fetch, so the engine downloads the
segments itself.

**Tests:** engine media/extract tests; harness
`parseHlsMaster exposes subtitle languages`.

## 3. Speed limit setting

**What:** the engine could already throttle; the app UI now
exposes it. A new Settings tab (desktop frontend) has a speed
limit input (KB/s, empty = no limit), saved via POST /settings
(`update_settings`, clamped; 0 = unlimited) and persisted in
`settings.json`. The engine applies it two ways: the shared
`SharedLimiter` (1s window) in the HLS/DASH segment paths, and
yt-dlp `--limit-rate <kbps>K`. The desktop and bridge workers
both read it from settings at worker start.

**Limitation:** a change applies to the NEXT download, not to
one already running (workers load settings once at start).

## 4. Per-language audio choice

**What:** the popup lets the user pick the audio language when
several tracks exist. Two parts:

- **The bug:** content.js `parseHlsMaster` kept only the FIRST
  audio rendition per GROUP-ID — novahd's 7 tracks in one group
  collapsed to one. It now keeps ALL renditions (language +
  name + URI) and exposes `audioLanguages` (distinct LANGUAGE,
  else NAME) and `audioGroups` for pick-time resolution. The
  variant rows default to the group's first rendition; the
  language select overrides it at pick time; each rendition
  also gets its own "Audio only" row carrying its language.
- **The wiring:** the choice is baked into the yt-dlp selector
  at plan time (`format_selector_for_language` — video + best
  audio of one language, falling back to the plain selector);
  HLS/DASH pick the rendition/representation by language at
  download time. The desktop picker shows the same select when
  `audio_languages` has more than one entry.

**Tests:** harness — a novahd-style 7-track master keeps all 7
renditions, 7 audio-only rows, 7 languages; variant rows
default to the first rendition and carry the group id.

## 5. Batch download

**What:** queue several items at once from one action.

- **Bridge:** POST /downloads/batch (cap 50 items), per-item
  results (`{ added, errors }`).
- **Desktop:** Tauri `add_download_batch` + the Media Detector
  panel's "Download selected" now calls the batch endpoint
  instead of one add per item.
- **Extension popup:** a "Download all" button checks every
  media item on the page and sends them through the existing
  selected path.

**Honest note:** the popup path sends items one by one
(sequential `sendUrl` calls, one per item); the true batch
endpoint is used by the desktop detector panel. Both are one
user action queuing several items.

## 6. Clipboard monitoring

**What:** opt-in, default OFF. When enabled, a 1-minute alarm
reads the clipboard and OFFERS any media URL it finds — it
never starts a download. The offer lands in the app's Media
Detector panel (POST /media/detected) like any detected item;
the user downloads it from there.

- **Permission:** `clipboardRead` is an OPTIONAL permission
  (both manifests); the popup requests it when the user enables
  the toggle, and the background verifies it is still granted
  before every read.
- **Reading:** Chromium MV3 service workers cannot use
  `navigator.clipboard.readText()` (no document focus), so the
  read goes through a short-lived offscreen document
  (`offscreen.html` + `offscreen.js`, chromium-only — NOT a
  shared file) that reads on load and replies. Firefox's
  background page reads directly.
- **Matching:** `extractMediaUrlFromText` finds the first media
  URL (known media-page host or media file/manifest extension),
  stripping trailing sentence punctuation.
- **Dedupe:** the last-offered URL is kept in storage.local, so
  the same clipboard text is offered once, not once a minute.

**Limitation:** the offer needs the bridge running and paired
(otherwise the POST fails silently — best-effort); the poll is
once a minute, not instant.

**Tests:** harness — YouTube link in surrounding text,
trailing-period stripping, non-media URLs skipped, first media
URL wins.

## 7. Shutdown when finished

**What:** an explicit opt-in (global toggle in the Settings
tab, default OFF). Checking the box asks once more in a
confirm dialog; a cancelled confirm leaves it unchecked. It
fires only after a worker COMPLETES successfully, with no
Queued/Downloading/Paused records remaining (fresh disk reload)
and no other active jobs in the process, guarded once-only by
an `AtomicBool`. The command is Windows `shutdown /s /t 60`
(a 60-second abort window — `shutdown /a` stops it). Failed
downloads block it; it never fires on startup; non-Windows
logs a warning (Windows-only deployment).

**Limitation:** a tiny race remains between the disk reload and
the command (a new add in that window); the 60s window is the
safety valve.

**Tests:** bridge `shutdown_guard_fires_once_and_only_when_idle`
(a Queued record blocks it; then the guard fires; a second call
is a no-op).

## 8. Completion notification

**What:** the frontend's Web Notification API (a desktop toast
in Tauri's WebView2) fires when a download finishes. The
seen-set is seeded on the first load, so downloads that
finished before the page opened never notify. Gated by the
`notifications_enabled` setting (cached from the settings
load; default ON when never loaded). The user is asked for
permission once, when the first download finishes.

**Limitation:** fires only while the app runs — there is no
background notifier, so a download that finishes while the app
is closed (bridge-only run) produces no notification.

## 9. Retry with quality downgrade

**What:** if a chosen quality fails, the worker retries
automatically at the next lower quality instead of failing
outright. `try_downgrade` re-lists formats once
(`list_formats_with_referer_and_proxy`, which covers HLS/DASH
manifests as well as yt-dlp pages), picks the highest format
strictly below the current height (`next_lower_format`),
deletes `.part` files before a yt-dlp retry
(`remove_partial_files` — only `<stem>.part`,
`<stem>.<ext>.part`, `<stem>.f*.*.part` and `.ytdl`
equivalents; never finished files, never another download's
parts), resets the record via `reset_for_downgrade` (back to
Queued, "pending", progress cleared, `downgraded_from` set),
and spawns a fresh worker that re-plans. The row shows
"Retried at a lower quality (was 1080p)". ONE step down only;
`downgrade_attempted` prevents a repeat on manual resume.
Never on pause/cancel or planning failure — only after a real
download failure.

**Limitation:** a failed re-listing (e.g. a signed manifest
link that only worked once) means no downgrade — the download
fails with its original error.

**Tests:** bridge `try_downgrade_refuses_without_a_ladder` (no
height / already attempted / gltf — no network).

## 10. Proxy support

**What:** a proxy input in the Settings tab, persisted in
`settings.json` as `proxy_url`. Validated
(`validate_proxy_url`: http/https/socks5, host required,
credentials allowed, no path/query/fragment, max 2048 chars)
— and NEVER logged. Applied to the engine (`reqwest::Proxy::all`,
error never includes the URL) and to yt-dlp (`--proxy`).
Threaded through every engine path that fetches: probes,
sniffs, format listings, plans, HLS/DASH downloads, glTF.

**Not done, honestly:** tool downloads (the ffmpeg/yt-dlp
first-use fetch in `ffmpeg.rs::download_verified`) do not use
the proxy — the tools are already installed on the user's
laptop, so that path is out of scope. The validator normalises
via the `url` crate, which appends a trailing `/` to a bare
host (cosmetic only).

**Tests:** engine validate/download tests; bridge
`update_settings_merges_validates_and_persists` (an invalid
proxy is rejected without echoing the value).

## 11. Playlist download

**What:** yt-dlp `--flat-playlist -J` (`list_playlist`), each
entry its own record with `playlist` title + `playlist_index`
(cap 200 entries). Bridge: POST /media/playlist (list) +
POST /downloads/playlist (queue). Desktop: Tauri
`media_playlist` + `add_playlist_download`, and the format
picker shows "Download all N videos" when `MediaInfo.is_playlist`
(the formats listing already knows — no extra call). The
picker's language/subtitle/audio-format options apply to every
video in the list. Rows show "Playlist: <title> · #<index>".

**Limitation:** movie-site series pages (no yt-dlp playlist)
are covered by batch (feature 5) instead, not by playlist
detection.

## 12. Metadata and chapter embedding

**What:** yt-dlp `--embed-metadata --embed-chapters` whenever
ffmpeg is available — resolved via `find_ffmpeg`, never
downloaded just for this (metadata alone is not worth a
120 MB first-use fetch). Native HLS/DASH downloads embed the
title metadata when a merge already runs (`merge_tracks`).

**Not done, honestly:** no chapters for HLS/DASH (manifests do
not carry them); direct downloads get no metadata embedding.

## 13. Audio format choice

**What:** for audio-only downloads the user picks MP3 / Opus /
M4A, not just m4a. The record carries `audio_format`
(default m4a). yt-dlp gets `--extract-audio --audio-format`
ONLY when the format kind is audio (the worker checks
`format_kind`; when yt-dlp already extracted, the worker skips
its own post-step). Native HLS/DASH/direct audio goes through
`extract_audio_as` (copy first, then re-encode with
libmp3lame/libopus/aac). The duplicate check uses the chosen
extension, not a hardcoded `.m4a`. UI: the content-script
popup's audio-format select (shown when the popup offers an
audio-only row) and the desktop picker's select (shown when
the chosen format is audio-only).

## 14. Browser download interception

**What:** studied first (IDM/XDM-style: the extension takes
over downloads started in the browser and routes them through
the download manager), then implemented as an opt-in, default
OFF (popup toggle, persisted as `interceptDownloads`). When
enabled, `chrome.downloads.onCreated` checks each new download:
media by extension or MIME type only (video/audio). The flow is
pause → send to FetchForge → cancel the browser copy on
success / resume it on failure (fail-open — the user never
loses a download because the app was unreachable). No extra
permission: `downloads` was already granted. No notification
on takeover (the app list shows it).

**The loop guard:** the browser-fallback path (`sendDownload`'s
last resort) registers its URLs in `browserFallbackUrls` (capped
at 100, each entry consumed by the next onCreated for it), so
interception never re-grabs a file the extension itself just
handed to the browser.

**Limitation:** a file that finishes before the pause lands is
not intercepted (fail-open); there is no per-site control.

**Tests:** harness — MIME/extension matching, non-media
rejection, basename reduction.

## Harness repair (this round)

The two new popup helpers (`configurePopupOptions`,
`currentPopupOptions`) are called from `openHlsPopup`,
`openFormatsPopup`, `openSimplePopup` and `pickAndSend`, so the
older test files' fixed extraction lists broke: 5 failures in
`content.test.js`, 2 in `round10.test.js`. Fixed by adding the
helpers to those contexts' extraction lists and adding the
option-select fields to the `floatOverlay` stubs (which mirror
`createFloatingButton`'s return). Note: `round11.test.js` had
been passing vacuously — its `openFormatsPopup` success path
threw a ReferenceError that the popup's `.catch` swallowed and
showed as the unreachable state, and no test checked the rows.
Its context is fixed the same way, so the real path now runs.

## Tests

- **cargo:** **232 passed, 0 failed** (baseline 210; +22 new:
  18 engine — 2 state, 2 validate, 2 download, 2 ffmpeg,
  4 media, 6 extract — and 4 bridge). Breakdown: engine lib
  196, bridge lib 22, bridge native-host integration 4,
  engine integration 5+1+1+1+2.
- **Extension harness:** **116 passed, 0 failed** across 7
  files (baseline 105; +11 new in `round11ext.test.js`).
- **node --check:** all touched extension JS files (chromium
  + firefox content/background/popup, chromium offscreen.js,
  hook-main.js) and all 3 inline script blocks in index.html.
- **Manifests:** both at **0.2.10**. Chromium/firefox
  byte-identical on all 6 shared files (content.js,
  background.js, popup.js, popup.html, hook-main.js,
  rules/ads.json). `alarms` added to permissions and
  `clipboardRead` to optional_permissions in both manifests.
- **`cargo fmt` clean** on the workspace; rustfmt clean on
  the desktop crate.
- **Desktop crate (cannot compile on this VM):** 5 new Tauri
  commands (`get_settings`, `update_settings`,
  `add_download_batch`, `media_playlist`,
  `add_playlist_download`), all async + `Result`, all
  registered in `generate_handler!`; `add_download` gained
  the 3 new params with `rename_all = "snake_case"` (as
  before). Every engine API change was followed by a grep of
  the desktop crate — including two `DownloadOptions` /
  `GltfDownloadOptions` literals that were missing the new
  `proxy_url` field (a CI failure waiting to happen). Windows
  CI is the compile gate.

## NOT verified (needs the laptop + the user's browser)

Nothing in this round was live-tested: the laptop currently
has NO FetchForge installed (full reset #2, 2026-11-24), so
there was no app, bridge, or extension to test against. After
install + re-pair, the retest should cover:
- Item 1: queue two downloads, quit the app mid-first, relaunch
  — the queued one starts on its own; a paused one does not.
- Item 2: a YouTube video with subtitles — check the subtitle
  checkboxes, then probe the file for the subtitle stream; a
  novahd film with subtitles — same check.
- Item 3: set a 500 KB/s limit, start a download, watch the
  speed column stay near it.
- Item 4: novahd — the popup's Audio select lists 7 languages;
  picking Spanish delivers Spanish audio.
- Item 5: detector "Download selected" queues several at once;
  popup "Download all" on a page with several media items.
- Item 6: enable clipboard monitoring (permission prompt
  appears), copy a YouTube link — it appears in the Media
  Detector within a minute, and nothing downloads on its own.
- Item 7: enable shutdown, run one download to completion —
  Windows shows the 60-second shutdown countdown; abort with
  `shutdown /a`.
- Item 8: a download finishes while the app is open — a desktop
  toast appears; no toast for downloads that finished before
  the app opened.
- Item 9: pick a quality, break it (e.g. an expired manifest),
  watch the retry at the next lower quality and the row note.
- Item 10: set a proxy (bad one first — the save must reject it
  without echoing the value), then a working one — downloads go
  through it.
- Item 11: a YouTube playlist URL — the picker offers
  "Download all N videos"; each video becomes its own row with
  the playlist label.
- Item 12: probe a yt-dlp download for embedded title/chapters.
- Item 13: an audio-only pick with MP3 — the file is `.mp3`.
- Item 14: enable interception, start a browser download of an
  .mp4 — it lands in FetchForge instead; a .pdf downloads
  normally; with the app closed, the .mp4 still downloads in
  the browser (fail-open).

## Known limitations (by design, not bugs)

- A settings change applies to the NEXT download, not to one
  already running (workers load settings once at start).
- Auto-start is bridge-only: with no bridge running, queued
  records still sit until resumed by hand.
- Notifications fire only while the app runs (no background
  notifier).
- Downgrade is ONE step down; a failed re-listing (signed
  manifest link that only worked once) means no downgrade.
- DASH subtitles are WebVTT only; TTML is unsupported.
- No chapters for HLS/DASH; no metadata embedding for direct
  downloads.
- Tool downloads (ffmpeg/yt-dlp first-use fetch) do not use
  the proxy.
- The popup's batch path sends items one by one; the true
  batch endpoint is used by the desktop detector panel.
- Interception has no per-site control, and a file that
  finishes before the pause lands is not intercepted.
- The shutdown check has a tiny race (a new add between the
  disk reload and the command); the 60s abort window is the
  safety valve.

## Deferred (not in this round)

- **Excluded by the user — do NOT build:** download scheduler,
  export/import of the download list, live-stream recording.
  Also not in scope: filename templates, per-segment content
  validation.
- Pairing README text (next-update item B from 2026-11-23).
- Bundling the extension into the installer (round 11, item 12).
