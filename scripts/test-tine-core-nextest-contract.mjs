#!/usr/bin/env node

import assert from "node:assert/strict";
import {
  LINUX_CORE_RELEASE_FILTERSET,
  LINUX_TINE_CORE_SHARD_COUNT,
  KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES,
  KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES,
  WINDOWS_CORE_CAPTURE_WITNESS_NAMES,
  WINDOWS_CORE_EXACT_TEST_NAMES,
  WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES,
  WINDOWS_CORE_SMOKE_FILTERSET,
  inventoryFromNextestList,
  verifyLinuxReleaseSelection,
  verifyLinuxShardCoverage,
  verifyWindowsCoreSmokeSelection,
} from "./tine-core-nextest-contract.mjs";

function listedInventory(packageName, testNames) {
  return inventoryFromNextestList(packageName, {
    "rust-suites": {
      [packageName]: {
        "package-name": packageName,
        testcases: Object.fromEntries(testNames.map((testName) => [testName, {
          ignored: false,
          "filter-match": { status: "matches" },
        }])),
      },
    },
  });
}

const shardNames = ["alpha", "beta", "gamma", "delta"];
const fullCore = listedInventory("tine-core", shardNames);
const shards = shardNames.map((name) => listedInventory("tine-core", [name]));
assert.deepEqual(
  verifyLinuxShardCoverage(fullCore, shards),
  { testCount: 4, shardCounts: [1, 1, 1, 1] }
);
assert.throws(
  () => verifyLinuxShardCoverage(fullCore, [shards[0], shards[1], shards[2], listedInventory("tine-core", [])]),
  /selected no non-ignored tests/
);

const releaseSelectedNames = [
  "model::tests::ordinary_semantic_contract",
  "sync_runtime::tests::current_clean_runtime_contract",
  "sync_runtime::tests::new_production_test_is_selected_automatically",
];
const coreWithKnownRedOracle = listedInventory("tine-core", [
  ...releaseSelectedNames,
  ...KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES,
]);
const releaseWithoutKnownRedOracle = listedInventory("tine-core", releaseSelectedNames);
assert.deepEqual(
  verifyLinuxReleaseSelection(coreWithKnownRedOracle, releaseWithoutKnownRedOracle),
  {
    coreTestCount: releaseSelectedNames.length + KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES.length,
    releaseTestCount: releaseSelectedNames.length,
    knownRedTestCount: KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES.length,
  }
);
assert.throws(
  () => verifyLinuxReleaseSelection(
    coreWithKnownRedOracle,
    listedInventory("tine-core", releaseSelectedNames.slice(0, 2))
  ),
  /Linux release exclusion contract changed.*new_production_test_is_selected_automatically/
);

// And a listed exclusion whose test no longer exists must fail too, so the list
// cannot rot through renames or deletions.
const staleOracleName = KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES[0];
const coreWithoutOneOracleTest = listedInventory("tine-core", [
  ...releaseSelectedNames,
  ...KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES.filter((name) => name !== staleOracleName),
]);
assert.throws(
  () => verifyLinuxReleaseSelection(coreWithoutOneOracleTest, releaseWithoutKnownRedOracle),
  new RegExp(`Linux release exclusion contract changed; missing \\[${staleOracleName}\\]`)
);

// The allow-by-default filter must not permit a non-oracle omission, whether
// the omitted test is in sync_runtime or another module.
assert.throws(
  () => verifyLinuxReleaseSelection(
    coreWithKnownRedOracle,
    listedInventory("tine-core", releaseSelectedNames.filter((name) => !name.startsWith("model::tests::")))
  ),
  /excluded non-oracle test/
);
const familyNames = Object.values(KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES).flat();
assert.deepEqual([...familyNames].sort(), KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES);
assert.deepEqual(Object.keys(KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES), [
  "activationAndEnrollment",
  "applicationAndSemanticConvergence",
  "providerRecoveryAndPublication",
  "boundedDiscoveryAndTraversal",
]);
for (const names of Object.values(KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES)) {
  assert.ok(names.length > 0);
}
for (const name of KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES) {
  assert.match(name, /^sync_runtime::tests::/);
}
assert.equal(
  new Set(KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES).size,
  KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES.length
);

