#!/usr/bin/env node

// Martin authorized this exact baseline exception to cut a release from a
// master branch with a measured pre-existing red set: v0.6.981 first, and
// v0.6.982 after he re-approved it on 2026-09-04 against a freshly measured
// baseline. It is intentionally NOT a rolling allowlist. Every helper returns
// the ordinary strict contract for any other version, including v0.6.983 and
// the next minor release, so each release must re-measure and re-decide.
//
// Re-baselining is the whole point of the expiry, so do it honestly: carry a
// name forward only after measuring that it is still red. The v0.6.982 pass
// dropped three names that had recovered
// (`managed_barrier_collapse_does_not_change_direct_files_retire_publish_barriers`,
// `managed_projection_replacement_falls_back_but_direct_files_stays_strict`,
// `projection_producer_census::g_e_shipped_native_targets_and_writers_are_pinned`),
// permanently returning them to the gate. `retiredManagedV1AllowedProblems` is
// likewise already inert — `check-retired-managed-v1.mjs` passes under the
// strict contract — and should be emptied (schemaVersion 2, allowing empty
// arrays) in the next cycle rather than during a release freeze.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const exceptionPath = path.join(root, "scripts/release-ci-exception.json");
const packagePath = path.join(root, "package.json");

export const ONE_RELEASE_CI_EXCEPTION_VERSION = "0.6.982";
export const PROJECT_VERSION = JSON.parse(fs.readFileSync(packagePath, "utf8")).version;
export const ONE_RELEASE_CI_EXCEPTION = Object.freeze(JSON.parse(fs.readFileSync(exceptionPath, "utf8")));

if (ONE_RELEASE_CI_EXCEPTION.schemaVersion !== 1) {
  throw new Error(`unsupported release CI exception schema ${ONE_RELEASE_CI_EXCEPTION.schemaVersion}`);
}
if (ONE_RELEASE_CI_EXCEPTION.releaseVersion !== ONE_RELEASE_CI_EXCEPTION_VERSION) {
  throw new Error(
    `release CI exception ledger names ${ONE_RELEASE_CI_EXCEPTION.releaseVersion}, expected ${ONE_RELEASE_CI_EXCEPTION_VERSION}`
  );
}

for (const field of [
  "linuxAdditionalKnownRedTestNames",
  "releaseE2eNonblockingScenarioKeys",
  "retiredManagedV1AllowedProblems",
  "windowsMissingRequiredTestNames",
]) {
  const values = ONE_RELEASE_CI_EXCEPTION[field];
  if (!Array.isArray(values) || values.length === 0 || values.some((value) => typeof value !== "string" || value === "")) {
    throw new Error(`release CI exception ${field} must be a non-empty string array`);
  }
  if (new Set(values).size !== values.length) {
    throw new Error(`release CI exception ${field} contains duplicates`);
  }
  const sorted = [...values].sort();
  if (values.some((value, index) => value !== sorted[index])) {
    throw new Error(`release CI exception ${field} must stay sorted`);
  }
}

export function oneReleaseCiExceptionActive(version = PROJECT_VERSION) {
  return version === ONE_RELEASE_CI_EXCEPTION_VERSION;
}

export function linuxReleaseExcludedTestNames(ordinaryNames, version = PROJECT_VERSION) {
  const names = oneReleaseCiExceptionActive(version)
    ? [...ordinaryNames, ...ONE_RELEASE_CI_EXCEPTION.linuxAdditionalKnownRedTestNames]
    : [];
  return [...new Set(names)].sort();
}

export function windowsRequiredTestNames(ordinaryNames, version = PROJECT_VERSION) {
  if (!oneReleaseCiExceptionActive(version)) return [...ordinaryNames];
  const missing = new Set(ONE_RELEASE_CI_EXCEPTION.windowsMissingRequiredTestNames);
  return ordinaryNames.filter((name) => !missing.has(name));
}

export function classifyRetiredManagedV1Problems(problems, version = PROJECT_VERSION) {
  const allowed = oneReleaseCiExceptionActive(version)
    ? new Set(ONE_RELEASE_CI_EXCEPTION.retiredManagedV1AllowedProblems)
    : new Set();
  return {
    allowed: problems.filter((problem) => allowed.has(problem)),
    unexpected: problems.filter((problem) => !allowed.has(problem)),
  };
}

export function releaseE2eScenarioIsNonblocking(suiteName, scenarioId, version = PROJECT_VERSION) {
  if (!oneReleaseCiExceptionActive(version)) return false;
  return ONE_RELEASE_CI_EXCEPTION.releaseE2eNonblockingScenarioKeys.includes(`${suiteName}:${scenarioId}`);
}
