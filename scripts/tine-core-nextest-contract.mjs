#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

const CORE_EXACT_WITNESS_SUFFIXES = Object.freeze([
  "a_live_promoted_runtime_blocks_every_newcomer_before_any_durable_write",
  "bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "authoritative_snapshot_prunes_lock_namespaces_before_reading_files",
  "a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "separate_process_workspace_lease_contends_and_crash_releases",
]);

const CORE_WITNESS_PREFIXES = Object.freeze([
  "inactive_bootstrap_capture",
]);

const STORAGE_EXACT_WITNESS_SUFFIXES = Object.freeze([
  "nonblocking_lock_contention_classifier_is_narrow_and_platform_explicit",
]);

function fail(message) {
  throw new Error(`tine-core nextest contract: ${message}`);
}

function testKey(binaryId, testName) {
  return `${binaryId}\u0000${testName}`;
}

export function inventoryFromNextestList(packageName, list) {
  if (!list || typeof list !== "object" || !list["rust-suites"] || typeof list["rust-suites"] !== "object") {
    fail(`${packageName} list did not contain rust-suites`);
  }

  const tests = new Map();
  for (const [binaryId, suite] of Object.entries(list["rust-suites"])) {
    if (suite?.["package-name"] !== packageName) continue;
    if (!suite.testcases || typeof suite.testcases !== "object") {
      fail(`${packageName} binary ${binaryId} did not contain testcases`);
    }
    for (const [testName, testcase] of Object.entries(suite.testcases)) {
      // `cargo nextest run` does not run #[ignore] tests without an explicit
      // --run-ignored argument. Match that exact normal test-run inventory.
      if (testcase?.ignored || testcase?.["filter-match"]?.status !== "matches") continue;
      const key = testKey(binaryId, testName);
      if (tests.has(key)) fail(`${packageName} listed ${binaryId} ${testName} twice`);
      tests.set(key, { binaryId, testName });
    }
  }
  if (tests.size === 0) fail(`${packageName} selected no non-ignored tests`);
  return { packageName, tests };
}

export function verifyLinuxShardCoverage(fullInventory, shardInventories) {
  if (fullInventory?.packageName !== "tine-core") fail("Linux full inventory is not tine-core");
  if (!Array.isArray(shardInventories) || shardInventories.length !== LINUX_TINE_CORE_SHARD_COUNT) {
    fail(`expected ${LINUX_TINE_CORE_SHARD_COUNT} Linux shard inventories`);
  }

  const ownerByTest = new Map();
  for (const [index, shard] of shardInventories.entries()) {
    if (shard?.packageName !== "tine-core") fail(`Linux shard ${index + 1} is not tine-core`);
    if (shard.tests.size === 0) fail(`Linux shard ${index + 1} selected no tests`);
    for (const [key, test] of shard.tests) {
      if (!fullInventory.tests.has(key)) {
        fail(`Linux shard ${index + 1} selected non-inventory test ${test.binaryId} ${test.testName}`);
      }
      const priorOwner = ownerByTest.get(key);
      if (priorOwner !== undefined) {
        fail(`Linux shards ${priorOwner} and ${index + 1} both selected ${test.binaryId} ${test.testName}`);
      }
      ownerByTest.set(key, index + 1);
    }
  }

  const missing = [...fullInventory.tests.entries()]
    .filter(([key]) => !ownerByTest.has(key))
    .map(([, test]) => `${test.binaryId} ${test.testName}`);
  if (missing.length > 0) fail(`Linux shards omitted ${missing.length} tests (first: ${missing[0]})`);

  return { testCount: fullInventory.tests.size, shardCounts: shardInventories.map((shard) => shard.tests.size) };
}

function requireUniqueSuffix(inventory, suffix) {
  const matching = [...inventory.tests.values()].filter(
    (test) => test.binaryId === inventory.packageName && test.testName.endsWith(suffix)
  );
  if (matching.length !== 1) {
    fail(`${inventory.packageName} must contain exactly one required witness ending ${suffix}; found ${matching.length}`);
  }
}

function requirePrefix(inventory, prefix) {
  const matching = [...inventory.tests.values()].filter(
    (test) => test.binaryId === inventory.packageName && test.testName.includes(prefix)
  );
  if (matching.length === 0) fail(`${inventory.packageName} omitted required witness family ${prefix}`);
  return matching.length;
}

export function verifyWindowsFullSelection(coreInventory, storageInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Windows core inventory is not tine-core");
  if (storageInventory?.packageName !== "tine-storage") fail("Windows storage inventory is not tine-storage");

  for (const suffix of CORE_EXACT_WITNESS_SUFFIXES) requireUniqueSuffix(coreInventory, suffix);
  const bootstrapWitnessCount = CORE_WITNESS_PREFIXES.reduce(
    (count, prefix) => count + requirePrefix(coreInventory, prefix),
    0
  );
  for (const suffix of STORAGE_EXACT_WITNESS_SUFFIXES) requireUniqueSuffix(storageInventory, suffix);

  const windowsNamedCount = [...coreInventory.tests.values()]
    .filter((test) => test.testName.toLowerCase().includes("windows"))
    .length;
  if (windowsNamedCount === 0) fail("Windows core inventory contains no explicitly Windows-named tests");

  return {
    coreTestCount: coreInventory.tests.size,
    storageTestCount: storageInventory.tests.size,
    windowsNamedCount,
    bootstrapWitnessCount,
  };
}

function nextestList(profile, packageName, partition) {
  const args = ["nextest", "list", "--profile", profile, "--package", packageName, "--message-format", "json"];
  if (partition) args.push("--partition", partition);
  const result = spawnSync("cargo", args, { cwd: process.cwd(), encoding: "utf8" });
  if (result.error) fail(`could not start cargo nextest list: ${result.error.message}`);
  if (result.status !== 0) {
    fail(`cargo nextest list for ${packageName}${partition ? ` (${partition})` : ""} failed:\n${result.stderr}`);
  }
  try {
    return inventoryFromNextestList(packageName, JSON.parse(result.stdout));
  } catch (error) {
    if (error instanceof SyntaxError) fail(`cargo nextest list for ${packageName} did not emit JSON: ${error.message}`);
    throw error;
  }
}

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function main() {
  const mode = option("--mode");
  if (mode === "linux") {
    const full = nextestList("ci", "tine-core");
    const shards = Array.from({ length: LINUX_TINE_CORE_SHARD_COUNT }, (_, index) =>
      nextestList("ci", "tine-core", `hash:${index + 1}/${LINUX_TINE_CORE_SHARD_COUNT}`)
    );
    const result = verifyLinuxShardCoverage(full, shards);
    console.log(
      `Linux nextest contract OK: ${result.testCount} tine-core tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}).`
    );
    return;
  }
  if (mode === "windows") {
    const core = nextestList("ci-windows", "tine-core");
    const storage = nextestList("ci-windows", "tine-storage");
    const result = verifyWindowsFullSelection(core, storage);
    console.log(
      `Windows nextest contract OK: ${result.coreTestCount} tine-core tests, ${result.storageTestCount} tine-storage tests, ${result.windowsNamedCount} Windows-named tests, and ${result.bootstrapWitnessCount} bootstrap witnesses selected.`
    );
    return;
  }
  fail("pass --mode linux or --mode windows");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
