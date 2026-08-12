import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

const css = readFileSync("src/styles/app.css", "utf8");

function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`${escaped}\\s*\\{([^}]*)\\}`).exec(css)?.[1] ?? "";
}

describe("page-selection UI chrome (GH #328)", () => {
  it.each([".page-trailing-block-target", ".references-header"])(
    "keeps %s out of native text selections",
    (selector) => {
      expect(rule(selector)).toMatch(/-webkit-user-select:\s*none/);
      expect(rule(selector)).toMatch(/(?:^|;)\s*user-select:\s*none/);
    },
  );
});
