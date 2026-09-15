#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

// Linux runs the complete current tine-core inventory. Every test the release
// gate drops must be a NAMED, deliberately dropped test listed here, with a
// named cause and a fix that lands outside a test file. The list is empty: the
// 2026-09-07 W6-red-corpus ledger named only Managed Storage runtime defects,
// and Managed Storage was removed from Tine on 2026-09-15. Adding a name here
// is an open-bug declaration, not a waiver; shrinking it means fixing the
// product, not renaming the test.
export const KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES = Object.freeze([]);

export function linuxCoreReleaseFilterset(excluded = KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES) {
  return excluded.length === 0
    ? "all()"
    : "not (" + [...excluded]
      .sort()
      .map((testName) => "test(=" + testName + ")")
      .join(" | ") + ")";
}

export const LINUX_CORE_RELEASE_EXCLUDED_TEST_NAMES = Object.freeze(
  [...KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES].sort()
);
export const LINUX_CORE_RELEASE_FILTERSET = linuxCoreReleaseFilterset();
// Windows is deliberately not a second complete tine-core behavior matrix.
// Linux carries that full inventory in four isolated shards. This exact list is
// the Windows release contract: every explicitly Windows-named core test. Keep
// names explicit so a rename, removal, or newly added Windows test cannot
// silently shrink the release gate.
export const WINDOWS_CORE_EXACT_TEST_NAMES = Object.freeze([
  "model::tests::page_name_encoding_is_injective_reversible_and_windows_safe",
  "model::tests::windows_handle_relative_noreplace_renames_the_exact_source",
  "model::tests::windows_handle_relative_noreplace_moves_between_nonstandard_retained_directories_with_unicode",
  "model::tests::windows_handle_relative_noreplace_preserves_occupied_destination",
  "model::tests::windows_first_save_and_ordinary_rename_preserve_exact_projection",
  "model::tests::windows_directory_durability_limit_does_not_block_save_or_rename",
  "model::tests::windows_direct_publication_event_waits_for_inflight_writer_receipt",
  "model::tests::windows_direct_publication_receipt_requires_revision_and_file_identity",
  "model::tests::windows_ambiguous_callback_cannot_interrupt_inflight_direct_creation",
  "model::tests::checked_open_accepts_an_approved_windows_assets_junction",
  "model::tests::projection_windows_held_handle_link_count_tracks_one_and_two_links",
  "model::tests::windows_live_graph_root_move_is_denied_without_rebinding",
]);

export function windowsCoreSmokeTestNames() {
  return [...WINDOWS_CORE_EXACT_TEST_NAMES];
}

export const WINDOWS_CORE_SMOKE_TEST_NAMES = Object.freeze(windowsCoreSmokeTestNames());

// The same declared names drive nextest and the inventory verifier. Do not
// replace this with a broad package filter: that would bring platform-neutral
// tests (including currently known Windows-incompatible ones) back into the
// release gate without an intentional policy change.
export const WINDOWS_CORE_SMOKE_FILTERSET = WINDOWS_CORE_SMOKE_TEST_NAMES
  .map((testName) => `test(=${testName})`)
  .join(" | ");

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

export function verifyLinuxReleaseSelection(
  coreInventory,
  releaseInventory,
  excludedTestNames = KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES
) {
  if (coreInventory?.packageName !== "tine-core") fail("Linux core inventory is not tine-core");
  if (releaseInventory?.packageName !== "tine-core") fail("Linux release inventory is not tine-core");

  for (const [key, test] of releaseInventory.tests) {
    if (!coreInventory.tests.has(key)) {
      fail(`Linux release selection contains non-inventory test ${test.binaryId} ${test.testName}`);
    }
  }

  const excluded = [...coreInventory.tests.entries()]
    .filter(([key]) => !releaseInventory.tests.has(key))
    .map(([, test]) => test);
  // Module membership is not oracle-ness. The claim this contract has to make
  // is that every test the release gate drops is a NAMED, deliberately dropped
  // test, so compare the actual excluded names against the exclusion contract
  // in both directions: an unlisted exclusion means a healthy test was silently
  // un-gated, and a listed name with no test behind it means the list has
  // rotted. Names, never counts.
  requireExactNameSet(
    excluded.map((test) => test.testName),
    excludedTestNames,
    "Linux release exclusion contract"
  );

  return {
    coreTestCount: coreInventory.tests.size,
    releaseTestCount: releaseInventory.tests.size,
    knownRedTestCount: excluded.length,
  };
}

function testNames(inventory) {
  return [...inventory.tests.values()].map((test) => test.testName).sort();
}

function requireUniqueName(inventory, name) {
  const matching = [...inventory.tests.values()].filter((test) => test.testName === name);
  if (matching.length !== 1) {
    fail(`${inventory.packageName} must contain exactly one required test ${name}; found ${matching.length}`);
  }
  return matching[0];
}

function requireNamesSelected(fullInventory, selectedInventory, names, label) {
  for (const name of names) {
    const expected = requireUniqueName(fullInventory, name);
    if (!selectedInventory.tests.has(testKey(expected.binaryId, expected.testName))) {
      fail(`${label} omitted required test ${name}`);
    }
  }
}

