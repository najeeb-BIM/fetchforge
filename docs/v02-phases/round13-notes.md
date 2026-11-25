# Round 13 fix list (collected from round-12 retests, 2026-11-25)

Not yet built. Items are in evidence order; the Atlantic items are the most
serious (a reproducible download killer + a resume bug that destroys a partial
and lies about completion).

## 1. Atlantic segment timeout — reproducible, same segment twice (HIGH)
- Evidence: two Atlantic sends of The Odyssey (2026), 1080p, totallyacdn.cc
  byterange HLS, both died on **segment 229 of 1731** with
  "Segment 229 of 1731 could not be downloaded after 3 tries. (error decoding
  response body: operation timed out)".
  - First record dl-33c2fbbaa5c85d3e26d2e858444037e9: 262 segments / 1.87 GB,
    failed 14:38 PKT.
  - Fresh re-send dl-ceb31e30cbfe2a65f6d4614643b8e036: 248 segments /
    1.76 GB, failed ~17:01 PKT.
  - AES-128 key handling works (hundreds of segments in both times); the
    timeout on one slow segment is the blocker.
- Fix direction: per-segment retry with a longer timeout and backoff for
  byterange segments; do not fail the whole download after 3 tries on one
  slow segment. Consider resuming just that segment rather than failing.
- User decision 2026-11-25 17:35 PKT: leave Atlantic failed, log for round 13
  (720p retry NOT taken).

## 2. Resume accepts a contradicting fresh playlist — destroys partial, marks stub "completed" (HIGH)
- Evidence: resuming dl-33c2fbbaa (1.87 GB, 262 segments, failed) re-resolved
  the stream with a fresh signed URL; the new playlist estimated only
  ~976 KB. The engine downloaded 232 KB, marked the record "completed", and
  the file on disk is a 64-second stub (232,045 B, Duration 00:01:04.04,
  1280x720) — NOT the film. The 1.87 GB / 262 segments are gone from the
  accounting.
- Second confirmation, same bug class, different stream: resuming the two
  wrong-film MoviesJoy partials (dl-ebf3841d0, dl-96cb096de) finished
  ~17:32 PKT marked "completed", both pointing at
  index-s1080p-v1-a1 (1).ts, 4,514,046,830 B, probe Duration 01:38:09.75 —
  only ~60% of the 2:44:31 film — with segments_done short of total
  (959/983 and 973/983) yet marked completed. The full film already exists
  as index-s1080p-v1-a1.ts (7,600,381,902 B, 02:44:31.29).
- Fix direction: on resume, validate the fresh playlist against stored
  progress (same segment count / total size). Never mark complete when the
  playlist contradicts the stored segments_done, and never discard stored
  segments. A record with fewer segments than before must stay failed, not
  complete.

## 3. LookMovie stale-verdict fix did not hold — wrong film sent again
- Evidence: retest #3, The Mongoose (2026) via VidFast Vip. Popup showed The
  Mongoose's ladder, but the app received the old MoviesJoy link
  (moon.goldenkite.top, filename index-s1080p-v1-a1.ts) and re-downloaded it.
  Round-12 item 9 (tabObserved cleared on navigation + origin filter +
  pick-time re-validation) did not fix it.

## 4. PrimeWire popup claims "Sent" instead of refusing/warning
- Evidence: retest #4. Player dead ("Error Code: 101102"). Popup showed only
  "Video" + "Audio only", no labels/bitrates/sizes, no warning; clicking
  Video showed "Sent to FetchForge". The `video.error` popup guard (round-12
  item 10) did not fire. App-side validation caught it (record failed
  immediately, 0 B, home address rejected) — but the popup itself must
  refuse/warn.

## 5. YouTube dedupe — duplicates wider than 144p
- Evidence: retest #5, Big Buck Bunny 4K 60fps. 46 choices (37 video + 9
  audio, same count as round 11). The identical size-less "mp4 · video only
  (merged with best audio)" row appears TWICE at 1080p/720p/480p/360p/240p
  and THREE times at 144p; single at 2160p/1440p. No bitrates on video rows.

## 6. NOXX audio-only file lands in the Video folder with a "(1)" name
- Evidence: retest #7b. "The Odyssey (1).m4a", 167,476,889 B, full 2:52:35
  soundtrack, audio only — but saved under Downloads\Video\ instead of
  Downloads\Audio\. Audio-only downloads must route to the Audio folder.

## 7. Sites tab NOXX link is stale
- Evidence: noxx.gg states "noxx.to is retired". The Sites tab NOXX link
  should point to noxx.gg.

