# Fix round 12 — notes (2026-11-25)

Ordered by the user on a voice call 2026-11-25 12:19 PKT, after reading out
the round-12 sweep results (nime / Atlantic / LookMovie / PrimeWire /
FlixTor / Soap2Day / FMovies / Plex retests, bottom-row films, 1080p only),
plus two orders from the same call: app auto-wake when a download starts
while the app is off, and removing YouTube from the Sites tab. Item 14
(legal-alternative note) was added by the user 12:22 PKT; the old item 14
(Soap2Day verify-only) is renumbered 15.

Extension 0.2.10 → **0.2.11** (both manifests).

## How it was built

Two implementers worked in parallel worktrees off `f2f8de3` (round 11 +
CI cleanup) and were merged into `audit-round`:

- Extension side — commit `ba8fb60` (branch `r12-ext`): items 1, 2, 3, 6,
  7, 9, 10, 11, 14, 15.
- Engine + desktop side — commit `3b923f0` (branch `r12-engine`): items
  4, 5, 8, 12, 13.
- Merge commit `e9d0fd9`. This notes file supersedes the two per-side
  fragments (`round12-notes-ext.md`, `round12-notes-engine.md`); their
  content is folded in here.

## Verification (coordinator, run on the merged tree)

- `cargo test --workspace`: **239 passed, 0 failed** (baseline 232; +7
  new — suites 23+4+202+5+1+1+1+2+0). `cargo fmt --check` clean on the
  touched Rust files. One intermediate verification run showed a single
  failure in the known pre-existing timing-sensitive stub-test area
  (the same flake recorded in the round-11 notes; it passes on rerun
  and on CI 10/10); two subsequent full runs were green, so it is not
  a round-12 regression.
- Extension harness `node extensions/harness/run-all.js`: **146 passed,
  0 failed** across 8 files (baseline 116; +30 new in
  `round12.test.js` — background 12, content 32, progress-percent 3,
  round10 26, round11 13, round11ext 11, round12 30, round9 19).
  `node --check` clean on every touched extension JS file.
- Both manifests 0.2.11; chromium/firefox byte-identical on all 6
  shared files (content.js, background.js, popup.js, popup.html,
  hook-main.js, rules/ads.json). `offscreen.js` is chromium-only by
  design (round 11).
- No Tauri command was added or changed (`apps/desktop/src-tauri/
  src/main.rs` untouched; `git diff` confirms) — the round-8/round-10
  call-site lessons do not trigger this round. No engine API signature
  changed (only new pub items + new deps `aes 0.8.4`, `cipher 0.4.4`).
  No workflow YAML touched.
- **Not run:** no laptop retest (no FetchForge on the laptop for this
  agent); the desktop crate cannot be type-checked on this VM (no
  GTK/WebKit dev libs) — Windows CI is the compile gate, as always.
  **Not pushed, no CI dispatched** — the push needs a fresh PAT from
  the user (asked after verification).

## 1. Audio-only row in the bridge-driven formats popup — FIXED

**Defect:** `openFormatsPopup` built rows only from `resp.formats`,
so a listing with no audio-only format offered no audio choice at
all — the novahd, NOXX, 123Movies, ThugTV and MoviesJoy retests all
showed video rows only.

**Fix** (content.js, `openFormatsPopup` success path): when no listed
format has `kind === "audio_only"`, a synthesized "Audio only" row is
appended (last). Its pick sends the same audio-only shape the
extension-side popups already use — `pickAndSend(btn, dl,
{ audioOnly: true })`, no format id, no variant URL — so the engine
extracts the audio from the stream (`download_hls_audio_only` for HLS
manifests, yt-dlp audio extraction for pages). The audio-format select
is now shown whenever the popup offers an audio send. A listing that
already has an audio row gains no second one.

**Tests:** harness — a video-only listing gains the row and its pick
sends `{ audioOnly: true }` with no format id/URL; a listing with an
audio row gains no duplicate.

