import { describe, expect, it, vi } from "vitest";
import { graphRowMenuActions, openKnownGraph, openSidebarPageTarget, type GraphRowMenuDeps, type KnownGraphOpenDeps, type SidebarPageOpenDeps } from "./Sidebar";
import { favorites, isFavorite, pageIdentityKey, setAliasMap, setFavorites, toggleFavorite } from "../ui";

describe("known graph open gesture", () => {
  it("uses an in-place switch for an ordinary click", async () => {
    const deps: KnownGraphOpenDeps = {
      switchInPlace: vi.fn().mockResolvedValue(undefined),
      openNewWindow: vi.fn().mockResolvedValue(undefined),
    };
    await openKnownGraph("/graphs/a", false, deps);
    expect(deps.switchInPlace).toHaveBeenCalledWith("/graphs/a");
    expect(deps.openNewWindow).not.toHaveBeenCalled();
  });

  it("opens a new OS window for shift-click", async () => {
    const deps: KnownGraphOpenDeps = {
      switchInPlace: vi.fn().mockResolvedValue(undefined),
      openNewWindow: vi.fn().mockResolvedValue({ kind: "loaded" }),
    };
    await openKnownGraph("/graphs/b", true, deps);
    expect(deps.openNewWindow).toHaveBeenCalledWith("/graphs/b");
    expect(deps.switchInPlace).not.toHaveBeenCalled();
  });
});

describe("graph switcher row context menu", () => {
  const graph = { path: "/graphs/a", name: "a" };
  const deps = (over: Partial<GraphRowMenuDeps> = {}): GraphRowMenuDeps => ({
    openKnown: vi.fn().mockResolvedValue({ kind: "loaded" as const }),
    reveal: vi.fn().mockResolvedValue(undefined),
    copyPath: vi.fn().mockResolvedValue(undefined),
    forget: vi.fn().mockResolvedValue(undefined),
    desktop: true,
    isCurrent: false,
    ...over,
  });

  it("offers the full desktop set for a graph this window does not have open", () => {
    const items = graphRowMenuActions(graph, deps());
    expect(items.map((item) => item.label)).toEqual([
      "Open in a new window",
      "Open here",
      "Show in folder",
      "Copy path",
      "Remove from this list",
    ]);
    expect(items.some((item) => item.disabled)).toBe(false);
  });

  it("routes each action to its own handler with this row's path", async () => {
    const d = deps();
    const items = graphRowMenuActions(graph, d);
    const run = (label: string) => items.find((item) => item.label === label)?.run?.();

    run("Open in a new window");
    run("Open here");
    run("Show in folder");
    run("Copy path");
    run("Remove from this list");
    await Promise.resolve();

    expect(d.openKnown).toHaveBeenNthCalledWith(1, "/graphs/a", true);
    expect(d.openKnown).toHaveBeenNthCalledWith(2, "/graphs/a", false);
    expect(d.reveal).toHaveBeenCalledWith("/graphs/a");
    expect(d.copyPath).toHaveBeenCalledWith("/graphs/a");
    expect(d.forget).toHaveBeenCalledWith("/graphs/a");
  });

  it("marks removal as destructive so it cannot be mistaken for a navigation", () => {
    const items = graphRowMenuActions(graph, deps());
    expect(items.find((item) => item.label === "Remove from this list")?.danger).toBe(true);
    expect(items.filter((item) => item.danger)).toHaveLength(1);
  });

  it("keeps both open actions visible but inert for the current graph, saying why", () => {
    const d = deps({ isCurrent: true });
    const items = graphRowMenuActions(graph, d);
    expect(items.map((item) => item.label)).toEqual([
      "Open in a new window (already open here)",
      "Open here (current graph)",
      "Show in folder",
      "Copy path",
      "Remove from this list",
    ]);
    expect(items[0].disabled).toBe(true);
    expect(items[1].disabled).toBe(true);
    // Still reachable: the row is in the list, so forgetting it stays available.
    expect(items[4].disabled).toBeFalsy();
  });

  it("drops the peer-window and file-manager actions on mobile, which has neither", () => {
    const items = graphRowMenuActions(graph, deps({ desktop: false }));
    expect(items.map((item) => item.label)).toEqual([
      "Open here",
      "Copy path",
      "Remove from this list",
    ]);
  });
});

describe("favorite alias navigation", () => {
  it("resolves the canonical page for normal, sidebar, new-tab, and context gestures", () => {
    setAliasMap({ shortcut: "Canonical" });
    const deps: SidebarPageOpenDeps = {
      normal: vi.fn(),
      sidebar: vi.fn(),
      newTab: vi.fn(),
      context: vi.fn(),
    };

    openSidebarPageTarget("Shortcut", "page", "normal", undefined, deps);
    expect(deps.normal).toHaveBeenCalledWith("Canonical", "page");
    openSidebarPageTarget("Shortcut", "page", "sidebar", undefined, deps);
    expect(deps.sidebar).toHaveBeenCalledWith("Canonical", "page");
    openSidebarPageTarget("Shortcut", "page", "new-tab", undefined, deps);
    expect(deps.newTab).toHaveBeenCalledWith("Canonical", "page");
    openSidebarPageTarget("Shortcut", "page", "context", { x: 12, y: 34 }, deps);
    expect(deps.context).toHaveBeenCalledWith(12, 34, "Canonical", "page");

    setFavorites([{ name: "Shortcut", kind: "page" }]);
    expect(isFavorite("Canonical")).toBe(true);
    toggleFavorite("Canonical", "page");
    expect(favorites()).toEqual([]);

    setAliasMap({});
  });

  it("resolves mixed-case real-page identities across every sidebar gesture", () => {
    setAliasMap({ page1: "page1" });
    const deps: SidebarPageOpenDeps = {
      normal: vi.fn(),
      sidebar: vi.fn(),
      newTab: vi.fn(),
      context: vi.fn(),
    };

    openSidebarPageTarget("Page1", "page", "normal", undefined, deps);
    openSidebarPageTarget("PAGE1", "page", "sidebar", undefined, deps);
    openSidebarPageTarget("pAgE1", "page", "new-tab", undefined, deps);
    openSidebarPageTarget("PaGe1", "page", "context", { x: 4, y: 8 }, deps);

    expect(deps.normal).toHaveBeenCalledWith("page1", "page");
    expect(deps.sidebar).toHaveBeenCalledWith("page1", "page");
    expect(deps.newTab).toHaveBeenCalledWith("page1", "page");
    expect(deps.context).toHaveBeenCalledWith(4, 8, "page1", "page");
    setAliasMap({});
  });

  it("uses the same contextual Unicode lowercase key as core refs::page_key", () => {
    expect(pageIdentityKey(" ΟΣ ")).toBe("ος");
    expect(pageIdentityKey("/Cafe\u{301}/")).toBe("café");
    setAliasMap({ ος: "ΟΣ" });
    const normal = vi.fn();
    openSidebarPageTarget("ΟΣ", "page", "normal", undefined, {
      normal,
      sidebar: vi.fn(),
      newTab: vi.fn(),
      context: vi.fn(),
    });
    expect(normal).toHaveBeenCalledWith("ΟΣ", "page");
    setAliasMap({});
  });
});
