#!/usr/bin/env node

import assert from "node:assert/strict";
import {
  LINUX_TINE_CORE_SHARD_COUNT,
  inventoryFromNextestList,
  verifyLinuxShardCoverage,
  verifyWindowsFullSelection,
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

const coreWitnesses = [
  "oplog::local_active::tests::a_live_promoted_runtime_blocks_every_newcomer_before_any_durable_write",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
  "oplog::local_active::tests::authoritative_snapshot_prunes_lock_namespaces_before_reading_files",
  "oplog::enrollment::tests::a_second_live_session_cannot_write_the_journal_and_dropping_one_releases_it",
  "oplog::sqlite::tests::separate_process_workspace_lease_contends_and_crash_releases",
  "model::tests::inactive_bootstrap_capture_preserves_exact_nested_unicode_org_and_semantic_kinds",
  "model::tests::windows_live_graph_root_move_is_denied_without_rebinding",
];
const storageWitnesses = ["filesystem::tests::nonblocking_lock_contention_classifier_is_narrow_and_platform_explicit"];
assert.deepEqual(
  verifyWindowsFullSelection(
    listedInventory("tine-core", coreWitnesses),
    listedInventory("tine-storage", storageWitnesses)
  ),
  { coreTestCount: 10, storageTestCount: 1, windowsNamedCount: 1, bootstrapWitnessCount: 1 }
);
assert.throws(
  () => verifyWindowsFullSelection(
    listedInventory("tine-core", coreWitnesses.filter((name) => !name.includes("windows_live_graph"))),
    listedInventory("tine-storage", storageWitnesses)
  ),
  /contains no explicitly Windows-named tests/
);
assert.equal(LINUX_TINE_CORE_SHARD_COUNT, 4);

console.log("tine-core nextest contract fixture tests passed.");
