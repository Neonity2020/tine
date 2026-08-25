import { describe, expect, it } from "vitest";
import type { BlockDto } from "./types";
import {
  emptyLayout,
  layoutFromBlocks,
  layoutMembers,
  layoutToMarkdown,
  linkOnlyTarget,
  reconcileLayout,
  uniqueGroupName,
} from "./favoritesLayout";

const block = (raw: string, children: BlockDto[] = [], collapsed = false): BlockDto => ({
  id: raw,
  raw,
  collapsed,
  children,
});

describe("favorites layout — page round trip", () => {
  it("reads ungrouped favorites and one level of named groups", () => {
    const layout = layoutFromBlocks([
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]"), block("[[Gamma]]")], true),
      block("Reading", [block("[[Delta]]")]),
    ]);
    expect(layout.map((g) => g.name)).toEqual([null, "Work", "Reading"]);
    expect(layout[0].items.map((i) => i.name)).toEqual(["Alpha"]);
    expect(layout[1].items.map((i) => i.name)).toEqual(["Beta", "Gamma"]);
    expect(layout[1].collapsed).toBe(true);
    expect(layoutMembers(layout).map((i) => i.name)).toEqual([
      "Alpha",
      "Beta",
      "Gamma",
      "Delta",
    ]);
  });

  it("round-trips through markdown unchanged", () => {
    const source = [
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]")]),
    ];
    const markdown = layoutToMarkdown(layoutFromBlocks(source));
    expect(markdown).toBe("- [[Alpha]]\n- Work\n\t- [[Beta]]\n");
  });

  it("classifies a journal title as a journal favorite", () => {
    const layout = layoutFromBlocks([block("[[Jun 29th, 2026]]")]);
    expect(layout[0].items).toEqual([{ name: "Jun 29th, 2026", kind: "journal" }]);
  });

  it("treats only a bullet that is NOTHING but a link as a favorite", () => {
    expect(linkOnlyTarget("[[Alpha]]")).toBe("Alpha");
    expect(linkOnlyTarget("  [[Alpha]]  ")).toBe("Alpha");
    expect(linkOnlyTarget("see [[Alpha]]")).toBeNull();
    expect(linkOnlyTarget("[[Alpha]] and [[Beta]]")).toBeNull();
    expect(linkOnlyTarget("Work")).toBeNull();
    expect(linkOnlyTarget("[[]]")).toBeNull();
  });

  // This is a real page and the user may type in it. Anything we do not
  // understand is preserved verbatim rather than silently deleted on the next
  // arrangement change.
  it("preserves unrecognized content instead of dropping it", () => {
    const layout = layoutFromBlocks([
      block("Work", [block("[[Beta]]"), block("a note to self")]),
    ]);
    expect(layout[1].passthrough).toEqual(["a note to self"]);
    expect(layoutToMarkdown(layout)).toBe("- Work\n\t- [[Beta]]\n\t- a note to self\n");
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
    expect(next[1].items.map((i) => i.name)).toEqual(["Beta"]);
    expect(next[0].items.map((i) => i.name)).toEqual(["Alpha"]);
  });

  it("drops a favorite that membership no longer has, from inside its group", () => {
    const next = reconcileLayout(arranged(), ["Alpha"]);
    expect(layoutMembers(next).map((i) => i.name)).toEqual(["Alpha"]);
    expect(next[1].name).toBe("Work");
    expect(next[1].items).toEqual([]);
  });

  it("appends unknown membership to the ungrouped section, in arrival order", () => {
    const next = reconcileLayout(arranged(), ["Alpha", "Beta", "New A", "New B"]);
    expect(next[0].items.map((i) => i.name)).toEqual(["Alpha", "New A", "New B"]);
  });

  it("never resurrects an item membership dropped, even if it reappears later", () => {
    const removed = reconcileLayout(arranged(), ["Alpha"]);
    const readded = reconcileLayout(removed, ["Alpha", "Beta"]);
    // Back at the END of ungrouped, not silently restored into "Work".
    expect(readded[0].items.map((i) => i.name)).toEqual(["Alpha", "Beta"]);
    expect(readded[1].items).toEqual([]);
  });

  it("folds case when matching membership and de-duplicates", () => {
    const next = reconcileLayout(arranged(), ["alpha", "ALPHA", "Beta"]);
    expect(layoutMembers(next).map((i) => i.name)).toEqual(["Alpha", "Beta"]);
  });

  it("survives a layout with no ungrouped section", () => {
    const next = reconcileLayout([{ name: "Work", items: [], passthrough: [] }], ["Solo"]);
    expect(next[0].name).toBeNull();
    expect(next[0].items.map((i) => i.name)).toEqual(["Solo"]);
  });
});

describe("favorites layout — group names", () => {
  it("does not collide with an existing group", () => {
    const layout = layoutFromBlocks([block("Work"), block("Work 2")]);
    expect(uniqueGroupName(layout, "Reading")).toBe("Reading");
    expect(uniqueGroupName(layout, "Work")).toBe("Work 3");
  });
});
