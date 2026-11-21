# Phase 6 — Media extraction (yt-dlp) + quality selection + merging

Date: 2026-11-21. Implemented by the desktop worker; verified independently
by the coordinator (cargo tests + `tests/media_extraction.sh` re-run below).

## What was implemented
- **Extraction in the engine** (`crates/engine/src/extract.rs`, NEW),
  shared by the bridge HTTP path and the Tauri invoke path. A probe
  returning `text/html` now routes to extraction; direct links are
  unchanged. YouTube/Drive/generic pages via `yt-dlp -J` (90 s timeout,
  drained reader threads, CREATE_NO_WINDOW on Windows). Drive fallback:
  rewrite to `uc?export=download&id=…`, accepted only on a non-HTML probe.
- **Quality choice:** `POST /media/formats` returns title + formats +
  default; the chosen `format_id` is passed to `POST /downloads`;
  extension-sent URLs auto-pick (Drive `source` → best combined ≤1080p →
  best video-only ≤1080p merged → …). Default format: mp4 wins ties, then
  larger file. Container-aware selector computed at plan time
  (`160+bestaudio[ext=m4a]/160+bestaudio/160`), stored on the record,
  reused on resume — fixes an mp4+opus→`.mkv` bug the e2e test caught.
  Formats with `has_drm` dropped, all-DRM refused, live refused,
  storyboard formats dropped.
- **Provisioning (verified with real downloads):**
  - yt-dlp **bundled** — pinned 2026.10.26 `yt-dlp.exe`, 23,575,744 bytes,
    SHA-256 `f5389d36…77a7d` (coordinator re-verified the hash against the
    downloaded binary). NSIS LZMA roughly halves it in the installer.
    Installer + workflow updated to stage it; self-healing first-use
    download with the same hash if missing.
  - ffmpeg **download-on-first-use only when a merge is needed** — gyan.dev
    9.1 essentials zip is 119,467,219 bytes with a 110,590,976-byte
    `ffmpeg.exe`; bundling would more than double the installer. Pinned URL
    + SHA-256 `b9bbbb3a…58fd`, only `bin/ffmpeg.exe` extracted, system
    ffmpeg preferred.
  - Lookup order for both: env override → beside executable →
    `<data-dir>/tools/` → PATH → pinned download.
- **Desktop:** Tauri `media_formats` command + self-contained format-picker
  section in the inlined `index.html` script; dead `app.js` deleted. No
  `isTauri` declaration anywhere (both script blocks pass `node --check`,
  coordinator re-ran).
- **Also fixed:** reqwest `system-proxy` feature enabled (the engine had
  silently ignored all proxy config); progress parsing with multi-phase
  accumulation; pause keeps `.part` (yt-dlp native resume), cancel removes
  temp files; reader-thread join hang (bounded grace) and fast-child race
  (drain after join); filesize plausibility filter (Drive transcodes
  reporting ~2.9 KB dropped).
- **Version** bumped 0.1.0 → 0.2.0 (workspace Cargo.toml + Tauri config;
  extension manifests were bumped in Phase 4/8).

## Files modified and created
- Created: `crates/engine/src/extract.rs`, `tests/media_extraction.sh`.
- Modified: `crates/engine/src/ffmpeg.rs`, `crates/engine/src/lib.rs`,
  `crates/engine/src/state.rs`, `crates/engine/Cargo.toml`,
  `crates/bridge/src/main.rs`, `apps/desktop/src-tauri/src/main.rs`,
  `apps/desktop/src/index.html`, `installer/fetchforge.nsi`,
  `.github/workflows/windows-build.yml`, `docs/ARCHITECTURE.md`,
  `docs/SECURITY.md`, `docs/FEATURE_MATRIX.md`, `Cargo.lock`.
- Deleted: `apps/desktop/src/app.js` (dead weight, unreferenced).
- Commits: `0416bd0`, `6cce113`, `569d0f9`, `b3c92ff`, `da0dbe1`.

## Dependencies added
- `zip` (ffmpeg zip extraction) in the engine. (`quick-xml` is Phase 5.)

## Tests run (with results)
- Full workspace: 94 passed, 0 failed (coordinator re-ran; baseline was
  34; no existing test weakened or deleted).
- `tests/media_extraction.sh` — COORDINATOR RE-RAN, exit 0, against real
  public URLs: Drive `/media/formats` (title, `source` format/default);
  Drive format 22 download (961,726 bytes, real file on disk); YouTube
  Big Buck Bunny 49 formats / default 234; YouTube 144p+audio merged to a
  real 14,877,817-byte `.mp4` (ffprobe: 1 video + 1 audio stream);
  auto-pick records format 234. Real extraction evidence: YouTube BBB
  `yt-dlp -J` exit 0, 53 formats; Drive sample file 6 formats incl.
  `source`.
- `tests/e2e.sh` (direct-link regression) — COORDINATOR RE-RAN, exit 0:
  5 MB file SHA-256 matches, invalid scheme rejected, native ping works.

## Remaining limitations
- yt-dlp pinned to 2026.10.26 (a newer one ships with the next app
  release); ffmpeg on non-Windows requires a system install (no pinned
  binary).
- ffmpeg first-use download on Windows: zip hash + extraction unit-tested,
  the download path itself untested.
- First-use yt-dlp download through the Rust code on this VM: blocked by
  the sandbox MITM proxy vs rustls/webpki roots (verified via unit test +
  curl + hash instead; TLS roots deliberately not weakened).
- Drive large-file virus-scan warning page: fallback accepts only non-HTML
  probes; no large Drive file tested.
- No DRM/auth/paywall bypass, ever.
