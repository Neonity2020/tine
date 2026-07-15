import { describe, expect, it, vi } from "vitest";
import type { BlockDto, PageDto, PageKind, RefGroup } from "../types";
import { queryGroupsToExportNodes } from "./ExportModal";

const shallow = (id: string): BlockDto => ({
  id,
  raw: id,
  collapsed: false,
  children: [],
});

describe("query clipboard/export hydration budget", () => {
  it("counts all shallow memberships but hydrates only the first 50 result roots", async () => {
    const groups: RefGroup[] = Array.from({ length: 20_000 }, (_, index) => ({
      page: `Page ${index}`,
      kind: "page",
      blocks: [shallow(`block-${index}`)],
    }));
    const loadedPages: string[] = [];
    const loadPage = vi.fn(
      async (
        _cache: Map<string, Promise<PageDto | null>>,
        page: string,
        _kind: PageKind,
      ): Promise<PageDto | null> => {
        loadedPages.push(page);
        return null;
      },
    );

    const result = await queryGroupsToExportNodes(groups, new Map(), loadPage);

    expect(result.total).toBe(20_000);
    expect(result.shown).toBe(50);
    expect(loadPage).toHaveBeenCalledTimes(50);
    expect(loadedPages).toEqual(
      Array.from({ length: 50 }, (_, index) => `Page ${index}`),
    );
  });
});
