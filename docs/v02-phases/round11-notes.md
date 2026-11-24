# Fix round 11 — notes (2026-11-24)

Fixes for everything the 2026-11-24 round-10 retests found
(items 1–5, diagnosed from the live retests), four layout changes
the user dictated on the 17:10 PKT voice call (items 6–9), and the
study-driven work he ordered at 17:13–17:16 PKT (items 10–13:
exact total size, progress display, extension auto-load, filename
fixes). The downloader study that drove items 10–13 is committed
with this round: `docs/v02-phases/downloader-size-and-features-study.md`.
Extension 0.2.9 → **0.2.10**.

Scope: engine, bridge, desktop (Rust + frontend), and extension.
The desktop crate cannot be type-checked on this VM — Windows CI
is the compile gate — so every engine API change was followed by
a grep of `apps/desktop/src-tauri/src/main.rs` for affected call
sites (round-8 lesson) and a check that every async Tauri command
returns a `Result` (round-10 lesson; verified at the end of the
round: all 11 async commands return `Result`, and the four
`DownloadRecord` literals in the desktop crate carry the two new
fields).

Baseline (measured this round, before any edit): commit
`30653d6` (round-10 CI fixup), cargo **199** passing, extension
harnesses **92** passing.

## 1. Popup fallthrough (NOXX + 123Movies)

**The defect:** on NOXX (15:36–15:42 PKT) and 123Movies
(15:42–15:53 PKT) the button clicked ONCE and sent one-click
with NO popup. Root cause: the stream URL is extensionless
(`https://firandfable.space/pl/IG6fqf...?token=...`) and its
response Content-Type did not classify as a playlist MIME, so
the election carried kind "file" and the URL failed every popup
branch in `onOverlayClick` (masterText, kind==="playlist",
viaPage, isDashUrl, mp4-family clip guard,
isManifestUrl/isVideoFileUrl) and reached the final one-click
`sendCurrentDownload`.

**The fix:** a kind-based fallthrough. The final branch now
routes to `openFormatsPopup` for any verdict that is not a
named non-video file: the bridge sniffs the URL (round 10,
item 4) and lists its qualities, or degrades to the single
known quality. The one-click send survives only for URLs that
name a non-video kind — `isOneClickFileUrl` (content.js):
images, 3D models, documents, and direct audio files — where
there is no quality or audio choice to make. The overlay
election only ever holds video/audio streams (frame candidates
come from `<video>` elements; observed findings are classified
video/audio media; embedded-scan fallbacks are stream links),
so an extensionless verdict reaching the fallthrough is a
stream the URL cannot name, and a popup is always correct for
it.

**Tests:** harness — an extensionless file-kind verdict opens
the formats popup, never sends; image, 3D, PDF and audio URLs
still one-click send. The round-10 test "a file-kind verdict
keeps the one-click send" was renamed and re-scoped to the
named-image case (its URL was photo.jpg all along).

## 2. Novahd filename overwrite

**The defect:** in the novahd retest the filename field started
pre-filled "The Odyssey · NOVA" (good) but CHANGED to a hash
string once the qualities loaded. `openFormatsPopup`
overwrote the field with the bridge's `resp.title`
(content.js:1748-1750), meant to upgrade to yt-dlp's clean
title — but for novahd, yt-dlp's title for the extensionless
playlist URL is the URL's hash segment.

**The fix:** `isHashLikeTitle` (content.js) — true for a
single token of 20+ characters from `[A-Za-z0-9_-+=]` with no
spaces, dots or slashes. Real titles always contain spaces or
punctuation a token lacks; the 20-character minimum keeps short
single-word titles ("Dune", "Inception") safe; dots are
excluded so a dotted release name ("The.Odyssey.2026.1080p")
is not mistaken for a hash. `openFormatsPopup` now skips BOTH
the filename overwrite AND the subtitle when `resp.title` is
hash-like. `popupDefaultName` skips a hash-like server title
the same way.

**Tests:** harness — the hash title is spotted, real titles
are not; a hash-like server title does not overwrite the
pre-fill and is not shown as the subtitle; a clean server
title still upgrades the field.

## 3. YouTube duplicate formats

**The defect:** the formats popup listed identical entries
twice (m4a 129 kbps 9.8 MB twice; 49 kbps m4a twice; mp4 audio
twice; merged video entries duplicated at 1080p/720p/480p/
360p/240p twice each, 144p THREE times). Cause: both dedupes
keyed on format_id only, but yt-dlp lists the SAME stream
under multiple ids (140 vs 140-drc).

