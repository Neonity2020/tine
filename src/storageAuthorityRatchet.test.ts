// Source-scan ratchet over `managedStorageRuntime.snapshot(` readers (I-6).
//
// House pattern: the `hmac::verify` source-count guard. This is the CHEAP half
// of the guard pair — it cannot prove reachability (aliasing evades a grep), so
// it exists to make a NEW authority read visible in review, not to certify the
// ones that remain. The reachability claim is proved by
// `src/storageDispatch.test.ts` (route level) and
// `src/storageDispatchRoutes.test.ts` (the real store/filedrop/carry paths).
//
// Equality is deliberate: deleting a legitimate reader must force a census
// edit too, so a refactor cannot silently trade that deletion for a new reader
// elsewhere and keep the same total. The allowlist below IS the census, in
// code. Every entry is either the front door's single selection read, an
// authority value capture / I-20 staleness re-check, or a display-only status
// reader. A read that decides where a semantic storage operation runs does not
// belong outside `storageDispatch.ts`.

import { describe, it, expect } from "vitest";
import { readdirSync, readFileSync, statSync } from "node:fs";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";

const SRC = fileURLToPath(new URL(".", import.meta.url));

const SYMBOL = "managedStorageRuntime.snapshot(";

/**
 * Files that are allowed to name the symbol because they OWN it, not because
 * they consume it. Excluded from the census rather than allowlisted with a
 * count, so ordinary edits inside them do not churn this test.
 */
const OWNERS = new Set([
  // Publishes the snapshot: binds it, validates the envelope, revokes writability.
  "managedStorageRuntime.ts",
]);

/**
 * The exact production census, with the class that makes
 * it legitimate. `count` is the number of times the symbol appears in the file.
 *
 * class (b) = authority selection/capture or I-20 re-check against a stamp.
 * class (c) = display, notice, or search-index status only; no write routing.
 */
const CENSUS: readonly { file: string; count: number; class: "b" | "c"; why: string }[] = [
  {
    file: "App.tsx",
    count: 3,
    class: "c",
    why: "the generation/authority memos scope the absence-sweep subscription, "
      + "and the notice effect displays runtime feedback; none routes a write.",
  },
  {
    file: "components/QuickSwitcher.tsx",
    count: 1,
    class: "c",
    why: "reads search-index-building status to render search feedback only.",
  },
  {
    file: "components/Settings.tsx",
    count: 2,
    class: "c",
    why: "reads managed runtime status and error for the settings status panel only.",
  },
  {
    file: "storageDispatch.ts",
    count: 1,
    class: "b",
    why: "the one sanctioned authority-selection read for semantic storage operations.",
  },
  {
    file: "store.ts",
    count: 7,
    class: "b",
    why: "the test-only Direct bootstrap plus value captures and their I-20 re-checks: "
      + "createPageMutationPlan/pageMutationPlanCurrent, "
      + "consumeManagedBulkInsertionAdmission, captureBulkRouteFence/bulkRouteFenceCurrent, "
      + "and managedMoveAdmission() "
      + "(the managed arm's own writability accessor, used inside the managed "
      + "choreography after the route is already chosen).",
  },
];

function sourceFiles(dir: string, acc: string[] = []): string[] {
  for (const entry of readdirSync(dir)) {
    const full = join(dir, entry);
    if (statSync(full).isDirectory()) {
      sourceFiles(full, acc);
      continue;
    }
    if (!/\.tsx?$/.test(entry)) continue;
    if (/\.test\.tsx?$/.test(entry)) continue;
    if (/^testSetup\./.test(entry)) continue;
    acc.push(full);
  }
  return acc;
}

function occurrences(): Map<string, number> {
  const found = new Map<string, number>();
  for (const file of sourceFiles(SRC)) {
    const name = relative(SRC, file).replaceAll("\\", "/");
    if (OWNERS.has(name)) continue;
    const count = readFileSync(file, "utf8").split(SYMBOL).length - 1;
    if (count > 0) found.set(name, count);
  }
  return found;
}

const RULE =
  `I-6 — storage authority is selected in ONE place and then flows as a value.\n`
  + `A semantic storage operation must state its intent to src/storageDispatch.ts `
  + `(the exemplars to imitate: dispatchCrossPageMove / dispatchDroppedFileInsertion / `
  + `dispatchBulkInsertion) `
  + `instead of reading \`${SYMBOL}\` and branching at its own call site — that is how `
  + `four cross-page-move forks drifted apart (audit UI-3).\n`
  + `If your read is genuinely an authority capture/re-check or a display-only status reader, `
  + `add its exact per-file count to the CENSUS table with its class and reason; equality is `
  + `intentional, so deletions require a census edit too.`;

describe("managedStorageRuntime snapshot reader ratchet", () => {
  it("has exactly the censused production readers, and no others", () => {
    const found = occurrences();
    const expected = new Map(CENSUS.map((row) => [row.file, row.count]));

    const unexpected = [...found.keys()].filter((file) => !expected.has(file)).sort();
    expect(unexpected, `New authority reader in ${unexpected.join(", ")}.\n\n${RULE}`).toEqual([]);

    const drifted = [...expected]
      .filter(([file, count]) => (found.get(file) ?? 0) !== count)
      .map(([file, count]) => `${file}: censused ${count}, found ${found.get(file) ?? 0}`);
    expect(drifted, `Censused reader count changed:\n${drifted.join("\n")}\n\n${RULE}`).toEqual([]);
  });

  it("keeps the cross-page-move refusal message in exactly one place", () => {
    // The four move forks each spelled this out; a copy reappearing anywhere
    // else means an arm was re-implemented rather than dispatched.
    const message = "Can't move between pages while managed storage is changing state.";
    const copies = sourceFiles(SRC)
      .filter((file) => readFileSync(file, "utf8").includes(message))
      .map((file) => relative(SRC, file).replaceAll("\\", "/"))
      .sort();
    expect(copies, `Refusal text duplicated outside the front door.\n\n${RULE}`)
      .toEqual(["storageDispatch.ts"]);
  });
});
