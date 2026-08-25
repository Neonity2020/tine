// One-level Favorites groups: the arrangement lives in a real page, but this
// asserts the sidebar half — that groups render, collapse, rename, and that
// deleting one keeps its favorites (the Capacities contract; Obsidian's
// bookmark folders lose the reference instead).
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { favorites, favoritesLayout, seedFavorites, setFavorites, setRecentPages } from "../ui";
import { openJournals } from "../router";
import { layoutFromBlocks } from "../favoritesLayout";
import { persistFavoritesLayout, resetFavoritesLayout } from "../favoritesStore";
import { Sidebar } from "./Sidebar";
import type { BlockDto } from "../types";

const block = (raw: string, children: BlockDto[] = []): BlockDto => ({
  id: raw,
  raw,
  collapsed: false,
  children,
});

afterEach(async () => {
  await new Promise((r) => setTimeout(r, 5));
  resetFavoritesLayout();
  setFavorites([]);
  setRecentPages([]);
  document.body.innerHTML = "";
  localStorage.clear();
  openJournals();
  vi.restoreAllMocks();
});

function mountGrouped() {
  vi.spyOn(backend(), "savePage").mockResolvedValue("rev");
  vi.spyOn(backend(), "setFavorites").mockResolvedValue();
  vi.spyOn(backend(), "setFavoritesPage").mockResolvedValue();
  seedFavorites(["Alpha", "Beta", "Gamma"]);
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(() => <Sidebar />, root);
  return {
    root,
    dispose,
    rows: () => [...root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")],
    // The star is a Twemoji <img> (WebKitGTK crashes painting a raw colour
    // emoji, #29), so it contributes no text — assert on the names.
    names: () =>
      [...root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")].map((r) =>
        r.textContent!.trim()
      ),
    groups: () => [...root.querySelectorAll<HTMLInputElement>(".nav-fav-group-name")],
  };
}

describe("favorites groups in the sidebar", () => {
  it("renders a flat list until a group exists", () => {
    const { names, groups, dispose } = mountGrouped();
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(groups()).toHaveLength(0);
    dispose();
  });

  it("adds a group from the sidebar and renders its members indented", async () => {
    const { root, rows, names, groups, dispose } = mountGrouped();
    root.querySelector<HTMLButtonElement>(".nav-fav-add-group")!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(groups().map((g) => g.value)).toEqual(["New group"]);

    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]"), block("[[Gamma]]")])])
    );
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(rows().slice(1).every((r) => r.classList.contains("grouped"))).toBe(true);
    // Membership follows the arrangement's display order.
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);
    dispose();
  });

  it("collapses a group without unfavoriting anything", async () => {
    const { root, rows, names, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")])])
    );
    expect(rows()).toHaveLength(2);

    root.querySelector<HTMLButtonElement>(".nav-fav-group-toggle")!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(names()).toEqual(["Alpha"]);
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta"]);
    dispose();
  });

  it("renames a group in place", async () => {
    const { groups, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("Work", [block("[[Beta]]")])])
    );
    const input = groups()[0];
    input.value = "Projects";
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(favoritesLayout().map((g) => g.name)).toEqual([null, "Projects"]);
    dispose();
  });

  it("deleting a group keeps its favorites, moving them to the ungrouped list", async () => {
    const { root, names, groups, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]"), block("[[Gamma]]")])])
    );
    root.querySelector<HTMLButtonElement>(".nav-fav-group-delete")!.click();
    await new Promise((r) => setTimeout(r, 0));

    expect(groups()).toHaveLength(0);
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);
    dispose();
  });
});
