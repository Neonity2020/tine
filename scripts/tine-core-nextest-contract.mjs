#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

// Windows is deliberately not a second complete tine-core behavior matrix.
// Linux carries that full inventory in four isolated shards. This exact list is
// the Windows release contract: every explicitly Windows-named core test, plus
// the bootstrap/durability/lifecycle witnesses that exercised the Windows
// failures fixed for v0.6.90. Keep the names explicit so a rename, removal, or
// newly added Windows test cannot silently shrink the release gate.
export const WINDOWS_CORE_EXACT_TEST_NAMES = Object.freeze([
  "fail_before_projection_crash_windows_recover_without_unauthorized_execution",
  "model::tests::page_name_encoding_is_injective_reversible_and_windows_safe",
  "model::tests::windows_handle_relative_noreplace_renames_the_exact_source",
  "model::tests::windows_handle_relative_noreplace_moves_between_nonstandard_retained_directories_with_unicode",
  "model::tests::windows_handle_relative_noreplace_preserves_occupied_destination",
  "model::tests::windows_first_save_and_ordinary_rename_preserve_exact_projection",
  "model::tests::windows_directory_durability_limit_does_not_block_save_or_rename",
  "model::tests::checked_open_accepts_an_approved_windows_assets_junction",
  "model::tests::projection_windows_held_handle_link_count_tracks_one_and_two_links",
  "model::tests::windows_live_graph_root_move_is_denied_without_rebinding",
  "oplog::sqlite::tests::windows_entry_file_identity_classifies_reparse_lease_as_replaced",
  "windows_no_follow_publication_read_and_directory_flush_succeed",
  "windows_reparse_files_and_directories_are_rejected",
]);

export const WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES = Object.freeze([
  "oplog::local_active::tests::a_live_promoted_runtime_blocks_every_newcomer_before_any_durable_write",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "oplog::local_active::tests::authoritative_snapshot_prunes_lock_namespaces_before_reading_files",
  "oplog::enrollment::tests::a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "oplog::sqlite::tests::separate_process_workspace_lease_contends_and_crash_releases",
]);

export const WINDOWS_CORE_CAPTURE_WITNESS_NAMES = Object.freeze([
  "model::tests::inactive_bootstrap_capture_exact_64_mib_sparse_file_is_accepted",
  "model::tests::inactive_bootstrap_capture_external_sort_is_buffer_bounded_without_real_files",
  "model::tests::inactive_bootstrap_capture_ignores_residue_is_idempotent_and_rejects_conflicting_seal",
  "model::tests::inactive_bootstrap_capture_is_deterministic_and_chunks_zero_one_and_many_files",
  "model::tests::inactive_bootstrap_capture_preserves_exact_nested_unicode_org_and_semantic_kinds",
  "model::tests::inactive_bootstrap_capture_rejects_bad_logical_name_frames",
  "model::tests::inactive_bootstrap_capture_rejects_between_pass_and_before_final_proof_mutations",
  "model::tests::inactive_bootstrap_capture_rejects_file_cap_before_streaming",
]);

// Linux is the complete tine-storage semantic suite. Windows compiles every
// storage target, then runs this bounded smoke: the durability, restart, and
// nonblocking-lock contracts whose platform behavior is material to Tine.
// Keep this exact and inventory-backed; a broad package filter would silently
// promote known Windows-incompatible legacy tests into the Tine release gate.
export const WINDOWS_STORAGE_SMOKE_TEST_NAMES = Object.freeze([
  "filesystem::tests::nonblocking_lock_contention_classifier_is_narrow_and_platform_explicit",
  "filesystem::tests::validated_real_directory_has_explicit_windows_durability_limit",
  "filesystem::tests::windows_directory_validation_rejects_reparse_and_non_directory_handles",
  "local_journal::tests::one_append_performs_exactly_one_durability_barrier",
  "local_journal::tests::a_completed_append_survives_a_restart",
  "local_journal::tests::a_duplicate_open_is_refused_while_the_first_is_live",
]);