**The fix:** dedupe on visible identity (label + kind +
bitrate + size) in BOTH places — `parse_media_info`
(extract.rs, after the existing id-dedupe) and
`sortFormatsForPopup` (content.js). The first occurrence wins.
A format with NO measurement at all (no bitrate, no size)
skips the visible dedupe: its identity is too thin to prove
two entries are the same stream. That rule matters — the
first version of this fix merged YouTube's HLS audio entries
233/234 (both "Audio only · mp4", no bitrate or size in the
fixture, yet different qualities) and broke three cargo tests;
the measurement rule keeps them while still collapsing every
reported duplicate (all of which carry bitrate + size).

**Tests:** cargo — 140/140-drc collapse, 233/234 survive;
harness — same, in the popup. The round-10 popup test
"id-less rows dedupe by label + kind" was updated to the
round-11 rule (measured rows collapse, measurement-less rows
stay).

## 4. Archive best-encode

**The defect:** the Internet Archive pick improved to the
standard mp4 (597 MB) but still did not reach the 1080p mkv.
The 1080p file lives in the Archive's file list as an `<a>`
link, not in a `<source>` element, so ranking `<source>`
elements alone never saw it.

**The fix:** `sameDirectoryEncodes` (content.js) — when NO
source is playing, same-directory `<a>` links to media files
join the ranking pool. The same directory is a strong signal
they are encodes of the same item, not other videos. A
playing currentSrc still always wins (round 10, item 8's rule
stands — the playing source is the site's choice).

Two things the harness caught while building this:
- `qualityHintScore` scored the WHOLE URL, so the Archive's
  item directory ("Night.Of.The.Living.Dead_1080p") made
  every file in it look 1080p and the standard mp4 tied the
  real 1080p mkv. It now scores the FILENAME only. This also
  fixed the round-10 Archive ranking: the 512kb file scores
  512 (its "1080p" is followed by "_", no word boundary) and
  the plain 1080p file scores 11080, so the better encode
  wins instead of document order keeping the worse file. The
  round-10 test was updated to the corrected expectation.
- A tie between two genuinely-1080p files (mp4 and mkv) keeps
  document order — either is a fine pick.

**Tests:** harness — with no playing source, a same-directory
1080p mkv link wins over the standard mp4; a playing
currentSrc still wins; hints are read from the filename, not
the directory.

## 5. MoviesJoy popup 403

**The defect:** in the MoviesJoy retest the popup appeared but
its manifest fetch failed HTTP 403, so it offered only
"Video · default quality" (no ladder, no Audio only) — while
the engine fetched the same manifest fine. The extension's
popup-side fetch lacked the engine's browser-like headers;
vidrift-class hosts 403 a manifest fetch without the page
Referer they expect.

**The fix:** the referer is threaded through the formats path
(the cleaner route — the popup already asks the bridge, so no
new fetch was needed anywhere): content.js
`openFormatsPopup` sends `dl.referer` with the `getFormats`
message; background.js `getFormats(url, referer)` sends it in
the POST body (falling back to the sender tab's URL when the
verdict carries none); the bridge's `FormatsRequest` gains
`#[serde(default)] referer: Option<String>` and calls the new
`list_formats_with_referer(data_dir, url, referer)` (extract.rs),
which passes the referer to `sniff_stream_kind` and
`list_stream_formats` (the old `list_formats` delegates with
`None`). The desktop `media_formats` command gains the same
optional `referer` parameter (already async + `Result`; the
frontend sends `{url}` only, and Tauri maps the missing
`Option<String>` to `None`).

**Tests:** cargo — a local server records the Referer header
on every manifest fetch (master + default variant); harness —
`getFormats` sends the referer in the POST body, and omits
the key entirely when there is none.

## 6. Theme toggle → pill/slider switch

**The change (user-dictated):** the "Light mode"/"Dark mode"
text button is now a pill/slider switch: a moon icon for dark
(left), a bulb icon for light (right), and a knob that slides
side to side. The knob position follows `[data-theme]` in CSS
(dark is the default, no attribute set). The button keeps
`id="themeBtn"` and gains `role="switch"`, `aria-checked`, and
an `aria-label`/`title` that name the current mode; the choice
still persists in localStorage and is still applied before
first paint. Inline SVG icons in the static HTML (feather-style
moon path; a simple bulb path).

**Note for the retest:** the round-10 DevTools theme check
(`check-r10-themes.js`) reads `themeBtn.textContent`
("Light mode"/"Dark mode") — that text no longer exists. The
retest script must read `aria-checked`/`aria-label` and the
knob position instead.

**Tests:** node --check on the inline script blocks; visual
verification needs the installed app (see "NOT verified").

## 7. Downloads-table columns resizable by dragging

**The change (user-dictated):** a drag handle on the right
edge of every header cell except the last (which takes the
remaining width). Minimum 60px. Widths persist in localStorage
(`fetchforge-col-widths`) and are restored on load, so a long
filename can be given room once instead of pushing the other
columns to the far end on every refresh. Implemented in
index.html (`initColumnResize`, called at startup) with a
`.col-resize` handle style in styles.css.

**Tests:** node --check; behaviour needs the installed app
(see "NOT verified").

## 8. Speed column fix

**The defect:** the Speed column hardcoded `"-"` for every row
(index.html:632). The engine already computed the rate —
`Progress.bytes_per_sec` exists on every download path — but
no record field carried it to the UI.

**The fix:** `DownloadRecord.speed_bps: Option<f64>`
(`#[serde(default)]`); `apply_progress` stores
`p.bytes_per_sec` when it is positive (a 0 report at the very
start must not overwrite a real speed). The UI shows a
`fmtSpeed` value (B/s, KB/s, MB/s) for Downloading rows only
— a paused row keeps its last speed in the record, but showing
it would be a stale speed — and `"-"` otherwise.

**Tests:** cargo — `apply_progress` stores the speed, a 0
report does not overwrite it; harness — `fmtSpeed` renders
B/s, KB/s, MB/s.

## 9. Actions in ONE ROW, side by side

**The change (user-dictated):** pause/play toggle FIRST,
delete icon SECOND, in one row. This REVERSES the round-10
vertical stack. Only a CSS change: `.actions-cell` is now
`flex-direction: row` (the DOM order is already
toggle-then-delete). The double-confirmation delete dialog and
its wording are untouched.

**Tests:** visual verification needs the installed app (see
"NOT verified").

## 10. Exact total size (study doc §0–§3)

**The study's finding:** no mechanism gives an exact
pre-download total for a typical movie-site HLS stream —
yt-dlp itself returns `filesize=None` for manifest formats.
The "exact size" seen elsewhere is direct files
(Content-Length), YouTube/Drive API lengths, or byte-ranged
streams — all of which FetchForge already handled. Round 11
implements the study's recommendation: the XDM estimate
ladder, honest markers, and learn-as-you-go.

**The ladder** (`SizeKind`, media.rs, serde lowercase):
`Exact` (byte-range sum / Content-Length / extractor length)
→ `Average` (`AVERAGE-BANDWIDTH × duration`, usually within
~1%) → `Peak` (peak `BANDWIDTH × duration`, often 2–4× high)
→ `Measured` (running average during the download). Markers:
exact plain, average `≈`, peak/measured `~`.
`SizeKind::weaker` combines kinds worst-first for merged
totals.

**What changed:**
- `HlsVariant` gains `average_bandwidth: Option<u64>`;
  `parse_hls_master` reads `AVERAGE-BANDWIDTH`.
- `list_hls_formats` is restructured: the default variant's
  playlist is fetched FIRST (it was fetched after the formats
  were built), so the duration is available for the ladder.
  Each format's size is the exact byte-range sum for the
  default variant when its playlist is ranged (Atlantic-style),
  else `AVERAGE-BANDWIDTH × duration`, else peak `BANDWIDTH ×
  duration`. The media-playlist case gets the exact sum when
  ranged, else nothing (it declares no bandwidth). Other
  variants keep the bandwidth estimate — fetching every
  variant's playlist to total its ranges would multiply
  manifest requests for a listing the user may not pick from.
- `list_dash_formats`: `BANDWIDTH × duration` (Peak) per
  representation; the default representation gets the exact
  sum when its segments are all ranged (SegmentList with
  `@mediaRange`). DASH declares no average bandwidth, so
  there is no middle rung.
- `MediaFormat` gains `size_kind: Option<SizeKind>`; the
  `filesize_approx` bool stays for serde compat and mirrors
  kind != Exact. `format_label`'s `filesize_approx: bool`
  parameter became `size_kind: Option<SizeKind>` (all 3 call
  sites updated); a missing kind with a size is treated as
  exact. `stream_info_to_media_info` passes the kind through.
- `MediaPlan` gains `size_kind`; `plan_from_info` computes it
  mirroring `est_size` (a reported size keeps its kind, a
  bitrate × duration fallback is Peak) and combines merged
  totals worst-kind-wins. Both workers set
  `updated.size_kind = plan.size_kind` next to the total.
- `DownloadRecord` gains `size_kind: Option<SizeKind>`.
  Direct downloads with a probed Content-Length get Exact at
  creation (bridge + desktop). `apply_progress` sets Measured
  when the measured estimate is the only size known and no
  plan kind was set — a plan kind is never downgraded.
  `complete_with_file` sets Exact (the file on disk is the
  exact total).
- The UI's `displayTotal` (index.html) shows the plan total
  with its marker, else the measured estimate with `~`.

**Learn-as-you-go — what was and was not done:** the study
recommended Motrix's pattern (learn each segment's size from
response headers as it starts, so the total converges to
exact). That pattern is mathematically identical to the
running-average estimate FetchForge already keeps in
`estimated_total_bytes` (sum of known segment sizes +
average × remaining = average × count), so no new mechanism
was needed for the convergence itself — the round-11 addition
is the Measured labelling and the marker. One promotion was
deliberately NOT implemented: promoting the estimate to Exact
when all segments land. A two-phase download (DASH video then
audio) reports done == total at the end of phase 1, and
promoting then would show 100% with the video phase's total
alone. The exact total arrives at completion, when
`complete_with_file` reconciles against the real file. No
pre-HEAD of all segments (2,000+ requests = the nime 429
failure mode).

