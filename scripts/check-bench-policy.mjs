#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const policy = JSON.parse(fs.readFileSync(path.join(root, "scripts/bench-policy.json"), "utf8"));
const version = JSON.parse(fs.readFileSync(path.join(root, "src-tauri/tauri.conf.json"), "utf8")).version;
const problems = [];

if (policy.schemaVersion !== 2) problems.push(`bench policy schema is ${policy.schemaVersion}; expected 2`);
if (!Number.isInteger(policy.reliability?.rounds) || policy.reliability.rounds < 3) {
  problems.push("bench policy must require at least three interleaved rounds");
}
if (!Number.isInteger(policy.reliability?.runsPerRound) || policy.reliability.runsPerRound < 2) {
  problems.push("bench policy must require at least two measured runs per round");
}
for (const [name, budget] of Object.entries(policy.metrics ?? {})) {
  if (!Number.isFinite(budget.maxRoundSpreadPct) || budget.maxRoundSpreadPct <= 0) {
    problems.push(`${name} is missing a positive maxRoundSpreadPct reliability budget`);
  }
}
if (!Number.isInteger(policy.storageMode?.requiredFixture?.minTextFiles)
    || policy.storageMode.requiredFixture.minTextFiles < 1_000) {
  problems.push("storageMode must require a real-scale fixture of at least 1,000 text files");
}
for (const [name, budget] of Object.entries(policy.storageMode?.operations ?? {})) {
  if (!Number.isFinite(budget.managedMaxMs) || budget.managedMaxMs <= 0) {
    problems.push(`storageMode.${name} is missing a positive managedMaxMs`);
  }
  if (budget.managedMaxDeltaPct !== undefined
      && (!Number.isFinite(budget.managedMaxDeltaPct) || budget.managedMaxDeltaPct < 0)) {
    problems.push(`storageMode.${name} has an invalid managedMaxDeltaPct`);
  }
  if (budget.maxRoundSpreadPct !== undefined
      && (!Number.isFinite(budget.maxRoundSpreadPct) || budget.maxRoundSpreadPct <= 0)) {
    problems.push(`storageMode.${name} has an invalid maxRoundSpreadPct`);
  }
}

function argument(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function gitOutput(args) {
  const result = spawnSync("git", args, { cwd: root, encoding: "utf8" });
  // Some restricted process supervisors report EPERM on the wrapper wait even
  // though the child completed, returned status 0, and supplied its output.
  // The exit status remains the authority; a real git failure is still fatal.
  if (result.status !== 0 || result.signal || typeof result.stdout !== "string") {
    const detail = result.stderr || result.error?.message || `status ${result.status}`;
    throw new Error(`git ${args.join(" ")} failed: ${detail}`);
  }
  return result.stdout;
}

function reachableReleaseTags() {
  const output = gitOutput(["tag", "--merged", "HEAD", "--sort=-version:refname"]);
  return output
    .split(/\r?\n/)
    .filter((tag) => /^v\d+\.\d+\.\d+$/.test(tag));
}

let expectedPrevious = argument("--expected-previous");
if (!expectedPrevious) {
  const candidateTag = `v${version}`;
  const workflowTag = process.env.GITHUB_REF?.startsWith("refs/tags/")
    ? process.env.GITHUB_REF.slice("refs/tags/".length)
    : undefined;
  let tags = reachableReleaseTags();

  // A tagged candidate still compares with the release before itself. Manual
  // candidate runs and ordinary master builds have no candidate tag at HEAD.
  const candidateAtHead = gitOutput(["tag", "--points-at", "HEAD"])
    .split(/\r?\n/)
    .includes(candidateTag);
  if (workflowTag === candidateTag || candidateAtHead) tags = tags.filter((tag) => tag !== candidateTag);
  expectedPrevious = tags[0];
}

if (!/^v\d+\.\d+\.\d+$/.test(policy.immutableBaseline?.ref ?? "")) {
  problems.push("immutableBaseline.ref is not a release tag");
}
if (!expectedPrevious) {
  problems.push("could not determine the most recent published release tag; fetch full tag history");
} else if (policy.previousRelease?.ref !== expectedPrevious) {
  problems.push(
    `previousRelease.ref is ${policy.previousRelease?.ref ?? "missing"}; expected most recent published release ${expectedPrevious}`
  );
}
if (policy.immutableBaseline?.ref !== "v0.4.7") {
  problems.push(`immutableBaseline.ref moved from the fixed v0.4.7 anchor to ${policy.immutableBaseline?.ref ?? "missing"}`);
}

if (problems.length) {
  console.error(`Benchmark policy failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}

console.log(
  `Benchmark policy OK: immutable ${policy.immutableBaseline.ref}, previous ${policy.previousRelease.ref}.`
);