// Every explicitly Windows-named tine-storage test must be deliberately
// classified. These two are executed by the storage smoke; the packed-Patricia
// crash-window test below is a known incompatible legacy runtime test and is
// deferred to tine-storage's own Windows certification. It is not masked,
// retried, or claimed to have passed here.
export const WINDOWS_STORAGE_SELECTED_WINDOWS_NAMED_TEST_NAMES = Object.freeze([
  "filesystem::tests::validated_real_directory_has_explicit_windows_durability_limit",
  "filesystem::tests::windows_directory_validation_rejects_reparse_and_non_directory_handles",
]);

export const WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES = Object.freeze([
  "packed_patricia::tests::pack_catalog_and_head_crash_windows_are_invisible_or_retry_exactly",
]);

const WINDOWS_CORE_SMOKE_TEST_NAMES = Object.freeze([
  ...new Set([
    ...WINDOWS_CORE_EXACT_TEST_NAMES,
    ...WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES,
    ...WINDOWS_CORE_CAPTURE_WITNESS_NAMES,
  ]),
]);

// The same declared names drive nextest and the inventory verifier. Do not
// replace this with a broad package filter: that would bring platform-neutral
// tests (including currently known Windows-incompatible ones) back into the
// release gate without an intentional policy change.
export const WINDOWS_CORE_SMOKE_FILTERSET = WINDOWS_CORE_SMOKE_TEST_NAMES
  .map((testName) => `test(=${testName})`)
  .join(" | ");

export const WINDOWS_STORAGE_SMOKE_FILTERSET = WINDOWS_STORAGE_SMOKE_TEST_NAMES
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

// Linux runs the whole tine-storage suite in one job — no partitioning, so the
// coverage question is different from tine-core's. What has to hold is that the
// job is not silently narrowed, and that the tests Windows explicitly defers are
// actually executed *somewhere*. Without this, `WINDOWS_STORAGE_DEFERRED_...`
// would read as "covered on Linux" while being covered nowhere.
export function verifyLinuxStorageCoverage(inventory, deferredWindowsNames) {
  if (inventory?.packageName !== "tine-storage") fail("Linux storage inventory is not tine-storage");
  if (inventory.tests.size === 0) fail("Linux storage inventory selected no tests");

  for (const name of deferredWindowsNames) {
    // requireUniqueName also proves the name still exists: a deferred test that
    // was renamed or deleted must not keep its deferral silently.
    requireUniqueName(inventory, name);
  }

  return {
    testCount: inventory.tests.size,
    deferredWindowsCount: deferredWindowsNames.length,
  };
}

