#!/usr/bin/env node

// The graph-wide `.tine-sync/v1` CRDT prototype was retired before 0.7.  This
// is intentionally a source-architecture guard, separate from the regression
// catalog: Direct Files and sparse-v2 must not quietly regain its lifecycle.
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const problems = [];

const retiredPaths = [
  "crates/tine-core/src/crdt",
  "crates/tine-core/tests/crdt_convergence.rs",
  "crates/tine-core/tests/crdt_store.rs",
  "crates/tine-core/tests/crdt_structure.rs",
];

for (const retiredPath of retiredPaths) {
  if (fs.existsSync(path.join(root, retiredPath))) {
    problems.push(`retired prototype path returned: ${retiredPath}`);
  }
}

const sourceRoots = ["crates/tine-core/src", "src-tauri/src", "src"];
const sourceFiles = [];

function collectSourceFiles(relative) {
  const absolute = path.join(root, relative);
  for (const entry of fs.readdirSync(absolute, { withFileTypes: true })) {
    const child = path.join(relative, entry.name);
    if (entry.isDirectory()) collectSourceFiles(child);
    else if (entry.isFile() && /\.(?:rs|ts|tsx)$/.test(entry.name)) sourceFiles.push(child);
  }
}

for (const sourceRoot of sourceRoots) collectSourceFiles(sourceRoot);

function compiledSource(relative) {
  const source = fs.readFileSync(path.join(root, relative), "utf8");
  if (!relative.endsWith(".rs")) return source;

  // Every in-source Rust test module in the affected paths is an end-of-file
  // `#[cfg(test)] mod tests` block. Keeping test fixtures out of this source
  // guard lets them preserve inert-byte coverage without allowing a compiled
  // lifecycle to return.
  const testModule = source.search(/^#\[cfg\(test\)\]\s*\nmod tests\s*\{/m);
  return testModule < 0 ? source : source.slice(0, testModule);
}

const retiredLifecycle = [
  "enable_managed_sync",
  "start_managed_sync",
  "project_all_managed_sync",
  "pull_managed_sync",
  "ManagedSyncStoreState",
  "CrdtGraph",
];

for (const relative of sourceFiles) {
  const source = compiledSource(relative);
  for (const name of retiredLifecycle) {
    if (new RegExp(`\\b${name}\\b`).test(source)) {
      problems.push(`retired prototype lifecycle ${name} is compiled in ${relative}`);
    }
  }

  // A sole transitional U2c no-follow validation remains in Graph::open_checked.
  // It is not an engine root and U2c removes it; all other compiled legacy-path
  // references are forbidden. Comments are excluded because documentation of
  // the retirement is useful and cannot activate a protocol.
  for (const [index, line] of source.split("\n").entries()) {
    if (line.trimStart().startsWith("//")) continue;
    if (!line.includes(".tine-sync/v1")) continue;
    const u2cValidation =
      relative === "crates/tine-core/src/model.rs" &&
      line.includes('validate_managed_dir(&graph.root, ".tine-sync/v1", "managed sync store")?;');
    if (!u2cValidation) {
      problems.push(
        `legacy v1 path is compiled outside inert fixture/U2c transition: ${relative}:${index + 1}`,
      );
    }
  }
}

if (problems.length) {
  console.error(`Retired managed-v1 source guard failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}

console.log(`Retired managed-v1 source guard OK: ${sourceFiles.length} production source files checked.`);
