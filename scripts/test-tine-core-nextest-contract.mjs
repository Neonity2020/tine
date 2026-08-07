#!/usr/bin/env node

import assert from "node:assert/strict";
import {
  LINUX_TINE_CORE_SHARD_COUNT,
  WINDOWS_CORE_CAPTURE_WITNESS_NAMES,
  WINDOWS_CORE_EXACT_TEST_NAMES,
  WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES,
  WINDOWS_CORE_SMOKE_FILTERSET,
  WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES,
  WINDOWS_STORAGE_SMOKE_FILTERSET,
  WINDOWS_STORAGE_SMOKE_TEST_NAMES,
  WINDOWS_STORAGE_SELECTED_WINDOWS_NAMED_TEST_NAMES,
  inventoryFromNextestList,
  verifyLinuxShardCoverage,
  verifyLinuxStorageCoverage,
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
assert.throws(
  () => verifyLinuxShardCoverage(fullCore, [shards[0], shards[1], shards[2], shards[2]]),
  /both selected tine-core gamma/
);

// The Linux storage run is unpartitioned, so its contract is not coverage-by-
// shard but "nothing narrowed it, and the Windows-deferred tests are actually
// executed here". A deferred test that has been renamed away must fail rather
// than keep a deferral that now covers nothing.
const fullStorage = listedInventory("tine-storage", ["alpha", "deferred_on_windows"]);
assert.deepEqual(
  verifyLinuxStorageCoverage(fullStorage, ["deferred_on_windows"]),
  { testCount: 2, deferredWindowsCount: 1 }
);
assert.throws(
  () => verifyLinuxStorageCoverage(fullStorage, ["renamed_away"]),
  /exactly one required test renamed_away/
);
assert.throws(
  () => verifyLinuxStorageCoverage(fullCore, []),
  /Linux storage inventory is not tine-storage/
);

const coreWindowsTests = [
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
];
assert.deepEqual(WINDOWS_CORE_EXACT_TEST_NAMES, coreWindowsTests);

const coreLifecycleWitnesses = [
  "oplog::local_active::tests::a_live_promoted_runtime_blocks_every_newcomer_before_any_durable_write",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "oplog::local_active::tests::authoritative_snapshot_prunes_lock_namespaces_before_reading_files",
  "oplog::enrollment::tests::a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "oplog::sqlite::tests::separate_process_workspace_lease_contends_and_crash_releases",
];
assert.deepEqual(WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES, coreLifecycleWitnesses);

const coreCaptureWitnesses = [
  "model::tests::inactive_bootstrap_capture_exact_64_mib_sparse_file_is_accepted",
  "model::tests::inactive_bootstrap_capture_external_sort_has_fixed_rows_without_real_files",
  "model::tests::inactive_bootstrap_capture_ignores_residue_is_idempotent_and_rejects_conflicting_seal",
  "model::tests::inactive_bootstrap_capture_is_deterministic_and_chunks_zero_one_and_many_files",
  "model::tests::inactive_bootstrap_capture_preserves_exact_nested_unicode_org_and_semantic_kinds",
  "model::tests::inactive_bootstrap_capture_rejects_bad_logical_name_frames",
  "model::tests::inactive_bootstrap_capture_rejects_between_pass_and_before_final_proof_mutations",
  "model::tests::inactive_bootstrap_capture_rejects_file_cap_before_streaming",
];
assert.deepEqual(WINDOWS_CORE_CAPTURE_WITNESS_NAMES, coreCaptureWitnesses);

const storageSmokeTests = [
  "filesystem::tests::nonblocking_lock_contention_classifier_is_narrow_and_platform_explicit",
  "filesystem::tests::validated_real_directory_has_explicit_windows_durability_limit",
  "filesystem::tests::windows_directory_validation_rejects_reparse_and_non_directory_handles",
  "local_journal::tests::one_append_performs_exactly_one_durability_barrier",
  "local_journal::tests::a_completed_append_survives_a_restart",
  "local_journal::tests::a_duplicate_open_is_refused_while_the_first_is_live",
];
const deferredWindowsStorageTests = [
  "packed_patricia::tests::pack_catalog_and_head_crash_windows_are_invisible_or_retry_exactly",
];
assert.deepEqual(WINDOWS_STORAGE_SMOKE_TEST_NAMES, storageSmokeTests);
assert.deepEqual(WINDOWS_STORAGE_SELECTED_WINDOWS_NAMED_TEST_NAMES, [
  "filesystem::tests::validated_real_directory_has_explicit_windows_durability_limit",
  "filesystem::tests::windows_directory_validation_rejects_reparse_and_non_directory_handles",
]);
assert.deepEqual(WINDOWS_STORAGE_DEFERRED_WINDOWS_NAMED_TEST_NAMES, deferredWindowsStorageTests);
const coreSmokeTests = [...new Set([...coreWindowsTests, ...coreLifecycleWitnesses, ...coreCaptureWitnesses])];
assert.deepEqual(
  verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", [...coreSmokeTests, "unselected_platform_neutral_test"]),
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-storage", [...storageSmokeTests, ...deferredWindowsStorageTests]),
    listedInventory("tine-storage", storageSmokeTests)
  ),
  {
    coreTestCount: 30,
    coreSmokeTestCount: 29,
    storageTestCount: 7,
    storageSmokeTestCount: 6,
    windowsNamedCount: 13,
    windowsNamedStorageCount: 3,
    deferredWindowsStorageCount: 1,
    bootstrapWitnessCount: 8,
  }
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("windows_live_graph"))),
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("windows_live_graph"))),
    listedInventory("tine-storage", [...storageSmokeTests, ...deferredWindowsStorageTests]),
    listedInventory("tine-storage", storageSmokeTests)
  ),
  /Windows-named tine-core test inventory changed/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-core", coreSmokeTests.filter((name) => !name.includes("inactive_bootstrap_capture_rejects_file_cap"))),
    listedInventory("tine-storage", [...storageSmokeTests, ...deferredWindowsStorageTests]),
    listedInventory("tine-storage", storageSmokeTests)
  ),
  /Windows core smoke selection omitted required test/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-storage", [...storageSmokeTests, ...deferredWindowsStorageTests, "new_windows_storage_test"]),
    listedInventory("tine-storage", storageSmokeTests)
  ),
  /Windows-named tine-storage test inventory changed/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-storage", [...storageSmokeTests, ...deferredWindowsStorageTests]),
    listedInventory("tine-storage", storageSmokeTests.slice(0, -1))
  ),
  /Windows storage smoke selection omitted required test/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-core", coreSmokeTests),
    listedInventory("tine-storage", storageSmokeTests),
    listedInventory("tine-storage", storageSmokeTests)
  ),
  /Windows-named tine-storage test inventory changed/
);
assert.match(WINDOWS_CORE_SMOKE_FILTERSET, /test\(=model::tests::windows_live_graph_root_move_is_denied_without_rebinding\)/);
assert.doesNotMatch(WINDOWS_CORE_SMOKE_FILTERSET, /all\(\)|fast_commit/);
assert.match(WINDOWS_STORAGE_SMOKE_FILTERSET, /test\(=local_journal::tests::a_completed_append_survives_a_restart\)/);
assert.doesNotMatch(WINDOWS_STORAGE_SMOKE_FILTERSET, /all\(\)|fast_commit/);
assert.equal(LINUX_TINE_CORE_SHARD_COUNT, 4);

console.log("tine-core nextest contract fixture tests passed.");
