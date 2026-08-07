#!/usr/bin/env node

// Same-runner release A/B orchestrator. Each tree is measured in every round,
// with the order rotated so runner warm-up/load cannot consistently favor one
// version. The aggregate preserves every round and uses the median of round
// minima; check-bench-ab.mjs separately rejects excessive per-metric spread.

import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

function arg(name) {
  const index = process.argv.indexOf(name);
  if (index < 0 || !process.argv[index + 1]) throw new Error(`missing ${name}`);
  return process.argv[index + 1];
}

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const policyPath = path.resolve(arg("--policy"));
const policy = JSON.parse(readFileSync(policyPath, "utf8"));
const outputDir = path.resolve(arg("--output-dir"));
const rounds = policy.reliability?.rounds;
const runsPerRound = policy.reliability?.runsPerRound;
if (!Number.isInteger(rounds) || rounds < 3) throw new Error("policy reliability.rounds must be at least 3");
if (!Number.isInteger(runsPerRound) || runsPerRound < 2) throw new Error("policy reliability.runsPerRound must be at least 2");

const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};
const round1 = (value) => Math.round(value * 10) / 10;
const spreadPct = (values) => (Math.max(...values) / Math.min(...values) - 1) * 100;

// This is deliberately an opt-in sibling of the release-version A/B below.
// Each invocation of bench-storage-mode.mjs measures Direct and managed
// storage against the same graph and profile, so its result is a paired mode
// comparison rather than a fourth release baseline. There is no performance
// budget for this axis yet; check-bench-ab.mjs reports it only.
if (process.argv.includes("--storage-mode")) {
  const app = path.resolve(arg("--app"));
  const storage = policy.storageMode;
  if (!storage || !Array.isArray(storage.modes) || storage.modes.join(",") !== "direct,managed") {
    throw new Error("policy storageMode.modes must be [\"direct\", \"managed\"]");
  }
  if (!storage.operations || typeof storage.operations !== "object") {
    throw new Error("policy storageMode.operations is required");
  }
  mkdirSync(path.join(outputDir, "rounds"), { recursive: true });
  const roundRecords = [];
  const orderLog = [];
  for (let round = 0; round < rounds; round++) {
    const runs = [];
    const order = [];
    for (let runIndex = 0; runIndex < runsPerRound; runIndex++) {
      // Alternate which mode performs its save/keystroke sequence first. Cold
      // open remains a fresh native launch in both modes; the mode transition
      // itself is intentionally excluded from these measurements.
      const firstMode = storage.modes[(round + runIndex) % storage.modes.length];
      const destination = path.join(outputDir, "rounds", `storage-mode-r${round + 1}-${runIndex + 1}.json`);
      console.log(`\nround ${round + 1}/${rounds}, run ${runIndex + 1}/${runsPerRound}: ${firstMode} first`);
      const result = spawnSync(
        process.execPath,
        [
          path.join(root, "scripts", "bench-storage-mode.mjs"),
          "--app", app,
          "--mode-order", `${firstMode},${storage.modes.find((mode) => mode !== firstMode)}`,
          "--output", destination,
        ],
        { cwd: root, encoding: "utf8", stdio: "inherit" },
      );
      if (result.status !== 0) process.exit(result.status ?? 1);
      const measurement = JSON.parse(readFileSync(destination, "utf8"));
      runs.push(measurement);
      order.push(measurement.modeOrder);
    }
    roundRecords.push({ round: round + 1, runs });
    orderLog.push(order);
  }

  const operationNames = Object.keys(storage.operations);
  const modes = {};
  for (const mode of storage.modes) {
    const metrics = {};
    for (const operation of operationNames) {
      const roundMins = roundRecords.map(({ runs }) => {
        const values = runs.map((run) => run.modes?.[mode]?.metrics?.[operation]);
        if (values.some((value) => !Number.isFinite(value) || value <= 0)) {
          throw new Error(`${mode}/${operation}: missing or invalid storage-mode measurement`);
        }
        return Math.min(...values);
      });
      metrics[operation] = {
        rawMedianOfRoundMins: round1(median(roundMins)),
        roundMins,
        roundSpreadPct: Number(spreadPct(roundMins).toFixed(1)),
      };
    }
    modes[mode] = { metrics };
  }

  const aggregate = {
    schemaVersion: 1,
    kind: "storage-mode",
    app,
    modes,
    rounds: roundRecords,
    manifest: {
      rounds,
      runsPerRound,
      modeOrder: orderLog,
      graph: "synthetic storage-mode fixture (120 Markdown pages, 80-block edited page)",
      pairing: "each run uses one graph and one XDG profile for both modes",
    },
  };
  writeFileSync(path.join(outputDir, "storage-mode.json"), JSON.stringify(aggregate, null, 2) + "\n");
  console.log(`\npaired storage-mode measurements written to ${path.join(outputDir, "storage-mode.json")}`);
  process.exit(0);
}

