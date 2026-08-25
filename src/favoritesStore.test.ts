import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { layoutFromBlocks, layoutMembers, layoutToMarkdown } from "./favoritesLayout";
import {
  addGroup,
  adoptExternalMembership,
  deleteGroup,
  favoritesPageChanged,
  moveFavoriteRow,
  renameGroup,
  setGroupCollapsed,
  setMembershipSink,
  storedFavoritesLayout,
  favoritesLayoutPage,
  loadFavoritesLayout,
  persistFavoritesLayout,
  resetFavoritesLayout,
} from "./favoritesStore";
import type { BlockDto } from "./types";

const block = (raw: string, children: BlockDto[] = []): BlockDto => ({
  id: raw,
  raw,
  collapsed: false,
  children,
});

afterEach(() => {
  resetFavoritesLayout();
  setMembershipSink(() => {});
  vi.restoreAllMocks();
});

const names = (layout: ReturnType<typeof layoutFromBlocks>) =>
  layoutMembers(layout).map((i) => i.name);

describe("favorites arrangement store", () => {
  it("loads the arrangement page and honours config.edn membership over it", async () => {
    vi.spyOn(backend(), "getPage").mockResolvedValue({
      name: "Favorites",
      kind: "page",
      title: "Favorites",
      pre_block: "tine/favorites:: true",
      rev: "r1",
      blocks: [block("[[Alpha]]"), block("Work", [block("[[Beta]]"), block("[[Stale]]")])],
    });
    // config.edn no longer lists Stale (unfavorited in Logseq) and adds Fresh.
    await loadFavoritesLayout(["Alpha", "Beta", "Fresh"], "Favorites");
    expect(layoutToMarkdown(storedFavoritesLayout())).toBe(
      "- [[Alpha]]\n- Work\n\t- [[Beta]]\n- [[Fresh]]\n"
    );
  });

  it("keeps favorites when the arrangement page cannot be read", async () => {
    vi.spyOn(backend(), "getPage").mockRejectedValue(new Error("gone"));
    await loadFavoritesLayout(["Alpha", "Beta"], "Favorites");
    expect(names(storedFavoritesLayout())).toEqual(["Alpha", "Beta"]);
  });

  // The important one: a user who never groups anything must not find a page in
  // their graph they did not ask for, and must keep exactly today's behaviour.
  it("does not create an arrangement page for a flat favorites list", async () => {
    const savePage = vi.spyOn(backend(), "savePage");
    const setFavorites = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const setFavoritesPage = vi.spyOn(backend(), "setFavoritesPage").mockResolvedValue();
    await loadFavoritesLayout(["Alpha"], null);

    await persistFavoritesLayout(layoutFromBlocks([block("[[Alpha]]"), block("[[Beta]]")]));

    expect(savePage).not.toHaveBeenCalled();
    expect(setFavoritesPage).not.toHaveBeenCalled();
    expect(setFavorites).toHaveBeenCalledWith(["Alpha", "Beta"]);
    expect(favoritesLayoutPage()).toBeNull();
  });

  it("materializes the page as soon as the arrangement carries a group", async () => {
    const savePage = vi.spyOn(backend(), "savePage").mockResolvedValue("r2");
    const setFavorites = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const setFavoritesPage = vi.spyOn(backend(), "setFavoritesPage").mockResolvedValue();
    await loadFavoritesLayout(["Alpha", "Beta"], null);

    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")])])
    );

    expect(setFavoritesPage).toHaveBeenCalledWith("Favorites");
    const [page] = savePage.mock.calls[0];
    expect(page.name).toBe("Favorites");
    expect(page.pre_block).toBe("tine/favorites:: true");
    expect(page.blocks.map((b) => b.raw)).toEqual(["[[Alpha]]", "Work"]);
    expect(page.blocks[1].children.map((b) => b.raw)).toEqual(["[[Beta]]"]);
    // Membership is still projected, flat and in display order.
    expect(setFavorites).toHaveBeenCalledWith(["Alpha", "Beta"]);
  });

  it("still projects membership when writing the arrangement page fails", async () => {
    vi.spyOn(backend(), "savePage").mockRejectedValue(new Error("conflict"));
    vi.spyOn(backend(), "setFavoritesPage").mockResolvedValue();
    const setFavorites = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    await loadFavoritesLayout(["Alpha"], null);

    await persistFavoritesLayout(
      layoutFromBlocks([block("Work", [block("[[Alpha]]")])])
    );

    // Losing an arrangement is survivable; losing the favorites is not.
    expect(setFavorites).toHaveBeenCalledWith(["Alpha"]);
  });

  it("folds an external membership change in without writing anything", async () => {
    const savePage = vi.spyOn(backend(), "savePage");
    const setFavorites = vi.spyOn(backend(), "setFavorites");
    vi.spyOn(backend(), "getPage").mockResolvedValue({
      name: "Favorites",
      kind: "page",
      title: "Favorites",
      pre_block: null,
      rev: "r1",
      blocks: [block("Work", [block("[[Alpha]]")])],
    });
    await loadFavoritesLayout(["Alpha"], "Favorites");

    adoptExternalMembership(["Alpha", "AddedInLogseq"]);

    expect(layoutToMarkdown(storedFavoritesLayout())).toBe(
      "- Work\n\t- [[Alpha]]\n- [[AddedInLogseq]]\n"
    );
    expect(savePage).not.toHaveBeenCalled();
    expect(setFavorites).not.toHaveBeenCalled();
  });
});

