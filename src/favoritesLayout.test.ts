import { describe, expect, it } from "vitest";
import type { BlockDto } from "./types";
import {
  depthRange,
  emptyLayout,
  layoutFromBlocks,
  layoutMembers,
  layoutToMarkdown,
  linkOnlyTarget,
  moveNode,
  nodeAt,
  promoteChildrenAt,
  reconcileLayout,
  resolveDrop,
  uniqueGroupName,
  visibleRows,
} from "./favoritesLayout";

const block = (raw: string, children: BlockDto[] = [], collapsed = false): BlockDto => ({
  id: raw,
  raw,
  collapsed,
  children,
});

const names = (layout: ReturnType<typeof layoutFromBlocks>) =>
  layoutMembers(layout).map((i) => i.name);

describe("favorites layout — page round trip", () => {
  it("reads favorites, labels and nesting from the page's own block tree", () => {
    const layout = layoutFromBlocks([
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]"), block("[[Gamma]]")], true),
      block("Reading", [block("[[Delta]]")]),
    ]);
    expect(layout.map((n) => n.raw)).toEqual(["[[Alpha]]", "Work", "Reading"]);
    expect(layout[0].target).toBe("Alpha");
    expect(layout[1].target).toBeNull();
    expect(layout[1].collapsed).toBe(true);
    expect(names(layout)).toEqual(["Alpha", "Beta", "Gamma", "Delta"]);
  });

  // The one-level version dropped everything below the second level and
  // re-emitted a favorite's own children as unrelated top-level bullets. Depth
  // is free here because a node is a node.
  it("preserves arbitrary depth, including favorites nested under favorites", () => {
    const source = [
      block("Work", [
        block("[[Beta]]"),
        block("Active", [block("[[Gamma]]", [block("[[Delta]]")])]),
      ]),
    ];
    const layout = layoutFromBlocks(source);
    expect(names(layout)).toEqual(["Beta", "Gamma", "Delta"]);
    expect(layoutToMarkdown(layout)).toBe(
      "- Work\n\t- [[Beta]]\n\t- Active\n\t\t- [[Gamma]]\n\t\t\t- [[Delta]]\n"
    );
  });

  it("round-trips through markdown unchanged", () => {
    const markdown = layoutToMarkdown(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")])])
    );
    expect(markdown).toBe("- [[Alpha]]\n- Work\n\t- [[Beta]]\n");
  });

  it("classifies a journal title as a journal favorite", () => {
    const layout = layoutFromBlocks([block("[[Jun 29th, 2026]]")]);
    expect(layoutMembers(layout)).toEqual([{ name: "Jun 29th, 2026", kind: "journal" }]);
  });

  it("treats only a bullet that is NOTHING but a link as a favorite", () => {
    expect(linkOnlyTarget("[[Alpha]]")).toBe("Alpha");
    expect(linkOnlyTarget("  [[Alpha]]  ")).toBe("Alpha");
    expect(linkOnlyTarget("see [[Alpha]]")).toBeNull();
    expect(linkOnlyTarget("[[Alpha]] and [[Beta]]")).toBeNull();
    expect(linkOnlyTarget("Work")).toBeNull();
    expect(linkOnlyTarget("[[]]")).toBeNull();
  });

  // This is a real page and the user may type in it. A bullet we do not
  // recognize is just a label with no children — preserved, at its depth.
  it("preserves unrecognized content, with its own subtree, instead of dropping it", () => {
    const layout = layoutFromBlocks([
      block("Work", [block("[[Beta]]"), block("a note to self", [block("and a detail")])]),
    ]);
    expect(layoutToMarkdown(layout)).toBe(
      "- Work\n\t- [[Beta]]\n\t- a note to self\n\t\t- and a detail\n"
    );
  });

  it("drops a blank bullet but never what it held", () => {
    const layout = layoutFromBlocks([block("", [block("[[Alpha]]")])]);
    expect(layoutToMarkdown(layout)).toBe("- [[Alpha]]\n");
  });

  it("emits nothing for an empty layout rather than a stray bullet", () => {
    expect(layoutToMarkdown(emptyLayout())).toBe("");
  });
});

