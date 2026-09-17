#!/usr/bin/env node

import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import {
  LINUX_CORE_RELEASE_FILTERSET,
  LINUX_CORE_RELEASE_EXCLUDED_TEST_NAMES,
  LINUX_TINE_CORE_SHARD_COUNT,
  nextestRemedy,
  KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES,
  WINDOWS_CORE_EXACT_TEST_NAMES,
  WINDOWS_CORE_SMOKE_FILTERSET,
  WINDOWS_CORE_SMOKE_TEST_NAMES,
  inventoryFromNextestList,
  linuxCoreReleaseFilterset,
  verifyLinuxReleaseSelection,
  verifyLinuxShardCoverage,
  verifyWindowsCoreSmokeSelection,
  windowsCoreSmokeTestNames,
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

// The exclusion ledger is empty: the Linux release gate runs every current
// tine-core test. The contract still has to work when a name is added, so the
// fixture below exercises it with an explicit fixture ledger.
assert.deepEqual(KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES, []);
assert.deepEqual(LINUX_CORE_RELEASE_EXCLUDED_TEST_NAMES, []);
assert.equal(LINUX_CORE_RELEASE_FILTERSET, "all()");
assert.equal(linuxCoreReleaseFilterset([]), "all()");

const releaseSelectedNames = [
  "model::tests::ordinary_semantic_contract",
  "direct_projection::tests::current_clean_runtime_contract",
  "direct_projection::tests::new_production_test_is_selected_automatically",
];
const fixtureExcludedNames = [
  "model::tests::fixture_known_red_zulu",
  "model::tests::fixture_known_red_alpha",
];
assert.equal(
  linuxCoreReleaseFilterset(fixtureExcludedNames),
  "not (test(=model::tests::fixture_known_red_alpha) | test(=model::tests::fixture_known_red_zulu))"
);
assert.doesNotMatch(linuxCoreReleaseFilterset(fixtureExcludedNames), /not test\(\/.*\/\)/);

// With the empty ledger, the release selection must equal the full inventory.
const coreOnly = listedInventory("tine-core", releaseSelectedNames);
assert.deepEqual(
  verifyLinuxReleaseSelection(coreOnly, coreOnly),
  {
    coreTestCount: releaseSelectedNames.length,
    releaseTestCount: releaseSelectedNames.length,
    knownRedTestCount: 0,
  }
);
assert.throws(
  () => verifyLinuxReleaseSelection(coreOnly, listedInventory("tine-core", releaseSelectedNames.slice(0, 2))),
  /Linux release exclusion contract changed.*new_production_test_is_selected_automatically/
);

const coreWithKnownRedOracle = listedInventory("tine-core", [
  ...releaseSelectedNames,
  ...fixtureExcludedNames,
]);
const releaseWithoutKnownRedOracle = listedInventory("tine-core", releaseSelectedNames);
assert.deepEqual(
  verifyLinuxReleaseSelection(coreWithKnownRedOracle, releaseWithoutKnownRedOracle, fixtureExcludedNames),
  {
    coreTestCount: releaseSelectedNames.length + fixtureExcludedNames.length,
    releaseTestCount: releaseSelectedNames.length,
    knownRedTestCount: fixtureExcludedNames.length,
  }
);
assert.throws(
  () => verifyLinuxReleaseSelection(
    coreWithKnownRedOracle,
    listedInventory("tine-core", releaseSelectedNames.slice(0, 2)),
    fixtureExcludedNames
  ),
  /Linux release exclusion contract changed.*new_production_test_is_selected_automatically/
);

// And a listed exclusion whose test no longer exists must fail too, so the list
// cannot rot through renames or deletions.
const staleOracleName = fixtureExcludedNames[0];
const coreWithoutOneOracleTest = listedInventory("tine-core", [
  ...releaseSelectedNames,
  ...fixtureExcludedNames.filter((name) => name !== staleOracleName),
]);
assert.throws(
  () => verifyLinuxReleaseSelection(coreWithoutOneOracleTest, releaseWithoutKnownRedOracle, fixtureExcludedNames),
  new RegExp(`Linux release exclusion contract changed; missing \\[${staleOracleName}\\]`)
);

// The allow-by-default filter must not permit a non-oracle omission, whichever
// module the omitted test lives in.
assert.throws(
  () => verifyLinuxReleaseSelection(
    coreWithKnownRedOracle,
    listedInventory("tine-core", releaseSelectedNames.filter((name) => !name.startsWith("model::tests::"))),
    fixtureExcludedNames
  ),
  /Linux release exclusion contract changed.*model::tests::ordinary_semantic_contract/
);
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
];
assert.deepEqual(WINDOWS_CORE_EXACT_TEST_NAMES, coreWindowsTests);

