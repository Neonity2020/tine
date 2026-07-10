import { describe, it, expect, afterEach } from "vitest";
import { setDoc } from "../store";
import { pdfFileForPage } from "./annotation";

// pdfFileForPage reduces an hls__ page's `file-path::` to a basename. A graph
// edited on Windows can carry backslash paths, so the split must handle BOTH
// separators (gh #61) — otherwise the whole path is handed to the asset reader.
describe("pdfFileForPage", () => {
  afterEach(() => setDoc("pages", []));
  const seed = (preBlock: string) =>
    setDoc("pages", [{ name: "hls__book", preBlock, roots: [], format: "markdown" } as any]);

  it("basenames a forward-slash relative path", () => {
    seed("file-path:: ../assets/book_123.pdf");
    expect(pdfFileForPage("hls__book")).toBe("book_123.pdf");
  });

  it("basenames a Windows backslash relative path", () => {
    seed("file-path:: ..\\assets\\book_123.pdf");
    expect(pdfFileForPage("hls__book")).toBe("book_123.pdf");
  });

  it("basenames an absolute Windows path", () => {
    seed("file-path:: C:\\Users\\me\\graph\\assets\\book_123.pdf");
    expect(pdfFileForPage("hls__book")).toBe("book_123.pdf");
  });

  it("returns null when the page has no file-path", () => {
    seed("some:: other\n");
    expect(pdfFileForPage("hls__book")).toBeNull();
  });
});