**Tests:** cargo — AVERAGE-BANDWIDTH parsing; the estimate
ladder (average preferred over peak, peak fallback, none
without duration/bandwidth); DASH peak estimate; markers and
`weaker`; label markers (exact plain, `~`, `≈`); plan kinds
(Peak on the bitrate fallback, Exact on a reported filesize,
worst-kind-wins on merge); `apply_progress` speed + Measured
labelling (never downgrading a plan kind);
`complete_with_file` marking Exact. Harness — `displayTotal`
markers and the estimate fallback.

## 11. Progress display

**The change (user-dictated):** NO pieces/segments shown
anywhere in the UI. The progress cell now reads
"87% (459 MB of 4.9 GB, 3.9 GB left)" — downloaded/total plus
the REMAINING amount; when no total is known, "87% (459 MB
downloaded)". The pieces text and the ", ~X est." suffix were
removed from the render loop. Segment counts stay in the
record/JSON for the engine; the UI never shows them. (The
Clear-cache confirm dialog still says "temporary download
pieces" — that is about cached files, not progress display,
and was left alone.)

**Tests:** harness — `displayTotal` (see item 10); the render
text needs the installed app (see "NOT verified").

## 12. Extension auto-load

**The investigation:** true auto-install is NOT possible for
an unpacked extension in Chrome. External extension
deployment (registry/policy install) requires a Web Store item
or a .crx hosted at a policy-listed URL, and Chrome refuses
silent installs of unpacked extensions by design — loading
one fundamentally needs the user to enable Developer Mode and
click "Load unpacked". The extension is unpacked, not from the
Web Store, and the installer is unsigned, so none of the
policy routes apply. (Bundling the extension into the
installer was also considered and NOT done: where Tauri puts
`resources` on Windows cannot be verified without a build,
and an unverifiable bundle path is worse than the honest
fallback. It is a follow-up, not a silent gap.)

