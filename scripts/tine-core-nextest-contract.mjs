#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

// `sync_runtime::tests` still contains the pre-0.7 adversarial actor as a
// differential oracle. Production no longer opens that actor, so its hundreds
// of implementation-shaped scenarios are not a release contract for the clean
// baseline-plus-manifest runtime. Keep the current production journeys exact
// and explicit here. Any new sync-runtime release test must be deliberately
// added; all other tine-core modules remain selected in full.
export const CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES = Object.freeze([
  "sync_runtime::tests::activation_rediscovery_does_not_erase_the_physical_failure",
  "sync_runtime::tests::pre_promotion_receipt_retry_preserves_one_diagnostic_tree",
  "sync_runtime::tests::storage_contract_limits_receipt_rebuild_to_pre_enrollment_android_state",
  "sync_runtime::tests::pre_07_activation_oracle_has_no_production_entry_point",
  "sync_runtime::tests::clean_reopen_reads_an_unchanged_page_before_deferred_full_scan_catch_up",
  "sync_runtime::tests::clean_reopen_refuses_disk_divergence_without_hiding_it_behind_sqlite",
  "sync_runtime::tests::clean_reopen_negative_lookup_settles_closed_interval_addition_and_rename",
  "sync_runtime::tests::clean_runtime_factories_adopt_marker_and_cold_reopen_without_legacy_enrollment",
  "sync_runtime::tests::clean_actor_core_retains_one_manifested_save_until_projection_finishes",
  "sync_runtime::tests::clean_runtime_actor_assembles_without_legacy_authority_and_saves_one_edit",
  "sync_runtime::tests::clean_runtime_handle_serves_sqlite_queries_and_stops_without_legacy_handoff",
  "sync_runtime::tests::clean_runtime_cross_page_move_commits_once_and_cold_reopens",
  "sync_runtime::tests::clean_runtime_serves_regime_neutral_graph_pdf_and_guide_journeys",
  "sync_runtime::tests::public_cold_open_prefers_clean_marker_without_discovering_legacy_enrollment",
  "sync_runtime::tests::public_fresh_activation_commits_clean_marker_and_skips_legacy_promotion",
  "sync_runtime::tests::public_clean_activation_loads_saves_and_cold_reopens_an_editor_page",
  "sync_runtime::tests::public_clean_activation_loads_and_saves_the_application_page_contract",
  "sync_runtime::tests::public_clean_runtime_reconciles_an_exact_external_edit_and_reopens_it",
  "sync_runtime::tests::public_clean_cold_open_discovers_an_external_edit_while_tine_was_closed",
  "sync_runtime::tests::public_clean_runtime_reconciles_external_create_delete_and_rename_as_one_batch",
  "sync_runtime::tests::shared_provider_clean_late_join_installs_provider_history_without_rewriting_graph",
  "sync_runtime::tests::shared_provider_clean_late_join_refuses_unmatched_local_graph_without_changing_authority",
  "sync_runtime::tests::shared_provider_clean_two_device_unicode_join_and_restart",
]);

export const LINUX_CORE_RELEASE_FILTERSET = [
  "not test(/sync_runtime::tests::/)",
  ...CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES.map((testName) => `test(=${testName})`),
].join(" | ");

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
  "model::tests::inactive_bootstrap_capture_seals_one_pass_and_final_proof_rejects_later_mutations",
  "model::tests::inactive_bootstrap_capture_rejects_file_cap_before_streaming",
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

export function verifyLinuxReleaseSelection(coreInventory, releaseInventory) {
  if (coreInventory?.packageName !== "tine-core") fail("Linux core inventory is not tine-core");
  if (releaseInventory?.packageName !== "tine-core") fail("Linux release inventory is not tine-core");

  requireNamesSelected(
    coreInventory,
    releaseInventory,
    CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES,
    "Linux clean-runtime release selection"
  );
  for (const [key, test] of releaseInventory.tests) {
    if (!coreInventory.tests.has(key)) {
      fail(`Linux release selection contains non-inventory test ${test.binaryId} ${test.testName}`);
    }
  }

  const selectedSyncRuntime = [...releaseInventory.tests.values()]
    .filter((test) => test.testName.startsWith("sync_runtime::tests::"))
    .map((test) => test.testName);
  requireExactNameSet(
    selectedSyncRuntime,
    CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES,
    "Linux clean-runtime release selection"
  );

  const excluded = [...coreInventory.tests.entries()]
    .filter(([key]) => !releaseInventory.tests.has(key))
    .map(([, test]) => test);
  if (excluded.length === 0) fail("Linux release selection did not isolate the legacy runtime oracle");
  const unexpected = excluded.find((test) => !test.testName.startsWith("sync_runtime::tests::"));
  if (unexpected) {
    fail(`Linux release selection excluded non-oracle test ${unexpected.binaryId} ${unexpected.testName}`);
  }
  return {
    coreTestCount: coreInventory.tests.size,
    releaseTestCount: releaseInventory.tests.size,
    legacyOracleTestCount: excluded.length,
    cleanRuntimeTestCount: CLEAN_SYNC_RUNTIME_RELEASE_TEST_NAMES.length,
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
  requireNamesSelected(coreInventory, smokeInventory, WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");
  requireExactNameSet(testNames(smokeInventory), WINDOWS_CORE_SMOKE_TEST_NAMES, "Windows core smoke selection");

  return {
    coreTestCount: coreInventory.tests.size,
    coreSmokeTestCount: smokeInventory.tests.size,
    windowsNamedCount: windowsNamed.length,
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

function runLinuxShard(shard) {
  const result = spawnSync(
    "cargo",
    [
      "nextest",
      "run",
      "--profile",
      "ci",
      "--package",
      "tine-core",
      "--filterset",
      LINUX_CORE_RELEASE_FILTERSET,
      "--partition",
      `hash:${shard}/${LINUX_TINE_CORE_SHARD_COUNT}`,
    ],
    { cwd: process.cwd(), stdio: "inherit" }
  );
  if (result.error) fail(`could not start Linux tine-core shard ${shard}: ${result.error.message}`);
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
      `Linux nextest contract OK: ${result.testCount} release tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}); ${selection.cleanRuntimeTestCount} clean runtime journeys selected and ${selection.legacyOracleTestCount} pre-0.7 oracle tests isolated from the release gate.`
    );
    const runShard = option("--run-shard");
    if (runShard !== undefined) {
      const shard = Number(runShard);
      if (!Number.isInteger(shard) || shard < 1 || shard > LINUX_TINE_CORE_SHARD_COUNT) {
        fail(`--run-shard must be an integer from 1 to ${LINUX_TINE_CORE_SHARD_COUNT}`);
      }
      runLinuxShard(shard);
    }
    return;
  }
  if (mode === "windows") {
    const core = nextestList("ci-windows", "tine-core");
    const smoke = nextestList("ci-windows", "tine-core", { filterset: WINDOWS_CORE_SMOKE_FILTERSET });
    const result = verifyWindowsCoreSmokeSelection(core, smoke);
    console.log(
      `Windows nextest contract OK: ${result.coreTestCount} compiled tine-core tests, ${result.coreSmokeTestCount} contract-selected cross-layer smokes, ${result.windowsNamedCount} Windows-named core tests, and ${result.bootstrapWitnessCount} bootstrap capture witnesses.`
    );
    if (process.argv.includes("--run-smoke")) {
      runWindowsSmoke("tine-core", WINDOWS_CORE_SMOKE_FILTERSET, "Windows core/storage integration smoke");
    }
    return;
  }
  fail("pass --mode linux or --mode windows (add --run-smoke to execute the verified Windows core/storage integration selection)");
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