function requireExactNameSet(actualNames, expectedNames, label) {
  const actual = [...actualNames].sort();
  const expected = [...expectedNames].sort();
  if (actual.length !== expected.length || actual.some((name, index) => name !== expected[index])) {
    const missing = expected.filter((name) => !actual.includes(name));
    const unexpected = actual.filter((name) => !expected.includes(name));
    fail(
      `${label} changed; missing [${missing.join(", ") || "none"}], unexpected [${unexpected.join(", ") || "none"}]`
    );
  }
}

export function verifyWindowsCoreSmokeSelection(coreInventory, smokeInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Windows core inventory is not tine-core");
  if (smokeInventory?.packageName !== "tine-core") fail("Windows core smoke inventory is not tine-core");

  const windowsNamed = [...coreInventory.tests.values()]
    .filter((test) => test.testName.toLowerCase().includes("windows"))
    .map((test) => test.testName);
  requireExactNameSet(windowsNamed, WINDOWS_CORE_EXACT_TEST_NAMES, "Windows-named tine-core test inventory");
  const requiredSmokeNames = windowsCoreSmokeTestNames();
  requireNamesSelected(coreInventory, smokeInventory, requiredSmokeNames, "Windows core smoke selection");
  requireExactNameSet(testNames(smokeInventory), requiredSmokeNames, "Windows core smoke selection");

  return {
    coreTestCount: coreInventory.tests.size,
    coreSmokeTestCount: smokeInventory.tests.size,
    windowsNamedCount: windowsNamed.length,
  };
}

function nextestList(profile, packageName, { partition, filterset } = {}) {
  const args = ["nextest", "list", "--profile", profile, "--package", packageName, "--message-format", "json"];
  if (partition) args.push("--partition", partition);
  if (filterset) args.push("--filterset", filterset);
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

function runWindowsSmoke(packageName, filterset, label) {
  const result = spawnSync(
    "cargo",
    ["nextest", "run", "--profile", "ci-windows", "--package", packageName, "--filterset", filterset],
    { cwd: process.cwd(), stdio: "inherit" }
  );
  if (result.error) fail(`could not start ${label}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function runLinuxSelection({ shard } = {}) {
  const args = [
    "nextest",
    "run",
    "--profile",
    "ci",
    "--package",
    "tine-core",
    "--filterset",
    LINUX_CORE_RELEASE_FILTERSET,
  ];
  if (shard !== undefined) args.push("--partition", `hash:${shard}/${LINUX_TINE_CORE_SHARD_COUNT}`);
  const label = shard === undefined ? "Linux tine-core release selection" : `Linux tine-core shard ${shard}`;
  const result = spawnSync("cargo", args, { cwd: process.cwd(), stdio: "inherit" });
  if (result.error) fail(`could not start ${label}: ${result.error.message}`);
  if (result.status !== 0) process.exit(result.status ?? 1);
}

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function main() {
  const mode = option("--mode");
  if (mode === "linux") {
    const core = nextestList("ci", "tine-core");
    const full = nextestList("ci", "tine-core", { filterset: LINUX_CORE_RELEASE_FILTERSET });
    const selection = verifyLinuxReleaseSelection(core, full);
    const shards = Array.from({ length: LINUX_TINE_CORE_SHARD_COUNT }, (_, index) =>
      nextestList("ci", "tine-core", {
        partition: `hash:${index + 1}/${LINUX_TINE_CORE_SHARD_COUNT}`,
        filterset: LINUX_CORE_RELEASE_FILTERSET,
      })
    );
    const result = verifyLinuxShardCoverage(full, shards);
    console.log(
      `Linux nextest contract OK: ${result.testCount} release tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}); every current tine-core test is selected except exactly the ${selection.knownRedTestCount} named known-red tests.`
    );
    const runShard = option("--run-shard");
    if (runShard !== undefined) {
      const shard = Number(runShard);
      if (!Number.isInteger(shard) || shard < 1 || shard > LINUX_TINE_CORE_SHARD_COUNT) {
        fail(`--run-shard must be an integer from 1 to ${LINUX_TINE_CORE_SHARD_COUNT}`);
      }
      runLinuxSelection({ shard });
    }
    // The PR gate runs the whole verified selection in one job instead of four
    // sharded ones; both paths execute the identical filterset and `ci` profile.
    if (process.argv.includes("--run-selection")) runLinuxSelection();
    return;
  }
  if (mode === "windows") {
    const core = nextestList("ci-windows", "tine-core");
    const smoke = nextestList("ci-windows", "tine-core", { filterset: WINDOWS_CORE_SMOKE_FILTERSET });
    const result = verifyWindowsCoreSmokeSelection(core, smoke);
    console.log(
      `Windows nextest contract OK: ${result.coreTestCount} compiled tine-core tests, ${result.coreSmokeTestCount} contract-selected smokes, ${result.windowsNamedCount} Windows-named core tests.`
    );
    if (process.argv.includes("--run-smoke")) {
      runWindowsSmoke("tine-core", WINDOWS_CORE_SMOKE_FILTERSET, "Windows core integration smoke");
    }
    return;
  }
  fail(
    "pass --mode linux (add --run-shard N for one release shard, or --run-selection for the whole verified release selection) or --mode windows (add --run-smoke to execute the verified Windows core integration selection)"
  );
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
