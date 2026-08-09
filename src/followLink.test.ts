import { afterEach, describe, expect, it, vi } from "vitest";
import { followLinkUnderCaret, openLinkUnderCaretInSidebar } from "./followLink";
import * as router from "./router";
import * as ui from "./ui";
import { backend } from "./backend";

const read = (text: string, caret: number) => () => ({ text, caret });

afterEach(() => vi.restoreAllMocks());

// GH #274. OG parity: :editor/follow-link (mod+o) and
// :editor/open-link-in-sidebar (mod+shift+o).
describe("follow the link at the caret", () => {
  it("navigates to a page ref", () => {
    const openPage = vi.spyOn(router, "openPage").mockImplementation(() => {});
    expect(followLinkUnderCaret({ read: read("see [[Some Page]] here", 8) })).toBe(true);
    expect(openPage).toHaveBeenCalledWith("Some Page");
  });

  it("navigates to a tag's page, hash stripped", () => {
    const openPage = vi.spyOn(router, "openPage").mockImplementation(() => {});
    expect(followLinkUnderCaret({ read: read("a #project note", 4) })).toBe(true);
    expect(openPage).toHaveBeenCalledWith("project");
  });

  it("opens a URL externally instead of navigating", () => {
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const openPage = vi.spyOn(router, "openPage").mockImplementation(() => {});
    expect(followLinkUnderCaret({ read: read("docs at https://example.test/a", 12) })).toBe(true);
    expect(openExternal).toHaveBeenCalledWith("https://example.test/a");
    expect(openPage).not.toHaveBeenCalled();
  });

  it("does nothing when the block holds no link", () => {
    const openPage = vi.spyOn(router, "openPage").mockImplementation(() => {});
    expect(followLinkUnderCaret({ read: read("just prose", 4) })).toBe(false);
    expect(openPage).not.toHaveBeenCalled();
  });

  it("does nothing when nothing is being edited", () => {
    expect(followLinkUnderCaret({ read: () => null })).toBe(false);
  });
});

describe("open the link at the caret in the sidebar", () => {
  it("opens a page ref in the sidebar, not in place", () => {
    const sidebar = vi.spyOn(ui, "openPageInSidebar").mockImplementation(() => {});
    const openPage = vi.spyOn(router, "openPage").mockImplementation(() => {});
    expect(openLinkUnderCaretInSidebar({ read: read("see [[Some Page]]", 8) })).toBe(true);
    expect(sidebar).toHaveBeenCalledWith("Some Page");
    expect(openPage).not.toHaveBeenCalled();
  });

  it("ignores URLs — a URL has no sidebar representation, as in OG", () => {
    const openExternal = vi.spyOn(backend(), "openExternal").mockResolvedValue(undefined);
    const sidebar = vi.spyOn(ui, "openPageInSidebar").mockImplementation(() => {});
    expect(openLinkUnderCaretInSidebar({ read: read("docs at https://example.test/a", 12) })).toBe(false);
    expect(openExternal).not.toHaveBeenCalled();
    expect(sidebar).not.toHaveBeenCalled();
  });
});
