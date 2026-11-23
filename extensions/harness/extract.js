// Shared helpers for the FetchForge extension harnesses.
//
// The extension sources are NOT modules: content.js is one big IIFE
// and background.js a service-worker script, so the harnesses test
// them by extracting the real function source and evaluating it in a
// vm context with stubbed browser globals. Nothing is retyped:
// every function under test is the exact source from the shipped
// file, so a change to the shipped code is a change to the test.
// (The round-6 harnesses lived in /tmp and were lost; these live in
// the repo so the next round keeps them.)
"use strict";
const fs = require("fs");
const path = require("path");
const vm = require("vm");

const REPO_ROOT = path.join(__dirname, "..", "..");
const EXT_DIR = path.join(__dirname, "..", "chromium");

function readSource(relPath) {
  return fs.readFileSync(path.join(REPO_ROOT, relPath), "utf8");
}

// Extract one function declaration at the given indent: slice from
// its declaration line to its closing brace (the first line at
// exactly `indent` that is just "}"). The slice is verified by
// parsing it standalone, so a style change that breaks extraction
// fails loudly instead of silently testing the wrong code.
function extractFunction(source, name, indent) {
  const pad = " ".repeat(indent);
  const lines = source.split("\n");
  const esc = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const startRe = new RegExp("^" + pad + "(?:async )?function " + esc + "\\(");
  let start = -1;
  for (let i = 0; i < lines.length; i++) {
    if (startRe.test(lines[i])) { start = i; break; }
  }
  if (start < 0) throw new Error("function " + name + " not found at indent " + indent);
  const closeRe = new RegExp("^" + pad + "}\\s*$");
  let end = -1;
  for (let i = start + 1; i < lines.length; i++) {
    if (closeRe.test(lines[i])) { end = i; break; }
  }
  if (end < 0) throw new Error("closing brace for " + name + " not found");
  const code = lines.slice(start, end + 1).join("\n");
  new vm.Script(code, { filename: "extracted-" + name + ".js" }); // parse check
  return code;
}

function extractMany(source, names, indent) {
  return names.map((n) => extractFunction(source, n, indent)).join("\n\n");
}

// Extract one top-level const/let statement (single line) by name.
function extractConst(source, name, indent) {
  const pad = " ".repeat(indent);
  const esc = name.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  const re = new RegExp("^" + pad + "(?:const|let) " + esc + "\\s*=");
  for (const line of source.split("\n")) {
    if (re.test(line)) return line;
  }
  throw new Error("const " + name + " not found");
}

// Build a vm context from sandbox globals + source strings
// (prelude first, then extracted functions). The sandbox object is
// the context's global object, so tests can mutate stub state
// (e.g. document.title) between cases.
function makeContext(sandbox, sources) {
  const ctx = vm.createContext(Object.assign({
    console: console,
    setTimeout: setTimeout,
    clearTimeout: clearTimeout,
    setInterval: setInterval,
    clearInterval: clearInterval,
    URL: URL,
    Uint8Array: Uint8Array,
    DataView: DataView,
    ArrayBuffer: ArrayBuffer,
    TextEncoder: TextEncoder,
    TextDecoder: TextDecoder,
    AbortController: AbortController,
    Promise: Promise,
    JSON: JSON,
    Math: Math,
    Number: Number,
    String: String,
    Array: Array,
    Object: Object,
    Map: Map,
    Set: Set,
    RegExp: RegExp,
    Error: Error,
    BigInt: BigInt,
    parseInt: parseInt,
    isFinite: isFinite,
  }, sandbox));
  for (const src of sources) {
    vm.runInContext(src, ctx);
  }
  return ctx;
}

function runIn(ctx, code) {
  return vm.runInContext(code, ctx);
}

// Tiny test runner: `test(name, fn)` registers, `run(file)` runs all
// (sync or async) and sets a non-zero exit code on any failure.
const tests = [];
function test(name, fn) { tests.push({ name: name, fn: fn }); }
function assert(cond, msg) {
  if (!cond) throw new Error(msg || "assertion failed");
}
function assertEq(actual, expected, msg) {
  if (actual !== expected) {
    throw new Error((msg ? msg + ": " : "") + "expected " + JSON.stringify(expected) + ", got " + JSON.stringify(actual));
  }
}
async function run(file) {
  let pass = 0;
  const failures = [];
  for (const t of tests) {
    try {
      await t.fn();
      pass++;
      console.log("  ok - " + t.name);
    } catch (e) {
      failures.push(t.name + ": " + (e && e.stack || e));
      console.log("  FAIL - " + t.name + ": " + e.message);
    }
  }
  console.log(file + ": " + pass + " passed, " + failures.length + " failed");
  if (failures.length) {
    for (const f of failures) console.log("\n" + f);
    process.exitCode = 1;
  }
}

module.exports = { REPO_ROOT, EXT_DIR, readSource, extractFunction, extractMany, extractConst, makeContext, runIn, test, assert, assertEq, run };
