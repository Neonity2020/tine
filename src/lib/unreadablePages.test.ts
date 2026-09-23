import { describe, expect, it } from "vitest";
import { unreadablePagesMessage } from "./unreadablePages";

describe("unreadablePagesMessage", () => {
  it("names the page and what fixes it", () => {
    const message = unreadablePagesMessage(["pages/Broken.md"]);
    expect(message).toContain("pages/Broken.md");
    expect(message).toContain("Fix or restore the file");
    expect(message).not.toContain("retrying");
  });

  it("names the first pages of many and counts the rest", () => {
    const message = unreadablePagesMessage(["a.md", "b.md", "c.md", "d.md", "e.md"]);
    expect(message).toContain("a.md, b.md, c.md and 2 more");
    expect(message).not.toContain("d.md");
  });
});
