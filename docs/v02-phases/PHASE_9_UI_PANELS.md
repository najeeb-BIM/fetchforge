# Phase 9 — IDM-style UI panels + batch download

> Reconstructed in Phase 10 from the committed diff and re-run test
> evidence. The Phase 9 worker's final report was not filed in the
> repo (it went to the parent agent in chat), so this report is built
> from what the commit contains and what Phase 10 re-verified. Where
> the worker's intent is only visible in code comments, the report
> says so.

## Commit

- `8b0823f` Phase 9 (UI panels): IDM-style tabs, Media Detector panel, batch download
  (7 files, +373/−27)

## What was implemented

### IDM-style panels (`apps/desktop/src/index.html`, +265 lines)

- The single-page structure and visual style were kept; panels were
  added progressively as tabs/sections, not a rewrite (629 lines
  total now; both script blocks pass `node --check`, re-verified in
  Phase 10).
- Tabs: **Dashboard** (summary counts + recent activity from the
  existing list data), **Download Manager** (the existing list,
  kept), **Media Detector** (auto-list from `GET /media/detected`,
  refreshed with the existing 2 s cycle), **Video / Images / 3D**
  (filtered views over the existing list using the real category /
  kind fields), **Queue** (queued/active filter), **History**
  (completed/failed/cancelled filter), **Browser Integration** (the
  existing pairing section, kept, not rebuilt).
- Filtered views reuse the Downloads section and its table; only
  Dashboard and Media Detector have their own markup. A filtered
  view resets the status filter to All (a stale "Failed" filter
  would otherwise hide everything on the Queue tab).
- The 3D view matches `d.category === "3D" || d.category ===
  "Models"` or a real 3D filename extension (verified in Phase 10
  against Phase 7's "3D" category — it matches).
- The Video view matches `category === "Video"` or `stream_kind`
  `"hls"`/`"dash"`; Images matches `category === "Images"`.
- The existing format picker, pairing box, search/filter, and list
  actions were kept working exactly as before (regression-verified
  by the Phase 10 re-run of `e2e.sh` and `pairing_e2e.sh`).

### Media Detector data source (one new Tauri invoke)

- `list_detected_media` added in `apps/desktop/src-tauri/src/main.rs`
  (line 129, registered line 1126) — the ONE new invoke. It reads
  `detected.json` from the data dir with a LOCAL struct (serde derive
  in main.rs); `DetectedItem` was deliberately NOT moved into the
  engine crate.
- In plain-browser mode the UI follows the existing browser-mode
  pattern (bridge over HTTP, same token handling).
- Only confirmed metadata is shown (spec §8): unknown fields are
  omitted, never guessed. Each row shows kind, MIME type, size,
  duration, page hostname, and detection time where known.

### Multi-select batch download

- Checkboxes on detected-media items in the Media Detector panel +
  "Download selected" button: one add per item through the existing
  add path. A page URL takes the auto-picked quality (the code
  comment states the reason: a batch must never open the picker
  once per item).
- Same multi-select in the extension popup media list
  (`extensions/*/popup.html` +2 lines, `popup.js` +31 lines, both
  browsers byte-identical): checkboxes + "Download selected" via
  the existing `sendUrl` path.
- `styles.css` +20 lines for the new panel/checkbox styling.

## What was deliberately omitted

- **Settings panel: OMITTED.** The brief allowed it only if a
  working backend exists; no invoke exposes settings, so the panel
  was left out rather than faked. Verified in Phase 10: the string
  "settings" appears nowhere in `index.html` or the Tauri main.rs.
  (The bridge itself still reads/writes `settings.json` — see
  `tests/settings_per_field.sh`, which passes.)
- No fake or non-functional UI: every added control calls a real
  path (verified by the worker's re-read and by Phase 10's
  `node --check` + suite re-runs).

## Dependencies added

None (as expected).

## Tests / verification done by the worker

- `node --check` on both `index.html` script blocks and both
  `popup.js` files (re-run clean in Phase 10).
- `cargo build` / `cargo test` green at the time (94 passed, 0
  failed — Phase 7's 9 tests landed after, taking the total to 103).
- Tauri main.rs change verified via the stub-tauri type-check
  approach (the full Tauri app cannot run on this Linux VM).
- A careful re-read of the UI code for dead buttons.

## Remaining limitations

- No real-browser verification of the panels or the popup batch
  download (no browser on the build VM).
- No Settings panel (see above).
- The Speed column in the downloads table is still a placeholder
  (`"-"`); the bridge progress API carries bytes and the UI
  refreshes every 2 s, but no live speed is computed.
- No remove/sort controls in the UI (search + status filter only;
  the Dashboard sorts its recent list by recency).
- Batch download uses the auto-picked quality for page URLs; there
  is no per-item picker in a batch (by design — see above).
