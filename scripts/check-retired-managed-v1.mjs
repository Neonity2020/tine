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
  "legacy_v1_namespace_present",
];

// The clean baseline-plus-manifest actor is the only production actor. These
// names belonged to the interleaved pre-cutover actor state and must not return
// above Rust's test module merely because a legacy regression fixture still
// mentions the old implementation.
const retiredActorResidue = [
  "PendingLocalMutation",
  "PendingSharedJoin",
  "SharedJoinProviderPass",
  "SharedJoinLocalPass",
  "SharedJoinPhase",
  "ProviderDependencyIndex",
  "correlated_move_feed_handoffs",
  "legacy_binding",
  "shared_descriptor",
  "provider_observation_full",
  "provider_scan_valid_heads",
  "provider_scan_invalid_head",
  "provider_head_generations",
  "provider_own_heads",
  "provider_discovery_scan_complete",
  "provider_current_head",
  "provider_dependency_recheck_frontier",
  "provider_accepted_manifest_audit_covered_sequence",
  "provider_accepted_manifest_revalidation_next_sequence",
  "provider_accepted_manifest_revalidation_ready",
  "provider_accepted_manifest_revalidation_after_external_tick",
  "provider_namespace_repair_active",
  "provider_recovery_coverage_root",
];

const retiredV1SplitPath = /\.join\(\s*["']\.tine-sync["']\s*\)\s*\.join\(\s*["']v1["']\s*\)/s;

function sourceProblems(relative, source) {
  const findings = [];
  const executable = source
    .split("\n")
    .filter((line) => !line.trimStart().startsWith("//"))
    .join("\n");
  for (const name of retiredLifecycle) {
    if (new RegExp(`\\b${name}\\b`).test(executable)) {
      findings.push(`retired prototype lifecycle ${name} is compiled in ${relative}`);
    }
  }
  if (relative === "crates/tine-core/src/sync_runtime.rs") {
    for (const name of retiredActorResidue) {
      if (new RegExp(`\\b${name}\\b`).test(executable)) {
        findings.push(`retired actor residue ${name} is compiled in ${relative}`);
      }
    }
  }

  // Comments are excluded because documentation of the retirement is useful and
  // cannot activate a protocol. All compiled legacy-path references are
  // forbidden; test fixtures live below Rust's end-of-file test modules.
  for (const [index, line] of source.split("\n").entries()) {
    if (line.trimStart().startsWith("//")) continue;
    if (!line.includes(".tine-sync/v1")) continue;
    findings.push(`legacy v1 path is compiled outside inert fixture: ${relative}:${index + 1}`);
  }
  if (retiredV1SplitPath.test(executable)) {
    findings.push(`legacy v1 split path is compiled outside inert fixture: ${relative}`);
  }
  return findings;
}

function assertDetectorSelfTests() {
  const probes = [
    ["literal child path", 'let path = ".tine-sync/v1";', "legacy v1 path", "guard-self-test.rs"],
    [
      "split child path",
      'let path = root.join(".tine-sync").join("v1");',
      "legacy v1 split path",
      "guard-self-test.rs",
    ],
    [
      "retired helper",
      "fn legacy_v1_namespace_present() {}",
      "legacy_v1_namespace_present",
      "guard-self-test.rs",
    ],
    [
      "retired actor state",
      "struct PendingSharedJoin {}",
      "PendingSharedJoin",
      "crates/tine-core/src/sync_runtime.rs",
    ],
  ];
  for (const [label, source, expected, relative] of probes) {
    if (!sourceProblems(relative, source).some((finding) => finding.includes(expected))) {
      throw new Error(`retired managed-v1 source guard self-test missed ${label}`);
    }
  }
}

assertDetectorSelfTests();

for (const relative of sourceFiles) {
  problems.push(...sourceProblems(relative, compiledSource(relative)));
}

if (problems.length) {
  console.error(`Retired managed-v1 source guard failed (${problems.length} problem(s)):`);
  for (const problem of problems) console.error(`  ${problem}`);
  process.exit(1);
}

console.log(`Retired managed-v1 source guard OK: ${sourceFiles.length} production source files checked.`);