**The agreed fallback (implemented):** a README in the app.
The Browser Integration strip gains an "Extension setup"
button opening a panel with the COMPLETE layman steps:
chrome://extensions → Developer mode on → Load unpacked →
paste `%USERPROFILE%\ff-extension` into the picker address
bar → Select Folder → pin from the puzzle-piece menu → pair
from this app. The panel says plainly where the files live
(`C:\Users\<your name>\ff-extension`, put there when
FetchForge was installed) and what to do if the folder is
missing. No new Rust command was needed.

**Tests:** node --check; the panel needs the installed app
(see "NOT verified"). Pairing README text (next-update item B)
remains on the next-update list, NOT in this round.

## 13. Filename fixes

Bug 2 above, plus the round-10 item-11 remnants:
- Engine `stem_for_title` (extract.rs) also returns `"video"`
  for hash-like titles (second line of defence — the
  extension skips hash-like server titles first).
- `stemOfFilename` returns `""` for a hash-like stem, so
  `popupDefaultName` falls through to `"download"` instead of
  saving the hash.
- The field stays editable; the preference order (video
  title / h1 / og:title, then page title, then server title,
  then verdict stem) is unchanged. Conservative only — no
  title is rewritten, only hash-like tokens are refused as
  names.

**Tests:** cargo — `stem_for_title` returns "video" for a
hash token, keeps real titles and dotted release names;
harness — hash stems fall through, page titles beat hashes.

