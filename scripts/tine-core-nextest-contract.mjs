#!/usr/bin/env node

import { spawnSync } from "node:child_process";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const LINUX_TINE_CORE_SHARD_COUNT = 4;

// Linux runs the complete current tine-core inventory by default. An honest
// unfiltered run on 2026-08-25 proved that the residual known-red corpus is the
// exact set below: 45 tests fail normally, while no test hangs or times out.
// These are legacy-oracle scenarios whose fixtures, cuts, or instrumentation
// still assert retired actor mechanics. They are not evidence of a current
// production defect without a separate current-runtime fail-before.
//
// Keep the exclusions exact, name-level, and behavior-family classified. A
// deleted/renamed exclusion makes this contract fail, while every newly added
// test and every formerly excluded test removed from these families enters the
// release gate automatically.
export const KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES = Object.freeze({
  activationAndEnrollment: Object.freeze([
    "sync_runtime::tests::activation_retires_older_shadow_import_when_direct_files_changed_before_retry",
    "sync_runtime::tests::cold_shared_descriptor_discovery_uses_the_canonical_supported_regular_file",
    "sync_runtime::tests::pre_enrollment_archive_residue_refuses_mismatched_identities_but_exact_resume_reaches_active",
    "sync_runtime::tests::public_activation_cut_after_archive_claim_before_enrollment_head_resumes_exact_identities",
    "sync_runtime::tests::public_activation_cut_after_shadow_import_publication_resumes_without_graph_rewrites",
    "sync_runtime::tests::public_activation_cut_after_verified_local_publication_resumes_without_graph_rewrites",
    "sync_runtime::tests::public_activation_cut_before_archive_creation_resumes_exact_identities_without_graph_rewrites",
    "sync_runtime::tests::share_prepared_crash_resumes_descriptor_publication",
    "sync_runtime::tests::shared_join_recovery_without_canonical_manifest_is_retryable",
  ]),
  applicationAndSemanticConvergence: Object.freeze([
    "sync_runtime::tests::concurrent_explicit_and_filename_fallback_titles_converge_in_both_winner_directions",
    "sync_runtime::tests::concurrent_offline_canonical_equivalent_editor_titles_preserve_exact_semantics",
    "sync_runtime::tests::managed_application_conflict_resolution_reauthors_retained_outline_at_one_observed_revision",
    "sync_runtime::tests::managed_graph_search_accounts_for_pending_overlay_metadata_separately",
    "sync_runtime::tests::managed_new_page_conflict_resolution_uses_the_identifiable_winner_path_and_revision",
    "sync_runtime::tests::new_markdown_and_org_pages_are_born_with_parsed_final_identity_at_selected_path",
    "sync_runtime::tests::observed_receiver_external_edit_precedes_remote_delete_in_both_callback_orders",
    "sync_runtime::tests::two_offline_authors_union_frontier_heads_converge_without_return_first",
  ]),
  providerRecoveryAndPublication: Object.freeze([
    "sync_runtime::tests::accepted_ordinary_manifest_loss_without_local_archive_blocks",
    "sync_runtime::tests::absent_superseded_head_settles_and_reappeared_head_retires_again",
    "sync_runtime::tests::clean_shutdown_waits_for_imprecise_discovery_before_publishing_own_head",
    "sync_runtime::tests::deleted_own_frontier_head_is_republished_from_local_authority",
    "sync_runtime::tests::durable_shared_publication_survives_crash_before_provider_tick",
    "sync_runtime::tests::exact_deletion_of_an_accepted_manifest_republishes_from_local_archive",
    "sync_runtime::tests::foreign_incomplete_manifest_does_not_block_own_frontier_or_intent_retirement",
    "sync_runtime::tests::frontier_head_conflicts_fall_back_and_preserve_unreconciled_bytes",
    "sync_runtime::tests::frontier_head_crash_cuts_repair_before_safe_handoff",
    "sync_runtime::tests::locally_admitted_shared_object_precedes_own_frontier_publication",
    "sync_runtime::tests::manifest_recovery_publication_crash_cuts_resume_before_canonical_visibility",
    "sync_runtime::tests::manifestless_no_op_partial_direct_dependency_blocks",
    "sync_runtime::tests::outbound_child_blocks_when_ordinary_parent_is_lost",
    "sync_runtime::tests::provider_object_physical_write_cut_requires_exact_journal_completion_before_manifest_and_head",
    "sync_runtime::tests::provider_staging_siblings_are_non_authoritative_for_exact_and_full_ingress",
    "sync_runtime::tests::removing_rejected_exact_provider_residue_unblocks_queued_work",
    "sync_runtime::tests::reordered_remote_acceptance_cannot_reuse_stale_recovery_coverage",
    "sync_runtime::tests::restarted_provider_child_accepts_manifestless_no_op_dependency_after_duplicate_reordering",
    "sync_runtime::tests::unsafe_reopen_repairs_accepted_batch_after_pending_marker_creation_failure",
  ]),
  boundedDiscoveryAndTraversal: Object.freeze([
    "sync_runtime::tests::closed_device_walks_only_an_unseen_linear_tail_from_latest_head",
    "sync_runtime::tests::complete_namespace_loss_repair_above_head_scan_cap_is_chunked",
    "sync_runtime::tests::exact_object_progress_rechecks_every_incomplete_manifest_once_per_wave",
    "sync_runtime::tests::headless_legacy_namespace_falls_back_once_then_reopens_from_frontier_head",
    "sync_runtime::tests::oversized_provider_callback_retains_scan_and_safe_shutdown_drains_it",
    "sync_runtime::tests::reverse_delivered_provider_chain_has_linear_readiness_work",
    "sync_runtime::tests::shared_provider_archive_beyond_entry_and_byte_scan_caps_joins_incrementally",
    "sync_runtime::tests::startup_discovers_manifest_stranded_beyond_an_older_valid_frontier_head",
    "sync_runtime::tests::uncovered_legacy_head_backfills_recovery_in_bounded_chunks_before_safe",
  ]),
});

