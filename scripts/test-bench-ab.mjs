#!/usr/bin/env node

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { spawnSync } from "node:child_process";
import os from "node:os";
import path from "node:path";

const root = process.cwd();
const policy = JSON.parse(readFileSync(path.join(root, "scripts", "bench-policy.json"), "utf8"));
const temporary = mkdtempSync(path.join(os.tmpdir(), "tine-bench-ab-test-"));

function measurement(label, bigLoad, scrollBig) {
  const round = (load, scroll, index) => ({
    calib: 100 + index * 0.2,
    metrics: { bigLoad: { rawMin: load }, scrollBig: { rawMin: scroll } },
    parseStats: { calls: 12, hits: 0, misses: 12 },
  });
  const median = (values) => [...values].sort((a, b) => a - b)[1];
  const spread = (values) => Number(((Math.max(...values) / Math.min(...values) - 1) * 100).toFixed(1));
  return {
    schemaVersion: 2,
    label,
    rounds: bigLoad.map((load, index) => round(load, scrollBig[index], index)),
    calib: 100.2,
    metrics: {
      bigLoad: {
        rawMedianOfRoundMins: median(bigLoad),
        roundMins: bigLoad,
        roundSpreadPct: spread(bigLoad),
      },
      scrollBig: {
        rawMedianOfRoundMins: median(scrollBig),
        roundMins: scrollBig,
        roundSpreadPct: spread(scrollBig),
      },
    },
    parseStats: { calls: 12, hits: 0, misses: 12 },
  };
}

function check(candidate, immutable, previous) {
  const files = { candidate, immutable, previous };
  const args = [path.join(root, "scripts", "check-bench-ab.mjs"), "--policy", path.join(root, "scripts", "bench-policy.json")];
  for (const [label, value] of Object.entries(files)) {
    const file = path.join(temporary, `${label}.json`);
    writeFileSync(file, JSON.stringify(value));
    args.push(`--${label}`, file);
  }
  return spawnSync(process.execPath, args, { cwd: root, encoding: "utf8" });
}

try {
  const stableImmutable = measurement("immutable", [100, 101, 99], [100, 101, 99]);
  const stablePrevious = measurement("previous", [105, 106, 104], [105, 106, 104]);
  const stableCandidate = measurement("candidate", [110, 111, 109], [110, 111, 109]);
  const stable = check(stableCandidate, stableImmutable, stablePrevious);
  assert.equal(stable.status, 0, stable.stderr || stable.stdout);

  // The immutable anchor may contain historical variance that cannot be
  // remeasured. When candidate and previous are reliable and the candidate is
  // within both budgets, baseline-only variance warns but does not fail.
  const unstableImmutable = measurement("immutable", [100, 101, 99], [75.4, 100, 100.2]);
  const unstable = check(stableCandidate, unstableImmutable, stablePrevious);
  assert.equal(unstable.status, 0, unstable.stderr || unstable.stdout);
  assert.match(
    `${unstable.stdout}\n${unstable.stderr}`,
    /warning: immutable\/scrollBig: .*immutable baseline-only variance accepted/,
  );

  // A candidate's favorable median does not waive its own reliability failure.
  const fastOutlierCandidate = measurement(
    "candidate",
    [90, 92, 91],
    [80, 95, 80],
  );
  const fastOutlier = check(fastOutlierCandidate, stableImmutable, stablePrevious);
  assert.notEqual(fastOutlier.status, 0);
  assert.match(`${fastOutlier.stdout}\n${fastOutlier.stderr}`, /candidate\/scrollBig: .*round spread exceeds/);

  // Previous-release variance remains a hard reliability blocker too.
  const unstablePrevious = measurement("previous", [105, 106, 104], [80, 100, 100.2]);
  const previousSpread = check(stableCandidate, stableImmutable, unstablePrevious);
  assert.notEqual(previousSpread.status, 0);
  assert.match(`${previousSpread.stdout}\n${previousSpread.stderr}`, /previous\/scrollBig: .*round spread exceeds/);

  const regressedCandidate = measurement("candidate", [140, 141, 139], [140, 141, 139]);
  const regressed = check(regressedCandidate, stableImmutable, stablePrevious);
  assert.notEqual(regressed.status, 0);
  assert.match(`${regressed.stdout}\n${regressed.stderr}`, /slower than immutable/);

  assert.equal(policy.reliability.rounds, 3);
  console.log("Performance A/B multi-round reliability fixtures passed.");
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
