# Round 12 — engine + desktop notes (2026-11-25)

Engine/desktop half of round 12 (items 4, 5, 8, 12, 13). Extension
items are another agent's; `docs/v02-phases/round12-notes.md` is the
coordinator's. Base `f2f8de3` (branch `r12-engine`).

Baseline before edits: `cargo test --workspace` **232 passing, 0
failed**. After: **239 passing, 0 failed** (+7 new tests, 0
renamed-away net: the AES-128 parse test was renamed, the old
encrypted-refusal download test was replaced by two tests).
`cargo fmt --check` clean on the touched Rust files (only my own
formatting was changed; no unrelated reformat). No workflow YAML
touched. The desktop crate cannot be type-checked on this VM (no
GTK/WebKit dev libs) — hand-checked instead: **no engine API
signature changed** (only new pub items + new deps), no Tauri
command touched, so no desktop call site needed updating
(`ensure_ffmpeg` keeps its signature; `list_formats_with_referer`
keeps its signature).

## 4. Archive best-encode — FIXED (engine pick)

**What:** listing an `archive.org/download/<id>/<file>` link used to
return the single clicked file (the standard 597 MB mp4), so the
popup could only offer "Video · MP4". The engine now fetches the
item's public metadata (`https://archive.org/metadata/<id>`,
no auth — reading a public file list is not a bypass) and returns
the best encode as the single format, with `resolved_url` pointing
at it, so the worker downloads the best file, not the clicked one.
The 1080p mkv wins when present.

**How** (`crates/engine/src/extract.rs`):
- `list_formats_with_referer_and_proxy` tries
  `try_archive_best_encode(url)` first; any failure yields None
  and the normal yt-dlp path runs, so a metadata outage can only
  leave the old behaviour, never break it.
- `archive_encodes_from_metadata` reads the `files` array:
  top-level files only (a `dir/file.mp4` is not this item's
  encode), video extensions only (mp4/mkv/webm/mov/ogv).
- `archive_height_from_filename` scores the FILENAME only (the
  round-11 lesson: the `..._1080p` item directory made every file
  look 1080p). `1080p`/`720p` tokens, or a bare height token
  (240–4320, multiple of 120 — a year like 1968 or a bitrate like
  512 is not a height).
