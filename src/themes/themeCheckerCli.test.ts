import { afterEach, describe, expect, it } from "vitest";
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";

let scratch = "";

afterEach(() => {
  if (scratch) fs.rmSync(scratch, { recursive: true, force: true });
  scratch = "";
});

function check(overrides: Record<string, unknown> = {}) {
  if (scratch) fs.rmSync(scratch, { recursive: true, force: true });
  scratch = fs.mkdtempSync(path.join(os.tmpdir(), "tine-theme-check-"));
  const manifest = {
    schemaVersion: 1,
    id: "page.tine.theme.test",
    name: "Test",
    version: "1.0.0",
    apiVersion: "0.2",
    description: "A checker fixture.",
    author: "Tine",
    license: "MIT",
    source: "https://example.invalid/theme",
    modes: { light: { "--ls-primary-background-color": "#fff" } },
    screenshots: [],
    ...overrides,
  };
  fs.writeFileSync(path.join(scratch, "theme.json"), JSON.stringify(manifest));
  const result = spawnSync(process.execPath, ["scripts/tine-theme.mjs", "check", scratch, "--json"], {
    cwd: process.cwd(),
    encoding: "utf8",
  });
  return JSON.parse(result.stdout) as { status: "passed" | "failed"; errors: Array<{ code: string }> };
}

describe("theme package checker", () => {
  it("accepts API 0.2 host presentation presets", () => {
    expect(check({ presentation: {
      contentTypography: "editorial-serif",
      journalHeader: "editorial",
      todayTaskSummary: "compact",
    } }).status).toBe("passed");
  });

  it("keeps API 0.1 color-only and rejects fields the runtime rejects", () => {
    expect(check({ apiVersion: "0.1" }).status).toBe("passed");
    expect(check({ apiVersion: "0.1", presentation: { journalHeader: "editorial" } }).errors)
      .toEqual(expect.arrayContaining([expect.objectContaining({ code: "theme.presentation-api" })]));
    expect(check({ presentation: { rawCss: "body { display: none }" } }).errors)
      .toEqual(expect.arrayContaining([expect.objectContaining({ code: "theme.presentation-field" })]));
    expect(check({ arbitraryRootField: true }).errors)
      .toEqual(expect.arrayContaining([expect.objectContaining({ code: "theme.field" })]));
  });
});
