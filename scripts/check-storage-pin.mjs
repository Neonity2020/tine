#!/usr/bin/env node

import path from "node:path";
import { fileURLToPath } from "node:url";
import { storagePinProblems } from "./storage-pin-lib.mjs";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const problems = storagePinProblems(root);
if (problems.length) {
  console.error(`tine-storage pin check failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}
console.log("tine-storage pin OK: exact certified tag, commit, receipt, formats, and no local override.");
