import { describe, expect, it, vi } from "vitest";
import type { PaneRouter, Route } from "./router";
import type { PublishedSnapshot } from "./publishedBackend";
import {
  openPublishedPermalink,
  parsePublishedPermalinkHash,
  publishedPermalinkForWorkspace,
  publishedPermalinkHash,
  publishedPermalinkUrl,
  replacePublishedPermalink,
  resolvePublishedPermalink,
} from "./publishedPermalink";

const snapshot = {
  aliases: [["Old name", "A/B #100% 日本語"]],
  pages: [
    {
      name: "A/B #100% 日本語",
      title: "A/B #100% 日本語",
      kind: "page",
      pre_block: null,
      path: "pages/a.md",
      blocks: [{
        id: "root-runtime",
        raw: "Root\nid:: root-stable",
        collapsed: false,
        children: [{ id: "child-stable", raw: "Child", collapsed: false, children: [] }],
      }],
    },
  ],
} as unknown as PublishedSnapshot;

describe("published permalinks", () => {
  it("round-trips encoded page names and graph-wide block ids", () => {
    const page = { kind: "page" as const, page: "A/B #100% 日本語" };
    expect(publishedPermalinkHash(page)).toBe("#/page/A%2FB%20%23100%25%20%E6%97%A5%E6%9C%AC%E8%AA%9E");
    expect(parsePublishedPermalinkHash(publishedPermalinkHash(page))).toEqual(page);

    const block = { kind: "block" as const, block: "11111111-2222-3333-4444-555555555555" };
    expect(parsePublishedPermalinkHash(publishedPermalinkHash(block))).toEqual(block);
  });

  it("rejects malformed or extra path segments", () => {
    expect(parsePublishedPermalinkHash("#/page/%E0%A4%A")).toBeNull();
    expect(parsePublishedPermalinkHash("#/page/A/block/B")).toBeNull();
    expect(parsePublishedPermalinkHash("#/block/a/b")).toBeNull();
    expect(parsePublishedPermalinkHash("#unrelated")).toBeNull();
  });

  it("resolves page aliases and nested blocks to their current owning page", () => {
    expect(resolvePublishedPermalink(snapshot, { kind: "page", page: "old NAME" })?.page.name)
      .toBe("A/B #100% 日本語");
    expect(resolvePublishedPermalink(snapshot, { kind: "block", block: "root-stable" }))
      .toMatchObject({ page: { name: "A/B #100% 日本語" }, block: "root-stable" });
    expect(resolvePublishedPermalink(snapshot, { kind: "block", block: "child-stable" }))
      .toMatchObject({ page: { name: "A/B #100% 日本語" }, block: "child-stable" });
    expect(resolvePublishedPermalink(snapshot, { kind: "block", block: "missing" })).toBeNull();
  });

  it("opens a resolved block through the pane router", () => {
    const route: Route = { kind: "page", name: "A/B #100% 日本語", pageKind: "page" };
    const openPageAtBlock = vi.fn();
    const router = { openPageAtBlock, route: () => route } as unknown as PaneRouter;
    expect(openPublishedPermalink(snapshot, "#/block/child-stable", router)).toEqual({
      status: "opened",
      target: { kind: "block", block: "child-stable" },
      route,
    });
    expect(openPageAtBlock).toHaveBeenCalledWith({
      name: "A/B #100% 日本語",
      pageKind: "page",
      path: "pages/a.md",
      block: "child-stable",
    });
  });

  it("updates only an unambiguous one-pane, one-tab page address", () => {
    const page: Route = { kind: "page", name: "Page", pageKind: "page" };
    expect(publishedPermalinkForWorkspace(1, 1, page)).toEqual({ kind: "page", page: "Page" });
    expect(publishedPermalinkForWorkspace(1, 1, { ...page, block: "zoom" })).toEqual({
      kind: "block",
      block: "zoom",
    });
    expect(publishedPermalinkForWorkspace(1, 1, page, "revealed")).toEqual({
      kind: "block",
      block: "revealed",
    });
    expect(publishedPermalinkForWorkspace(2, 1, page)).toBeUndefined();
    expect(publishedPermalinkForWorkspace(1, 2, page)).toBeUndefined();
    expect(publishedPermalinkForWorkspace(1, 1, { kind: "journals" })).toBeNull();
  });

  it("preserves the deployed path and query when copying or replacing", () => {
    expect(publishedPermalinkUrl(
      { kind: "page", page: "Guide" },
      "https://tine.page/guide/?preview=1#/page/Old",
    )).toBe("https://tine.page/guide/?preview=1#/page/Guide");

    const location = {
      href: "https://tine.page/guide/?preview=1#/page/Old",
      hash: "#/page/Old",
    } as Location;
    const replaceState = vi.fn((_state, _unused, href: string | URL | null | undefined) => {
      if (href) {
        location.href = String(href);
        location.hash = new URL(String(href)).hash;
      }
    });
    replacePublishedPermalink(
      { kind: "block", block: "stable" },
      { location, history: { state: { kept: true }, replaceState } as unknown as History },
    );
    expect(replaceState).toHaveBeenCalledWith(
      { kept: true },
      "",
      "https://tine.page/guide/?preview=1#/block/stable",
    );
  });
});
