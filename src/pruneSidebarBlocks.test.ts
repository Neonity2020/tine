import { afterEach, describe, expect, it } from "vitest";
import { __setBackendForTest, QueryNotReadyError, type Backend } from "./backend";
import { bumpGraphBinding } from "./persistence";
import { pruneSidebarBlocks, rightSidebar, setRightSidebar } from "./ui";

// GH #543, audit R6-08: the launch-time prune of restored right-sidebar blocks
// persists what it removes, so only the current binding's definitive "no such
// block" may remove one.
describe("pruneSidebarBlocks", () => {
  afterEach(() => {
    __setBackendForTest(null);
    setRightSidebar([]);
  });

  const pin = (n: number) => ({
    kind: "block" as const,
    uuid: `65f0c0de-0000-4000-8000-00000000000${n}`,
    page: "Alpha",
    pageKind: "page" as const,
  });
  const pinnedBlocks = () => rightSidebar().filter((item) => item.kind === "block");

  it("keeps a restored block when resolving it is refused for indexing", async () => {
    setRightSidebar([pin(1)]);
    __setBackendForTest({
      resolveBlocks: async () => { throw new QueryNotReadyError("indexing"); },
    } as unknown as Backend);
    await pruneSidebarBlocks();
    expect(pinnedBlocks()).toHaveLength(1);
  });

  it("keeps a restored block when resolving it fails outright", async () => {
    setRightSidebar([pin(2)]);
    __setBackendForTest({
      resolveBlocks: async () => { throw new Error("stale-graph-binding"); },
    } as unknown as Backend);
    await pruneSidebarBlocks();
    expect(pinnedBlocks()).toHaveLength(1);
  });

  it("does not act on answers from a binding that has since moved", async () => {
    setRightSidebar([pin(3)]);
    __setBackendForTest({
      resolveBlocks: async (ids: string[]) => { bumpGraphBinding(); return ids.map(() => null); },
    } as unknown as Backend);
    await pruneSidebarBlocks();
    expect(pinnedBlocks()).toHaveLength(1);
  });

  it("still removes a block the current binding says is gone", async () => {
    setRightSidebar([pin(4)]);
    __setBackendForTest({ resolveBlocks: async (ids: string[]) => ids.map(() => null) } as unknown as Backend);
    await pruneSidebarBlocks();
    expect(pinnedBlocks()).toHaveLength(0);
  });
});
