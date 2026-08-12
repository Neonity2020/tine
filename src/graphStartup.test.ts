import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

describe("large Direct graph startup boundary (GH #266)", () => {
  it("does not synchronously await page-scanning optional startup work", () => {
    const source = readFileSync("src/graph.ts", "utf8");
    const loadBody = source.slice(
      source.indexOf("export async function loadGraphPath"),
      source.indexOf("let navigationEpoch"),
    );
    expect(loadBody).not.toContain("await ensureJournalTemplateForDay");
    expect(loadBody).not.toContain("await openConfiguredHomePage");
    expect(loadBody).toContain("void openConfiguredHomePage");
  });
});
