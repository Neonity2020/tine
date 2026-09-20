#!/usr/bin/env node
// Compact-projection budget driver (SPEC §3, packet P0a). Runs the real-graph
// mode of crates/tine-core/examples/graph_scale_bench.rs on a corpus, writes
// projection-budget.json, and compares it to scripts/projection-budget-policy.json.
//
//   TINE_PROJECTION_CORPUS=~/research/logseq-anonymized node scripts/measure-projection.mjs
//   node scripts/measure-projection.mjs --root <graph> --corpus brikas
//   node scripts/measure-projection.mjs --record-baseline   # today's numbers become the policy baseline
//   node scripts/measure-projection.mjs --paired-search <six-report-wrapper.json>
//   node scripts/measure-projection.mjs --paired-search <six-report-wrapper.json> --record-baseline
//
// Linux-only (the harness reads /proc/self/io and /proc/self/status). Exit code 1
// on a breached ceiling. Never run it on Martin's live graph: the harness copies
// the corpus into a scratch directory, but the corpus itself must be a test graph.

import { execFileSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  baselineFrom,
  baselineFromSearchScaling,
  evaluateBudget,
  evaluateSearchScaling,
  formatRows,
  formatSearchScalingRows,
} from "./lib/projection-budget.mjs";

const repo = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const args = process.argv.slice(2);
const flag = (name) => {
  const at = args.indexOf(name);
  return at >= 0 ? args[at + 1] : undefined;
};
const pairedSearch = flag("--paired-search");
if (args.includes("--paired-search") && !pairedSearch) {
  console.error("measure-projection: --paired-search needs a JSON path");
  process.exit(2);
}
const policyPath = path.join(repo, "scripts/projection-budget-policy.json");
const policy = JSON.parse(fs.readFileSync(policyPath, "utf8"));

if (pairedSearch) {
  const reportPath = path.resolve(pairedSearch);
  const report = JSON.parse(fs.readFileSync(reportPath, "utf8"));
  if (args.includes("--record-baseline")) {
    policy.searchScaling.baseline = baselineFromSearchScaling(report, policy);
    fs.writeFileSync(policyPath, `${JSON.stringify(policy, null, 2)}\n`);
    console.log(`recorded today's paired search-scaling evidence into ${path.relative(repo, policyPath)} (hard caps unchanged)`);
  }
  const { rows, breaches } = evaluateSearchScaling(report, policy);
  console.log(`projection search-scaling budget, report=${path.relative(repo, reportPath)}`);
  console.log(formatSearchScalingRows(rows));
  if (breaches.length) {
    console.error(`projection search-scaling budget: ${breaches.length} ceiling(s) breached: ${breaches.map((row) => row.id).join(", ")}`);
    process.exit(1);
  }
  console.log("projection search-scaling budget OK (strict ratio, hard-ms, and normalized-linearity gates passed; diagnostic rows remain unjudged)");
  process.exit(0);
}

const root = flag("--root") ?? process.env.TINE_PROJECTION_CORPUS;
if (!root) {
  console.error("measure-projection: pass --root <graph> or set TINE_PROJECTION_CORPUS");
  process.exit(2);
}
const corpus = flag("--corpus") ?? "anon";
const out = path.resolve(flag("--out") ?? path.join(repo, "projection-budget.json"));

if (!args.includes("--reuse")) {
  execFileSync(
    "bash",
    [
      "-c",
      `source scripts/env.sh >/dev/null 2>&1; exec cargo run --release -q -p tine-core --example graph_scale_bench -- --root "$0" --corpus "$1" --json "$2"`,
      root,
      corpus,
      out,
    ],
    { cwd: repo, stdio: "inherit" },
  );
}
const measurement = JSON.parse(fs.readFileSync(out, "utf8"));

if (args.includes("--record-baseline")) {
  policy.corpora[corpus].baseline = baselineFrom(measurement);
  fs.writeFileSync(policyPath, `${JSON.stringify(policy, null, 2)}\n`);
  console.log(`recorded today's ${corpus} baseline into ${path.relative(repo, policyPath)}`);
}

const { rows, breaches } = evaluateBudget(measurement, policy);
console.log(`projection budget, corpus=${corpus}, ${measurement.pages} pages, ${measurement.blocks} blocks, ${measurement.markdown_bytes} Markdown bytes`);
console.log(formatRows(rows));
if (breaches.length) {
  console.error(`projection budget: ${breaches.length} ceiling(s) breached: ${breaches.map((b) => b.id).join(", ")}`);
  process.exit(1);
}
console.log("projection budget OK");
