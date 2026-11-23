// Round 7 progress-percent harness (Finding B): the desktop app's
// progressPercent is extracted from apps/desktop/src/index.html and
// tested against multi-phase totals. The engine now sends a
// COMBINED estimate (completed phases + current phase), and the
// 100% cap is the safety net for any estimate that still runs low
// (a nime DASH download showed 1324% on 2026-11-23).
"use strict";
const { readSource, extractFunction, makeContext, runIn, test, assertEq, run } = require("./extract");

const html = readSource("apps/desktop/src/index.html");
const ctx = makeContext({}, [extractFunction(html, "progressPercent", 0)]);
const pct = (dl, total, est) => {
  ctx.dl = dl; ctx.total = total; ctx.est = est;
  return runIn(ctx, "progressPercent(dl, total, est)");
};

test("caps at 100 when cumulative bytes exceed a stale estimate", () => {
  // The live nime case: 2.2 GB downloaded against a ~166 MB
  // audio-phase estimate.
  assertEq(pct(2200000000, null, 166000000), 100);
  assertEq(pct(150, 100, null), 100);
});

test("combined estimate gives the true percent", () => {
  // 2.3 GB video phase + audio in progress, combined estimate
  // ~2.47 GB: 93%, not 1324%.
  assertEq(pct(2300000000, null, 2470000000), 93);
});

test("normal cases unchanged", () => {
  assertEq(pct(50, 100, null), 50);
  assertEq(pct(0, 100, null), 0);
  assertEq(pct(100, 100, null), 100);
  assertEq(pct(0, null, null), 0, "no total at all: 0%");
  assertEq(pct(25, null, 100), 25, "estimate stands in for the total");
  assertEq(pct(75, 100, 50), 75, "exact total wins over the estimate");
});

run("progress-percent.test.js");