const currentCoreSmokeTests = windowsCoreSmokeTestNames();
assert.deepEqual(WINDOWS_CORE_SMOKE_TEST_NAMES, currentCoreSmokeTests);
assert.deepEqual(currentCoreSmokeTests, coreWindowsTests);
assert.deepEqual(
  verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", [...currentCoreSmokeTests, "unselected_platform_neutral_test"]),
    listedInventory("tine-core", currentCoreSmokeTests)
  ),
  {
    coreTestCount: currentCoreSmokeTests.length + 1,
    coreSmokeTestCount: currentCoreSmokeTests.length,
    windowsNamedCount: 12,
  }
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", currentCoreSmokeTests.filter((name) => !name.includes("windows_live_graph"))),
    listedInventory("tine-core", currentCoreSmokeTests.filter((name) => !name.includes("windows_live_graph")))
  ),
  /Windows-named tine-core test inventory changed/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", [...currentCoreSmokeTests, "model::tests::some_new_windows_only_test"]),
    listedInventory("tine-core", currentCoreSmokeTests)
  ),
  /Windows-named tine-core test inventory changed; missing \[none\], unexpected \[model::tests::some_new_windows_only_test\]/
);
assert.throws(
  () => verifyWindowsCoreSmokeSelection(
    listedInventory("tine-core", currentCoreSmokeTests),
    listedInventory("tine-core", currentCoreSmokeTests.filter((name) => !name.includes("checked_open_accepts_an_approved_windows_assets_junction")))
  ),
  /Windows core smoke selection omitted required test/
);
assert.match(WINDOWS_CORE_SMOKE_FILTERSET, /test\(=model::tests::windows_live_graph_root_move_is_denied_without_rebinding\)/);
assert.match(WINDOWS_CORE_SMOKE_FILTERSET, /test\(=model::tests::windows_direct_publication_event_waits_for_inflight_writer_receipt\)/);
assert.doesNotMatch(WINDOWS_CORE_SMOKE_FILTERSET, /all\(\)|fast_commit/);
assert.equal(LINUX_TINE_CORE_SHARD_COUNT, 4);

// A ledger that excludes or requires a test BY NAME must name a test that
// exists. The release-selection contract already proves this, but only in
// hosted CI and only after a full compile, so a rename can ride master for a
// whole campaign first. Not hypothetical: Q1 of the query campaign renamed a
// ledgered test and left the release ledger naming the dead test. The filterset
// term test(=<dead name>) then excludes nothing, the observed exclusion set
// stops matching the contract, and EVERY master push fails on a message about
// an exclusion contract rather than about the rename that caused it. This scan
// is the cheap local copy of that proof: it runs with no cargo build at all.
const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const coreSourceRoot = path.join(repoRoot, "crates/tine-core/src");
const coreSourceText = fs
  .readdirSync(coreSourceRoot, { recursive: true })
  .filter((entry) => typeof entry === "string" && entry.endsWith(".rs"))
  .map((entry) => fs.readFileSync(path.join(coreSourceRoot, entry), "utf8"))
  .join("\n");
const declaredCoreTestFns = new Set(
  [...coreSourceText.matchAll(/\bfn\s+([A-Za-z0-9_]+)\s*\(/g)].map((match) => match[1])
);
for (const [ledger, names] of [
  ["KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES in tine-core-nextest-contract.mjs", KNOWN_RED_TINE_CORE_EXCLUDED_TEST_NAMES],
  ["WINDOWS_CORE_EXACT_TEST_NAMES in tine-core-nextest-contract.mjs", WINDOWS_CORE_EXACT_TEST_NAMES],
]) {
  const rotted = names.filter((name) => !declaredCoreTestFns.has(name.split("::").pop()));
  assert.deepEqual(
    rotted,
    [],
    `${ledger} names ${rotted.length} test(s) that crates/tine-core/src no longer declares: `
      + `${rotted.join(", ")}. A ledger entry by name selects or excludes nothing once the name is `
      + "dead, which breaks the nextest contract on every master push. Update the ledger in the "
      + "same commit as the rename, and when a known-red test has recovered drop its name outright "
      + "rather than carrying a green name forward as known-red."
  );
}

// The absence of cargo nextest must read as its own remedy. Two of the three
// call sites run with stdio:"inherit" and cannot improve cargo's own text, so
// the probe is the only place this can be said -- and it is worth nothing if it
// stops recognising what cargo actually prints.
assert.equal(
  typeof nextestRemedy("error: no such command: `nextest`\n\nhelp: a command with a similar name exists: `test`"),
  "string",
  "nextestRemedy no longer recognises cargo's missing-subcommand message, so a gate run from a "
    + "shell that has not sourced scripts/env.sh goes back to suggesting `cargo search cargo-nextest`."
);
assert.match(nextestRemedy("error: no such command: nextest"), /source scripts\/env\.sh/);
assert.equal(
  nextestRemedy("error: test run failed: 3 tests failed"),
  null,
  "nextestRemedy must not claim a missing toolchain when the tests simply failed."
);
assert.equal(nextestRemedy(undefined), null);

console.log("tine-core nextest contract fixture tests passed.");
