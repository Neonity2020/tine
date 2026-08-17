#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { coverageReferenceProblem } from "./lib/coverage-reference.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const catalogPath = path.join(root, "tests/ui-regressions/catalog.json");
const contractsPath = path.join(root, "tests/ui-regressions/e2e-contracts.json");
const catalog = JSON.parse(fs.readFileSync(catalogPath, "utf8"));
const contracts = JSON.parse(fs.readFileSync(contractsPath, "utf8"));
const problems = [];
const ids = new Set();
const issueOwners = new Map();
const allowedPlatforms = new Set(["all", "linux", "windows", "macos", "android"]);
const allowedLayers = new Set(["unit", "render", "browser", "native"]);
const allowedStatuses = new Set(["reported", "reproduced", "fixing", "covered", "released", "closed", "exempt"]);
const allowedFailBefore = new Set(["proven", "reconstructed", "inferred", "unavailable"]);
const allowedContractClasses = new Set(["exact-safety-interoperability", "core-operation", "stateful-ux", "flexible-presentation-heuristic"]);
const allowedStabilities = new Set(["stable", "burn-in", "quarantined"]);

if (catalog.schemaVersion !== 1 || !Array.isArray(catalog.entries)) {
  problems.push("catalog must have schemaVersion 1 and an entries array");
} else {
  for (const [index, entry] of catalog.entries.entries()) {
    const where = `entry ${index + 1}`;
    if (!/^UI-[A-Z0-9-]+$/.test(entry.id ?? "")) problems.push(`${where}: invalid id ${entry.id}`);
    if (ids.has(entry.id)) problems.push(`${where}: duplicate id ${entry.id}`);
    ids.add(entry.id);
    if (typeof entry.title !== "string" || entry.title.length < 5) problems.push(`${entry.id}: missing title`);
    if (typeof entry.family !== "string" || entry.family.length < 2) problems.push(`${entry.id}: missing family`);
    if (!Array.isArray(entry.platforms) || !entry.platforms.length || entry.platforms.some((p) => !allowedPlatforms.has(p))) {
      problems.push(`${entry.id}: invalid platforms`);
    }
    if (!allowedLayers.has(entry.layer)) problems.push(`${entry.id}: invalid layer ${entry.layer}`);
    if (!allowedStatuses.has(entry.status)) problems.push(`${entry.id}: invalid status ${entry.status}`);
    if (!entry.sources || !Array.isArray(entry.sources.issues) || !Array.isArray(entry.sources.commits)) {
      problems.push(`${entry.id}: sources must contain issues and commits arrays`);
    } else {
      for (const issue of entry.sources.issues) {
        if (!Number.isInteger(issue) || issue < 1) problems.push(`${entry.id}: invalid issue ${issue}`);
        const owners = issueOwners.get(issue) ?? [];
        owners.push(entry.id);
        issueOwners.set(issue, owners);
      }
      for (const commit of entry.sources.commits) {
        if (!/^[0-9a-f]{7,40}$/.test(commit)) problems.push(`${entry.id}: invalid commit ${commit}`);
      }
    }
    if (!entry.coverage || !Array.isArray(entry.coverage.tests) || !allowedFailBefore.has(entry.coverage.failBefore)) {
      problems.push(`${entry.id}: invalid coverage`);
      continue;
    }
    const covered = ["covered", "released", "closed"].includes(entry.status);
    if (covered && entry.coverage.tests.length === 0) problems.push(`${entry.id}: covered entry has no tests`);
    if (entry.status === "exempt" && !(entry.coverage.exemption?.length >= 10)) {
      problems.push(`${entry.id}: exemption needs substitute evidence and a reason`);
    }
    for (const test of entry.coverage.tests) {
      // The file AND the named symbol must resolve; a renamed test must move the
      // catalog row with it rather than leaving an unbacked coverage claim.
      const problem = coverageReferenceProblem(root, test);
      if (problem) problems.push(`${entry.id}: ${problem}`);
    }
  }
}

// One GitHub thread may legitimately accumulate a distinct follow-up regression
// after its original behavior shipped (GH #57 is the first concrete example).
// Stable catalog IDs identify behaviors; issue numbers are provenance, not a
// one-to-one ownership key. Keep validating each number above without rejecting
// several independently covered behaviors from the same thread.

const e2eScripts = fs.readdirSync(path.join(root, "scripts"))
  .filter((name) => /^e2e-.*\.mjs$/.test(name))
  .map((name) => `scripts/${name}`)
  .sort();
