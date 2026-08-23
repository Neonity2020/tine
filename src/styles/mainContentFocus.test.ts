import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

const css = readFileSync("src/styles/app.css", "utf8");

function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`${escaped}\\s*\\{([^}]*)\\}`).exec(css)?.[1] ?? "";
}

// GH #345: the pane scroller is `tabindex="-1"` — it is clicked it shows the tab's pin tooltip. The component renders the element so the rule pins the attribute contract; the style rule pins the default-frame suppression. jsdom can't answer the frame question either way (no layout/focus-visible heuristics).
describe("main-content default focus frame (GH #345)", () => {
  it("suppresses the default focus outline on the pane scroller", () => {
    expect(rule(".main-content:focus")).toMatch(/outline:\s*(none|0)\b/);
  });

  it("keeps the deliberate pane-select ring intact", () => {
    expect(rule(".pane-selected")).toMatch(/outline:\s*2px solid/);
  });
});
