# Fix round 12, extension — notes (2026-11-25)

Extension side of round 12 (items 1, 2, 3, 6, 7, 9, 10, 11, 14, 15).
Extension 0.2.10 → **0.2.11** (both manifests). No engine, bridge,
desktop, or CI files touched — extension + harness only.

Baseline (measured this round, before any edit): extension
harnesses **116 passed, 0 failed** across 7 files (round 11 ext
total). After: **143 passed, 0 failed** across 8 files
(+30 new in `round12.test.js`, −3 renamed/updated assertions
below — no test was weakened; the three updated assertions encode
rules this round explicitly redefined, see items 2 and 6).

## 1. Audio-only row in the bridge-driven formats popup

**The defect:** `openFormatsPopup` built rows only from
`resp.formats`, so a listing with no audio-only format offered no
audio choice at all — the novahd, NOXX, 123Movies, ThugTV and
MoviesJoy retests all showed video rows only.

**The fix** (content.js, `openFormatsPopup` success path): when no
listed format has `kind === "audio_only"`, a synthesized
"Audio only" row is appended (last). Its pick sends the same
audio-only shape the extension-side popups already use —
`pickAndSend(btn, dl, { audioOnly: true })`, no format id, no
variant URL — so the engine extracts the audio from the stream
(`download_hls_audio_only` for HLS manifests, yt-dlp audio
extraction for pages). The audio-format select is now shown
whenever the popup offers an audio send (listed or synthesized),
which in the success path is always. A listing that already has
an audio row gains no second one.

**Tests:** harness — a video-only listing gains the row and its
pick sends `{ audioOnly: true }` with no format id/URL; a listing
with an audio row gains no duplicate.

## 2. novahd filename overwrite — dots join the hash class

