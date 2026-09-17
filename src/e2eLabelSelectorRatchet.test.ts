// A selector keyed on visible text asserts the wording, not the behaviour.
//
// Thirteen journey sites opened Settings tabs by label — `button=Plugins`,
// XPath on `normalize-space(.)='Editor'`, `button=Installed (1)` — and not one
// of them asserted anything about the label; they all wanted "open that tab".
// The wording has already moved once ("Diagnostics" became "Help &
// diagnostics"), and **v0.8.0 localization reworks every label in the app at
// once**. A journey that fails then fails in a release E2E, twenty minutes into
// a hosted run, naming a button rather than the rename that broke it.
//
// `data-settings-tab` (src/components/Settings.tsx) and `data-plugin-view` are
// the blessed shape: the control's identity, addressable and stable across
// rewordings. `scripts/e2e-native-titlebar.mjs` is the exemplar call site.
//
// What this guard asserts, and why it is not merely a count:
//
//  1. No journey selects a Settings tab or a plugin view by its text. That
//     class is at ZERO, and a zero-violation class is the only kind that does
//     not train the next author by example.
//  2. Every remaining text selector is classified against the frontend source.
//     A text that appears in `src/` is `matched`; one that does not is a
//     fixture string (a plugin name, a graph's own content) and cannot break on
//     a rewording. Reword a label a journey depends on and its site flips from
//     matched to unmatched, so this fails in `npm test` naming the text — which
//     is the failing input the whole guard exists for.
//  3. Per file, neither count may move, so a new label-keyed selector cannot be
//     added by copying the nearest one.
import { describe, expect, it } from "vitest";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const scriptsDir = path.join(here, "..", "scripts");

export interface TextSelectorSite {
  line: number;
  text: string;
}

/**
 * Text-keyed selector sites: WebdriverIO's `tag=Text`, XPath
 * `normalize-space(.)='Text'`, and Playwright's `hasText: "Text"`.
 */
export function textSelectorSites(source: string): TextSelectorSite[] {
  const patterns = [
    /\$\(\s*"(?:[a-zA-Z][\w-]*)?=([^"]+)"/g,
    /normalize-space\(\.\)\s*=\s*'([^']+)'/g,
    /hasText:\s*"([^"]+)"/g,
  ];
  const sites: TextSelectorSite[] = [];
  for (const pattern of patterns) {
    pattern.lastIndex = 0;
    for (let match = pattern.exec(source); match; match = pattern.exec(source)) {
      sites.push({ line: source.slice(0, match.index).split("\n").length, text: match[1]! });
    }
  }
  return sites.sort((a, b) => a.line - b.line);
}

/** Sites that reach a Settings tab or plugin view through its wording. */
export function identityBypassSites(source: string): TextSelectorSite[] {
  const sites: TextSelectorSite[] = [];
  const lines = source.split("\n");
  lines.forEach((line, index) => {
    const named = /settings-nav-item|plugin-settings-nav/.test(line);
    const byText = /normalize-space\(\.\)\s*=|\$\(\s*"(?:[a-zA-Z][\w-]*)?=/.test(line);
    if (named && byText) sites.push({ line: index + 1, text: line.trim() });
    // `$("button=Plugins")` names no class at all; catch the tab labels by name.
    const bare = /\$\(\s*"button=(Plugins|Editor|Appearance|Journals|Files|Graph|About|Browse|Diagnostics)"/.exec(line);
    if (bare) sites.push({ line: index + 1, text: bare[0] });
  });
  return sites;
}

/**
 * Per journey: [matched in src/, fixture strings]. A journey absent from this
 * map must have no text selectors at all. Lowering an entry is always welcome —
 * give the control an identity attribute and drop the number.
 */
const ALLOWED: Record<string, [number, number]> = {
  "e2e-block-ref-count.mjs": [1, 0],
  "e2e-native-titlebar.mjs": [1, 0],
  "e2e-og-parity-references.mjs": [1, 0],
  "e2e-pdf-routes.mjs": [1, 0],
  "e2e-plugins.mjs": [0, 6],
  "e2e-search-parity.mjs": [1, 0],
  "e2e-theme-presentation.mjs": [1, 2],
};

function frontendSource(): string {
  const files: string[] = [];
  const walk = (dir: string) => {
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) walk(full);
      else if (/\.tsx?$/.test(entry.name) && !/\.test\.tsx?$/.test(entry.name)) files.push(full);
    }
  };
  walk(here);
  return files.map((file) => fs.readFileSync(file, "utf8")).join("\n");
}

