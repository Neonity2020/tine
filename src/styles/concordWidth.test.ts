import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = path.resolve(import.meta.dirname, "../..");
const app = fs.readFileSync(path.join(root, "src/styles/app.css"), "utf8");

function ruleBody(selectorPattern: RegExp): string {
  return app.match(selectorPattern)?.[1] ?? "";
}

describe("Concord review width", () => {
  it("uses the pane's wide content cap whenever the resolver is mounted", () => {
    const wide = ruleBody(/^\.wide-mode \.main-content-inner\s*\{([^}]*)\}/m);
    const concord = ruleBody(
      /^\.main-content-inner:has\(\.page-conflict-slot\)\s*\{([^}]*)\}/m,
    );

    expect(wide).toMatch(/max-width:\s*([^;]+);/);
    expect(concord.match(/max-width:\s*([^;]+);/)?.[1]).toBe(
      wide.match(/max-width:\s*([^;]+);/)?.[1],
    );
  });

  it("keys the width to the persistent slot so expanding the dock cannot collapse it", () => {
    expect(app).toContain(".main-content-inner:has(.page-conflict-slot)");
    expect(app).not.toContain(".main-content-inner:has(.page-conflict) {");
  });
});
