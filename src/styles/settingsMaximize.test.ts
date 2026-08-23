import { describe, expect, it } from "vitest";
import { readFileSync } from "node:fs";

const css = readFileSync("src/styles/app.css", "utf8");

function rule(selector: string): string {
  const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
  return new RegExp(`${escaped}\\s*\\{([^}]*)\\}`).exec(css)?.[1] ?? "";
}

// GH #287: a desktop-only maximized geometry for the Settings dialog — it
// fills the overlay with a small margin (not full-bleed), while narrow/mobile
// viewports keep their existing full-viewport sheet without redundant chrome.
describe("settings maximize geometry (GH #287)", () => {
  it("shrinks the overlay margins so the maximized dialog fills the viewport with a margin", () => {
    expect(rule(".modal-overlay.settings-maximized")).toMatch(/padding-top:\s*calc\(var\(--overlay-inset-top\) \+ 4vh\)/);
    expect(rule(".modal-overlay.settings-maximized")).toMatch(/padding-bottom:\s*calc\(var\(--overlay-inset-bottom\) \+ 4vh\)/);
  });

  it("lets the dialog itself fill that overlay on desktop", () => {
    const body = rule(".settings-maximized .settings-modal");
    expect(body).toMatch(/width:\s*100%/);
    expect(body).toMatch(/max-width:\s*none/);
    expect(body).toMatch(/height:\s*100%/);
    expect(body).toMatch(/max-height:\s*none/);
  });

  it("keeps the maximize control and geometry off narrow/mobile viewports", () => {
    const mobileStart = css.indexOf("@media (max-width: 480px)");
    expect(mobileStart).toBeGreaterThan(-1);
    // The desktop scope guard: maximized geometry must be media-gated so a
    // toggled-then-narrowed window can never break the mobile sheet's inset
    // padding.
    expect(css).toMatch(/@media \(min-width: 481px\)[^{]*\{\s*\.modal-overlay\.settings-maximized/);
    const mobileSlice = css.slice(mobileStart, mobileStart + 6000);
    expect(mobileSlice).toMatch(/\.settings-maximize\s*\{[^}]*display:\s*none/);
  });
});