describe("favorites layout — reconciling with external membership", () => {
  const arranged = () =>
    layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")])]);

  it("keeps arranged items where the user put them", () => {
    const next = reconcileLayout(arranged(), ["Alpha", "Beta"]);
    expect(layoutToMarkdown(next)).toBe("- [[Alpha]]\n- Work\n\t- [[Beta]]\n");
  });

  it("drops a favorite membership no longer has, from inside its label", () => {
    const next = reconcileLayout(arranged(), ["Alpha"]);
    expect(names(next)).toEqual(["Alpha"]);
    expect(layoutToMarkdown(next)).toBe("- [[Alpha]]\n- Work\n");
  });

  // Unstarring a parent must not silently unstar everything beneath it: the
  // children are favorites in their own right and membership still has them.
  it("promotes the children of a removed favorite rather than losing them", () => {
    const layout = layoutFromBlocks([block("[[Alpha]]", [block("[[Beta]]")])]);
    const next = reconcileLayout(layout, ["Beta"]);
    expect(names(next)).toEqual(["Beta"]);
    expect(layoutToMarkdown(next)).toBe("- [[Beta]]\n");
  });

  it("keeps labels, which are the user's text and not membership", () => {
    const next = reconcileLayout(arranged(), []);
    expect(layoutToMarkdown(next)).toBe("- Work\n");
  });

  it("appends unknown membership at the top level, in arrival order", () => {
    const next = reconcileLayout(arranged(), ["Alpha", "Beta", "New A", "New B"]);
    expect(names(next)).toEqual(["Alpha", "Beta", "New A", "New B"]);
    expect(next[next.length - 1].target).toBe("New B");
  });

  it("never resurrects an item membership dropped, even if it reappears later", () => {
    const removed = reconcileLayout(arranged(), ["Alpha"]);
    const readded = reconcileLayout(removed, ["Alpha", "Beta"]);
    // Back at the END, not silently restored into "Work".
    expect(layoutToMarkdown(readded)).toBe("- [[Alpha]]\n- Work\n- [[Beta]]\n");
  });

  it("folds case when matching membership and de-duplicates", () => {
    const next = reconcileLayout(arranged(), ["alpha", "ALPHA", "Beta"]);
    expect(names(next)).toEqual(["Alpha", "Beta"]);
  });
});

describe("favorites layout — labels", () => {
  it("does not collide with an existing label at any depth", () => {
    const layout = layoutFromBlocks([block("Work", [block("Work 2")])]);
    expect(uniqueGroupName(layout, "Reading")).toBe("Reading");
    expect(uniqueGroupName(layout, "Work")).toBe("Work 3");
  });
});

describe("favorites layout — visible rows", () => {
  const layout = () =>
    layoutFromBlocks([
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]"), block("Deep", [block("[[Gamma]]")])]),
    ]);

  it("walks pre-order and reports each row's depth and path", () => {
    expect(visibleRows(layout()).map((r) => [r.node.raw, r.depth, r.path.join(".")])).toEqual([
      ["[[Alpha]]", 0, "0"],
      ["Work", 0, "1"],
      ["[[Beta]]", 1, "1.0"],
      ["Deep", 1, "1.1"],
      ["[[Gamma]]", 2, "1.1.0"],
    ]);
  });

  it("hides what a collapsed row holds, at every depth", () => {
    const collapsed = layoutFromBlocks([
      block("Work", [block("[[Beta]]"), block("Deep", [block("[[Gamma]]")], true)]),
    ]);
    expect(visibleRows(collapsed).map((r) => r.node.raw)).toEqual([
      "Work",
      "[[Beta]]",
      "Deep",
    ]);
    // Hidden, not gone: membership still has it.
    expect(names(collapsed)).toEqual(["Beta", "Gamma"]);
  });
});

