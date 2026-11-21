# Phase 5 — HLS + MPEG-DASH analysis/downloading

Date: 2026-11-21. Implemented by the desktop worker; verified independently
by the coordinator (cargo tests + `tests/stream_download.sh` re-run below).

## What was implemented
HLS/DASH went from dead code/stub (see Phase 2) to wired-in, quality-aware
download paths. Both stream kinds look exactly like yt-dlp qualities in the
picker (same `MediaFormat`/`MediaInfo` shape, same default-pick rule:
best ≤1080p).

- **Routing:** `.m3u8`/`.mpd` URLs route to `media.rs` in the add path
  (bridge + desktop), before probing and after a manifest content-type
  probe. `list_formats`/`plan_media_download` route manifests to the stream
  path and never touch yt-dlp for them.
- **HLS:** master parsing (quote-aware attributes, CODECS commas, audio
  groups), variant listing, media-playlist single format, default-variant
  rule. Shared resumable segment downloader (per-segment retries, sidecar
  `<part>.segments` resume that restarts cleanly if the manifest changed,
  pause/cancel sentinels). Separate-audio variants merge via ffmpeg;
  refused with a clear error when ffmpeg is absent, never a silent video.
  Encrypted (EXT-X-KEY) refused; live playlists are a finite snapshot
  only, never re-fetched.
- **DASH:** real segment assembly for clear single-period manifests.
  quick-xml MPD parser (BaseURL inheritance MPD→Period→Set→Rep,
  SegmentTemplate Number + SegmentTimeline, SegmentList media + mediaRange,
  frameRate ratios, ISO 8601 durations, `$RepresentationID$`/`$Bandwidth$`/
  `$Number$`/`$Time$` substitution with `%0Nd` padding and `$$` escape).
  Refuses DRM (ContentProtection), live (dynamic), and multi-period
  manifests with plain explanations. One format per representation; video
  reps flagged `needs_merge` when audio is separate; ffmpeg resolved only
  when a merge is needed; stream-copy merge via `merge_av`.
- **Dispatch:** `DownloadRecord.stream_kind` ("hls"/"dash") drives worker
  dispatch in bridge (`run_stream_hls`/`run_stream_dash`) and desktop
  (`run_stream_hls_download`/`run_stream_dash_download`); unknown kinds
  fail honestly.
- **FFmpegProcessor** (`crates/engine/src/ffmpeg.rs`, commit `9a3a950`):
  `find_ffmpeg`/`ensure_ffmpeg`/`merge_av` extracted per the Phase 3 module
  map; same public names, no caller changes.

## Files modified and created
- Modified: `crates/engine/src/media.rs` (rewritten/extended),
  `crates/engine/src/extract.rs`, `crates/engine/src/lib.rs`,
  `crates/engine/src/state.rs`, `crates/bridge/src/main.rs`,
  `apps/desktop/src-tauri/src/main.rs`, `docs/FEATURE_MATRIX.md`,
  `docs/ARCHITECTURE.md`, `crates/engine/Cargo.toml`, `Cargo.lock`.
- Created: `crates/engine/src/ffmpeg.rs`, `tests/stream_download.sh`.
- Commits: `081fb8c` (HLS), `5e723ba` (DASH), `9a3a950` (ffmpeg.rs).

## Dependencies added
- `quick-xml 0.38.4` (`default-features = false`, no encoding_rs) in
  `crates/engine/Cargo.toml`; `Cargo.lock` updated.

## Tests run (with results)
- 12 new DASH tests + HLS tests (parser, substitution, timeline/list
  fixtures, local-server assembly, merge with real ffmpeg, range slicing,
  DRM refusal, DASH plan test). Full workspace: 94 passed, 0 failed
  (coordinator re-ran `cargo test --workspace`).
- `tests/stream_download.sh` — COORDINATOR RE-RAN, exit 0: HLS 3 variants /
  default 720p; muxed 360p `.ts` byte-identical to source; 1080p
  separate-audio merged `.mp4` (22,682 bytes); DASH formats
  ['v720','v360','a64k'] / default v720; 720p merge (21,887 bytes);
  audio-only `.m4a` byte-identical; `.mpd` auto-pick records v720; DRM
  manifest → Failed record with DRM error.

## Remaining limitations
- DASH covers only clear single-period manifests (SegmentTemplate
  Number/Timeline, SegmentList); no multi-period, no live, no DRM
  (by design). HLS: no EXT-X-KEY, no live recording.
- No last-segment-404 tolerance — a manifest overstating its segments fails
  honestly naming the segment (kept `.part` + sidecar allow retry).
- Public HLS/DASH streams end-to-end through the engine on this VM were NOT
  verified (sandbox MITM proxy vs rustls roots); verified via local fixture
  servers + the real Big Buck Bunny MPD used as a parser fixture.
  DASH `@mediaRange`, SegmentTimeline, and HLS separate-audio against real
  public streams are fixture-covered only.
