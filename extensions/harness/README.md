# FetchForge extension harnesses

Node harnesses for the extension (and the desktop app's percent
helper). Run them all with:

```
node extensions/harness/run-all.js
```

Or one file directly: `node extensions/harness/content.test.js`.

## How they work

The extension sources are not modules — `content.js` is one big
IIFE and `background.js` a service-worker script — so the harnesses
test them by extracting the **real function source** from the shipped
files and evaluating it in a `vm` context with stubbed browser
globals (`extract.js`). Nothing is retyped: a change to the shipped
code is a change to the test, and extraction fails loudly if the
file's style stops matching the extractor. The background Fix 4 test
goes further and evaluates the whole `background.js` with a stubbed
`chrome` API, driving the real `onMessage` listener end to end.

## Files

- `extract.js` — extraction (`extractFunction`, `extractConst`),
  vm contexts, and the tiny test runner. Shared by all test files.
- `content.test.js` — round 7 content-script fixes:
  Fix 4 (browser-fallback label), Fix 5 (title strips),
  Fix 6 (clip-guard probe + refusal), Fix 7 (DASH popup routing,
  single-format fallback, captured-manifest passthrough).
- `background.test.js` — round 7 background fixes: Fix 4
  (`viaBrowser` in the `downloadMedia`/`sendUrl` replies) and
  Fix 6 (`pageVideoDuration` in the election verdict).
- `progress-percent.test.js` — round 7 Finding B: the desktop app's
  `progressPercent` (extracted from `apps/desktop/src/index.html`),
  capped at 100% against multi-phase totals.
- `run-all.js` — runs every `*.test.js` here and exits non-zero on
  any failure.

## Notes

- The round-6 harnesses (popup 17 tests, worker 18 + 48) lived in
  `/tmp` and were lost when the VM restarted. These live in the repo
  so the next round keeps them. They cover the round-7 behaviour;
  re-adding round-6 popup/worker coverage here is still open.
- The harnesses run against `extensions/chromium/` only; the
  chromium and firefox copies are byte-identical by policy (diff
  them before committing), so one copy under test covers both.