**Limitation:** the synthesized row sends without a format id; for a
page URL (yt-dlp path) the audio extraction is the engine's existing
audio-only path, not a new one — the laptop retest should confirm an
audio file actually lands for the HLS sites, not just that the row
sends.

## 2. novahd filename overwrite — FIXED

**Defect:** `openFormatsPopup` overwrote the good pre-fill with
yt-dlp's hash title; the `isHashLikeTitle` guard (content.js
~line 1092) missed it because its character class had no `.`.

**Fix:** the class is now `[A-Za-z0-9_\-+=.]` — a no-spaces token
≥20 chars of alnum/dash/dot/underscore is hash-like. Verified true
for the real novahd title
`novahd-anon.0b8833fa881fe689.019314daf61830f3.1795600067`.

**Tests:** harness — the novahd title, the plain hash token, and the
dotted release name are hash-like; spaced titles and short single
words are not.

**Accepted trade-off:** a dotted release name with no spaces
(`The.Odyssey.2026.1080p`) now also reads as hash-like — the popup
then falls back to the page title, which names the film better
anyway. One round-11 harness assertion was updated to the new rule
(it directly contradicted the brief's definition).

## 3. YouTube video duplicates — FIXED

**Defect:** `sortFormatsForPopup` skipped the visible-identity dedupe
for no-measurement formats (the 233/234 audio exemption), so the
size-less "(merged with best audio)" mp4 rows survived twice at
1080p/720p/480p/360p/240p and three times at 144p.

**Fix** (content.js, `sortFormatsForPopup`): the visible-identity
dedupe now applies to VIDEO entries even with no measurements, keyed
on label + kind + height. The exemption stays for AUDIO only:
YouTube's HLS audio entries 233/234 (both "Audio only · mp4", no
bitrate or size, different qualities) must both survive. Measured
formats of any kind keep the full label + kind + bitrate + size key.

**Tests:** harness — a measurement-less 144p triple collapses to one;
the same label at different heights stays; 233/234 survive.

## 4. Archive best-encode — FIXED (engine pick)

**Defect:** listing an `archive.org/download/<id>/<file>` link
returned the single clicked file (the standard 597 MB mp4), so the
popup could only offer "Video · MP4" — never the 1080p mkv.