const runE2eSource = fs.readFileSync(path.join(root, "scripts/run-e2e.mjs"), "utf8");
const runnerScripts = new Set([...runE2eSource.matchAll(/["'](scripts\/[A-Za-z0-9._-]+\.mjs)["']/g)].map((match) => match[1]));
const allowedManifestScripts = new Set([...e2eScripts, ...runnerScripts]);

// ---------------------------------------------------------------------------
// Which journeys anything actually runs.
//
// The checker used to enforce one direction only -- every run-e2e target must be
// catalogued -- so a catalogued journey that no suite selected was invisible. On
// 2026-08-17 nine journeys were in that state, seven of them `stability: "stable"`
// including the published-site security journey: catalogued, contract-bearing,
// and executed by nothing. "Never describe mere test existence as a guarantee"
// (AGENTS.md §5) fails in its most literal form when the catalog itself is the
// only thing that remembers a journey. Enforce the reverse direction too.
// ---------------------------------------------------------------------------

// Only the scenario tuples of a `run-e2e.mjs` suite are *selected*; the file also
// mentions helper scripts it merely spawns as gates.
const suiteSelectedScripts = new Set(
  [...runE2eSource.matchAll(/\[\s*["'][^"']+["']\s*,\s*["'](scripts\/[A-Za-z0-9._-]+\.mjs)["']/g)].map((match) => match[1]),
);
const packageScriptSource = Object.values(JSON.parse(fs.readFileSync(path.join(root, "package.json"), "utf8")).scripts ?? {}).join("\n");
const workflowDir = path.join(root, ".github/workflows");
const workflowSource = fs.existsSync(workflowDir)
  ? fs.readdirSync(workflowDir)
      .filter((name) => /\.ya?ml$/.test(name))
      .map((name) => fs.readFileSync(path.join(workflowDir, name), "utf8"))
      .join("\n")
  : "";

const allScripts = fs.readdirSync(path.join(root, "scripts"))
  .filter((name) => name.endsWith(".mjs"))
  .map((name) => `scripts/${name}`);

// A helper module is exercised by whatever imports it, so follow relative
// imports out of everything a runner invokes directly.
function relativeScriptImports(script) {
  const file = path.join(root, script);
  if (!fs.existsSync(file)) return [];
  const source = fs.readFileSync(file, "utf8");
  return [...source.matchAll(/from\s+["']\.\/([A-Za-z0-9._-]+\.mjs)["']/g)].map((match) => `scripts/${match[1]}`);
}

const pending = allScripts.filter((script) =>
  suiteSelectedScripts.has(script)
  || packageScriptSource.includes(script)
  || workflowSource.includes(script));
const executedScripts = new Set(pending);
while (pending.length) {
  for (const imported of relativeScriptImports(pending.pop())) {
    if (executedScripts.has(imported)) continue;
    executedScripts.add(imported);
    pending.push(imported);
  }
}

if (contracts.schemaVersion !== 1 || !contracts.scenarios || Array.isArray(contracts.scenarios) || typeof contracts.scenarios !== "object") {
  problems.push("E2E contracts must have schemaVersion 1 and a scenarios object");
} else {
  const manifestScripts = Object.keys(contracts.scenarios);
  for (const script of e2eScripts) {
    if (!Object.hasOwn(contracts.scenarios, script)) problems.push(`E2E contracts: missing ${script}`);
  }
  for (const script of runnerScripts) {
    if (!Object.hasOwn(contracts.scenarios, script)) problems.push(`E2E contracts: run-e2e target is not covered: ${script}`);
  }
  for (const script of manifestScripts) {
    if (!fs.existsSync(path.join(root, script))) problems.push(`E2E contracts: nonexistent script ${script}`);
    if (!allowedManifestScripts.has(script)) problems.push(`E2E contracts: unexpected script ${script}`);
    const entry = contracts.scenarios[script];
    if (!entry || Array.isArray(entry) || typeof entry !== "object") {
      problems.push(`E2E contracts: ${script} must be an object`);
      continue;
    }
    if (!Array.isArray(entry.contracts) || entry.contracts.length === 0) {
      problems.push(`E2E contracts: ${script} needs one or more contracts`);
    } else {
      for (const [index, contract] of entry.contracts.entries()) {
        const where = `E2E contracts: ${script} contract ${index + 1}`;
        if (!contract || Array.isArray(contract) || typeof contract !== "object") {
          problems.push(`${where} must be an object`);
          continue;
        }
        if (!allowedContractClasses.has(contract.class)) problems.push(`${where}: invalid class ${contract.class}`);
        if (typeof contract.userOutcome !== "string" || contract.userOutcome.trim().length < 5) problems.push(`${where}: missing userOutcome`);
        if (typeof contract.blocking !== "boolean") problems.push(`${where}: blocking must be boolean`);
        if (contract.class === "exact-safety-interoperability" && !(typeof contract.authority === "string" && contract.authority.trim())) {
          problems.push(`${where}: exact-safety-interoperability requires authority`);
        }
        if (contract.class === "flexible-presentation-heuristic" && contract.blocking) {
          problems.push(`${where}: flexible-presentation-heuristic cannot block`);
        }
      }
    }
    for (const field of ["acceptableVariations", "nonRequirements"]) {
      if (!Array.isArray(entry[field]) || entry[field].some((value) => typeof value !== "string" || !value.trim())) {
        problems.push(`E2E contracts: ${script}: ${field} must be an array of non-empty strings`);
      }
    }
    if (!allowedStabilities.has(entry.stability)) problems.push(`E2E contracts: ${script}: invalid stability ${entry.stability}`);
    if (entry.stability === "quarantined") {
      if (!(typeof entry.quarantineReason === "string" && entry.quarantineReason.trim())) {
        problems.push(`E2E contracts: ${script}: quarantined entries require quarantineReason`);
      } else if (!/\b\d{4}-\d{2}-\d{2}\b/.test(entry.quarantineReason)) {
        // Undated quarantine debt has no expiry and no way to tell a two-day
        // pause from a two-year one.
        problems.push(`E2E contracts: ${script}: quarantineReason must record the date it was quarantined (YYYY-MM-DD)`);
      }
    }
    // The reverse direction: a catalogued journey either runs somewhere, or is
    // explicitly, datedly quarantined. Silence is the failure mode.
    if (!executedScripts.has(script) && entry.stability !== "quarantined") {
      problems.push(
        `E2E contracts: ${script} is catalogued as "${entry.stability}" but no run-e2e suite, npm script or workflow runs it`
        + " -- wire it into a runner, or quarantine it with a dated reason",
      );
    }
  }
}

if (problems.length) {
  console.error(`UI regression catalog failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}

console.log(`UI regression catalog OK: ${catalog.entries.length} entries, ${issueOwners.size} GitHub issues.`);