const versions = [
  { label: "immutable", dir: path.resolve(arg("--immutable-dir")) },
  { label: "previous", dir: path.resolve(arg("--previous-dir")) },
  { label: "candidate", dir: path.resolve(arg("--candidate-dir")) },
];
mkdirSync(path.join(outputDir, "rounds"), { recursive: true });

const measurements = new Map(versions.map(({ label }) => [label, []]));
const orderLog = [];
for (let round = 0; round < rounds; round++) {
  // Rotate the three-version order each round. With three rounds every version
  // occupies the first, middle, and last position exactly once.
  const order = versions.map((_, index) => versions[(index + round) % versions.length]);
  orderLog.push(order.map(({ label }) => label));
  for (let position = 0; position < order.length; position++) {
    const version = order[position];
    const destination = path.join(outputDir, "rounds", `${version.label}-r${round + 1}.json`);
    const port = 5260 + round * versions.length + position;
    console.log(`\nround ${round + 1}/${rounds}: ${version.label} (position ${position + 1})`);
    const run = spawnSync(
      process.execPath,
      [
        path.join(root, "scripts", "bench.mjs"),
        "--update",
        "--app-dir", version.dir,
        "--runs", String(runsPerRound),
        "--port", String(port),
        "--output", destination,
      ],
      { cwd: root, encoding: "utf8", stdio: "inherit" },
    );
    if (run.status !== 0) process.exit(run.status ?? 1);
    measurements.get(version.label).push(JSON.parse(readFileSync(destination, "utf8")));
  }
}

for (const { label } of versions) {
  const samples = measurements.get(label);
  const metricNames = Object.keys(samples[0].metrics);
  const aggregate = {
    schemaVersion: 2,
    label,
    rounds: samples,
    calib: round1(median(samples.map((sample) => sample.calib))),
    metrics: {},
    // A parse virtualization regression in any candidate round must remain
    // visible; taking a median could hide a one-round full-parse failure.
    parseStats: {
      calls: Math.max(...samples.map((sample) => sample.parseStats?.calls ?? 0)),
      hits: Math.max(...samples.map((sample) => sample.parseStats?.hits ?? 0)),
      misses: Math.max(...samples.map((sample) => sample.parseStats?.misses ?? 0)),
    },
  };
  for (const name of metricNames) {
    const roundMins = samples.map((sample) => sample.metrics[name].rawMin);
    aggregate.metrics[name] = {
      rawMedianOfRoundMins: round1(median(roundMins)),
      roundMins,
      roundSpreadPct: Number(spreadPct(roundMins).toFixed(1)),
    };
  }
  writeFileSync(path.join(outputDir, `${label}.json`), JSON.stringify(aggregate, null, 2) + "\n");
}

writeFileSync(
  path.join(outputDir, "manifest.json"),
  JSON.stringify({ schemaVersion: 1, rounds, runsPerRound, order: orderLog }, null, 2) + "\n",
);
console.log(`\ninterleaved A/B measurements written to ${outputDir}`);
