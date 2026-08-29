import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

/**
 * The system-inset contract for viewport-fixed overlays, checked as text.
 *
 * `scripts/check-mobile-safe-area.mjs` measures the real thing in a headless
 * browser and stays the authority; it needs a working Chromium, which not
 * every machine has. These assertions are the part that can run anywhere.
 */
describe("mobile safe-area insets", () => {
  const css = readFileSync("src/styles/app.css", "utf8");
  const main = readFileSync("src/main.tsx", "utf8");

  it("centralizes each platform inset behind one system token", () => {
    for (const side of ["top", "right", "bottom", "left"]) {
      expect(css).toContain(`--system-inset-${side}: env(safe-area-inset-${side}, 0px);`);
      expect(css).toContain(`--overlay-inset-${side}: var(--system-inset-${side});`);
    }
    expect(css.match(/env\(safe-area-inset-(?:top|right|bottom|left)(?:, 0px)?\)/gu)).toHaveLength(4);
  });

  it("gives Android's already-inset native viewport sole ownership", () => {
    const installation = "installSystemInsetOwner();";
    expect(main).toContain(installation);
    expect(main.indexOf(installation)).toBeLessThan(main.indexOf("applyTheme();"));
    expect(css).toContain('html[data-system-insets="native-viewport"]');
    for (const side of ["top", "right", "bottom", "left"]) {
      expect(css).toContain(`--system-inset-${side}: 0px;`);
    }
  });

  it("puts a phone-width modal's top edge on the inset and not below it", () => {
    // Martin, Aug 18: with `+ 2vh` the sheet sat noticeably under the status
    // bar and a lit strip of the workspace showed through the scrim above it.
    // A desktop-shaped breathing gap is wrong for a full-height sheet.
    const phone = css.slice(css.indexOf("@media (max-width: 480px)"));
    const overlay = phone.slice(phone.indexOf(".modal-overlay"), phone.indexOf(".settings-modal"));
    expect(overlay).toContain("padding-top: var(--overlay-inset-top);");
    expect(overlay).not.toMatch(/padding-top:\s*calc\(/u);
  });
});