**The fix** (content.js, `isHashLikeTitle`): the character class
is now `[A-Za-z0-9_\-+=.]` — a no-spaces token ≥20 chars of
alnum/dash/dot/underscore is hash-like. Verified against the real
novahd title
`novahd-anon.0b8833fa881fe689.019314daf61830f3.1795600067` (true:
dotted segments, no spaces). **Accepted trade-off:** a dotted
release name with no spaces (`The.Odyssey.2026.1080p`) now also
reads as hash-like — the popup then falls back to the page title,
which names the film better anyway. The round-11 harness
assertion for that string was updated to the new rule (it
directly contradicted the brief's definition).

**Tests:** harness — the novahd title, the plain hash token, and
the dotted release name are hash-like; spaced titles and short
single words are not.

## 3. YouTube video duplicates — dedupe without measurements

**The fix** (content.js, `sortFormatsForPopup`): the
visible-identity dedupe now applies to VIDEO entries even with
no measurements, keyed on label + kind + height (a merged video
row listed twice with no bitrate/size is still the same stream).
The exemption stays for AUDIO only: YouTube's HLS audio entries
233/234 (both "Audio only · mp4", no bitrate or size, different
qualities) must both survive. Measured formats of any kind keep
the full label + kind + bitrate + size key.

**Tests:** harness — a measurement-less 144p triple collapses to
one; the same label at different heights stays; 233/234 survive.

## 6. Filename pre-fill from stream names

**The defect:** the popup filename came from the stream/manifest
name instead of the film title — ThugTV `index_266`, MoviesJoy
`index-s1080p-v1-a1`, nime `index_web`, Soap2Day `video.mp4`, and
a bare `download`.

**The fix** (content.js): new `isStreamLikeName(name)` — a
no-spaces token that is exactly a generic word (`video`,
`download`, `index`, `master`, `playlist`, `manifest`), starts
with a manifest stem (`index_`/`index-`, `master_`,
`playlist_`, `manifest_`), or is `video`/`download` + digits.
Refused at every layer, so the page title always wins over it:
- `stemOfFilename` returns `""` for a stream-like stem (and
  `popupDefaultName` therefore falls through to the page title /
  video-title / h1 / og:title hint, then `download`);
- `popupDefaultName`'s server-title branch and the formats
  reply's title-overwrite branch both skip a stream-like server
  title (`index_web` as a yt-dlp title no longer overwrites a
  good page title);
- `defaultFilenameFor`'s manifest branch routes the verdict
  filename through `stemOfFilename`, so a one-click manifest
  send falls through to `undefined` (the engine names it)
  instead of saving `index-s1080p-v1-a1.m3u8`.
The preference order itself is unchanged: a clean server title
still beats the page title (round 11's rule stands) — item 6's
"prefer the page title" is enforced against *stream* names, not
against clean titles. Direct files keep their real filename
(`video.mp4` as an actual file's name is not a stream name).
Composes with item 2: hash-like server titles are still refused
first, by the same branches.

**Tests:** harness — every named stream stem falls through; the
page title and the h1 hint beat a stream stem and a stream-like
server title; a real server title still wins; `defaultFilenameFor`
returns `undefined` for a manifest with no usable title.
Two older assertions were updated to the new rule: round-10's
`popupDefaultName` fallback for `index_287.ts` (now `download`,
not `index_287`) and round-11's `stemOfFilename('index_287.ts')`
(now `""`).

## 7. Single-format popup labelling (+ item 11)

**The defect:** the bridge returns one format for a media
playlist with `id` = the manifest URL, `ext` = `ts`, `height` =
`None`, so the engine labels it `Video · ts` — no resolution,
no explanation.

**The fix** (content.js): new `resolutionFromFormat(f)` (height
first, else a `NNNp` / `NNNxNNN` hint in the format id / label —
the variant name / manifest URL, e.g. `index-s1080p-v1-a1.m3u8`)
and `singleFormatRowLabel(f)`, applied only when the bridge
returns exactly one format: the resolution replaces the bare
`Video` lead (`Video · ts` → `1080p · ts`), keeping the rest of
the label. When no resolution can be found anywhere, the label
says so (`Video · ts · quality unknown`) and the popup's note
line says WHY: "Quality unknown: the stream declares no
resolution, so FetchForge can't say which quality this is."
Multi-format listings are untouched.

**Item 11 (FlixTor):** covered by this fix — no separate code.
A lone format labelled bare `Video` becomes `Video · quality
unknown` + the same WHY note; one whose variant URL carries a
resolution shows it. Verified by harness with a FlixTor-shaped
single format.

**Tests:** harness — resolution from variant name, from height,
and the unknown case (label + note); the FlixTor `Video` case.

## 9. LookMovie WRONG-FILM SEND (serious)

**Diagnosis (traced, not guessed).** The popup's rows are built
for the verdict captured when the popup opened, and every row
pick sends that verdict's URL (`pickAndSend(btn, dl, …)` →
`sendCurrentDownload` uses `dl.url`). The verdict itself could
go stale two ways, and the LookMovie retest hit the combination:
1. **Background election state outlives navigation.**
   `tabObserved` entries live 30 minutes and nothing cleared
   them when the tab navigated — there was no `tabs.onUpdated`
   listener at all (only `onRemoved`). After a same-tab
   navigation (MoviesJoy → LookMovie), the old film's observed
   playlist stayed eligible, and the initiator-origin filter was
   skipped exactly when the new page's frames had not reported
   yet (`frameOrigins.size === 0` → no filtering) — the window
   in which the button first appears. The stale MoviesJoy
   (flixvision/vidrift) playlist, a master, outranked the
   Mongoose variant in the election, so the button, the popup,
   and the send all carried the old film's URL. (The popup's
   filename field showed the LookMovie page's title — the
   filename comes from the *current document*, not the verdict
   — which is why the popup looked like Mongoose while the send
   was MoviesJoy.)
2. **No re-validation at pick time.** The verdict-change handler
   normally closes an open popup on a URL change, but a missed
   verdict message (or a stale verdict the background re-elected
   from an old observation) left the popup open on a dead
   verdict, and the row pick then sent it without checking.

**The fix — three layers:**
- background.js: new `clearTabElectionState(tabId)` (clears
  `tabObserved` + `tabOverlayCandidates`) called from a new
  `tabs.onUpdated` listener when `changeInfo.url` is set — a
  navigation commits the tab to a new URL, so the previous
  page's observations and candidates must not elect the new
  page's stream. Frames re-report within a scan.
- background.js, `bestDownloadSource`: an observed finding with
  NO initiator is skipped once frames have reported for the
  current page — it cannot be attributed to any page, so it may
  be a stale entry from a previous navigation (or a previous
  tab, on tab-id reuse). Before any frame reports, observed
  findings are still the only signal (round 9 preserved).
- content.js, `pickAndSend`: re-validates at pick time — if the
  popup is open and the captured `dl.url` no longer matches
  `currentDownload.url` (or there is no current verdict), the
  pick sends nothing, closes the popup, and the button says
  "Stream changed — try again".

**Tests:** harness — fail-before proven on the pre-fix sources
(see below). New tests: an unattributed stale observation no
longer beats the current page's stream; an attributed stale
observation is still filtered by origin (control); observed-alone
election still works with no frames (round 9 preserved); the
whole-background test drives a real navigation (MoviesJoy
observation → `onUpdated` to LookMovie → new observation) and
the verdict switches to the Mongoose stream; a stale pick sends
nothing and closes the popup; a fresh pick still sends; a pick
with no current verdict sends nothing.

**Fail-before proof:** the new tests were run against the
pre-fix sources (from git HEAD) via a targeted check
(`/tmp/r12-before-check.js`, not committed): old
`bestDownloadSource` elected the stale MoviesJoy master, old
`pickAndSend` sent the stale verdict, and the old background had
no `onUpdated` listener or `clearTabElectionState` — 10/10
old-code checks behaved as the pre-fix code.

## 10. PrimeWire home-address send

**The defect:** the player showed "Unable to play media" — no
stream ever loaded — but the elected verdict was a viaPage
source whose URL the background fills with the page address,
and the popup's default send posted that address to the engine,
which refused it.

**The fix** (content.js + background.js): the frame that owns
the failed player now flags it instead of offering the page URL
as a download. In `mediaSource`'s blob branch, when no stream
was found in the timing log/captures AND the element carries a
media error (`el.error`), it returns `{ url: null, viaPage: true,
noStream: true }` instead of a plain viaPage source. The flag
rides the election (`reportOverlayCandidates` → background
`bestDownloadSource` carries `noStream` in the verdict), and
`onOverlayClick` refuses it: the button shows "No stream found"
("The player did not load a video on this page, so there is
nothing to download.") and no popup opens, nothing is sent. A
healthy blob player (YouTube: no error) is untouched, and a
failed element WITH an observed stream still returns the stream
(the stream wins over the element error). `extractHtmlMedia`
also drops noStream sources, so the Media Detector / toolbar
popup never lists the bare page URL as a download.

**Tests:** harness — a failed blob player reports `noStream`;
a healthy blob player returns plain viaPage; a failed player
with an observed stream returns the stream; the detector does
not list a failed player (and still lists a healthy one); the
overlay click on a noStream verdict shows "No stream found"
and sends nothing; a healthy viaPage verdict still opens the
formats popup.

**Limitation (honest):** the flag depends on the failed player
setting `video.error`. If the laptop retest shows PrimeWire's
player failing WITHOUT setting it (an MSE/custom player that
fails at the fetch level and only paints "Unable to play media"
in its own overlay), this guard will not fire and the send will
still go through — that would need a different signal (e.g. the
player's overlay text), as a follow-up. Retest item below.

## 14. Legal-alternative note in the popup (NEW scope, 2026-11-25)

**What:** when a stream is DRM-protected or otherwise
access-restricted, the popup shows an "authorized alternative"
note ALONGSIDE the existing honest refusal.

**Implementation (extension-side only; no new engine signal was
needed — the bridge already refuses with a plain error that
reaches the popup as the degraded-popup reason or the send
error, e.g. "This video is DRM-protected. FetchForge does not
bypass DRM or encryption.").** New top-level helpers in
content.js: `isAccessRestrictedReason` (triggers on `drm`,
`sign-in`/`sign in`, `private`) and `legalAlternativeNote`
(returning the note or `""`). A 403 / access-denied *fetch*
failure deliberately does NOT trigger it (the MoviesJoy 403 was
a missing Referer, not access restriction). The note is shown in
a new second line under the popup subtitle (`popupNote`,
wrapping text, hidden unless set; cleared on every popup open):
- DRM: "This video is DRM-protected, so FetchForge can't
  download it. You can still watch it on a legal streaming
  service or in the site's own player."
- Sign-in/private: "This video needs a sign-in or is private,
  so FetchForge can't download it. You can still watch it on a
  legal streaming service or in the site's own player."
Wired into `showDegradedPopup` (formats-listing refusal) and
`showPopupSendError` (send refusal). The wording is generic on
purpose: it names NO specific service (never claims a service
carries that title — unverified) and adds NO license-server or
DRM-system integration (that needs official partnerships — out
of scope).

**Tests:** harness — the DRM refusal shows the note in the
degraded popup (and names no specific service); a timeout and a
403 show no note; the helper triggers on DRM / sign-in /
private only.

## 15. Soap2Day notes (old item 14, renumbered — verify-only)

**Verified, no code change.** Grepped the extension: content.js
creates only the floating button + quality popup (shadow DOM,
top-layer popover) and option/label/input elements inside that
popup — no page overlays, no "verification" UI. hook-main.js
only wraps fetch/XHR to forward captured playlist text
(observe-only; the original promises are returned) — no DOM
injection, no player control. There is no `.pause()` / `.play()`
call and no "open Run and paste" text anywhere in the extension
sources. The fake "open Run and paste" verification overlay is
the site's own scam content, and the unpausable player is the
site's overlay — neither has a detection-side cause in the
extension. Nothing to fix in FetchForge.

## Version bump

Both manifests 0.2.10 → **0.2.11**. No other manifest changes.
Chromium/firefox byte-identical on all 6 shared files
(content.js, background.js, popup.js, popup.html, hook-main.js,
rules/ads.json). `offscreen.js` is chromium-only by design
(round 11) and is not a shared file.

## Tests

- **Extension harness:** **143 passed, 0 failed** across 8 files
  (baseline 116; +30 new in `round12.test.js`, −3 updated
  assertions in round10/round11 harnesses per items 2/6, +1
  updated row-count assertion in content.test.js per item 1).
- **node --check:** all touched extension JS files (chromium +
  firefox content/background/popup/hook-main, chromium
  offscreen.js, harness round12.test.js).
- **Fail-before:** the new behavioral tests were checked against
  the pre-fix sources from git HEAD — old code kept the 144p
  triple, missed the dotted novahd title, elected the stale
  MoviesJoy master, sent the stale verdict from `pickAndSend`,
  returned a plain viaPage source for a failed player, and kept
  `index_266` as a filename (10/10 old-code checks).
- **Not run:** cargo (no engine files touched); no laptop
  retest (not available to this agent).

## NOT verified (needs the laptop + the user's browser)

- Item 1: novahd / NOXX / 123Movies / ThugTV / MoviesJoy — the
  formats popup must now show an "Audio only" row; picking it
  must deliver an audio file (same send shape as the existing
  HLS audio rows — confirm the file lands, not just that the row
  sends).
- Item 2: novahd — the filename field must keep "The Odyssey ·
  NOVA" after the qualities load (the dotted hash title must not
  overwrite it).
- Item 3: YouTube — no duplicate merged-video rows in the
  formats popup; 233/234 both present.
- Item 6: ThugTV / MoviesJoy / nime / Soap2Day / LookMovie —
  the filename field must show the film title (page title / h1),
  never `index_266`, `index-s1080p-v1-a1`, `index_web`,
  `video.mp4`, or `download`.
- Item 7: a single-format stream (media playlist) must show its
  resolution (e.g. `1080p · ts`) or `quality unknown` + the WHY
  note when the stream declares none. FlixTor: the single
  `Video` row must now be labelled.
- Item 9: LookMovie — after watching a film on MoviesJoy (or
  any site) in the same tab, navigating to The Mongoose must
  download The Mongoose, never the previous film. Also: with the
  popup open, if the stream changes, the pick must refuse with
  "Stream changed — try again".
- Item 10: PrimeWire — the button must say "No stream found"
  and send nothing when the player shows "Unable to play
  media". **Confirm the player's failure sets `video.error`**
  (see Limitation above); if it does not, this item needs a
  follow-up signal.
- Item 14: Plex (or any DRM page) — the popup must show the
  refusal AND the "legal streaming service" note; a sign-in-only
  page shows the sign-in variant; an ordinary failure shows no
  note.
- Item 15: Soap2Day — nothing changed; confirm the fake
  verification overlay is still the site's own (it is not ours).

## Known limitations (by design, not bugs)

- Item 2's dot rule treats dotted release names without spaces
  as hash-like; the fallback is the page title, which is the
  preferred name anyway.
- Item 9's navigation clear fires on `changeInfo.url` (full
  navigations). Same-document (SPA pushState) navigations do not
  fire it — the origin filter and the pick-time revalidation are
  the cover there.
- Item 9's unattributed-observation filter only applies once
  frames have reported; before that, observed findings remain
  the only signal (round 9's rule stands).
- Item 10's no-stream flag depends on `video.error` being set
  by the failed player (see above).
- Item 14's note triggers on the refusal's wording (DRM /
  sign-in / private). A refusal phrased differently by the
  engine would show no note — the refusal itself is still shown.
- The synthesized audio row sends without a format id; for a
  page URL (yt-dlp path) the audio extraction is the engine's
  existing audio-only path, not a new one — the laptop retest
  should confirm an audio file lands for the HLS sites.