describe("favorites layout — drop placement", () => {
  //  0 [[Alpha]]
  //  1 Work
  //  2   [[Beta]]
  //  3   [[Gamma]]
  //  4 [[Delta]]
  const rows = () =>
    visibleRows(
      layoutFromBlocks([
        block("[[Alpha]]"),
        block("Work", [block("[[Beta]]"), block("[[Gamma]]")]),
        block("[[Delta]]"),
      ])
    );

  it("allows one level deeper than the row above, and no shallower than the row below", () => {
    expect(depthRange(rows(), 0)).toEqual({ min: 0, max: 0 });
    expect(depthRange(rows(), 2)).toEqual({ min: 1, max: 1 });
    // Between Gamma (depth 1) and Delta (depth 0): either level is meant.
    expect(depthRange(rows(), 4)).toEqual({ min: 0, max: 2 });
  });

  it("places a drop at the top level", () => {
    expect(resolveDrop(rows(), 4, 0)).toEqual({ parent: [], index: 2 });
  });

  it("places a drop INTO the label above it", () => {
    expect(resolveDrop(rows(), 4, 1)).toEqual({ parent: [1], index: 2 });
    expect(resolveDrop(rows(), 2, 1)).toEqual({ parent: [1], index: 0 });
  });

  // A pointer dragged far to the right asks for depth 9; the tree has no such
  // level, and inventing the missing parents would be a different arrangement
  // from the one the user can see.
  it("clamps to one level under the row above rather than inventing parents", () => {
    expect(resolveDrop(rows(), 1, 9)).toEqual({ parent: [0], index: 0 });
    expect(resolveDrop(rows(), 3, 9)).toEqual({ parent: [1, 0], index: 0 });
  });
});

describe("favorites layout — moving nodes", () => {
  const layout = () =>
    layoutFromBlocks([
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]"), block("[[Gamma]]")]),
    ]);

  it("moves a top-level favorite into a label", () => {
    const next = moveNode(layout(), [0], [1], 0);
    expect(layoutToMarkdown(next)).toBe("- Work\n\t- [[Alpha]]\n\t- [[Beta]]\n\t- [[Gamma]]\n");
  });

  it("moves a nested favorite back out to the top level", () => {
    const next = moveNode(layout(), [1, 0], [], 0);
    expect(layoutToMarkdown(next)).toBe("- [[Beta]]\n- [[Alpha]]\n- Work\n\t- [[Gamma]]\n");
  });

  it("carries a whole subtree with the row that is dragged", () => {
    const source = layoutFromBlocks([
      block("[[Alpha]]", [block("[[Beta]]")]),
      block("Work"),
    ]);
    const next = moveNode(source, [0], [1], 0);
    expect(layoutToMarkdown(next)).toBe("- Work\n\t- [[Alpha]]\n\t\t- [[Beta]]\n");
  });

  it("reorders within one parent, treating the index as post-removal", () => {
    const next = moveNode(layout(), [1, 0], [1], 1);
    expect(layoutToMarkdown(next)).toBe("- [[Alpha]]\n- Work\n\t- [[Gamma]]\n\t- [[Beta]]\n");
  });

  // A node cannot become its own descendant; the alternative is losing it.
  it("refuses to move a node inside itself", () => {
    const before = layout();
    expect(moveNode(before, [1], [1, 0], 0)).toBe(before);
  });

  it("expands a collapsed row it drops into, so the move stays visible", () => {
    const source = layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")], true)]);
    const next = moveNode(source, [0], [1], 0);
    expect(next[0].collapsed).toBeUndefined();
    expect(visibleRows(next).map((r) => r.node.raw)).toEqual(["Work", "[[Alpha]]", "[[Beta]]"]);
  });
});

describe("favorites layout — deleting a label", () => {
  it("keeps what it held, one level up", () => {
    const source = layoutFromBlocks([
      block("Work", [block("[[Beta]]"), block("Deep", [block("[[Gamma]]")])]),
    ]);
    const next = promoteChildrenAt(source, [0]);
    expect(layoutToMarkdown(next)).toBe("- [[Beta]]\n- Deep\n\t- [[Gamma]]\n");
    expect(nodeAt(next, [1])?.raw).toBe("Deep");
  });
});
