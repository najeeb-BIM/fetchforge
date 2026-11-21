# Phase 7 — Images + 3D asset downloading

> Reconstructed in Phase 10 from the committed diffs and re-run test
> evidence. The Phase 7 worker's final report was not filed in the
> repo (it went to the parent agent in chat), so this report is built
> from what the commits contain and what Phase 10 re-verified. Where
> the worker's intent is only visible in code comments, the report
> says so.

## Commits

- `a98ca46` Phase 7 (engine): image classification + srcset, 3D models + glTF bundles
- `cf62d32` Phase 7 (desktop/bridge): route images + 3D in the add path, glTF worker
- `99a1013` Phase 7 (extension): image + 3D detectors in the content script
- `62e782f` Phase 7 (tests): image_3d e2e + fixture server content types

## What was implemented

### Image engine (`crates/engine/src/image.rs`, 293 lines, new)

- Classification by file extension (`.jpg/.jpeg/.png/.webp/.gif/.svg/.avif`)
  and, for extension-less URLs, by probed `image/*` content type
  (bridge + Tauri add path append the right extension so the file
  opens and categorises as Images instead of landing in "Other").
- `srcset` parsing (width + density descriptors) and
  full-resolution pick: the largest candidate wins. The content
  script mirrors this in JS (`pickBestSrcsetUrl`).
- **Original format only.** No conversion dependency was added
  (no `image` crate in Cargo.toml) and the module comment states
  the rule: a converted file would silently lose quality, and the
  spec forbids advertising a converted file as original quality.
  PNG/JPG/WebP conversion was considered and deliberately NOT done.
- **Never upscale**, never advertise output as original unless it is
  the original file.

### 3D engine (`crates/engine/src/threed.rs`, 553 lines, new)

- Single-file models (GLB, OBJ, FBX, STL, USDZ) download as direct
  files with category "3D".
- glTF (`.gltf` JSON): the file is parsed, relative dependency URIs
  (buffers `.bin`, images/textures) are resolved, each dependency is
  probed before queueing, and the bundle downloads with its folder
  structure preserved next to the main file. A broken glTF fails
  with "not a valid glTF model file (the JSON could not be read)";
  an unreachable dependency fails naming the file
  ("3D model dependency <url> could not be reached ...; the model
  was not downloaded") and nothing is downloaded.
- Protected/DRM assets are refused with a plain explanation.
- Category "3D" added in `crates/engine/src/category.rs`
  (also covers `.gltf` case-insensitively, e.g. `scene.GLTF`).

### Add-path routing (bridge + Tauri, `cf62d32`)

- Image and 3D URLs route to the new engines in the add path of
  BOTH the bridge (`crates/bridge/src/main.rs`) and the Tauri app
  (`apps/desktop/src-tauri/src/main.rs`), mirroring each other.
- `.gltf` URLs are planned first (`fetch_gltf_plan`: fetch the JSON,
  resolve and probe every dependency) and queued as a bundle with
  `stream_kind: "gltf"`; a plan failure records a Failed row with
  the reason instead of queueing a job that would fail halfway.
- Direct video/file links and HTML-page extraction are untouched
  (regression-verified by the Phase 10 re-run of `e2e.sh` and
  `media_extraction.sh`).

### Detection (`extensions/chromium/content.js`, `extensions/firefox/content.js`)

- Image + 3D detectors registered in the existing detector registry
  (`registerDetector`, kind/selector/overlay/extract). Both files
  byte-identical (+126 lines each).
- Images: `<img>` `src`/`srcset`/`data-src` (lazy), `<picture>` /
  `<source srcset>`; the full-resolution URL is picked.
- 3D: `<model-viewer src>` and links loading model files
  (kind `"model"`). No overlay button on images or 3D assets (that
  would spam galleries) — they flow through the existing `reportMedia`
  path into the popup list and the Media Detector panel.
- Chromium and Firefox content.js kept byte-identical (verified by
  Phase 10: `diff -q` reports identical).

## Tests

- 9 new unit tests in the engine (srcset parsing incl. width/density
  descriptors, glTF dependency resolution incl. nested paths,
  classification incl. query strings).
- `tests/image_3d.sh` (new, 139 lines): 6 e2e checks against a local
  fixture server — direct image, extension-less image (content-type
  classification), single-file GLB, glTF bundle (folder preserved),
  broken glTF fails honestly, unreachable dependency fails honestly
  naming the file. `tests/server.py` gained the fixture content
  types.
- Phase 10 re-run: `tests/image_3d.sh` passes 6/6; `cargo test
  --workspace` 103 passed, 0 failed (baseline before Phase 7 was
  94 — the 9 new unit tests are Phase 7's).

## Dependencies added

None. (The `image` crate was considered for format conversion and
rejected — original-format-only needs no conversion dependency.)

## Remaining limitations

- No real-browser verification of the image/3D detectors (no browser
  on the build VM). Detection is code-reviewed + syntax-checked only.
- No format conversion: a PNG stays a PNG, a WebP stays a WebP.
- glTF covers standard clear bundles; protected/DRM assets refused.
- `model-viewer` detection is DOM-only; a WebGL viewer that loads
  models without DOM-visible URLs cannot be detected (noted in the
  spec §13 matrix as DOM-detection-only).

## What Phase 10 changed about Phase 7's work

- The Media Detector 3D view was confirmed to match Phase 7's "3D"
  category (`index.html` line 212: `d.category === "3D"`); only a
  stale comment was fixed ("3D files have no category of their own
  yet" → "3D downloads carry category '3D' (Phase 7)").
- No engine code was changed. Phase 10's probe-error fix (bridge +
  Tauri) also benefits image/3D adds that fail the probe.
