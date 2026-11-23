// Run every harness test file in this directory and report the
// totals. Usage: node extensions/harness/run-all.js
"use strict";
const fs = require("fs");
const path = require("path");
const { spawnSync } = require("child_process");

const files = fs.readdirSync(__dirname).filter((f) => f.endsWith(".test.js")).sort();
let failed = 0;
for (const f of files) {
  console.log("== " + f + " ==");
  const r = spawnSync(process.execPath, [path.join(__dirname, f)], { stdio: "inherit" });
  if (r.status !== 0) failed++;
}
if (failed) {
  console.log("\n" + failed + " harness file(s) failed");
  process.exit(1);
}
console.log("\nAll harness files passed (" + files.length + " files)");
