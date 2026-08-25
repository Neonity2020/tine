import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { layoutFromBlocks } from "./favoritesLayout";
import {
  addGroup,
  adoptExternalMembership,
  deleteGroup,
  locateRow,
  moveFavoriteRow,
  renameGroup,
  setGroupCollapsed,
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
  vi.restoreAllMocks();
});

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
    expect(storedFavoritesLayout().map((g) => g.name)).toEqual([null, "Work"]);
    expect(storedFavoritesLayout()[0].items.map((i) => i.name)).toEqual(["Alpha", "Fresh"]);
    expect(storedFavoritesLayout()[1].items.map((i) => i.name)).toEqual(["Beta"]);
  });

  it("keeps favorites when the arrangement page cannot be read", async () => {
    vi.spyOn(backend(), "getPage").mockRejectedValue(new Error("gone"));
    await loadFavoritesLayout(["Alpha", "Beta"], "Favorites");
    expect(storedFavoritesLayout()[0].items.map((i) => i.name)).toEqual(["Alpha", "Beta"]);
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

    expect(storedFavoritesLayout()[0].items.map((i) => i.name)).toEqual(["AddedInLogseq"]);
    expect(storedFavoritesLayout()[1].items.map((i) => i.name)).toEqual(["Alpha"]);
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

  it("locates a global row index inside its group", () => {
    const l = arranged();
    expect(locateRow(l, 0)).toEqual({ group: 0, index: 0 });
    expect(locateRow(l, 1)).toEqual({ group: 1, index: 0 });
    expect(locateRow(l, 2)).toEqual({ group: 1, index: 1 });
    expect(locateRow(l, 3)).toBeNull();
  });

  // `to` is a global row index in the array AFTER removal — the same contract
  // rowReorder's commit(from, to) already uses.
  it("moves a favorite into a group, at the drop position", () => {
    const head = moveFavoriteRow(arranged(), 0, 0, 1);
    expect(head[0].items).toEqual([]);
    expect(head[1].items.map((i) => i.name)).toEqual(["Alpha", "Beta", "Gamma"]);

    const middle = moveFavoriteRow(arranged(), 0, 1, 1);
    expect(middle[1].items.map((i) => i.name)).toEqual(["Beta", "Alpha", "Gamma"]);
  });

  it("appends to the target group when the drop index lands outside it", () => {
    const next = moveFavoriteRow(arranged(), 0, 99, 1);
    expect(next[1].items.map((i) => i.name)).toEqual(["Beta", "Gamma", "Alpha"]);
  });

  it("moves a favorite out of a group back to ungrouped", () => {
    const next = moveFavoriteRow(arranged(), 1, 0, 0);
    expect(next[0].items.map((i) => i.name)).toEqual(["Beta", "Alpha"]);
    expect(next[1].items.map((i) => i.name)).toEqual(["Gamma"]);
  });

  it("reorders within a group", () => {
    const next = moveFavoriteRow(arranged(), 2, 1, 1);
    expect(next[1].items.map((i) => i.name)).toEqual(["Gamma", "Beta"]);
  });

  it("adds groups without colliding", () => {
    const next = addGroup(addGroup(arranged(), "Work"), "Work");
    expect(next.map((g) => g.name)).toEqual([null, "Work", "Work 2", "Work 3"]);
  });

  it("renames a group and refuses an empty name", () => {
    expect(renameGroup(arranged(), 1, "Projects")[1].name).toBe("Projects");
    expect(renameGroup(arranged(), 1, "   ")[1].name).toBe("Work");
    // The ungrouped section is not a group and cannot be renamed.
    expect(renameGroup(arranged(), 0, "Nope")[0].name).toBeNull();
  });

  // The contract worth stealing from Capacities, and the one Obsidian's
  // bookmark folders get wrong: deleting a group must not unfavorite anything.
  it("deletes a group without losing its favorites", () => {
    const next = deleteGroup(arranged(), 1);
    expect(next.map((g) => g.name)).toEqual([null]);
    expect(next[0].items.map((i) => i.name)).toEqual(["Alpha", "Beta", "Gamma"]);
  });

  it("collapses and expands a group", () => {
    expect(setGroupCollapsed(arranged(), 1, true)[1].collapsed).toBe(true);
    expect(setGroupCollapsed(arranged(), 1, false)[1].collapsed).toBeUndefined();
  });
});
