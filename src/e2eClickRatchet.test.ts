// A bare `.click()` in a journey asserts that nothing is on top of the element.
//
// WebDriver aims at the centre of the element's FIRST CSS box clipped to the
// viewport and hit-tests there. If anything covers that point — a sticky
// first-run toast, a native `title` tooltip left by the pointer's own previous
// move, a still-animating panel — the driver refuses the click, and the journey
// fails ten seconds later on whatever the click was supposed to cause. During
// v0.6.984 that shape cost two hosted assembly round trips (~90 minutes each)
// and produced failure text that named the button but never what was covering
// it.
//
// `scripts/lib/e2e-click.mjs` exports `clickWhenReachable(browser, selector,
// { what })`: it parks the pointer, waits for the aim point to belong to the
// element, waits for the box to stop moving, retries only a refusal (a refused
// click was never dispatched, so retrying it cannot double-fire), and on
// failure reports what `elementFromPoint` found instead. It is the blessed
// exemplar; `scripts/e2e-print-security.mjs` is a short call site to imitate.
//
// This ratchet does not demand the 241 pre-existing sites be converted — that
// is a separate, larger piece of work, and most of them click things nothing
// ever covers. It requires only that the count never rise, so the next journey
// is written with the helper instead of copying the nearest bare click.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "scripts");

/**
 * Awaited WebDriver `.click()` calls, by line.
 *
 * `await` is the discriminator. A journey also contains `element.click()` calls
 * inside `browser.execute(() => …)` callbacks; those run in page context, are
 * never awaited, and dispatch a DOM event rather than a synthetic pointer — no
 * hit test, so no interception. Walking back to the enclosing statement's
 * boundary (`;`, `{` or `}`) separates the two without parsing JavaScript: the
 * arrow body's own `{` terminates the search for a page-context call.
 */
export function awaitedClickSites(source: string): number[] {
  const boundary = /[;{}]/;
  const sites: number[] = [];
  for (let cursor = 0; ; ) {
    const at = source.indexOf(".click()", cursor);
    if (at < 0) break;
    cursor = at + ".click()".length;
    let start = at;
    while (start > 0 && !boundary.test(source[start - 1]!)) start -= 1;
    if (/\bawait\b/.test(source.slice(start, at))) {
      sites.push(source.slice(0, at).split("\n").length);
    }
  }
  return sites;
}

/**
 * The count each journey carried when the ratchet was written. A journey absent
 * from this map must have zero. Lowering an entry is always welcome — route a
 * site through `clickWhenReachable` and drop the number; the test then pins the
 * gain.
 */
const ALLOWED: Record<string, number> = {
  "e2e-alias.mjs": 3,
  "e2e-block-embed.mjs": 2,
  "e2e-block-ref-count.mjs": 8,
  "e2e-blockselect.mjs": 2,
  "e2e-capture.mjs": 3,
  "e2e-caret.mjs": 1,
  "e2e-clickcaret-repro.mjs": 1,
  "e2e-clipboard-roundtrip.mjs": 3,
  "e2e-compat-home-current-page.mjs": 4,
  "e2e-concord-focus-freshness.mjs": 2,
  "e2e-concord-live-save.mjs": 6,
  "e2e-concord-sync-copy.mjs": 5,
  "e2e-conflict-keep-mine.mjs": 3,
  "e2e-dirty-editor-replacement.mjs": 3,
  "e2e-editor-interactions.mjs": 4,
  "e2e-empty-query-workspace.mjs": 9,
  "e2e-external-assets.mjs": 1,
  "e2e-external-graph-wide-changes.mjs": 1,
  "e2e-media.mjs": 3,
  "e2e-mobile-drawers.mjs": 9,
  "e2e-multigraph.mjs": 1,
  "e2e-native-titlebar.mjs": 3,
  "e2e-og-parity-references.mjs": 19,
  "e2e-outline-guide.mjs": 4,
  "e2e-page-file-actions.mjs": 3,
  "e2e-page-properties.mjs": 8,
  "e2e-page-trailing-block.mjs": 3,
  "e2e-pdf-logseq.mjs": 8,
  "e2e-pdf-ownership.mjs": 5,
  "e2e-pdf-routes.mjs": 6,
  "e2e-pdf-scroll-resources.mjs": 1,
  "e2e-plugin-graph-ownership.mjs": 2,
  "e2e-plugin-revocation.mjs": 6,
  "e2e-plugins.mjs": 2,
  "e2e-print-security.mjs": 1,
  "e2e-publish-query.mjs": 3,
  "e2e-query-export.mjs": 1,
  "e2e-query-sheet.mjs": 14,
  "e2e-query-vocabulary.mjs": 10,
  "e2e-query-workspace.mjs": 31,
  "e2e-rename.mjs": 3,
  "e2e-right-sidebar-collapse.mjs": 2,
  "e2e-scrollbars.mjs": 1,
  "e2e-search-parity.mjs": 1,
  "e2e-selection-actions.mjs": 2,
  "e2e-selectwrap.mjs": 1,
  "e2e-sheets.mjs": 7,
  "e2e-sidebar-sections.mjs": 2,
  "e2e-split-history.mjs": 2,
  "e2e-structured-paste.mjs": 2,
  "e2e-tab-overflow.mjs": 3,
  "e2e-tag-autocomplete.mjs": 1,
  "e2e-theme-presentation.mjs": 6,
  "e2e-windows-page-reference-latency.mjs": 3,
  "e2e-windows-smoke.mjs": 1,
};

describe("E2E click ratchet", () => {
  const journeys = fs.readdirSync(scriptsDir).filter((name) => /^e2e-.*\.mjs$/.test(name)).sort();

  it("finds the journeys it is supposed to scan", () => {
    expect(journeys.length).toBeGreaterThan(50);
  });

  it("detects the shape it exists to catch", () => {
    const regression = [
      'const row = await browser.$(".switcher-row");',
      "await row.click();",
      'await browser.$("button=Diagnostics").click();',
    ].join("\n");
    expect(awaitedClickSites(regression)).toEqual([2, 3]);
  });

  it("accepts the helper, and page-context clicks that cannot be intercepted", () => {
    const fixed = [
      'await clickWhenReachable(browser, ".switcher-row", { what: "the first result" });',
      "await browser.execute(() => {",
      '  document.querySelector(".ctx-item")?.click();',
      "});",
    ].join("\n");
    expect(awaitedClickSites(fixed)).toEqual([]);
  });

  for (const journey of journeys) {
    const budget = ALLOWED[journey] ?? 0;
    it(`${journey}: no NEW bare click stands in for a reachability precondition`, () => {
      const sites = awaitedClickSites(fs.readFileSync(path.join(scriptsDir, journey), "utf8"));
      expect(
        sites.length,
        sites.length > budget
          ? `${journey} clicks without establishing reachability at line(s) ${sites.join(", ")} `
            + `(budget ${budget}). WebDriver aims at the first CSS box clipped to the viewport and `
            + "refuses the click if anything covers that point; the journey then fails later, on "
            + "the effect rather than the cause. Use clickWhenReachable() from "
            + "scripts/lib/e2e-click.mjs — see scripts/e2e-print-security.mjs for a short call site."
          : `${journey} improved to ${sites.length} sites; lower its entry in ALLOWED to pin the gain.`,
      ).toBe(budget);
    });
  }
});
