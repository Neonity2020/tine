import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = path.resolve(import.meta.dirname, "..");
const app = fs.readFileSync(path.join(root, "src/styles/app.css"), "utf8");

describe("persistent search result geometry (GH #140)", () => {
  it("allows every result-row flex item to shrink and wrap inside its pane", () => {
    const rule = app.match(/\.query-result-row\s*\{([^}]*)\}/s)?.[1] ?? "";
    expect(rule).toContain("box-sizing: border-box");
    expect(rule).toContain("max-width: 100%");
    expect(rule).toContain("min-width: 0");
    expect(rule).toContain("white-space: normal");
  });
});
