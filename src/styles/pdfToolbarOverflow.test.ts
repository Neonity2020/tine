import { describe, expect, it } from "vitest";
import { readAppStylesheet } from "../testSource";

const css = readAppStylesheet();

describe("PDF toolbar responsive overflow", () => {
  it("lets the viewer-width container control both toolbar actions and their sibling menu copies", () => {
    const viewer = css.match(/\.pdf-viewer\s*\{([^}]*)\}/)?.[1] ?? "";
    expect(viewer).toMatch(/container-type:\s*inline-size/);
    expect(viewer).toMatch(/container-name:\s*pdfviewer/);

    const narrow = css.match(/@container\s+pdfviewer\s*\(max-width:\s*520px\)\s*\{([\s\S]*?)\n\}/)?.[1] ?? "";
    expect(narrow).toMatch(/\.pdf-toolbar\s+\.pdf-overflow-action\s*\{\s*display:\s*none/);
    expect(narrow).toMatch(/\.pdf-settings-overflow\s*\{\s*display:\s*grid/);
    expect(narrow).not.toMatch(/\.pdf-toolbar\s+\.pdf-settings-overflow/);
  });
});
