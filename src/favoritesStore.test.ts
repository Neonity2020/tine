import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { layoutFromBlocks } from "./favoritesLayout";
import {
  adoptExternalMembership,
  favoritesLayout,
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
    expect(favoritesLayout().map((g) => g.name)).toEqual([null, "Work"]);
    expect(favoritesLayout()[0].items.map((i) => i.name)).toEqual(["Alpha", "Fresh"]);
    expect(favoritesLayout()[1].items.map((i) => i.name)).toEqual(["Beta"]);
  });

  it("keeps favorites when the arrangement page cannot be read", async () => {
    vi.spyOn(backend(), "getPage").mockRejectedValue(new Error("gone"));
    await loadFavoritesLayout(["Alpha", "Beta"], "Favorites");
    expect(favoritesLayout()[0].items.map((i) => i.name)).toEqual(["Alpha", "Beta"]);
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

    expect(favoritesLayout()[0].items.map((i) => i.name)).toEqual(["AddedInLogseq"]);
    expect(favoritesLayout()[1].items.map((i) => i.name)).toEqual(["Alpha"]);
    expect(savePage).not.toHaveBeenCalled();
    expect(setFavorites).not.toHaveBeenCalled();
  });
});
