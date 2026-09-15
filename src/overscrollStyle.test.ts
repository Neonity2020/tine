import { describe, expect, it } from "vitest";
import { readAppStylesheet } from "./testSource";

const app = readAppStylesheet();

describe("application overscroll containment (GH #177)", () => {
  it("terminates every descendant scroll chain at both the viewport and app shell", () => {
    expect(app).toMatch(/html\s*,\s*body\s*\{[^}]*overscroll-behavior:\s*none/s);
    expect(app).toMatch(/\.app-container\s*\{[^}]*overscroll-behavior:\s*none/s);
  });

  it("does not blanket-disable scrolling or touch gestures on nested scroll regions", () => {
    expect(app).not.toMatch(/\*\s*\{[^}]*overscroll-behavior/s);
    expect(app).not.toMatch(/\.(?:main-content|left-sidebar-scroll|right-sidebar|pdf-scroll|sheet-scroll)\s*\{[^}]*touch-action:\s*none/s);
  });
});