**Fix** (`crates/engine/src/extract.rs`): `list_formats_with_referer_
and_proxy` tries `try_archive_best_encode(url)` first — fetches the
item's public metadata (`https://archive.org/metadata/<id>`, no auth;
reading a public file list is not a bypass) and returns the best
encode as the single format, with `resolved_url` pointing at it, so
the worker downloads the best file, not the clicked one. Ranking
(`rank_archive_encodes`): height desc, then container (mkv first —
the Archive's high-quality encode — then mp4/webm/mov/ogv), then size
desc. `archive_height_from_filename` scores the FILENAME only (the
round-11 lesson: the `..._1080p` item directory made every file look
1080p); `1080p`/`720p` tokens, or a bare height token (240–4320,
multiple of 120 — a year like 1968 or a bitrate like 512 is not a
height). Top-level files only, video extensions only. Any metadata
failure falls through to yt-dlp, so an outage can only leave the old
behaviour, never break it. Deliberately pick-best, not
offer-all-encodes: the popup needs no change, and the existing
`resolved_url` → `effective_url` plumbing already downloads the
resolved file.

**Verified:** new tests `archive_best_encode_picks_1080p_mkv_from_
metadata`, `archive_height_hint_reads_filename_only`,
`archive_download_parts_only_matches_download_links` (all pass).
Live check against the real item (2026-11-25): the standard mp4 is
`NightOfTheLivingDead.mp4`, 596,808,390 bytes (~597 MB — exactly the
round-11 figure), and the best encode is
`NightOfTheLivingDead_1080p.mkv`, 11,523,767,255 bytes (~11.5 GB);
the ranking picks the mkv over the same-height `..._1080p.mp4` by the
container tie-break. Non-Archive direct-file behaviour is untouched
(the hook only fires for `archive.org/download/<id>/<file>` URLs).

**Limitations:** the metadata fetch adds one HTTPS request per Archive
listing; an item whose files carry no height hint falls back to
biggest-file-wins. Laptop retest must confirm the user wants the
~11.5 GB mkv, not the 597 MB mp4.

## 5. EXT-X-KEY regression (novahd + Atlantic) — FIXED (native AES-128)

**Defect:** the native HLS path refused every `#EXT-X-KEY` playlist
("This stream is encrypted (EXT-X-KEY)..."), while round 9 downloaded
the same streams via yt-dlp.

**Fix** (`crates/engine/src/media.rs`; new deps `aes 0.8.4` +
`cipher 0.4.4` in `crates/engine/Cargo.toml` — CBC is hand-rolled on
the block cipher, so no separate `cbc` crate). Standard HLS AES-128
is now supported natively. This is NOT a DRM bypass: the key is
served openly at the URI the playlist itself declares, and ffmpeg
handles the same streams natively (spec §10's rule is about DRM /
auth / paywall bypass — none of that is touched).
- `parse_hls_segments` parses `#EXT-X-KEY` lines: METHOD=AES-128
  stores the key (URI resolved against the playlist URL, IV parsed
  when declared); METHOD=NONE clears it (plain/encrypted segments
  may mix); any other method (SAMPLE-AES, …) is refused with the DRM
  explanation, as is a KEY with no METHOD or no URI, or a malformed
  IV. Each segment records its key + sequence number
  (`#EXT-X-MEDIA-SEQUENCE` + index).
- `HlsMediaPlaylist::resolve_keys` fetches each distinct key URI
  ONCE (same client/referer as the segments, so signed keys work),
  requiring exactly 16 bytes; a segment with no declared IV decrypts
  with its sequence number as a 128-bit big-endian IV (RFC 8216
  §4.3.2.4). Used by `download_hls_variant_opts` (video + separate
  audio) and `download_hls_pair_opts` (video + audio); the listing
  path keeps using `into_segments` (keyless — counts/sizes only).
- `decrypt_aes128_cbc` decrypts after fetch, before the piece is
  written, in both the parallel and the legacy sequential paths — so
  resumed pieces on disk are always plaintext. Valid PKCS#7 padding is
  stripped; a last byte that is not valid padding leaves the plaintext
  untouched (some packagers emit unpadded block-aligned TS). A
  decryption failure is fatal, not retried (re-fetching the same
  ciphertext with the same key fails the same way). A non-block-
  multiple ciphertext is an error.
- The six blanket `#EXT-X-KEY` refusals in the listing/download paths
  are now `check_hls_encryption_method` (AES-128/NONE pass, anything
  else refuses). Encrypted SUBTITLE playlists are still refused
  (`parse_hls_subtitle_playlist` untouched) — a keyed subtitle track
  fails as a warning and the video continues without subtitles, as
  before.

**Verified:** new tests (all pass) —
`decrypt_aes128_cbc_round_trips_and_strips_pkcs7` (encrypt with the
same crate, decrypt, PKCS#7 strip, unpadded pass-through, bad-length
error, IV parsing),
`parse_hls_segments_resolves_relative_urls_and_parses_aes128_key`
(KEY in force across segments, METHOD=NONE clears it, sequence
numbers, no-URI / bad-IV / SAMPLE-AES refused),
`download_hls_decrypts_aes128_playlist` (local fixture server in the
style of the existing HLS tests: key at `k.bin`, two encrypted
segments — seg0 explicit IV, seg1 default sequence IV — assembled
file is the plaintext, key fetched exactly once),
`download_hls_still_refuses_sample_aes_playlist`. The Widevine/DASH
DRM refusal tests are untouched and still pass (full suite green).

**Limitations:** retest novahd + Atlantic natively — the round-9
yt-dlp route is the fallback if a site's key fetch needs headers the
native client does not send (the key fetch uses the same browser
headers + referer as segment fetches); key rotation mid-playlist
works (per-segment keys), but a resume after a KEY URI change reuses
old decrypted pieces (the segment-dir meta pins count/first-URL/
range, not the key) — an edge case, not hit in the retests;
SAMPLE-AES / FairPlay and Widevine stay refused, honestly.

## 6. Filename pre-fill from stream names — FIXED

**Defect:** the popup filename came from the stream/manifest name
instead of the film title — ThugTV `index_266`, MoviesJoy
`index-s1080p-v1-a1`, nime `index_web`, Soap2Day `video.mp4`, and a
bare `download`.

**Fix** (content.js): new `isStreamLikeName(name)` — a no-spaces token
that is exactly a generic word (`video`, `download`, `index`,
`master`, `playlist`, `manifest`), starts with a manifest stem
(`index_`/`index-`, `master_`, `playlist_`, `manifest_`), or is
`video`/`download` + digits. Refused at every layer, so the page
title always wins over it:
- `stemOfFilename` returns `""` for a stream-like stem (and
  `popupDefaultName` therefore falls through to the page title /
  video-title / h1 / og:title hint, then `download`);
- `popupDefaultName`'s server-title branch and the formats reply's
  title-overwrite branch both skip a stream-like server title
  (`index_web` as a yt-dlp title no longer overwrites a good page
  title);
- `defaultFilenameFor`'s manifest branch routes the verdict filename
  through `stemOfFilename`, so a one-click manifest send falls
  through to `undefined` (the engine names it) instead of saving
  `index-s1080p-v1-a1.m3u8`.
The preference order itself is unchanged: a clean server title still
beats the page title (round 11's rule stands) — item 6's "prefer the
page title" is enforced against *stream* names, not against clean
titles. Direct files keep their real filename (`video.mp4` as an
actual file's name is not a stream name). Composes with item 2:
hash-like server titles are still refused first, by the same branches.

**Tests:** harness — every named stream stem falls through; the page
title and the h1 hint beat a stream stem and a stream-like server
title; a real server title still wins; `defaultFilenameFor` returns
`undefined` for a manifest with no usable title. Two older assertions
were updated to the new rule: round-10's `popupDefaultName` fallback
for `index_287.ts` (now `download`, not `index_287`) and round-11's
`stemOfFilename('index_287.ts')` (now `""`).

## 7. Single-format popup labelling (+ item 11) — FIXED

**Defect:** the bridge returns one format for a media playlist with
`id` = the manifest URL, `ext` = `ts`, `height` = `None`, so the
engine labels it `Video · ts` — no resolution, no explanation.

**Fix** (content.js): new `resolutionFromFormat(f)` (height first,
else a `NNNp` / `NNNxNNN` hint in the format id / label — the variant
name / manifest URL, e.g. `index-s1080p-v1-a1.m3u8`) and
`singleFormatRowLabel(f)`, applied only when the bridge returns
exactly one format: the resolution replaces the bare `Video` lead
(`Video · ts` → `1080p · ts`), keeping the rest of the label. When no
resolution can be found anywhere, the label says so (`Video · ts ·
quality unknown`) and the popup's note line says WHY: "Quality
unknown: the stream declares no resolution, so FetchForge can't say
which quality this is." Multi-format listings are untouched.

**Item 11 (FlixTor):** covered by this fix — no separate code. A lone
format labelled bare `Video` becomes `Video · quality unknown` + the
same WHY note; one whose variant URL carries a resolution shows it.
Verified by harness with a FlixTor-shaped single format.

**Tests:** harness — resolution from variant name, from height, and
the unknown case (label + note); the FlixTor `Video` case.

## 8. BBB audio os error 2 (ffmpeg first-use race) — FIXED (lock)

**Defect:** a job starting while ffmpeg was still being fetched could
fail with "The system cannot find the file specified. (os error 2)"
— a Windows spawn error for a not-yet-present binary.
`ensure_ffmpeg` had no serialisation: two jobs both saw "no ffmpeg",
both downloaded the zip to the same path, and a job could spawn
ffmpeg mid-fetch. Cause is timing-supported, not log-proven — no log
ever captured the race in the act.

**Fix** (`crates/engine/src/ffmpeg.rs`): `ensure_ffmpeg` now takes a
process-wide `OnceLock<Mutex<()>>` first-use lock for the whole
lookup + download + extract (the body moved to
`ensure_ffmpeg_locked`). A second caller waits, then re-checks and
finds the finished binary. A poisoned lock (a panicking holder) is
treated as free rather than failing every later download. All
`ensure_ffmpeg` call sites (bridge + desktop workers,
`download_media`) go through this one function, so the merge /
subtitle / audio-format paths are all covered.

**Verified:** new test `ensure_ffmpeg_serializes_concurrent_first_use`
(8 threads, all return the same binary, no panic/deadlock) + full
suite green. Honest limit: the real first-use download race is
Windows-only and cannot be exercised on this VM, so the download
path itself is verified by code inspection + the suite, not by this
test.

**Left out deliberately:** the same race class exists in
`ensure_ytdlp` (same `download_verified` helper, same shared zip
path) — out of this item's scope; flagged for a later round.

## 9. LookMovie WRONG-FILM SEND (serious) — FIXED

**Diagnosis (traced, not guessed).** The popup's rows are built for
the verdict captured when the popup opened, and every row pick sends
that verdict's URL (`pickAndSend(btn, dl, …)` → `sendCurrentDownload`
uses `dl.url`). The verdict itself could go stale two ways, and the
LookMovie retest hit the combination:
1. **Background election state outlives navigation.**
   `tabObserved` entries live 30 minutes and nothing cleared them
   when the tab navigated — there was no `tabs.onUpdated` listener at
   all (only `onRemoved`). After a same-tab navigation (MoviesJoy →
   LookMovie), the old film's observed playlist stayed eligible, and
   the initiator-origin filter was skipped exactly when the new page's
   frames had not reported yet (`frameOrigins.size === 0` → no
   filtering) — the window in which the button first appears. The
   stale MoviesJoy (flixvision/vidrift) playlist, a master, outranked
   the Mongoose variant in the election, so the button, the popup, and
   the send all carried the old film's URL. (The popup's filename
   field showed the LookMovie page's title — the filename comes from
   the *current document*, not the verdict — which is why the popup
   looked like Mongoose while the send was MoviesJoy.)
2. **No re-validation at pick time.** The verdict-change handler
   normally closes an open popup on a URL change, but a missed
   verdict message (or a stale verdict the background re-elected from
   an old observation) left the popup open on a dead verdict, and the
   row pick then sent it without checking.

**Fix — three layers:**
- background.js: new `clearTabElectionState(tabId)` (clears
  `tabObserved` + `tabOverlayCandidates`) called from a new
  `tabs.onUpdated` listener when `changeInfo.url` is set — a
  navigation commits the tab to a new URL, so the previous page's
  observations and candidates must not elect the new page's stream.
  Frames re-report within a scan.
- background.js, `bestDownloadSource`: an observed finding with NO
  initiator is skipped once frames have reported for the current page
  — it cannot be attributed to any page, so it may be a stale entry
  from a previous navigation (or a previous tab, on tab-id reuse).
  Before any frame reports, observed findings are still the only
  signal (round 9 preserved).
- content.js, `pickAndSend`: re-validates at pick time — if the popup
  is open and the captured `dl.url` no longer matches
  `currentDownload.url` (or there is no current verdict), the pick
  sends nothing, closes the popup, and the button says "Stream
  changed — try again".

**Tests:** harness — fail-before proven on the pre-fix sources (new
tests run against git HEAD: old `bestDownloadSource` elected the
stale MoviesJoy master, old `pickAndSend` sent the stale verdict,
and the old background had no `onUpdated` listener or
`clearTabElectionState` — 10/10 old-code checks behaved as the
pre-fix code). New tests: an unattributed stale observation no
longer beats the current page's stream; an attributed stale
observation is still filtered by origin (control); observed-alone
election still works with no frames (round 9 preserved); the
whole-background test drives a real navigation (MoviesJoy observation
→ `onUpdated` to LookMovie → new observation) and the verdict
switches to the Mongoose stream; a stale pick sends nothing and
closes the popup; a fresh pick still sends; a pick with no current
verdict sends nothing.

**Limitation:** the navigation clear fires on `changeInfo.url` (full
navigations). Same-document (SPA pushState) navigations do not fire
it — the origin filter and the pick-time revalidation are the cover
there. The unattributed-observation filter only applies once frames
have reported; before that, observed findings remain the only signal
(round 9's rule stands).

## 10. PrimeWire home-address send — FIXED (with a caveat)

**Defect:** the player showed "Unable to play media" — no stream ever
loaded — but the elected verdict was a viaPage source whose URL the
background fills with the page address, and the popup's default send
posted that address to the engine, which refused it.

**Fix** (content.js + background.js): the frame that owns the failed
player now flags it instead of offering the page URL as a download. In
`mediaSource`'s blob branch, when no stream was found in the timing
log/captures AND the element carries a media error (`el.error`), it
returns `{ url: null, viaPage: true, noStream: true }` instead of a
plain viaPage source. The flag rides the election
(`reportOverlayCandidates` → background `bestDownloadSource` carries
`noStream` in the verdict), and `onOverlayClick` refuses it: the
button shows "No stream found" ("The player did not load a video on
this page, so there is nothing to download.") and no popup opens,
nothing is sent. A healthy blob player (YouTube: no error) is
untouched, and a failed element WITH an observed stream still returns
the stream (the stream wins over the element error).
`extractHtmlMedia` also drops noStream sources, so the Media Detector
/ toolbar popup never lists the bare page URL as a download.

**Tests:** harness — a failed blob player reports `noStream`; a
healthy blob player returns plain viaPage; a failed player with an
observed stream returns the stream; the detector does not list a
failed player (and still lists a healthy one); the overlay click on a
noStream verdict shows "No stream found" and sends nothing; a healthy
viaPage verdict still opens the formats popup.

**Limitation (honest):** the flag depends on the failed player setting
`video.error`. If the laptop retest shows PrimeWire's player failing
WITHOUT setting it (an MSE/custom player that fails at the fetch level
and only paints "Unable to play media" in its own overlay), this guard
will not fire and the send will still go through — that would need a
different signal (e.g. the player's overlay text), as a follow-up.

## 12. App auto-wake — FIXED (closest working mechanism)

**User order (2026-11-25):** when a download is started while the app
is off, the extension/native host must trigger the app to open
automatically instead of falling back to a typical browser download.

**Investigation first:** the gap was narrower than the order assumed.
The native host ALREADY spawns a detached bridge when no bridge is
listening (`spawn_detached_bridge`, bridge case 2), so the download
itself survives the app being off — what was missing was only the app
UI opening. The desktop app binary (`FetchForge.exe`) ships next to
the bridge binary (installer `stage/FetchForge.exe` + bridge in the
same install folder), so the native host can launch it directly.

**Fix** (`crates/bridge/src/main.rs`):
- `desktop_app_path()`: `FETCHFORGE_APP_PATH` override when it names
  a real file (dev/test, mirroring `FETCHFORGE_FFMPEG_PATH`), else
  `FetchForge.exe` / `FetchForge` next to the bridge exe. None when
  neither exists.
- `wake_desktop_app() -> bool`: spawns the app detached (stdin/
  stdout/stderr null; Windows `CREATE_NO_WINDOW` +
  `DETACHED_PROCESS`), no wait, never an error — a failed wake never
  fails the download. Guarded by `cfg!(test)` exactly like the
  round-11 shutdown guard (commit `d785573`): a test binary never
  launches a real app.
- Call site: the native-host `addDownload` branch where the bridge had
  to be started (`bridge_started: true`) — that is the signal the app
  is off, because the app always spawns a bridge on launch. The reply
  gains `"app_woken": true/false` (ignored by older extension
  versions). The existing shutdown test is untouched and still green.

**Verified:** new test
`auto_wake_finds_app_by_override_and_never_spawns_from_tests`
(finder prefers the override; wake returns false from a test binary
even with a real executable as the override) + full suite green. No
engine API changed; no desktop code changed (the app launches with no
arguments, as normal).

**Limitations, honestly:** (a) the wake fires ONLY when the bridge had
to be started. A bridge already running without the app (a detached
bridge outliving an app close) is not woken — the native host cannot
tell that state apart from "app open with its bridge"; (b) there is no
single-instance plugin in the Tauri app, so a wake in a stale state
could open a second window; (c) NOT verified on the laptop — the spawn
path is Windows-only in effect and this VM cannot run the app.

## 13. Remove YouTube from the Sites tab — FIXED

Deleted the `["YouTube", "https://www.youtube.com"]` line from
`SITE_DIRECTORY` in `apps/desktop/src/index.html` (line 506).
YouTube downloading itself stays working: the only other
`youtube.com` references in that file are in `MEDIA_HOSTS` (a picker
hint for pasted/detected links, not a directory button), and no other
code references the removed entry (grepped `SITE_DIRECTORY`,
`siteList`, `YouTube` across the desktop frontend — the directory is
rendered only from that one array).

**Verified:** `grep -n "YouTube" apps/desktop/src/index.html` returns
nothing; the array edit is a one-line deletion (no JS behaviour
change to test beyond rendering, which needs the installed app).

## 14. Legal-alternative note in the popup (NEW scope, 2026-11-25) — FIXED

**What:** when a stream is DRM-protected or otherwise
access-restricted, the popup shows an "authorized alternative" note
ALONGSIDE the existing honest refusal.

**Implementation (extension-side only; no new engine signal was
needed — the bridge already refuses with a plain error that reaches
the popup as the degraded-popup reason or the send error, e.g. "This
video is DRM-protected. FetchForge does not bypass DRM or
encryption.").** New top-level helpers in content.js:
`isAccessRestrictedReason` (triggers on `drm`, `sign-in`/`sign in`,
`private`) and `legalAlternativeNote` (returning the note or `""`). A
403 / access-denied *fetch* failure deliberately does NOT trigger it
(the MoviesJoy 403 was a missing Referer, not access restriction).
The note is shown in a new second line under the popup subtitle
(`popupNote`, wrapping text, hidden unless set; cleared on every
popup open):
- DRM: "This video is DRM-protected, so FetchForge can't download it.
  You can still watch it on a legal streaming service or in the
  site's own player."
- Sign-in/private: "This video needs a sign-in or is private, so
  FetchForge can't download it. You can still watch it on a legal
  streaming service or in the site's own player."
Wired into `showDegradedPopup` (formats-listing refusal) and
`showPopupSendError` (send refusal). The wording is generic on
purpose: it names NO specific service (never claims a service carries
that title — unverified) and adds NO license-server or DRM-system
integration (that needs official partnerships — out of scope).

**Tests:** harness — the DRM refusal shows the note in the degraded
popup (and names no specific service); a timeout and a 403 show no
note; the helper triggers on DRM / sign-in / private only.

**Limitation:** the note triggers on the refusal's wording (DRM /
sign-in / private). A refusal phrased differently by the engine would
show no note — the refusal itself is still shown.

## 15. Soap2Day notes (old item 14, renumbered — verify-only)

**Verified, no code change.** Grepped the extension: content.js
creates only the floating button + quality popup (shadow DOM,
top-layer popover) and option/label/input elements inside that popup
— no page overlays, no "verification" UI. hook-main.js only wraps
fetch/XHR to forward captured playlist text (observe-only; the
original promises are returned) — no DOM injection, no player
control. There is no `.pause()` / `.play()` call and no "open Run
and paste" text anywhere in the extension sources. The fake "open Run
and paste" verification overlay is the site's own scam content, and
the unpausable player is the site's overlay — neither has a
detection-side cause in the extension. Nothing to fix in FetchForge.

## Files changed (merged round 12)

20 files, +3033/−150 vs `f2f8de3` (before this notes file): extension
content.js/background.js (chromium + firefox, identical), both
manifests 0.2.10 → 0.2.11, harness `round12.test.js` (new, 30 tests)
+ small harness updates, `crates/engine/src/{media,extract,ffmpeg}.rs`,
`crates/engine/Cargo.toml` (+ `aes 0.8.4`, `cipher 0.4.4`),
`Cargo.lock`, `crates/bridge/src/main.rs`, `apps/desktop/src/index.html`
(−1 line). This notes file replaces the two per-side fragments.

## Laptop retest list (needs the installed build + the user's Chrome)

1. novahd — formats popup must now show an "Audio only" row; the
   filename field must keep "The Odyssey · NOVA" after the qualities
   load; the 1080p download must complete natively (AES-128) instead
   of failing with the EXT-X-KEY refusal.
2. Atlantic — Dune: Part Two must download natively (AES-128); the
   popup must show file sizes (bitrates were shown, sizes were not).
3. LookMovie — after watching a film on MoviesJoy (or any site) in
   the same tab, navigating to The Mongoose must download The
   Mongoose, never the previous film. With the popup open, if the
   stream changes, the pick must refuse with "Stream changed — try
   again".
4. PrimeWire — the button must say "No stream found" and send nothing
   when the player shows "Unable to play media". **Confirm the
   player's failure sets `video.error`** (see item 10); if it does
   not, this item needs a follow-up signal.
5. ThugTV / MoviesJoy / nime / Soap2Day — the filename field must
   show the film title (page title / h1), never `index_266`,
   `index-s1080p-v1-a1`, `index_web`, `video.mp4`, or `download`.
6. YouTube — no duplicate merged-video rows in the formats popup;
   233/234 both present.
7. Internet Archive — the popup must now offer/list the 1080p mkv and
   download it (~11.5 GB — confirm the user wants the big file, not
   the 597 MB one).
8. FlixTor — the single `Video` row must now be labelled (resolution
   or `quality unknown` + the WHY note).
9. 123Movies / NOXX — the formats popup must show an "Audio only"
   row; picking it must deliver an audio file.
10. Plex (or any DRM page) — the popup must show the refusal AND the
    "legal streaming service" note; a sign-in-only page shows the
    sign-in variant; an ordinary failure shows no note.
11. App auto-wake — (a) close the app completely, start a download
    from the extension, confirm the FetchForge window opens AND the
    download runs in it; (b) with the app already open, start another
    extension download and confirm NO second window appears; (c) check
    `bridge.log` for the wake lines on failure.
12. Sites tab — no YouTube button; a pasted YouTube link still
    downloads.
13. Big Buck Bunny audio — retry now that ffmpeg is fetched; the
    first-use lock should prevent the os error 2 race.
14. Soap2Day — nothing changed; confirm the fake verification overlay
    is still the site's own (it is not ours).

## Known limitations (by design, not bugs)

- Item 2's dot rule treats dotted release names without spaces as
  hash-like; the fallback is the page title, which is the preferred
  name anyway.
- Item 9's navigation clear fires on `changeInfo.url` (full
  navigations). Same-document (SPA pushState) navigations do not fire
  it — the origin filter and the pick-time revalidation are the cover
  there.
- Item 10's no-stream flag depends on `video.error` being set by the
  failed player.
- Item 12's wake fires only when the bridge had to be started; a
  detached bridge outliving an app close is not woken, and there is no
  single-instance plugin, so a stale-state wake could open a second
  window.
- Item 14's note triggers on the refusal's wording (DRM / sign-in /
  private). A refusal phrased differently shows no note — the refusal
  itself is still shown.
- Item 8's `ensure_ytdlp` has the same first-use race class — out of
  scope, flagged for a later round.
- Item 5's key-rotation resume edge case (segment-dir meta does not
  pin the key) and encrypted subtitle playlists staying refused (video
  continues without subtitles).
- Item 4 adds one HTTPS metadata request per Archive listing; an item
  with no height hints falls back to biggest-file-wins.
