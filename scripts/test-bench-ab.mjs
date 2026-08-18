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
  const median = (values) => {
    const sorted = [...values].sort((a, b) => a - b);
    const middle = Math.floor(sorted.length / 2);
    return sorted.length % 2 === 1 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
  };
  // Each scenario is written as three values whose min/median/max encode the
  // shape under test. The policy's round count is higher than three, so pad
  // with copies of the median: min, max, spread and median are all preserved,
  // and every scenario keeps meaning exactly what its three values say.
  const pad = (values) => {
    const padded = [...values];
    while (padded.length < policy.reliability.rounds) padded.push(median(values));
    return padded;
  };
  bigLoad = pad(bigLoad);
  scrollBig = pad(scrollBig);
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

function assertFails(result, expected, description) {
  assert.notEqual(result.status, 0, description);
  assert.match(`${result.stdout}\n${result.stderr}`, expected, description);
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

  // Candidate-only variance is safe when a fast outlier raises the symmetric
  // spread but the median beats both anchors and the slowest round remains
  // inside both performance budgets. This is the exact run 30963828453 shape.
  const favorableVariableCandidate = measurement(
    "candidate",
    [90, 92, 91],
    [80, 95, 80],
  );
  const favorableVariable = check(favorableVariableCandidate, stableImmutable, stablePrevious);
  assert.equal(favorableVariable.status, 0, favorableVariable.stderr || favorableVariable.stdout);
  assert.match(
    `${favorableVariable.stdout}\n${favorableVariable.stderr}`,
    /warning: candidate\/scrollBig: .*candidate median beats both anchors and its slowest round remains within both budgets/,
  );

  // The candidate waiver belongs only to candidate variance. If the immutable
  // anchor is unstable too, its own waiver cannot apply because the candidate
  // is no longer reliable.
  const candidateAndImmutableVariable = check(
    favorableVariableCandidate,
    unstableImmutable,
    stablePrevious,
  );
  assertFails(
    candidateAndImmutableVariable,
    /immutable\/scrollBig: .*round spread exceeds/,
    "candidate-only variance does not waive immutable-anchor variance",
  );

  // A favorable median cannot conceal an unsafe slow tail, including when it
  // only breaches the tighter previous-release budget.
  const unsafeTailCandidate = measurement("candidate", [90, 92, 91], [50, 50, 128]);
  const unsafeTail = check(unsafeTailCandidate, stableImmutable, stablePrevious);
  assertFails(
    unsafeTail,
    /candidate\/scrollBig: .*round spread exceeds/,
    "unsafe candidate slow tail remains a blocker",
  );

  // A variable candidate whose median does not beat both anchors remains a
  // reliability failure even though its slowest round itself is budget-safe.
  const nonFavorableMedianCandidate = measurement("candidate", [90, 92, 91], [90, 110, 120]);
  const nonFavorableMedian = check(nonFavorableMedianCandidate, stableImmutable, stablePrevious);
  assertFails(
    nonFavorableMedian,
    /candidate\/scrollBig: .*round spread exceeds/,
    "candidate variance requires a median no slower than both anchors",
  );

  // The rolling previous-release anchor can be the one noisy measurement:
  // accept it only when the candidate and immutable anchor are both reliable,
  // and the candidate median plus slowest round stay inside both budgets.
  const unstablePrevious = measurement("previous", [105, 106, 104], [80, 100, 100.2]);
  const previousSpread = check(stableCandidate, stableImmutable, unstablePrevious);
  assert.equal(previousSpread.status, 0, previousSpread.stderr || previousSpread.stdout);
  assert.match(
    `${previousSpread.stdout}\n${previousSpread.stderr}`,
    /warning: previous\/scrollBig: .*previous-release baseline-only variance accepted/,
  );

  // The previous-anchor exception is not a multi-anchor exception: a noisy
  // immutable anchor still blocks the run even when previous is the noisy
  // rolling anchor too.
  const previousAndImmutableVariable = check(
    stableCandidate,
    unstableImmutable,
    unstablePrevious,
  );
  assertFails(
    previousAndImmutableVariable,
    /immutable\/scrollBig: .*round spread exceeds/,
    "previous-anchor variance does not waive immutable-anchor variance",
  );

  // Candidate variance remains independently unsafe for the previous-anchor
  // exception, even if the older candidate-only exception can explain a fast
  // outlier on its own.
  const previousAndCandidateVariable = check(
    favorableVariableCandidate,
    stableImmutable,
    unstablePrevious,
  );
  assertFails(
    previousAndCandidateVariable,
    /previous\/scrollBig: .*round spread exceeds/,
    "previous-anchor variance requires a reliable candidate",
  );

  // A previous-anchor waiver also cannot hide a candidate slow tail, even when
  // the candidate median is favorable.
  const previousVarianceUnsafeTail = check(
    unsafeTailCandidate,
    stableImmutable,
    unstablePrevious,
  );
  assertFails(
    previousVarianceUnsafeTail,
    /candidate\/scrollBig: .*round spread exceeds/,
    "previous-anchor variance does not waive a candidate slow tail",
  );

  // Neither variance waiver can excuse a candidate that breaches either
  // baseline's median budget.
  const immutableBudgetBreach = check(
    measurement("candidate", [110, 111, 109], [131, 131, 131]),
    stableImmutable,
    measurement("previous", [200, 201, 199], [200, 201, 199]),
  );
  assertFails(
    immutableBudgetBreach,
    /scrollBig: 31\.0% slower than immutable/,
    "immutable budget breach remains a blocker",
  );

  const previousBudgetBreach = check(
    measurement("candidate", [110, 111, 109], [121, 121, 121]),
    measurement("immutable", [140, 141, 139], [140, 141, 139]),
    measurement("previous", [100, 101, 99], [100, 101, 99]),
  );
  assertFails(
    previousBudgetBreach,
    /scrollBig: 21\.0% slower than previous release/,
    "previous-release budget breach remains a blocker",
  );

  // Previous-anchor variance cannot conceal a candidate that breaches the
  // rolling median budget.
  const previousVarianceBudgetBreach = check(
    measurement("candidate", [110, 111, 109], [121, 121, 121]),
    measurement("immutable", [140, 141, 139], [140, 141, 139]),
    unstablePrevious,
  );
  assertFails(
    previousVarianceBudgetBreach,
    /scrollBig: 21\.0% slower than previous release/,
    "previous-anchor variance does not waive a candidate median budget breach",
  );

  // Global runner-load instability invalidates every comparison before either
  // metric-specific variance waiver is considered.
  const calibrationUnstableCandidate = measurement("candidate", [110, 111, 109], [110, 111, 109]);
  calibrationUnstableCandidate.rounds[0].calib = 150;
  const unstableCalibration = check(calibrationUnstableCandidate, stableImmutable, stablePrevious);
  assertFails(
    unstableCalibration,
    /runner load changed too much during A\/B measurement/,
    "calibration instability remains a blocker",
  );

  // The scenarios above are POLICY-DRIVEN (fixtures pad to the configured
  // round count), so this guard only pins the floor the padding logic needs.
  assert.ok(policy.reliability.rounds >= 3, "bench policy must run at least 3 rounds");
  console.log("Performance A/B multi-round reliability fixtures passed.");
} finally {
  rmSync(temporary, { recursive: true, force: true });
}
