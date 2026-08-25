// Favorites arrangement in the sidebar: the tree lives in a real page, but this
// asserts the sidebar half — that labels render at their depth, collapse, and
// rename, and that deleting a label keeps what it held (the Capacities
// contract; Obsidian's bookmark folders lose the reference instead).
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { favorites, favoritesLayout, seedFavorites, setFavorites, setRecentPages } from "../ui";
import { openJournals } from "../router";
import { layoutFromBlocks, layoutToMarkdown } from "../favoritesLayout";
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
  const all = () => [...root.querySelectorAll<HTMLElement>("#sidebar-favorites-list .nav-page")];
  return {
    root,
    dispose,
    rows: all,
    // The star is a Twemoji <img> (WebKitGTK crashes painting a raw colour
    // emoji, #29), so it contributes no text. The collapse caret does, so a row
    // that holds something would otherwise read as "▸ Alpha".
    names: () =>
      all()
        .filter((r) => !r.classList.contains("nav-fav-group"))
        .map((r) => r.textContent!.replace("▸", "").trim()),
    indents: () => all().map((r) => r.style.paddingLeft),
    groups: () => [...root.querySelectorAll<HTMLInputElement>(".nav-fav-group-name")],
  };
}

describe("favorites arrangement in the sidebar", () => {
  it("renders a flat list until a label exists", () => {
    const { names, groups, indents, dispose } = mountGrouped();
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(groups()).toHaveLength(0);
    expect(new Set(indents())).toEqual(new Set(["6px"]));
    dispose();
  });

  it("adds a label from the sidebar and renders what it holds one level in", async () => {
    const { root, names, groups, indents, dispose } = mountGrouped();
    root.querySelector<HTMLButtonElement>(".nav-fav-add-group")!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(groups().map((g) => g.value)).toEqual(["New group"]);

    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]"), block("[[Gamma]]")])])
    );
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    // Alpha, Work at the top level; Beta and Gamma one indent step in.
    expect(indents()).toEqual(["6px", "6px", "22px", "22px"]);
    // Membership follows the arrangement's display order.
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);
    dispose();
  });

  // The reason arbitrary depth is worth having at all: it is the same row
  // renderer, one step further right.
  it("renders nesting to any depth", async () => {
    const { names, indents, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([
        block("Work", [block("[[Alpha]]"), block("Deep", [block("[[Beta]]", [block("[[Gamma]]")])])]),
      ])
    );
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(indents()).toEqual(["6px", "22px", "22px", "38px", "54px"]);
    dispose();
  });

  it("collapses a row without unfavoriting what it hides", async () => {
    const { root, rows, names, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]")])])
    );
    expect(rows()).toHaveLength(3);

    root.querySelector<HTMLButtonElement>(".nav-fav-group-toggle")!.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(names()).toEqual(["Alpha"]);
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta"]);
    dispose();
  });

  // A favorite with children is a row like any other, so it collapses too.
  it("offers a caret on a favorite that holds something, and none on a leaf", async () => {
    const { root, names, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]", [block("[[Beta]]")]), block("[[Gamma]]")])
    );
    const carets = [...root.querySelectorAll("#sidebar-favorites-list .nav-fav-group-toggle")];
    expect(carets).toHaveLength(1);
    (carets[0] as HTMLButtonElement).click();
    await new Promise((r) => setTimeout(r, 0));
    expect(names()).toEqual(["Alpha", "Gamma"]);
    dispose();
  });

  it("renames a label in place", async () => {
    const { groups, dispose } = mountGrouped();
    await persistFavoritesLayout(layoutFromBlocks([block("Work", [block("[[Beta]]")])]));
    const input = groups()[0];
    input.value = "Projects";
    input.dispatchEvent(new Event("change", { bubbles: true }));
    await new Promise((r) => setTimeout(r, 0));
    expect(favoritesLayout()[0].raw).toBe("Projects");
    dispose();
  });

  it("deleting a label keeps what it held, one level up", async () => {
    const { root, names, groups, dispose } = mountGrouped();
    await persistFavoritesLayout(
      layoutFromBlocks([block("[[Alpha]]"), block("Work", [block("[[Beta]]"), block("[[Gamma]]")])])
    );
    root.querySelector<HTMLButtonElement>(".nav-fav-group-delete")!.click();
    await new Promise((r) => setTimeout(r, 0));

    expect(groups()).toHaveLength(0);
    expect(names()).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(favorites().map((f) => f.name)).toEqual(["Alpha", "Beta", "Gamma"]);
    expect(layoutToMarkdown(favoritesLayout())).toBe(
      "- [[Alpha]]\n- [[Beta]]\n- [[Gamma]]\n"
    );
    dispose();
  });
});