assert.match(
  LINUX_CORE_RELEASE_FILTERSET,
  /not \(test\(=sync_runtime::tests::/
);
assert.doesNotMatch(LINUX_CORE_RELEASE_FILTERSET, /not test\(\/sync_runtime::tests::\/\)/);
assert.throws(
  () => verifyLinuxShardCoverage(fullCore, [shards[0], shards[1], shards[2], shards[2]]),
  /both selected tine-core gamma/
);

const coreWindowsTests = [
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
  "oplog::sqlite::tests::windows_entry_file_identity_classifies_reparse_lease_as_replaced",
  "windows_no_follow_publication_read_and_directory_flush_succeed",
  "windows_reparse_files_and_directories_are_rejected",
];
assert.deepEqual(WINDOWS_CORE_EXACT_TEST_NAMES, coreWindowsTests);

const coreLifecycleWitnesses = [
  "oplog::local_active::bounded_admission::clean_admissions_are_bounded_at_one_one_thousand_and_ten_thousand",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "oplog::enrollment::tests::a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "oplog::sqlite::tests::separate_process_workspace_lease_contends_and_crash_releases",
];
assert.deepEqual(WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES, coreLifecycleWitnesses);

const coreCaptureWitnesses = [
  "model::tests::inactive_bootstrap_capture_exact_64_mib_sparse_file_is_accepted",
  "model::tests::inactive_bootstrap_capture_external_sort_is_buffer_bounded_without_real_files",
  "model::tests::inactive_bootstrap_capture_ignores_residue_is_idempotent_and_rejects_conflicting_seal",
  "model::tests::inactive_bootstrap_capture_is_deterministic_and_chunks_zero_one_and_many_files",
  "model::tests::inactive_bootstrap_capture_preserves_exact_nested_unicode_org_and_semantic_kinds",
  "model::tests::inactive_bootstrap_capture_rejects_bad_logical_name_frames",
  "model::tests::inactive_bootstrap_capture_seals_one_pass_and_final_proof_rejects_later_mutations",
  "model::tests::inactive_bootstrap_capture_rejects_file_cap_before_streaming",
];
assert.deepEqual(WINDOWS_CORE_CAPTURE_WITNESS_NAMES, coreCaptureWitnesses);

const coreSmokeTests = [...new Set([...coreWindowsTests, ...coreLifecycleWitnesses, ...coreCaptureWitnesses])];
assert.deepEqual(
  verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", [...coreSmokeTests, "unselected_platform_neutral_test"]),
    listedInventory("tine-core", coreSmokeTests)
  ),
  {
    coreTestCount: 31,
    coreSmokeTestCount: 30,
    windowsNamedCount: 15,
    bootstrapWitnessCount: 8,
  }
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("windows_live_graph"))),
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("windows_live_graph")))
  ),
  /Windows-named tine-core test inventory changed/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("inactive_bootstrap_capture_rejects_file_cap")))
  ),
  /Windows core smoke selection omitted required test/
);
assert.match(WINDOWS_CORE_SMOKE_FILTERSET, /test\(=model::tests::windows_live_graph_root_move_is_denied_without_rebinding\)/);
assert.match(WINDOWS_CORE_SMOKE_FILTERSET, /test\(=model::tests::windows_direct_publication_event_waits_for_inflight_writer_receipt\)/);
assert.doesNotMatch(WINDOWS_CORE_SMOKE_FILTERSET, /all\(\)|fast_commit/);
assert.equal(LINUX_TINE_CORE_SHARD_COUNT, 4);

console.log("tine-core nextest contract fixture tests passed.");