- `rank_archive_encodes`: height desc, then container (mkv first
  — the Archive's high-quality encode — then mp4/webm/mov/ogv),
  then size desc. The format is id `"0"` (the direct-file
  convention from `parse_media_info`'s single-file synthesis),
  kind Combined, size Exact from the metadata `size`.
- Deliberately the pick-best option, not offer-all-encodes: the
  popup then needs no change (another agent owns the extension),
  and the existing `resolved_url` → `effective_url` plumbing
  already downloads the resolved file.

**Verified:** new tests
`archive_best_encode_picks_1080p_mkv_from_metadata`,
`archive_height_hint_reads_filename_only`,
`archive_download_parts_only_matches_download_links` (all pass).
Live check against the real item (curl, 2026-11-25): the standard
mp4 is `NightOfTheLivingDead.mp4`, 596,808,390 bytes (~597 MB —
exactly the round-11 figure), and the best encode is
`NightOfTheLivingDead_1080p.mkv`, 11,523,767,255 bytes (~11.5 GB);
the ranking picks the mkv over the same-height
`..._1080p.mp4` by the container tie-break. Non-Archive
direct-file behaviour is untouched (the hook only fires for
`archive.org/download/<id>/<file>` URLs).

**Limitations / laptop retest:** (a) verify on the laptop that the
Archive popup now offers/lists the 1080p mkv and downloads it
(~11.5 GB — confirm the user wants the big file, not the 597 MB
one); (b) the metadata fetch adds one HTTPS request per Archive
listing; (c) an item whose files carry no height hint falls back
to biggest-file-wins (the standard mp4 over the 512kb mp4).

## 5. EXT-X-KEY regression (novahd + Atlantic) — FIXED (native AES-128)

**What:** the native HLS path refused every `#EXT-X-KEY`
playlist ("This stream is encrypted (EXT-X-KEY)..."), while round
9 downloaded the same streams via yt-dlp. Standard HLS AES-128
is now supported natively. This is NOT a DRM bypass: the key is
served openly at the URI the playlist itself declares, and ffmpeg
handles the same streams natively (spec §10's rule is about DRM /
auth / paywall bypass — none of that is touched).

**How** (`crates/engine/src/media.rs`; new deps `aes 0.8.4` +
`cipher 0.4.4` in `crates/engine/Cargo.toml` — CBC is hand-rolled
on the block cipher, so no separate `cbc` crate):
- `parse_hls_segments` parses `#EXT-X-KEY` lines: METHOD=AES-128
  stores the key (URI resolved against the playlist URL, IV parsed
  when declared); METHOD=NONE clears it (plain/encrypted segments
  may mix); any other method (SAMPLE-AES, …) is refused with the
  DRM explanation, as is a KEY with no METHOD or no URI, or a
  malformed IV. Each segment records its key + sequence number
  (`#EXT-X-MEDIA-SEQUENCE` + index).
- `HlsMediaPlaylist::resolve_keys` fetches each distinct key URI
  ONCE (same client/referer as the segments, so signed keys work),
  requiring exactly 16 bytes; a segment with no declared IV
  decrypts with its sequence number as a 128-bit big-endian IV
  (RFC 8216 §4.3.2.4). Used by `download_hls_variant_opts`
  (video + separate audio) and `download_hls_pair_opts`
  (video + audio); the listing path keeps using `into_segments`
  (keyless — counts/sizes only).
- `decrypt_aes128_cbc` decrypts after fetch, before the piece is
  written, in both the parallel and the legacy sequential paths —
  so resumed pieces on disk are always plaintext. Valid PKCS#7
  padding is stripped; a last byte that is not valid padding
  leaves the plaintext untouched (some packagers emit unpadded
  block-aligned TS). A decryption failure is fatal, not retried
  (re-fetching the same ciphertext with the same key fails the
  same way). A non-block-multiple ciphertext is an error.
- The six blanket `#EXT-X-KEY` refusals in the listing/download
  paths are now `check_hls_encryption_method` (AES-128/NONE pass,
  anything else refuses). Encrypted SUBTITLE playlists are still
  refused (`parse_hls_subtitle_playlist` untouched) — a keyed
  subtitle track fails as a warning and the video continues
  without subtitles, as before.

**Verified:** new tests (all pass) —
`decrypt_aes128_cbc_round_trips_and_strips_pkcs7` (encrypt with
the same crate, decrypt, PKCS#7 strip, unpadded pass-through,
bad-length error, IV parsing),
`parse_hls_segments_resolves_relative_urls_and_parses_aes128_key`
(KEY in force across segments, METHOD=NONE clears it, sequence
numbers, no-URI / bad-IV / SAMPLE-AES refused),
`download_hls_decrypts_aes128_playlist` (local fixture server in
the style of the existing HLS tests: key at `k.bin`, two
encrypted segments — seg0 explicit IV, seg1 default sequence IV
— assembled file is the plaintext, key fetched exactly once),
`download_hls_still_refuses_sample_aes_playlist`. The
Widevine/DASH DRM refusal tests are untouched and still pass
(full suite green).

**Limitations / laptop retest:** (a) retest novahd + Atlantic
natively — the round-9 yt-dlp route is the fallback if a site's
key fetch needs headers the native client does not send (the key
fetch uses the same browser headers + referer as segment
fetches); (b) key rotation mid-playlist works (per-segment keys),
but a resume after a KEY URI change reuses old decrypted pieces
(the segment-dir meta pins count/first-URL/range, not the key) —
an edge case, not hit in the retests; (c) SAMPLE-AES / FairPlay
and Widevine stay refused, honestly.

## 8. BBB audio os error 2 (ffmpeg first-use race) — FIXED (lock)

**What:** a job starting while ffmpeg was still being fetched
could fail with "The system cannot find the file specified.
(os error 2)" — a Windows spawn error for a not-yet-present
binary. `ensure_ffmpeg` had no serialisation: two jobs both saw
"no ffmpeg", both downloaded the zip to the same path, and a job
could spawn ffmpeg mid-fetch. Cause is timing-supported, not
log-proven — no log ever captured the race in the act.

**How** (`crates/engine/src/ffmpeg.rs`): `ensure_ffmpeg` now
takes a process-wide `OnceLock<Mutex<()>>` first-use lock for the
whole lookup + download + extract (the body moved to
`ensure_ffmpeg_locked`). A second caller waits, then re-checks
and finds the finished binary. A poisoned lock (a panicking
holder) is treated as free rather than failing every later
download. All `ensure_ffmpeg` call sites (bridge + desktop
workers, `download_media`) go through this one function, so the
merge / subtitle / audio-format paths are all covered.

**Verified:** new test
`ensure_ffmpeg_serializes_concurrent_first_use` (8 threads, all
return the same binary, no panic/deadlock) + full suite green.
Honest limit: the real first-use download race is Windows-only
and cannot be exercised on this VM, so the download path itself
is verified by code inspection + the suite, not by this test.

**Left out deliberately:** the same race class exists in
`ensure_ytdlp` (same `download_verified` helper, same shared zip
path) — out of this item's scope; flagging it for a later round
rather than expanding this one.

## 12. App auto-wake — FIXED (closest working mechanism)

**What the user ordered (2026-11-25):** when a download is
started while the app is off, the extension/native host must
trigger the app to open automatically instead of falling back to
a typical browser download.

**Investigation first:** the gap was narrower than the order
assumed. The native host ALREADY spawns a detached bridge when
no bridge is listening (`spawn_detached_bridge`, bridge case 2),
so the download itself survives the app being off — what was
missing was only the app UI opening. The desktop app binary
(`FetchForge.exe`) ships next to the bridge binary (installer
`stage/FetchForge.exe` + bridge in the same install folder), so
the native host can launch it directly.

**How** (`crates/bridge/src/main.rs`):
- `desktop_app_path()`: `FETCHFORGE_APP_PATH` override when it
  names a real file (dev/test, mirroring
  `FETCHFORGE_FFMPEG_PATH`), else `FetchForge.exe` / `FetchForge`
  next to the bridge exe. None when neither exists.
- `wake_desktop_app() -> bool`: spawns the app detached (stdin/
  stdout/stderr null; Windows `CREATE_NO_WINDOW` +
  `DETACHED_PROCESS`), no wait, never an error — a failed wake
  never fails the download. Guarded by `cfg!(test)` exactly like
  the round-11 shutdown guard (commit `d785573`): a test binary
  never launches a real app.
- Call site: the native-host `addDownload` branch where the
  bridge had to be started (`bridge_started: true`) — that is the
  signal the app is off, because the app always spawns a bridge
  on launch. The reply gains `"app_woken": true/false` (ignored
  by older extension versions). The existing shutdown test is
  untouched and still green.

**Verified:** new test
`auto_wake_finds_app_by_override_and_never_spawns_from_tests`
(finder prefers the override; wake returns false from a test
binary even with a real executable as the override) + full suite
green. No engine API changed; no desktop code changed (the app
launches with no arguments, as normal).

**Limitations, honestly:** (a) the wake fires ONLY when the
bridge had to be started. A bridge already running without the
app (a detached bridge outliving an app close) is not woken —
the native host cannot tell that state apart from "app open with
its bridge"; (b) there is no single-instance plugin in the Tauri
app, so a wake in a stale state could open a second window;
(c) NOT verified on the laptop — the spawn path is
Windows-only in effect and this VM cannot run the app.

**Laptop retest:** (1) close the app completely, start a
download from the extension, confirm the FetchForge window opens
AND the download runs in it; (2) with the app already open,
start another extension download and confirm NO second window
appears; (3) check `bridge.log` for the wake lines on failure.

## 13. Remove YouTube from the Sites tab — FIXED

**What:** deleted the `["YouTube", "https://www.youtube.com"]`
line from `SITE_DIRECTORY` in `apps/desktop/src/index.html`
(line 506). YouTube downloading itself stays working: the only
other `youtube.com` references in that file are in `MEDIA_HOSTS`
(a picker hint for pasted/detected links, not a directory
button), and no other code references the removed entry
(grepped `SITE_DIRECTORY`, `siteList`, `YouTube` across the
desktop frontend — the directory is rendered only from that one
array).

**Verified:** `grep -n "YouTube" apps/desktop/src/index.html`
returns nothing; the array edit is a one-line deletion (no JS
behaviour change to test beyond rendering, which needs the
installed app).

**Laptop retest:** open the Sites tab and confirm no YouTube
button, and that a pasted YouTube link still downloads.
