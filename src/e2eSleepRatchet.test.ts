// A fixed sleep before reading persisted bytes is an assertion about a debounce
// nobody observed.
//
// `e2e-pdf-routes` failed 2 runs in 5 on an unchanged binary with "restored PDF
// page was 1, expected 2". It had set page 2, slept a flat 4500ms "for both
// persistence debounces", asserted only that the session file *existed*, then
// killed the app. When the debounce had not fired inside 4.5s the page was
// never written, and the restore was faithfully restoring a file that still
// said page 1. The journey looked like it was testing restore; it was testing
// the machine's mood.
//
// `scripts/e2e-file-poll.mjs` already exports `waitForFileText(file, predicate,
// label)` — bounded polling that tolerates the unlink window of an atomic
// rename. It is the blessed exemplar; `scripts/e2e-page-file-actions.mjs` is a
// short call site to imitate. This ratchet does not demand the 25 pre-existing
// sites be converted — that is a separate, larger piece of work — it only
// requires that the count never rise, so the next journey is written with the
// predicate instead of copying the nearest sleep.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "scripts");

/**
 * Sites where a sleep of >=1s is followed, within a few lines, by reading a
 * file whose contents the journey then asserts on.
 */
export function sleepThenReadSites(source: string): number[] {
  const lines = source.split("\n");
  const sites: number[] = [];
  for (let index = 0; index < lines.length; index += 1) {
    const slept = /\bsleep\((\d{4,})\)/.exec(lines[index]!);
    if (!slept || Number(slept[1]) < 1000) continue;
    const window = lines.slice(index + 1, index + 9).join("\n");
    if (/fs\.(existsSync|readFileSync|statSync)\s*\(/.test(window)) sites.push(index + 1);
  }
  return sites;
}

/**
 * The count each journey carried when the ratchet was written. A journey absent
 * from this map must have zero. Lowering an entry is always welcome — convert a
 * site to `waitForFileText` and drop the number; the test then pins the gain.
 */
const ALLOWED: Record<string, number> = {
  "e2e-compat-home-current-page.mjs": 1,
  "e2e-concord-sync-copy.mjs": 1,
  "e2e-pdf-ownership.mjs": 1,
  // Documented in the journey itself: converting this one needs a product
  // answer about which store owns a restored PDF page, not a harness change.
  "e2e-pdf-routes.mjs": 1,
  "e2e-query-workspace.mjs": 1,
  "e2e-rename.mjs": 1,
  "e2e-search-parity.mjs": 1,
  "e2e-sheets.mjs": 19,
  "e2e-structured-paste.mjs": 1,
};

describe("E2E readiness ratchet", () => {
  const journeys = fs.readdirSync(scriptsDir).filter((name) => /^e2e-.*\.mjs$/.test(name)).sort();

  it("finds the journeys it is supposed to scan", () => {
    expect(journeys.length).toBeGreaterThan(50);
  });

  it("detects the shape it exists to catch", () => {
    const regression = [
      "await sleep(4500);",
      'const saved = fs.readFileSync(sessionPath, "utf8");',
      "assert(saved.includes('page'), 'not persisted');",
    ].join("\n");
    expect(sleepThenReadSites(regression)).toEqual([1]);
  });

  it("accepts a bounded predicate", () => {
    const fixed = [
      'await waitForFileText(sessionPath, (text) => text.includes("page"), "page 2");',
      'const saved = fs.readFileSync(sessionPath, "utf8");',
    ].join("\n");
    expect(sleepThenReadSites(fixed)).toEqual([]);
  });

  for (const journey of journeys) {
    const budget = ALLOWED[journey] ?? 0;
    it(`${journey}: no NEW sleep stands in for a persistence predicate`, () => {
      const sites = sleepThenReadSites(fs.readFileSync(path.join(scriptsDir, journey), "utf8"));
      expect(
        sites.length,
        sites.length > budget
          ? `${journey} sleeps then reads a file it asserts on at line(s) ${sites.join(", ")} `
            + `(budget ${budget}). A fixed sleep is not a readiness signal: if the debounce has not `
            + "fired, the assertion tests the machine's speed. Use waitForFileText() from "
            + "scripts/e2e-file-poll.mjs — see scripts/e2e-page-file-actions.mjs for a short call site."
          : `${journey} improved to ${sites.length} sites; lower its entry in ALLOWED to pin the gain.`,
      ).toBe(budget);
    });
  }
});
