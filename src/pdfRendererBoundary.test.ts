import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

const source = readFileSync("src/pdfRenderer.ts", "utf8");
const componentSource = readFileSync("src/components/PdfViewer.tsx", "utf8");
const tileSource = readFileSync("src/pdfTileRenderer.ts", "utf8");

describe("direct PDFPageView ownership boundary", () => {
  it("uses the exported page view directly instead of the full viewer", () => {
    expect(source).toContain("new PDFPageView(");
    expect(source).not.toMatch(/\bnew PDFViewer\b/);
  });

  it("does not invoke destructive shared-proxy lifecycle methods", () => {
    expect(source).not.toMatch(/\.destroy\s*\(/);
    expect(source).not.toMatch(/\.cleanup\s*\(/);
  });

  it("keeps the component behind the adapter instead of rebuilding PDF.js internals", () => {
    expect(componentSource).toContain("new PdfPageViewRenderer(");
    expect(componentSource).not.toMatch(/\bpage\.render\s*\(/);
    expect(componentSource).not.toContain("new (pdfjs as any).TextLayer");
    expect(componentSource).not.toContain("renderQueue");
  });

  it("uses clipped renderer-owned tiles and a dedicated area-capture source", () => {
    expect(tileSource).toContain("page.render({");
    expect(tileSource).toContain("-this.options.rect.left * pixelRatio");
    expect(componentSource).toContain("return renderer.captureArea(page, rect)");
    const cropFunction = componentSource.slice(
      componentSource.indexOf("async function cropArea"),
      componentSource.indexOf("const createAreaHighlightOwned"),
    );
    expect(cropFunction).not.toContain("querySelector(\"canvas\")");
    expect(cropFunction).not.toContain("drawImage(");
  });
});