describe("arrangement mutations", () => {
  const arranged = () =>
    layoutFromBlocks([
      block("[[Alpha]]"),
      block("Work", [block("[[Beta]]"), block("[[Gamma]]")]),
    ]);

  it("moves a favorite into a label, at the drop position", () => {
    expect(layoutToMarkdown(moveFavoriteRow(arranged(), [0], [1], 0))).toBe(
      "- Work\n\t- [[Alpha]]\n\t- [[Beta]]\n\t- [[Gamma]]\n"
    );
    expect(layoutToMarkdown(moveFavoriteRow(arranged(), [0], [1], 1))).toBe(
      "- Work\n\t- [[Beta]]\n\t- [[Alpha]]\n\t- [[Gamma]]\n"
    );
  });

  it("appends when the drop index lands past the end", () => {
    expect(layoutToMarkdown(moveFavoriteRow(arranged(), [0], [1], 99))).toBe(
      "- Work\n\t- [[Beta]]\n\t- [[Gamma]]\n\t- [[Alpha]]\n"
    );
  });

  it("moves a favorite back out to the top level", () => {
    expect(layoutToMarkdown(moveFavoriteRow(arranged(), [1, 0], [], 0))).toBe(
      "- [[Beta]]\n- [[Alpha]]\n- Work\n\t- [[Gamma]]\n"
    );
  });

  it("adds labels without colliding", () => {
    const next = addGroup(addGroup(arranged(), "Work"), "Work");
    expect(next.filter((n) => n.target === null).map((n) => n.raw)).toEqual([
      "Work",
      "Work 2",
      "Work 3",
    ]);
  });

  it("renames a label, refuses an empty name, and leaves favorites alone", () => {
    expect(renameGroup(arranged(), [1], "Projects")[1].raw).toBe("Projects");
    expect(renameGroup(arranged(), [1], "   ")[1].raw).toBe("Work");
    // Re-committing the same name must not turn "Work" into "Work 2".
    expect(renameGroup(arranged(), [1], "Work")[1].raw).toBe("Work");
    // A favorite is a link, not a label: its text is not the user's to type.
    expect(renameGroup(arranged(), [0], "Nope")[0].raw).toBe("[[Alpha]]");
  });

  // The contract worth stealing from Capacities, and the one Obsidian's
  // bookmark folders get wrong: deleting a label must not unfavorite anything.
  it("deletes a label without losing what it held", () => {
    const next = deleteGroup(arranged(), [1]);
    expect(layoutToMarkdown(next)).toBe("- [[Alpha]]\n- [[Beta]]\n- [[Gamma]]\n");
    expect(names(next)).toEqual(["Alpha", "Beta", "Gamma"]);
  });

  it("refuses to delete a favorite through the label path", () => {
    const before = arranged();
    expect(deleteGroup(before, [0])).toBe(before);
  });

  it("collapses and expands a row", () => {
    expect(setGroupCollapsed(arranged(), [1], true)[1].collapsed).toBe(true);
    expect(setGroupCollapsed(arranged(), [1], false)[1].collapsed).toBeUndefined();
  });
});

describe("the arrangement page changing under us", () => {
  const loaded = async () => {
    vi.spyOn(backend(), "getPage").mockResolvedValue({
      name: "Favorites",
      kind: "page",
      title: "Favorites",
      pre_block: "tine/favorites:: true",
      rev: "r1",
      blocks: [block("Work", [block("[[Alpha]]")])],
    });
    await loadFavoritesLayout(["Alpha"], "Favorites");
  };

  it("adopts a hand edit to the page, and projects it into membership", async () => {
    await loaded();
    const projected: string[][] = [];
    setMembershipSink((next) => projected.push(next));
    const setFavorites = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    const savePage = vi.spyOn(backend(), "savePage");
    vi.spyOn(backend(), "getPage").mockResolvedValue({
      name: "Favorites",
      kind: "page",
      title: "Favorites",
      pre_block: "tine/favorites:: true",
      rev: "r2",
      blocks: [block("Work", [block("[[Alpha]]"), block("[[TypedByHand]]")])],
    });

    await favoritesPageChanged(["Favorites"]);

    expect(names(storedFavoritesLayout())).toEqual(["Alpha", "TypedByHand"]);
    // Editing the page IS a membership statement, so config.edn follows...
    expect(setFavorites).toHaveBeenCalledWith(["Alpha", "TypedByHand"]);
    expect(projected).toEqual([["Alpha", "TypedByHand"]]);
    // ...but the page is never written back: that would echo the user's own
    // keystrokes into the file they are typing in.
    expect(savePage).not.toHaveBeenCalled();
  });

  it("ignores a change to any other page", async () => {
    await loaded();
    const getPage = vi.spyOn(backend(), "getPage");
    await favoritesPageChanged(["Some Other Page"]);
    expect(getPage).not.toHaveBeenCalled();
  });

  it("ignores the echo of Tine's own write", async () => {
    await loaded();
    const setFavorites = vi.spyOn(backend(), "setFavorites").mockResolvedValue();
    vi.spyOn(backend(), "getPage").mockResolvedValue({
      name: "Favorites",
      kind: "page",
      title: "Favorites",
      pre_block: "tine/favorites:: true",
      rev: "r1", // the revision the store already holds
      blocks: [block("Work", [block("[[Anything]]")])],
    });

    await favoritesPageChanged(["Favorites"]);

    expect(names(storedFavoritesLayout())).toEqual(["Alpha"]);
    expect(setFavorites).not.toHaveBeenCalled();
  });

  it("does nothing when this graph has no arrangement page", async () => {
    await loadFavoritesLayout(["Alpha"], null);
    const getPage = vi.spyOn(backend(), "getPage");
    await favoritesPageChanged(["Favorites"]);
    expect(getPage).not.toHaveBeenCalled();
  });
});
