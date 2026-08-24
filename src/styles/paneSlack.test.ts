import fs from "node:fs";
import path from "node:path";
import { describe, expect, it } from "vitest";

const root = path.resolve(import.meta.dirname, "../..");
const app = fs.readFileSync(path.join(root, "src/styles/app.css"), "utf8");

function ruleBody(selectorPattern: RegExp): string {
  return app.match(selectorPattern)?.[1] ?? "";
}

// GH #369: the end-of-page editing slack used to be `padding-bottom: 40vh` on
// .main-content-inner — 40% of the WINDOW inside every pane scroller. In a
// split pane a fraction of the window tall, content + 40vh still overflowed
// the pane, so short panes whose content fit showed a useless vertical
// scrollbar (the reporter's "dashboard"). The slack is now a flex spacer
// inside the pane's own scroller, so it measures against the pane, not the
// window: fitting panes show no bar, long panes keep their breathing room.
describe("pane end-of-page slack is pane-relative (GH #369)", () => {
  it("sizes the editing slack from the pane's own scroller, not the viewport (vh)", () => {
    // No viewport-relative vertical slack may be declared for the page column
    // anywhere: not on the inner wrapper, not in the mobile media override.
    const appNoComments = app.replace(/\/\*[^]*?\*\//g, "");
    expect(appNoComments).not.toMatch(/\.main-content-inner\s*\{[^}]*\b\d+vh\b/);
    expect(appNoComments).not.toMatch(/\.main-content-inner\b[^}]*\bpadding[^;]*vh\b/);
  });

  it("makes the pane scroller a column flex context and keeps it scrolling", () => {
    const rule = ruleBody(/^\.main-content\s*\{([^}]*)\}/m);
    expect(rule).toContain("display: flex");
    expect(rule).toContain("flex-direction: column");
    expect(rule).toMatch(/overflow-y:\s*auto/);
  });

  it("keeps the page column at natural height so only real overflow can scroll", () => {
    const inner = ruleBody(/^\.main-content-inner\s*\{([^}]*)\}/m);
    // As a flex item of the scroller, the column must never be compressed:
    // shrinking it would fake "content fits" while clipping the page tail.
    expect(inner).toContain("flex: 0 0 auto");
  });

  it("declares a spacer that absorbs free space down to 40% of the pane", () => {
    const spacer = ruleBody(/^\.main-content::after\s*\{([^}]*)\}/m);
    // grow to fill leftover pane space (no range, no scrollbar on fitting
    // content), never shrink below 40% of the pane (long pages keep slack).
    expect(spacer).toContain('content: ""');
    expect(spacer).toMatch(/flex:\s*1\s+0\s+40%/);
  });
});