export function verifyWindowsCoreSmokeSelection(coreInventory, smokeInventory, storageInventory, storageSmokeInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Windows core inventory is not tine-core");
  if (smokeInventory?.packageName !== "tine-core") fail("Windows core smoke inventory is not tine-core");
  if (storageInventory?.packageName !== "tine-storage") fail("Windows storage inventory is not tine-storage");
  if (storageSmokeInventory?.packageName !== "tine-storage") {
    fail("Windows storage smoke inventory is not tine-storage");
  }

  const windowsNamed = [...coreInventory.tests.values()]
    .filter((test) => test.testName.toLowerCase().includes("windows"))
    .map((test) => test.testName);
  requireExactNameSet(windowsNamed, WINDOWS_CORE_EXACT_TEST_NAMES, "Windows-named tine-core test inventory");
  requireNamesSelected(coreInventory, smokeInventory, WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");
  requireExactNameSet(testNames(smokeInventory), WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");
  const windowsNamedStorage = [...storageInventory.tests.values()]
    .filter((test) => test.testName.toLowerCase().includes("windows"))
    .map((test) => test.testName);
  requireExactNameSet(
    WINDOWS_STORAGE_SELECTED_WINDOWS_NAMED_TEST_NAMES,
    WINDOWS_STORAGE_SMOKE_TEST_NAMES.filter((testName) => testName.toLowerCase().includes("windows")),
    "Selected Windows-named tine-storage test declaration"
  );
  const classifiedWindowsStorage = [
    ...WINDOWS_STORAGE_SELECTED_WINDOWS_NAMED_TEST_NAMES,
    ...WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES,
  ];
  requireExactNameSet(
    windowsNamedStorage,
    classifiedWindowsStorage,
    "Windows-named tine-storage test inventory"
  );
  requireNamesSelected(
    storageInventory,
    storageSmokeInventory,
    WINDOWS_STORAGE_SMOKE_TEST_NAMES,
    "Windows storage smoke selection"
  );
  requireExactNameSet(
    testNames(storageSmokeInventory),
    WINDOWS_STORAGE_SMOKE_TEST_NAMES,
    "Windows storage smoke selection"
  );

  return {
    coreTestCount: coreInventory.tests.size,
    coreSmokeTestCount: smokeInventory.tests.size,
    storageTestCount: storageInventory.tests.size,
    storageSmokeTestCount: storageSmokeInventory.tests.size,
    windowsNamedCount: windowsNamed.length,
    windowsNamedStorageCount: windowsNamedStorage.length,
    deferredWindowsStorageCount: WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES.length,
    bootstrapWitnessCount: WINDOWS_CORE_CAPTURE_WITNESS_NAMES.length,
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

function option(name) {
  const index = process.argv.indexOf(name);
  return index >= 0 ? process.argv[index + 1] : undefined;
}

function main() {
  const mode = option("--mode");
  if (mode === "linux") {
    const full = nextestList("ci", "tine-core");
    const shards = Array.from({ length: LINUX_TINE_CORE_SHARD_COUNT }, (_, index) =>
      nextestList("ci", "tine-core", { partition: `hash:${index + 1}/${LINUX_TINE_CORE_SHARD_COUNT}` })
    );
    const result = verifyLinuxShardCoverage(full, shards);
    const storage = nextestList("ci", "tine-storage");
    const storageResult = verifyLinuxStorageCoverage(storage, WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES);
    console.log(
      `Linux nextest contract OK: ${result.testCount} tine-core tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}), and ${storageResult.testCount} tine-storage tests in one unpartitioned run including ${storageResult.deferredWindowsCount} Windows-deferred test(s).`
    );
    return;
  }
  if (mode === "windows") {
    const core = nextestList("ci-windows", "tine-core");
    const smoke = nextestList("ci-windows", "tine-core", { filterset: WINDOWS_CORE_SMOKE_FILTERSET });
    const storage = nextestList("ci-windows", "tine-storage");
    const storageSmoke = nextestList("ci-windows", "tine-storage", { filterset: WINDOWS_STORAGE_SMOKE_FILTERSET });
    const result = verifyWindowsCoreSmokeSelection(core, smoke, storage, storageSmoke);
    console.log(
      `Windows nextest contract OK: ${result.coreTestCount} compiled tine-core tests, ${result.coreSmokeTestCount} contract-selected core smoke tests, ${result.storageTestCount} tine-storage inventory tests, ${result.storageSmokeTestCount} contract-selected storage smoke tests, ${result.windowsNamedCount} Windows-named core tests, ${result.windowsNamedStorageCount} classified Windows-named storage tests (${result.deferredWindowsStorageCount} explicitly deferred), and ${result.bootstrapWitnessCount} bootstrap capture witnesses.`
    );
    if (process.argv.includes("--run-smoke")) {
      runWindowsSmoke("tine-core", WINDOWS_CORE_SMOKE_FILTERSET, "Windows core smoke");
      runWindowsSmoke("tine-storage", WINDOWS_STORAGE_SMOKE_FILTERSET, "Windows storage smoke");
    }
    return;
  }
  fail("pass --mode linux or --mode windows (add --run-smoke to execute the verified Windows core and storage selections)");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
