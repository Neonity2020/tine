#!/usr/bin/env node
// Re-anchor the I-5 print-site census to current line numbers — and ONLY that.
//
// Why this exists: crates/tine-core/tests/content_out_of_logs.rs pins every
// reviewed print site as (file, line). That is deliberate — the allowlist
// records that a HUMAN classified THAT site — but it means any edit adding
// lines above a print site reddens the census even though nothing about the
// program's output changed. During a campaign that edits sync_runtime.rs,
// model.rs, hot_engine.rs and import.rs across eleven successive packets, that
// is a guaranteed false failure per packet.
//
// This tool does NOT rescan the tree for print sites. The census test owns the
// definition of a production print site — which files are exempt, which macros
// count, how `#[path]`-included `_tests.rs` modules are excluded — and a second
// scanner here would be a second answer to one question (I-12). It runs the
// census and re-anchors from the census's own reported sites.
//
// The safety property it must never break: a genuinely NEW or REMOVED print
// site must still stop a human. It refuses to write unless the change is
// provably pure line drift — the multiset of (file, macro) is identical before
// and after. It can re-anchor; it can never bless.
//
// Usage: node scripts/reanchor-print-census.mjs [--check]
//   --check  report drift and exit 1 if the file would change; write nothing.

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const censusFile = path.join(root, "crates/tine-core/tests/content_out_of_logs.rs");
const checkOnly = process.argv.includes("--check");

const run = spawnSync(
  "cargo",
  ["test", "-p", "tine-core", "--test", "content_out_of_logs", "--", "--nocapture"],
  { cwd: root, encoding: "utf8", maxBuffer: 64 * 1024 * 1024 },
);
const output = `${run.stdout || ""}${run.stderr || ""}`;

if (/production_print_sites_equal_the_reviewed_content_free_census \.\.\. ok/.test(output)) {
  console.log("print-site census is already anchored to current line numbers.");
  process.exit(0);
}

const parseSide = (tag) => {
  const m = output.match(new RegExp(`^\\s*${tag}: \\[(.*?)\\]\\s*$`, "ms"));
  if (!m) return null;
  return [...m[1].matchAll(/PrintSite \{ file: "([^"]+)", line: (\d+), macro_name: "([^"]+)" \}/g)]
    .map((s) => ({ file: s[1], line: Number(s[2]), macro: s[3] }));
};

const actual = parseSide("left");
const expected = parseSide("right");
if (!actual || !expected) {
  console.error("could not read the census's reported sites. Raw output tail:\n");
  console.error(output.slice(-4000));
  process.exit(2);
}

const key = (s) => `${s.file} ${s.macro}`;
const tally = (rows) => {
  const c = new Map();
  for (const r of rows) c.set(key(r), (c.get(key(r)) || 0) + 1);
  return c;
};
const actualCounts = tally(actual);
const expectedCounts = tally(expected);

const problems = [];
for (const k of new Set([...actualCounts.keys(), ...expectedCounts.keys()])) {
  const [file, macro] = k.split(" ");
  const was = expectedCounts.get(k) || 0;
  const now = actualCounts.get(k) || 0;
  if (was !== now) {
    problems.push(
      `${file} (${macro}!): allowlist has ${was} site(s), source now has ${now}. ` +
        (now > was
          ? "A NEW print site appeared — classify it by hand in ALLOWLIST; this tool will not bless it."
          : "A print site was removed — delete its ALLOWLIST entry by hand."),
    );
  }
}
if (problems.length > 0) {
  console.error("refusing to re-anchor: this is not pure line drift.\n");
  for (const p of problems) console.error(`  - ${p}`);
  console.error("\nThe I-5 census records that a human classified each site.");
  console.error("Only line numbers may be re-anchored mechanically.");
  process.exit(2);
}

// Pure drift. Map expected -> actual per (file, macro) by sorted position.
const pool = new Map();
for (const s of actual) {
  if (!pool.has(key(s))) pool.set(key(s), []);
  pool.get(key(s)).push(s.line);
}
for (const list of pool.values()) list.sort((a, b) => a - b);

const censusText = fs.readFileSync(censusFile, "utf8");
const entryRe = /AllowedSite \{ file: "([^"]+)", lines: &\[([0-9, ]*)\], macro_name: "([^"]+)"/g;
const entries = [...censusText.matchAll(entryRe)].map((m) => ({
  file: m[1],
  lines: m[2].split(",").map((v) => v.trim()).filter(Boolean).map(Number),
  macro: m[3],
  raw: m[0],
}));

// Assign by LINE ORDER, not by entry order. Several AllowedSite entries can
// share one (file, macro) — each carries its own class/why/gate — and they are
// not written in line order. Consuming the pool in entry order would hand one
// entry's lines to another and silently re-classify a reviewed site, which is
// precisely the "bless" this tool must never do. Line drift preserves relative
// order, so the i-th smallest old line is the i-th smallest new line.
const slots = new Map(); // key -> [{entryIndex, position, oldLine}]
entries.forEach((entry, entryIndex) => {
  const k = `${entry.file} ${entry.macro}`;
  if (!slots.has(k)) slots.set(k, []);
  entry.lines.forEach((oldLine, position) =>
    slots.get(k).push({ entryIndex, position, oldLine }),
  );
});

const assigned = entries.map((e) => e.lines.slice());
for (const [k, list] of slots) {
  const available = pool.get(k);
  if (!available) continue;
  if (available.length !== list.length) {
    console.error(`internal: ${k} has ${list.length} allowlisted vs ${available.length} actual`);
    process.exit(2);
  }
  list.sort((a, b) => a.oldLine - b.oldLine);
  list.forEach((slot, i) => {
    assigned[slot.entryIndex][slot.position] = available[i];
  });
}

let updated = censusText;
let changed = 0;
entries.forEach((entry, entryIndex) => {
  const next = assigned[entryIndex];
  if (next.join(",") === entry.lines.join(",")) return;
  updated = updated.replace(
    entry.raw,
    entry.raw.replace(/lines: &\[[0-9, ]*\]/, `lines: &[${next.join(", ")}]`),
  );
  changed += 1;
  console.log(`  ${entry.file} (${entry.macro}!): [${entry.lines}] -> [${next}]`);
});
updated = updated.replace(
  /const RUST_PRINT_SITE_COUNT: usize = \d+;/,
  `const RUST_PRINT_SITE_COUNT: usize = ${actual.length};`,
);

if (updated === censusText) {
  console.log("census reports a failure this tool cannot express as line drift.");
  process.exit(2);
}
if (checkOnly) {
  console.error(`\ncensus is ${changed} entr${changed === 1 ? "y" : "ies"} out of date (pure line drift).`);
  console.error("run: node scripts/reanchor-print-census.mjs");
  process.exit(1);
}
fs.writeFileSync(censusFile, updated);
console.log(`\nre-anchored ${changed} entries; ${actual.length} sites total.`);