## Tests

- **cargo:** **210 passed, 0 failed** (baseline 199; +11 new:
  4 media.rs, 5 extract.rs, 2 state.rs). One transient flake
  seen once during the round
  (`download_media_with_stub_reports_progress_and_final_path`,
  passed in isolation and on reruns) — the same
  timing-sensitive class the round-10 notes recorded.
- **Extension harness:** **105 passed, 0 failed** across 6
  files (baseline 92; +13 new in `round11.test.js`).
- **node --check:** all 8 extension JS files (chromium +
  firefox) and all 3 inline script blocks in index.html.
- **Manifests:** both at **0.2.10**. Chromium/firefox
  byte-identical on all 6 shared files (content.js,
  background.js, popup.js, popup.html, hook-main.js,
  rules/ads.json).
- **Desktop crate (cannot compile on this VM):** grepped for
  affected call sites — the 4 `DownloadRecord` literals carry
  the new fields; `media_formats` is async + `Result` (as
  before, with the new optional `referer` param); all 11 async
  commands return `Result`. Windows CI is the compile gate.

## NOT verified (needs the laptop + the user's browser)

Nothing in this round was live-tested: the laptop currently
has NO FetchForge installed (full reset #2, 2026-11-24), so
there was no app, bridge, or extension to test against. After
install + re-pair, the retest should cover:
- Item 1: NOXX + 123Movies — the popup must appear on the
  button click (the formats popup, with the ladder or the
  degraded single quality).
- Item 2: novahd — the filename field must keep "The Odyssey
  · NOVA" after the qualities load.
- Item 3: YouTube (Big Buck Bunny) — no duplicate rows in the
  formats popup; 233/234 both present.
- Item 4: Internet Archive — the pick should reach a 1080p
  encode (mkv or mp4) when nothing is playing.
- Item 5: MoviesJoy — the popup must show the quality ladder
  (no 403 degradation) on the vidrift server.
- Item 6: the theme switch — knob slides, icons show, choice
  persists (retest script: read `aria-checked`, not
  textContent).
- Item 7: drag a column edge, reload, widths persist.
- Item 8: a running download shows a real speed; a paused row
  shows "-".
- Item 9: toggle + bin side by side on every row.
- Item 10: sizes carry the right markers (exact plain, ≈, ~);
  a stream with AVERAGE-BANDWIDTH shows ≈.
- Item 11: progress reads "X of Y, Z left", no pieces.
- Item 12: the Extension setup panel opens and the steps are
  complete and followable.
- Item 13: no hash filenames on any site.

## Known limitations (by design, not bugs)

- The size ladder's second rung (Average) only exists when
  the manifest declares `AVERAGE-BANDWIDTH`; movie-site
  manifests often do not, so Peak (~) is the common case.
  That is the honest answer to "exact size": for a typical
  movie-site HLS stream there is no exact pre-download total,
  and the UI says so with the marker instead of inventing
  precision.
- Only the DEFAULT variant's byte-range sum is exact; other
  variants keep the bandwidth estimate (see item 10).
- The formats popup still cannot show an "Audio only" choice
  for a stream whose listing carries no audio format (novahd
  retest showed no Audio only choice — the listing itself had
  none; not a popup bug).
- `qualityHintScore` now reads the filename only: a hint that
  lives ONLY in a directory segment (e.g. `/1080p/file.mp4`)
  no longer scores. The Archive case proved the old behaviour
  was worse (a shared directory name inflated every file in
  it).
- The extension still cannot auto-install (see item 12).

## Deferred (not in this round)

- The study's 20-item missing-functions list
  (`downloader-size-and-features-study.md` §4) — auto-start
  queued downloads, subtitles, speed limit UI, per-language
  audio choice, batch download, and the rest — is FOR THE
  USER to pick from after seeing it. None of it was added
  unilaterally. The Speed column (list item 4) is the one
  overlap, and it was already a round-11 layout item.
- Pairing README text (next-update item B from 2026-11-23).
- Bundling the extension into the installer (see item 12).