## 8. Still unverified (blocked by Chrome/browser policy after the 0.2.11 reload)
- FlixTor single-format labelling + "quality unknown" WHY note (retest #6).
- 123Movies audio-row delivery (retest #7).
- Soap2Day popup-over-overlay confirmation (retest #9).
- These need the user to allow the sites in his browser first, then re-run.
  The block is NOT the FetchForge extension (its rules hold only ads.json).

## 9. Downloads table: rows break when a name wraps to 2-3 lines (user recommendation, 2026-11-25 voice call)
- Problem: when a download's name wraps to two or three lines, the
  progress/speed row and the Actions row get cut off — the row breaks.
- Requirement: every row stays one aligned row from start to end no matter
  how many lines the name takes: the progress and speed cells and the
  Actions cell stay vertically centered against the full row height, and all
  action icons stay centered in their cells. Nothing gets cut off when the
  name wraps.

## 10. Completed downloads: no remarks line, dash in progress (user recommendation, 2026-11-25 voice call)
- When a download completes, do NOT show a remarks line under it.
- In the progress column of a completed download, show a dash (—) so it is
  immediately clear the download is complete.

## Follow-up (2026-11-25): item 6 residual gap closed — paired / DASH audio-only routing

The first item-6 fix covered only the direct HLS audio-only path
(`download_hls_audio_only_opts`). The residual gap was the NOXX evidence
case itself — record `audio_only=true, audio_format=m4a,
format_kind=combined`, i.e. an audio-only pick that falls back to the
muxed variant / paired path, not the direct audio-only path — plus DASH
audio-only, whose caller-computed dir the engine never saw.

Fix (no existing engine signature changed, so the desktop crate still
compiles untouched — its struct literals for `StreamDownloadOptions` /
`DashDownloadOptions` are exhaustive, so no field was added to either):
- New engine entry points carrying the audio-only knowledge explicitly:
  `download_hls_variant_audio_only_opts` (paired audio sibling downloaded
  alone — the plain variant entry point cannot tell that playlist from a
  video-only one), `download_hls_pair_audio_only_opts` (pair chain), and
  `download_dash_audio_only` / `download_dash_with_audio_only` (a video
  representation downloaded for an audio-only record — the manifest
  cannot tell it from a video download). Each re-routes the destination
  through `audio_destination_dir()`; video downloads never use them.
- `download_dash_with` now also re-routes on its own when the chosen
  representation IS audio (engine-detectable, no caller flag needed).
- Bridge (`run_stream_hls` / `run_stream_dash`) computes the dir with
  `category_for_download(&rec.filename, rec.audio_only)` and uses the new
  audio-only entry points for audio-only records; the planned record's
  category is set the same way, so it matches the folder the file lands in.
- `audio_destination_dir` edge fixed: only a category folder NESTED
  inside a download root is swapped (at least two Normal path
  components). A download root literally named `Video` (`D:\Video`,
  `/Video`, organising off) is left alone.

Still open — desktop follow-up (DONE 2026-11-25, commit `e452351` on
`r13-desktop`, merged into `audit-round`): `apps/desktop/src-tauri/src/main.rs`
`run_stream_hls_download` now computes its dir with
`category_for_download(&rec.filename, rec.audio_only)` and calls
`download_hls_variant_audio_only_opts` for the audio-only + audio-URL
branch; `run_stream_dash_download` does the same dir change and calls
`download_dash_audio_only` for audio-only records. Both the bridge and the
desktop processes now route paired/DASH audio-only downloads to Audio/.
No engine signature changed (new pub items only, plus `complete_with_file`
now returns `bool` — all 5 desktop + 5 bridge call sites use it in
statement position, so it is source-compatible; the desktop crate itself
is only type-checked by Windows CI).

## Verification (coordinator, run on the merged tree)

- `cargo test --workspace`: **253 passed, 0 failed** (round-12 baseline
  239; +10 engine tests in `70d8d23`, +4 in `ee7dc80`). `cargo fmt --check`
  clean on the touched Rust files.
- Extension harness `node extensions/harness/run-all.js`: **174 passed,
  0 failed** across 9 files (baseline 146; +28 new in `round13.test.js`).
  `node --check` clean on every touched extension JS file.
- Both manifests 0.2.12; chromium/firefox byte-identical on all 6 shared
  files (content.js, background.js, popup.js, popup.html, hook-main.js,
  rules/ads.json). `offscreen.js` is chromium-only by design (round 11).
- No Tauri command was added or changed (`git diff` confirms — only the
  two desktop functions' internals + imports). One engine signature
  changed: `complete_with_file() -> bool` (see above); all other changes
  are new pub items. No workflow YAML touched. No new dependencies.
- **Not run:** no laptop retest (the parent runs those after install);
  the desktop crate cannot be type-checked on this VM (no GTK/WebKit dev
  libs) — Windows CI is the compile gate, as always. The Atlantic
  segment-229 timeout fix, the resume validation, and the audio-folder
  routing are verified by loopback-server / unit tests only, not by a
  live CDN run.
