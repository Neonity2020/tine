import { describe, expect, it } from "vitest";
import { readAppStylesheet } from "./testSource";

const css = readAppStylesheet();

describe("page actions responsive discoverability", () => {
  it("keeps the ellipsis visible for keyboard focus and while its menu is open", () => {
    expect(css).toMatch(/\.page-actions-trigger:focus-visible,[\s\S]*?\.page-actions-trigger\[aria-expanded="true"\][\s\S]*?opacity:\s*1/);
  });

  it("gives coarse pointers a visible 36px target", () => {
    expect(css).toMatch(/@media \(pointer: coarse\)[\s\S]*?\.page-actions-trigger\s*\{[\s\S]*?min-width:\s*36px;[\s\S]*?min-height:\s*36px;[\s\S]*?opacity:\s*1/);
  });

  it("does not hide the ellipsis on narrow journal rows with carry actions", () => {
    expect(css).toContain(".page-title-row:has(.page-carry-actions) .fav-star");
    expect(css).not.toContain(".page-title-row:has(.page-carry-actions) .page-actions-trigger");
  });
});
