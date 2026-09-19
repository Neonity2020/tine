#!/usr/bin/env node

import fs from "node:fs";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { coverageReferenceProblem } from "./lib/coverage-reference.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
// Every sub-check runs, and the exit code is decided once at the end. Until
// 2026-09-19 each one exited immediately, so the FIRST failure hid every later
// problem: one missing E2E contract entry masked three unresolvable coverage
// references and an invalid status for as long as it stood. A checker that
// stops at the first thing it finds teaches its readers the queue is one deep.
const subChecks = [
  "scripts/check-ui-regression-catalog.mjs",
  "scripts/check-tao-native-titlebar.mjs",
  "scripts/check-linux-window-identity.mjs",
];
let subCheckStatus = 0;
for (const subCheck of subChecks) {
  const result = spawnSync(process.execPath, [path.join(root, subCheck)], { cwd: root, encoding: "utf8" });
  process.stdout.write(result.stdout);
  process.stderr.write(result.stderr);
  if (result.status !== 0) subCheckStatus ||= result.status ?? 1;
}

const index = JSON.parse(fs.readFileSync(path.join(root, "tests/regressions/catalog.json"), "utf8"));
const problems = [];
const inventoryIds = new Set();
// `needs-revalidation` implements the AGENTS.md section 5 evidence-currency rule:
// a relevant source/dependency/harness change moves affected evidence out of a
// coverage claim WITHOUT asserting the bug returned. Until 2026-09-01 the schema
// had no such state, so a row whose proof disappeared could only stay `covered`
// (a lie) or regress to `reported` (a different lie). The MS-14b retirement
// deleted 24 tests that 13 rows cited; that is what forced the state to exist.
const allowedStatuses = new Set([
  "reported",
  "reproduced",
  "fixing",
  "covered",
  "released",
  "closed",
  "exempt",
  "needs-revalidation",
]);

if (index.schemaVersion !== 1 || !Array.isArray(index.inventories) || index.inventories.length < 2) {
  problems.push("regression index must have schemaVersion 1 and at least two inventories");
} else {
  for (const inventory of index.inventories) {
    if (!inventory.id || inventoryIds.has(inventory.id)) problems.push(`duplicate or missing inventory id ${inventory.id}`);
    inventoryIds.add(inventory.id);
    const inventoryPath = path.join(root, inventory.path ?? "");
    if (!fs.existsSync(inventoryPath)) {
      problems.push(`${inventory.id}: missing ${inventory.path}`);
      continue;
    }
    const data = JSON.parse(fs.readFileSync(inventoryPath, "utf8"));
    if (data.schemaVersion !== 1 || !Array.isArray(data.entries)) {
      problems.push(`${inventory.id}: catalog must have schemaVersion 1 and entries`);
      continue;
    }
    for (const entry of data.entries) {
      if (!entry.id?.startsWith(inventory.idPrefix)) problems.push(`${inventory.id}: invalid entry id ${entry.id}`);
      if (!allowedStatuses.has(entry.status)) problems.push(`${entry.id}: invalid status ${entry.status}`);
      // A single issue can contain a later, separately testable regression after
      // an earlier fix shipped. Catalog IDs own behaviors; issue numbers only
      // preserve public provenance and therefore need not be unique.
      for (const issue of entry.sources?.issues ?? []) {
        if (!Number.isInteger(issue) || issue < 1) problems.push(`${entry.id}: invalid issue ${issue}`);
      }
      // Parity with the UI catalog checker, added 2026-09-01. Its absence here is
      // why 13 managed-storage rows could keep claiming `covered` while the tests
      // they cited were deleted: dropping the dead reference would have silently
      // produced a coverage claim backed by nothing. A row that no longer has a
      // test must say so through its status, not by holding an empty list.
      const claimsCoverage = ["covered", "released", "closed"].includes(entry.status);
      if (claimsCoverage && (entry.coverage?.tests?.length ?? 0) === 0) {
        problems.push(`${entry.id}: ${entry.status} entry has no tests`);
      }
      for (const test of entry.coverage?.tests ?? []) {
        // The file AND the named symbol must resolve: a catalog row that names a
        // renamed or deleted test is a coverage claim with no evidence behind it.
        const problem = coverageReferenceProblem(root, test);
        if (problem) problems.push(`${entry.id}: ${problem}`);
      }
    }
  }
}

if (problems.length) {
  console.error(`Regression catalog failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}
console.log(`Regression catalog index OK: ${index.inventories.length} inventories.`);
// A sub-check that failed above still fails the gate; its own output already
// said what and why, so there is nothing to reprint here.
if (subCheckStatus !== 0) process.exit(subCheckStatus);
