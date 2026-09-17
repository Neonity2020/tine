// One place decides what a failed browser launch tells you.
//
// 103 harness scripts launch Chromium. Run any of them without sourcing
// `scripts/env.sh` and Playwright fails with "Executable doesn't exist" plus a
// suggestion to run `npx playwright install`, which is the WRONG remedy: the
// browsers are already on the machine, in the sibling `.toolchain/` the repo
// deliberately keeps outside itself, and the one-line fix is to source env.sh.
// A message that names the wrong remedy is worse than one that names none —
// it sends the reader somewhere that cannot work.
//
// `scripts/lib/playwright.mjs` wraps `launch` and says that. It is only worth
// having if every script goes through it, which is what this guard enforces.
// It also pins the wrapper's completeness: the wrapper forwards `launch` and
// nothing else, so a script reaching for another member of a browser type
// would silently get `undefined` rather than a clear error. That must fail
// here instead.
import { describe, expect, it } from "vitest";
import { REMEDY, guarded } from "../scripts/lib/playwright.mjs";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptsDir = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "scripts");
const WRAPPER = "lib/playwright.mjs";

/** Files importing the package directly, relative to `scripts/`. */
export function directPackageImports(files: { name: string; source: string }[]): string[] {
  return files
    .filter(({ name, source }) => name !== WRAPPER && /\bfrom\s+["']playwright["']/.test(source))
    .map(({ name }) => name);
}

/** Browser-type members used anywhere, so the wrapper can be checked complete. */
export function browserTypeMembers(files: { name: string; source: string }[]): string[] {
  const members = new Set<string>();
  for (const { name, source } of files) {
    if (name === WRAPPER) continue;
    for (const match of source.matchAll(/\b(?:chromium|webkit|firefox)\.([A-Za-z]\w*)/g)) {
      members.add(match[1]!);
    }
  }
  return [...members].sort();
}

function scripts(dir: string, prefix = ""): { name: string; source: string }[] {
  return fs.readdirSync(dir, { withFileTypes: true }).flatMap((entry) => {
    const full = path.join(dir, entry.name);
    if (entry.isDirectory()) return scripts(full, `${prefix}${entry.name}/`);
    return entry.isFile() && entry.name.endsWith(".mjs")
      ? [{ name: `${prefix}${entry.name}`, source: fs.readFileSync(full, "utf8") }]
      : [];
  });
}

describe("Playwright import guard", () => {
  const files = scripts(scriptsDir);

  it("finds the scripts it is supposed to scan", () => {
    expect(files.length).toBeGreaterThan(100);
    expect(files.map((file) => file.name)).toContain(WRAPPER);
  });

  it("detects the shape it exists to catch, and leaves prose alone", () => {
    const sources = [
      { name: "e2e-new.mjs", source: 'import { chromium } from "playwright";' },
      { name: "check-new.mjs", source: "// Launched with playwright; see scripts/lib/playwright.mjs.\nchromium.launch();" },
    ];
    expect(directPackageImports(sources)).toEqual(["e2e-new.mjs"]);
  });

  it("every script launches browsers through the wrapper", () => {
    expect(
      directPackageImports(files),
      'these scripts import "playwright" directly, so a launch failure there names the wrong '
        + "remedy (`npx playwright install`) instead of `source scripts/env.sh`. Import "
        + '{ chromium } from "./lib/playwright.mjs" instead — see scripts/check-block-spacing.mjs.',
    ).toEqual([]);
  });

  it("the wrapper forwards every browser-type member the scripts use", () => {
    const wrapper = files.find((file) => file.name === WRAPPER)!.source;
    const missing = browserTypeMembers(files).filter((member) => !new RegExp(`\\b${member}\\s*\\(`).test(wrapper));
    expect(
      missing,
      `scripts/lib/playwright.mjs does not forward ${missing.join(", ")}, so a caller would get `
        + "undefined rather than a browser. Add the member to the wrapper (delegating to the real "
        + "browser type) before using it.",
    ).toEqual([]);
  });
});

// What the wrapper may and may not refuse.
//
// Its first version threw before launching whenever PLAYWRIGHT_BROWSERS_PATH
// was unset. That variable is set only by `scripts/env.sh`; the hosted Linux
// E2E job installs browsers with `npx playwright install` into Playwright's own
// default location and never sets it. So the check did not detect a broken
// environment — it declared the supported CI configuration broken, and locally
// it failed selection-wrap, publish-security and published-app before a browser
// was ever asked for. A precondition is not evidence: assert the property, and
// let the launch that actually cannot find a browser be the thing that speaks.
describe("Playwright launch wrapper", () => {
  it("launches with no PLAYWRIGHT_BROWSERS_PATH, because the hosted runner has none", async () => {
    const browser = { marker: "launched" };
    const stub = { launch: async () => browser };
    const previous = process.env.PLAYWRIGHT_BROWSERS_PATH;
    delete process.env.PLAYWRIGHT_BROWSERS_PATH;
    try {
      await expect(guarded(stub, "chromium").launch()).resolves.toBe(browser);
    } finally {
      if (previous !== undefined) process.env.PLAYWRIGHT_BROWSERS_PATH = previous;
    }
  });

  it("forwards the caller's launch options", async () => {
    const seen: unknown[] = [];
    const stub = { launch: async (...args: unknown[]) => { seen.push(...args); return {}; } };
    await guarded(stub, "chromium").launch({ headless: true });
    expect(seen).toEqual([{ headless: true }]);
  });

  it("names the remedy when the browser is genuinely missing", async () => {
    const stub = {
      launch: async () => {
        throw new Error("Executable doesn't exist at /x/ms-playwright/chromium-1234/chrome-linux/chrome");
      },
    };
    await expect(guarded(stub, "chromium").launch()).rejects.toThrow(REMEDY);
  });

  it("passes an unrelated failure through unchanged", async () => {
    const original = new Error("Target page, context or browser has been closed");
    const stub = { launch: async () => { throw original; } };
    await expect(guarded(stub, "chromium").launch()).rejects.toBe(original);
  });
});
