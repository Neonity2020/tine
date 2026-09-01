#!/usr/bin/env node
// Attribute the managed crash-reopen aging curve to an open STAGE.
//
// Runs `managed_open_stage_attribution_manual_benchmark` (release, ignored) a
// number of times and reports, per checkpoint, the median per-stage wall time,
// that stage's SHARE of the clean recovery, and its growth factor against the
// first checkpoint. The box this runs on is shared, so shares and growth
// factors within one process invocation are the signal; absolute milliseconds
// across runs are not.
//
//   node scripts/harvest-a1-open-attribution.mjs [--runs 3] [--checkpoints 50,400,800]
//
// Requires `source scripts/env.sh` first (cargo is not on the default PATH).

import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import path from "node:path";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");

function flag(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 && process.argv[index + 1] ? process.argv[index + 1] : fallback;
}

const runs = Number.parseInt(flag("runs", "3"), 10);
const checkpoints = flag("checkpoints", "50,400,800");
const testName = "managed_open_stage_attribution_manual_benchmark";

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = sorted.length >> 1;
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

// checkpoint -> { reopen: [], recovery: [], stages: Map<stage, []>, counters: {} }
const observed = new Map();
function slot(checkpoint) {
  if (!observed.has(checkpoint)) {
    observed.set(checkpoint, {
      reopen: [],
      recovery: [],
      stages: new Map(),
      counters: new Map(),
    });
  }
  return observed.get(checkpoint);
}

function fields(line) {
  const out = {};
  for (const token of line.trim().split(/\s+/).slice(1)) {
    const at = token.indexOf("=");
    if (at > 0) out[token.slice(0, at)] = token.slice(at + 1);
  }
  return out;
}

for (let run = 0; run < runs; run += 1) {
  process.stderr.write(`# run ${run + 1}/${runs}\n`);
  const result = spawnSync(
    "cargo",
    [
      "test",
      "-p",
      "tine-core",
      "--release",
      "--lib",
      testName,
      "--",
      "--ignored",
      "--nocapture",
      "--test-threads=1",
    ],
    {
      cwd: repoRoot,
      encoding: "utf8",
      maxBuffer: 256 * 1024 * 1024,
      env: { ...process.env, TINE_MANAGED_OPEN_ATTRIBUTION_CHECKPOINTS: checkpoints },
    },
  );
  if (result.status !== 0) {
    process.stderr.write(result.stdout ?? "");
    process.stderr.write(result.stderr ?? "");
    throw new Error(`${testName} failed on run ${run + 1}`);
  }
  for (const line of `${result.stdout}\n${result.stderr}`.split("\n")) {
    if (line.startsWith("attribution_stage ")) {
      const row = fields(line);
      const entry = slot(row.checkpoint);
      if (!entry.stages.has(row.stage)) entry.stages.set(row.stage, []);
      entry.stages.get(row.stage).push(Number.parseFloat(row.stage_ms));
    } else if (line.startsWith("attribution_counters ")) {
      const row = fields(line);
      const entry = slot(row.checkpoint);
      for (const [key, value] of Object.entries(row)) {
        if (key === "checkpoint") continue;
        if (!entry.counters.has(key)) entry.counters.set(key, []);
        entry.counters.get(key).push(value);
      }
    } else if (line.startsWith("attribution ")) {
      const row = fields(line);
      const entry = slot(row.checkpoint);
      entry.reopen.push(Number.parseFloat(row.reopen_ms));
      entry.recovery.push(Number.parseFloat(row.clean_recovery_ms));
    }
  }
}

const order = [...observed.keys()].sort((a, b) => Number(a) - Number(b));
if (order.length === 0) throw new Error("no attribution lines were produced");
const base = order[0];

const stageNames = [];
for (const checkpoint of order) {
  for (const stage of observed.get(checkpoint).stages.keys()) {
    if (!stageNames.includes(stage)) stageNames.push(stage);
  }
}

const report = [];
report.push(`# runs=${runs} checkpoints=${order.join(",")}`);
report.push("");
report.push("## Whole reopen (median of runs, ms)");
report.push("| N | reopen_ms | clean_recovery_ms | growth vs N=" + base + " |");
report.push("| --- | --- | --- | --- |");
const baseReopen = median(observed.get(base).reopen);
for (const checkpoint of order) {
  const entry = observed.get(checkpoint);
  report.push(
    `| ${checkpoint} | ${median(entry.reopen).toFixed(1)} | ${median(entry.recovery).toFixed(1)} | ${(median(entry.reopen) / baseReopen).toFixed(2)}x |`,
  );
}
report.push("");
report.push("## Per stage (median ms, share of clean recovery, growth vs N=" + base + ")");
report.push(
  "| stage | " +
    order.map((n) => `N=${n} ms`).join(" | ") +
    " | " +
    order.map((n) => `N=${n} share`).join(" | ") +
    ` | growth ${base}->${order[order.length - 1]} |`,
);
report.push("| --- |" + " --- |".repeat(order.length * 2 + 1));
const rows = [];
for (const stage of stageNames) {
  const cells = order.map((checkpoint) => {
    const samples = observed.get(checkpoint).stages.get(stage) ?? [0];
    return median(samples);
  });
  const shares = order.map((checkpoint, index) => {
    const recovery = median(observed.get(checkpoint).recovery) || 1;
    return cells[index] / recovery;
  });
  const growth = cells[0] > 0 ? cells[cells.length - 1] / cells[0] : Number.POSITIVE_INFINITY;
  rows.push({ stage, cells, shares, growth, last: cells[cells.length - 1] });
}
rows.sort((a, b) => b.last - a.last);
for (const row of rows) {
  report.push(
    `| ${row.stage} | ${row.cells.map((v) => v.toFixed(1)).join(" | ")} | ${row.shares
      .map((v) => `${(v * 100).toFixed(1)}%`)
      .join(" | ")} | ${Number.isFinite(row.growth) ? `${row.growth.toFixed(2)}x` : "n/a"} |`,
  );
}
report.push("");
report.push("## Counters (last run)");
const counterNames = [...(observed.get(order[0]).counters.keys() ?? [])];
report.push("| counter | " + order.map((n) => `N=${n}`).join(" | ") + " |");
report.push("| --- |" + " --- |".repeat(order.length));
for (const counter of counterNames) {
  const cells = order.map((checkpoint) => {
    const samples = observed.get(checkpoint).counters.get(counter) ?? [];
    return samples[samples.length - 1] ?? "";
  });
  report.push(`| ${counter} | ${cells.join(" | ")} |`);
}
report.push("");
report.push("Min/max per stage (ms), for the noise claim:");
for (const stage of stageNames) {
  const spans = order.map((checkpoint) => {
    const samples = observed.get(checkpoint).stages.get(stage) ?? [0];
    return `N=${checkpoint}:${Math.min(...samples).toFixed(1)}-${Math.max(...samples).toFixed(1)}`;
  });
  report.push(`- ${stage}: ${spans.join(" ")}`);
}
process.stdout.write(`${report.join("\n")}\n`);