describe("E2E text-selector ratchet", () => {
  const journeys = fs.readdirSync(scriptsDir).filter((name) => /^e2e-.*\.mjs$/.test(name)).sort();
  const src = frontendSource();

  it("finds the journeys it is supposed to scan", () => {
    expect(journeys.length).toBeGreaterThan(50);
  });

  it("detects the shapes it exists to catch", () => {
    const regression = [
      'await browser.$("button=Plugins").click();',
      "await browser.$(\"//button[contains(@class,'settings-nav-item') and normalize-space(.)='Editor']\").click();",
      'const row = page.locator(".settings-field", { hasText: "Bullet threading" });',
    ].join("\n");
    expect(textSelectorSites(regression).map((site) => site.text))
      .toEqual(["Plugins", "Editor", "Bullet threading"]);
    expect(identityBypassSites(regression).map((site) => site.line)).toEqual([1, 2]);
  });

  it("accepts the identity attributes that replace them", () => {
    const fixed = [
      "await browser.$('.settings-nav-item[data-settings-tab=\"plugins\"]').click();",
      "await browser.$('.plugin-settings-nav [data-plugin-view=\"installed\"]').click();",
    ].join("\n");
    expect(textSelectorSites(fixed)).toEqual([]);
    expect(identityBypassSites(fixed)).toEqual([]);
  });

  it("reaches no Settings tab or plugin view through its wording", () => {
    const offenders = journeys.flatMap((name) =>
      identityBypassSites(fs.readFileSync(path.join(scriptsDir, name), "utf8"))
        .map((site) => `scripts/${name}:${site.line} ${site.text}`));
    expect(
      offenders,
      "Address the tab by identity: .settings-nav-item[data-settings-tab=\"<id>\"] or "
      + "[data-plugin-view=\"<id>\"]. Both attributes exist in src/components/Settings.tsx; "
      + "scripts/e2e-native-titlebar.mjs is the exemplar. v0.8.0 localization reworks every "
      + "label at once, so a wording-keyed selector is a scheduled release failure.",
    ).toEqual([]);
  });

  it.each(journeys)("%s keeps its recorded text selectors, and they still exist in src/", (name) => {
    const sites = textSelectorSites(fs.readFileSync(path.join(scriptsDir, name), "utf8"));
    const matched = sites.filter((site) => src.includes(site.text));
    const fixtures = sites.filter((site) => !src.includes(site.text));
    const [allowedMatched, allowedFixtures] = ALLOWED[name] ?? [0, 0];
    expect(
      [matched.length, fixtures.length],
      `scripts/${name}: recorded ${allowedMatched} selector(s) keyed on text that exists in src/ `
      + `and ${allowedFixtures} keyed on fixture content, found ${matched.length} and `
      + `${fixtures.length}.\n`
      + `  in src/: ${matched.map((site) => `${site.line}:${JSON.stringify(site.text)}`).join(", ") || "none"}\n`
      + `  not in src/: ${fixtures.map((site) => `${site.line}:${JSON.stringify(site.text)}`).join(", ") || "none"}\n`
      + "A site that moved from the first list to the second is a UI label that was reworded "
      + "out from under this journey — give the control an identity attribute rather than "
      + "retyping the new wording. A rise in either count means a new text-keyed selector; "
      + "use an identity attribute instead. A fall is a gain: record it here.",
    ).toEqual([allowedMatched, allowedFixtures]);
  });
});