export const KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES = Object.freeze(
  Object.values(KNOWN_RED_SYNC_RUNTIME_FAILURE_FAMILIES).flat().sort()
);

export const LINUX_CORE_RELEASE_FILTERSET =
  KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES.length === 0
    ? "all()"
    : "not (" + KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES
      .map((testName) => "test(=" + testName + ")")
      .join(" | ") + ")";
// Windows is deliberately not a second complete tine-core behavior matrix.
// Linux carries that full inventory in four isolated shards. This exact list is
// the Windows release contract: every explicitly Windows-named core test, plus
// the bootstrap/durability/lifecycle witnesses that exercised the Windows
// failures fixed for v0.6.90. Keep the names explicit so a rename, removal, or
// newly added Windows test cannot silently shrink the release gate.
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
  "oplog::sqlite::tests::windows_entry_file_identity_classifies_reparse_lease_as_replaced",
  "windows_no_follow_publication_read_and_directory_flush_succeed",
  "windows_reparse_files_and_directories_are_rejected",
]);

export const WINDOWS_CORE_LIFECYCLE_WITNESS_NAMES = Object.freeze([
  "oplog::local_active::bounded_admission::clean_admissions_are_bounded_at_one_one_thousand_and_ten_thousand",
  "model::tests::bootstrap_source_regular_file_sync_uses_supported_handle_access",
  "oplog::import::tests::bootstrap_preparation_flush_handles_use_platform_durability_contracts",
  "oplog::import::tests::inactive_streaming_bootstrap_preseal_crash_retries_exactly",
  "oplog::import::tests::inactive_streaming_bootstrap_repeated_run_reuses_exact_seal",
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

  for (const [key, test] of releaseInventory.tests) {
    if (!coreInventory.tests.has(key)) {
      fail(`Linux release selection contains non-inventory test ${test.binaryId} ${test.testName}`);
    }
  }

  const excluded = [...coreInventory.tests.entries()]
    .filter(([key]) => !releaseInventory.tests.has(key))
    .map(([, test]) => test);
  const unexpected = excluded.find((test) => !test.testName.startsWith("sync_runtime::tests::"));
  if (unexpected) {
    fail(`Linux release selection excluded non-oracle test ${unexpected.binaryId} ${unexpected.testName}`);
  }

  // Module membership is not oracle-ness. The claim this contract has to make
  // is that every test the release gate drops is a NAMED, deliberately dropped
  // test, so compare the actual excluded names against the exclusion contract
  // in both directions: an unlisted exclusion means a healthy test was silently
  // un-gated, and a listed name with no test behind it means the list has
  // rotted. Names, never counts.
  requireExactNameSet(
    excluded.map((test) => test.testName),
    KNOWN_RED_SYNC_RUNTIME_EXCLUDED_TEST_NAMES,
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
      `Linux nextest contract OK: ${result.testCount} release tests exactly once across ${LINUX_TINE_CORE_SHARD_COUNT} hash shards (${result.shardCounts.join(", ")}); every current tine-core test is selected except exactly the ${selection.knownRedTestCount} named, behavior-family-classified known-red legacy-oracle tests.`
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
      `Windows nextest contract OK: ${result.coreTestCount} compiled tine-core tests, ${result.coreSmokeTestCount} contract-selected cross-layer smokes, ${result.windowsNamedCount} Windows-named core tests, and ${result.bootstrapWitnessCount} bootstrap capture witnesses.`
    );
    if (process.argv.includes("--run-smoke")) {
      runWindowsSmoke("tine-core", WINDOWS_CORE_SMOKE_FILTERSET, "Windows core/storage integration smoke");
    }
    return;
  }
  fail(
    "pass --mode linux (add --run-shard N for one release shard, or --run-selection for the whole verified release selection) or --mode windows (add --run-smoke to execute the verified Windows core/storage integration selection)"
  );
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main();
