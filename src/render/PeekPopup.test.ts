import { describe, expect, it } from "vitest";
import type { BlockDto } from "../types";
import { capBlockTree } from "./PeekPopup";

describe("PeekPopup hostile tree bounds", () => {
  it("truncates an iteratively-built tree past depth 64", () => {
    let block: BlockDto = { id: "leaf", raw: "leaf", collapsed: false, children: [] };
    for (let depth = 0; depth < 65; depth++) {
      block = { id: `depth-${depth}`, raw: `depth-${depth}`, collapsed: false, children: [block] };
    }

    const capped = capBlockTree([block], Number.MAX_SAFE_INTEGER);
    let retainedDepth = 0;
    let cursor: BlockDto | undefined = capped.blocks[0];
    while (cursor) {
      retainedDepth++;
      cursor = cursor.children[0];
    }
    expect(retainedDepth).toBe(64);
    expect(capped.truncated).toBeGreaterThan(0);
  });
});
