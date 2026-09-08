import { beforeEach, describe, expect, it } from "vitest";
import { buildPersistedSession, parsePersistedSession, type PersistedSession } from "./session";
import { resetPaneLayoutToSingle, restorePaneLayout, type LayoutNode } from "./panes";
import { makePdfRoute, type PaneSnapshot } from "./router";
import {
  applySidebarSession,
  favoritesSectionExpanded,
  recentSectionExpanded,
  rightSidebar,
  parseStoredSidebarItems,
  openBlockInSidebar,
  openPageInSidebar,
  recentPages,
  setRecentPages,
  setRightSidebar,
  setFavoritesSectionExpanded,
  setRecentSectionExpanded,
} from "./ui";

const journals = (): PaneSnapshot => ({
  tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }],
  activeIndex: 0,
  scrolls: [12],
});

const page = (name: string): PaneSnapshot => ({
  tabs: [{ history: [{ kind: "page", name, pageKind: "page" }], pos: 0, pinned: false }],
  activeIndex: 0,
  scrolls: [34],
});

beforeEach(() => {
  resetPaneLayoutToSingle(journals());
  applySidebarSession({});
});

describe("persisted split session", () => {
  it("round-trips only explicit PDF route fields and preserves view state", () => {
    const pdf = makePdfRoute("assets/paper.pdf", "Paper", {
      viewId: "pdf-view-stable", page: 7, scale: 1.75,
    });
    resetPaneLayoutToSingle({
      tabs: [{ history: [{ ...pdf, injected: { unsafe: true } } as typeof pdf], pos: 0, pinned: false }],
      activeIndex: 0,
    });

    const persisted = buildPersistedSession();
    expect(Object.hasOwn(persisted, "pdfTarget")).toBe(false);
    expect(persisted.tabs[0].history[0]).toEqual(pdf);
    const parsed = parsePersistedSession(JSON.stringify(persisted))!;
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual(pdf);
  });

  it("migrates a legacy dedicated PDF beside the intact desktop layout", () => {
    const raw = JSON.stringify({
      ...buildPersistedSession(),
      pdfTarget: { filename: "assets/paper.pdf", label: "Paper" },
    });

    const parsed = parsePersistedSession(raw, {
      mobile: false, legacyPdfWidth: 560, viewportWidth: 1_400,
    })!;
    expect(parsed.layout).toMatchObject({
      kind: "split", dir: "row", ratio: 0.6,
      children: [{ kind: "pane", paneId: "main" }, { kind: "pane", paneId: "pdf-migrated" }],
    });
    expect(parsed.snapshots.get("main")?.tabs).toHaveLength(1);
    expect(parsed.snapshots.get("pdf-migrated")?.tabs[0].history[0]).toMatchObject({
      kind: "pdf", filename: "assets/paper.pdf", label: "Paper",
    });
  });

  it("migrates a legacy dedicated PDF into mobile history so Back returns to its source", () => {
    const raw = JSON.stringify({
      ...buildPersistedSession(),
      pdfTarget: { filename: "assets/paper.pdf", label: "Paper" },
    });
    const parsed = parsePersistedSession(raw, { mobile: true })!;
    const tab = parsed.snapshots.get("main")!.tabs[0];
    expect(tab.history.map((route) => route.kind)).toEqual(["journals", "pdf"]);
    expect(tab.pos).toBe(1);
  });

  it("keeps a malformed PDF route as a closable error tab without discarding its pane", () => {
    const raw = JSON.stringify({
      tabs: [{
        history: [{ kind: "pdf", viewId: "bad", filename: 42, label: "Broken" }],
        pos: 0,
        pinned: false,
      }],
      activeIndex: 0,
    });
    const parsed = parsePersistedSession(raw)!;
    expect(parsed.layout).toEqual({ kind: "pane", paneId: "main" });
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toMatchObject({
      kind: "invalid", title: "Unavailable PDF",
    });
  });

  it("remints duplicate PDF view ids across the complete pane tree", () => {
    const route = { kind: "pdf", viewId: "duplicate", filename: "assets/paper.pdf", label: "Paper" };
    const snapshot = (paneId: string) => ({
      kind: "pane", paneId,
      tabs: [{ history: [route], pos: 0, pinned: false }], activeIndex: 0,
    });
    const parsed = parsePersistedSession(JSON.stringify({
      layout: {
        kind: "split", dir: "row", ratio: 0.5,
        children: [snapshot("main"), snapshot("pane-2")],
      },
      tabs: journals().tabs,
      activeIndex: 0,
    }), { mobile: false })!;
    const first = parsed.snapshots.get("main")!.tabs[0].history[0];
    const second = parsed.snapshots.get("pane-2")!.tabs[0].history[0];
    expect(first.kind === "pdf" ? first.viewId : null).toBe("duplicate");
    expect(second.kind === "pdf" ? second.viewId : null).not.toBe("duplicate");
  });

  it("copies only bounded route fields while retaining exact page ownership", () => {
    const path = "pages/client-b/Twin.md";
    const raw = JSON.stringify({
      tabs: [{
        history: [{
          kind: "page", name: "Twin", pageKind: "page", path,
          block: "11111111-1111-4111-8111-111111111111", injected: { unsafe: true },
        }],
        pos: 0,
        pinned: false,
      }],
      activeIndex: 0,
    });

    expect(parsePersistedSession(raw)?.snapshots.get("main")?.tabs[0].history[0]).toEqual({
      kind: "page", name: "Twin", pageKind: "page", path,
      block: "11111111-1111-4111-8111-111111111111",
    });
  });

  it("round-trips a two-pane layout with pane tabs and scrolls", () => {
    const root: LayoutNode = {
      kind: "split" as const,
      dir: "row" as const,
      ratio: 0.4,
      children: [
        { kind: "pane" as const, paneId: "main" },
        { kind: "pane" as const, paneId: "pane-2" },
      ],
    };
    restorePaneLayout(root, new Map([["main", journals()], ["pane-2", page("Side")]]), "pane-2");

    const raw = JSON.stringify(buildPersistedSession());
    const parsed = parsePersistedSession(raw)!;

    expect(parsed.layout).toEqual(root);
    expect(parsed.focusedPaneId).toBe("pane-2");
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual({ kind: "journals" });
    expect(parsed.snapshots.get("pane-2")?.tabs[0].history[0]).toEqual({
      kind: "page",
      name: "Side",
      pageKind: "page",
    });
    expect(parsed.snapshots.get("pane-2")?.scrolls).toEqual([34]);
  });

  it("parses a legacy flat session as a single main pane", () => {
    const legacy: PersistedSession = {
      tabs: [{ history: [{ kind: "page", name: "Legacy", pageKind: "page" }], pos: 0, pinned: true }],
      activeIndex: 0,
      scrolls: [99],
    };

    const parsed = parsePersistedSession(JSON.stringify(legacy))!;

    expect(parsed.layout).toEqual({ kind: "pane", paneId: "main" });
    expect(parsed.snapshots.get("main")?.tabs[0]).toMatchObject({
      history: [{ kind: "page", name: "Legacy", pageKind: "page" }],
      pinned: true,
    });
    expect(parsed.snapshots.get("main")?.scrolls).toEqual([99]);
  });

  it("round-trips a bounded virtual query workspace without persisting results", () => {
    const raw = JSON.stringify({
      tabs: [{
        history: [{
          kind: "query",
          id: "query-1",
          sourceKind: "search",
          source: "alpha -draft",
          presentation: "search",
        }],
        pos: 0,
        pinned: true,
      }],
      activeIndex: 0,
    });

    const parsed = parsePersistedSession(raw)!;
    expect(parsed.snapshots.get("main")?.tabs[0]).toMatchObject({
      pinned: true,
      history: [{
        kind: "query",
        id: "query-1",
        sourceKind: "search",
        source: "alpha -draft",
        presentation: "search",
      }],
    });
    expect(JSON.stringify(parsed)).not.toContain("results");
  });

  it("round-trips independent empty query routes in split panes with the chosen focused owner", () => {
    const empty = (id: string, source: string, presentation: "search" | "table"): PaneSnapshot => ({
      tabs: [{ history: [{ kind: "query", id, sourceKind: "search", source, presentation }], pos: 0, pinned: false }],
      activeIndex: 0,
    });
    const root: LayoutNode = {
      kind: "split", dir: "row", ratio: 0.5,
      children: [{ kind: "pane", paneId: "main" }, { kind: "pane", paneId: "pane-2" }],
    };
    restorePaneLayout(root, new Map([["main", empty("query-empty", "", "search")], ["pane-2", empty("query-alpha", "alpha", "table")]]), "pane-2");
    const parsed = parsePersistedSession(JSON.stringify(buildPersistedSession()))!;
    expect(parsed.focusedPaneId).toBe("pane-2");
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual({ kind: "query", id: "query-empty", sourceKind: "search", source: "", presentation: "search" });
    expect(parsed.snapshots.get("pane-2")?.tabs[0].history[0]).toEqual({ kind: "query", id: "query-alpha", sourceKind: "search", source: "alpha", presentation: "table" });
    expect(JSON.stringify(parsed)).not.toContain("results");
  });

  it("round-trips a query workspace display draft through restore and serialization", () => {
    const display = {
      sort: [["priority", "asc"]],
      group_by: "prop:state",
      columns: ["state", "prop:owner"],
      aggregates: [["", "count"]],
      sample: 25,
    };
    const route = {
      kind: "query", id: "query-display", sourceKind: "search",
      source: "alpha", presentation: "table", display,
    };
    const raw = JSON.stringify({
      tabs: [{ history: [route], pos: 0, pinned: false }],
      activeIndex: 0,
    });

    const restored = parsePersistedSession(raw)!;
    expect(restored.snapshots.get("main")?.tabs[0].history[0]).toEqual(route);

    resetPaneLayoutToSingle(restored.snapshots.get("main")!);
    const again = parsePersistedSession(JSON.stringify(buildPersistedSession()))!;
    expect(again.snapshots.get("main")?.tabs[0].history[0]).toEqual(route);
  });

  it("keeps a cleared display draft distinct from no draft at all", () => {
    const withRoute = (display?: unknown) => JSON.stringify({
      tabs: [{
        history: [{
          kind: "query", id: "query-1", sourceKind: "search",
          source: "alpha", presentation: "table",
          ...(display === undefined ? {} : { display }),
        }],
        pos: 0, pinned: false,
      }],
      activeIndex: 0,
    });

    const cleared = parsePersistedSession(withRoute({}))!
      .snapshots.get("main")!.tabs[0].history[0];
    expect(cleared).toMatchObject({ display: {} });

    const absent = parsePersistedSession(withRoute())!
      .snapshots.get("main")!.tabs[0].history[0];
    expect(Object.hasOwn(absent, "display")).toBe(false);
  });

  it("drops only a malformed display and keeps the rest of the query route", () => {
    const restore = (display: unknown) => parsePersistedSession(JSON.stringify({
      tabs: [{
        history: [{
          kind: "query", id: "query-1", sourceKind: "search",
          source: "alpha", presentation: "table", display,
        }],
        pos: 0, pinned: true,
      }],
      activeIndex: 0,
    }))!.snapshots.get("main")!.tabs[0];

    for (const bad of [
      { columns: ["bad;name"] }, { sort: [["priority", "sideways"]] },
      { sample: -1 }, { group_by: "status" }, { aggregates: [["", "avg"]] },
      "not-an-object", 3, null, [],
    ]) {
      const tab = restore(bad);
      expect(tab.pinned).toBe(true);
      expect(tab.history[0]).toEqual({
        kind: "query", id: "query-1", sourceKind: "search",
        source: "alpha", presentation: "table",
      });
    }
  });

  it("strips unsupported keys out of a persisted draft rather than restoring them", () => {
    const parsed = parsePersistedSession(JSON.stringify({
      tabs: [{
        history: [{
          kind: "query", id: "query-1", sourceKind: "search",
          source: "alpha", presentation: "table",
          display: { columns: ["state"], view: "board", results: [{ id: "b1" }] },
        }],
        pos: 0, pinned: false,
      }],
      activeIndex: 0,
    }))!;
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual({
      kind: "query", id: "query-1", sourceKind: "search",
      source: "alpha", presentation: "table", display: { columns: ["state"] },
    });
    expect(JSON.stringify(parsed)).not.toContain("results");
  });

  it("persists a copy of the draft, not the live route's own lists", () => {
    const display = {
      columns: ["prop:a"],
      sort: [["priority", "asc"]] as [string, "asc" | "desc"][],
    };
    resetPaneLayoutToSingle({
      tabs: [{
        history: [{
          kind: "query", id: "query-1", sourceKind: "search",
          source: "alpha", presentation: "table", display,
        }],
        pos: 0, pinned: false,
      }],
      activeIndex: 0,
    });

    const persisted = buildPersistedSession();
    const written = persisted.tabs[0].history[0] as { display: typeof display };
    expect(written.display).toEqual(display);
    expect(written.display.columns).not.toBe(display.columns);
    expect(written.display.sort[0]).not.toBe(display.sort[0]);

    display.columns.push("prop:b");
    display.sort[0][1] = "desc";
    expect(written.display).toEqual({ columns: ["prop:a"], sort: [["priority", "asc"]] });
  });

  it("round-trips independent page and block display state with explicit names membership", () => {
    const route = {
      kind: "query",
      id: "query-mixed",
      sourceKind: "search",
      source: "launch",
      presentation: "search",
      display: { columns: ["prop:legacy"] },
      pagePresentation: "table",
      pageDisplay: {},
      blockPresentation: "list",
      blockDisplay: { sort: [["priority", "desc"]], sample: 12 },
      pageMatchScope: "names",
    };
    const parsed = parsePersistedSession(JSON.stringify({
      tabs: [{ history: [route], pos: 0, pinned: false }],
      activeIndex: 0,
    }))!;

    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual(route);
    resetPaneLayoutToSingle(parsed.snapshots.get("main")!);
    const again = parsePersistedSession(JSON.stringify(buildPersistedSession()))!;
    expect(again.snapshots.get("main")?.tabs[0].history[0]).toEqual(route);
    expect(JSON.stringify(again)).not.toContain("results");
    expect(JSON.stringify(again)).not.toContain("metadata");
  });

  it("drops only malformed optional scoped fields and never broadens bad membership", () => {
    const restored = parsePersistedSession(JSON.stringify({
      tabs: [{
        history: [{
          kind: "query",
          id: "query-partial-scope",
          sourceKind: "search",
          source: "alpha",
          presentation: "table",
          display: { columns: ["prop:legacy"] },
          pagePresentation: "gallery",
          pageDisplay: { columns: ["bad;field"] },
          blockPresentation: "list",
          blockDisplay: { columns: ["prop:owner"] },
          pageMatchScope: "both-and-more",
        }],
        pos: 0,
        pinned: true,
      }],
      activeIndex: 0,
    }))!;
    const route = restored.snapshots.get("main")!.tabs[0].history[0];

    expect(route).toEqual({
      kind: "query",
      id: "query-partial-scope",
      sourceKind: "search",
      source: "alpha",
      presentation: "table",
      display: { columns: ["prop:legacy"] },
      blockPresentation: "list",
      blockDisplay: { columns: ["prop:owner"] },
    });
    expect(restored.snapshots.get("main")!.tabs[0].pinned).toBe(true);
    expect(Object.hasOwn(route, "pageMatchScope")).toBe(false);
  });

  it("serializes fresh copies of every singular and scoped display list", () => {
    const display = { columns: ["prop:legacy"] };
    const pageDisplay = {
      columns: ["name"],
      sort: [["name", "asc"]] as [string, "asc" | "desc"][],
    };
    const blockDisplay = {
      aggregates: [["prop:cost", "sum"]] as [string, "count" | "sum" | "avg"][],
    };
    resetPaneLayoutToSingle({
      tabs: [{
        history: [{
          kind: "query",
          id: "query-copy-scopes",
          sourceKind: "search",
          source: "alpha",
          presentation: "table",
          display,
          pageDisplay,
          blockDisplay,
        }],
        pos: 0,
        pinned: false,
      }],
      activeIndex: 0,
    });

    const written = buildPersistedSession().tabs[0].history[0];
    if (written.kind !== "query") throw new Error("expected persisted query route");
    expect(written.display).toEqual(display);
    expect(written.pageDisplay).toEqual(pageDisplay);
    expect(written.blockDisplay).toEqual(blockDisplay);
    expect(written.display!.columns).not.toBe(display.columns);
    expect(written.pageDisplay!.columns).not.toBe(pageDisplay.columns);
    expect(written.pageDisplay!.sort![0]).not.toBe(pageDisplay.sort[0]);
    expect(written.blockDisplay!.aggregates![0]).not.toBe(blockDisplay.aggregates[0]);

    display.columns.push("prop:changed");
    pageDisplay.sort[0][1] = "desc";
    blockDisplay.aggregates[0][1] = "avg";
    expect(written.display).toEqual({ columns: ["prop:legacy"] });
    expect(written.pageDisplay).toEqual({ columns: ["name"], sort: [["name", "asc"]] });
    expect(written.blockDisplay).toEqual({ aggregates: [["prop:cost", "sum"]] });
  });

  it("round-trips graph-scoped Favorites and Recent disclosure state and defaults legacy sessions open", () => {
    setRecentPages([{ name: "Twin", kind: "page", path: "pages/client-b/Twin.md" }]);
    setFavoritesSectionExpanded(false);
    setRecentSectionExpanded(true);
    const persisted = buildPersistedSession();
    expect(persisted.favoritesSectionExpanded).toBe(false);
    expect(persisted.recentSectionExpanded).toBe(true);
    expect(persisted.recentPages).toEqual([{ name: "Twin", kind: "page", path: "pages/client-b/Twin.md" }]);

    const parsed = parsePersistedSession(JSON.stringify(persisted))!;
    setFavoritesSectionExpanded(true);
    setRecentSectionExpanded(false);
    applySidebarSession(parsed.sidebar);
    setRecentPages(parsed.recent);
    expect(favoritesSectionExpanded()).toBe(false);
    expect(recentSectionExpanded()).toBe(true);
    expect(recentPages()).toEqual([{ name: "Twin", kind: "page", path: "pages/client-b/Twin.md" }]);

    applySidebarSession({});
    expect(favoritesSectionExpanded()).toBe(true);
    expect(recentSectionExpanded()).toBe(true);
  });

  it("round-trips each right-sidebar item's graph-local disclosure state", () => {
    setRightSidebar([
      { kind: "page", name: "Expanded", pageKind: "page", path: "pages/duplicates/Expanded.md", collapsed: false },
      { kind: "block", uuid: "stable-block", page: "Source", pageKind: "page", path: "pages/duplicates/Source.md", collapsed: true },
    ]);
    const persisted = buildPersistedSession();
    expect(persisted.rightSidebarItems?.map((item) => item.collapsed)).toEqual([false, true]);

    const parsed = parsePersistedSession(JSON.stringify(persisted))!;
    setRightSidebar([]);
    applySidebarSession(parsed.sidebar);
    expect(rightSidebar()).toEqual(persisted.rightSidebarItems);

    applySidebarSession({ items: [{ kind: "page", name: "Legacy", pageKind: "page" }] });
    expect(rightSidebar()[0].collapsed).toBeUndefined();
  });

  it("accepts legacy pathless localStorage items and round-trips new exact paths", () => {
    expect(parseStoredSidebarItems(JSON.stringify([
      { kind: "page", name: "Legacy", pageKind: "page" },
      { kind: "block", uuid: "legacy-id", page: "Legacy", pageKind: "page" },
    ]))).toEqual([
      { kind: "page", name: "Legacy", pageKind: "page" },
      { kind: "block", uuid: "legacy-id", page: "Legacy", pageKind: "page" },
    ]);

    const pathful = [
      { kind: "page" as const, name: "Twin", pageKind: "page" as const, path: "pages/noncanonical/Twin.md" },
      { kind: "block" as const, uuid: "twin-id", page: "Twin", pageKind: "page" as const, path: "pages/noncanonical/Twin.md" },
    ];
    expect(parseStoredSidebarItems(JSON.stringify(pathful))).toEqual(pathful);
  });

  it("replaces an incompatible same-name physical sidebar owner", () => {
    setRightSidebar([
      { kind: "page", name: "Twin", pageKind: "page", path: "pages/Twin.md" },
      { kind: "block", uuid: "canonical-block", page: "Twin", pageKind: "page", path: "pages/Twin.md" },
    ]);

    openPageInSidebar("Twin", "page", "pages/duplicates/Twin.md");
    expect(rightSidebar()).toEqual([
      { kind: "page", name: "Twin", pageKind: "page", path: "pages/duplicates/Twin.md", collapsed: false },
    ]);

    openBlockInSidebar({
      uuid: "third-owner-block",
      page: "Twin",
      pageKind: "page",
      path: "pages/third/Twin.md",
    });
    expect(rightSidebar()).toEqual([{
      kind: "block",
      uuid: "third-owner-block",
      page: "Twin",
      pageKind: "page",
      path: "pages/third/Twin.md",
    }]);
  });

  it("evicts an old same-name owner when an existing block UUID moves to an exact path", () => {
    setRightSidebar([
      { kind: "page", name: "Twin", pageKind: "page", path: "pages/Twin.md" },
      { kind: "block", uuid: "shared-block", page: "Twin", pageKind: "page", path: "pages/Twin.md" },
    ]);

    openBlockInSidebar({
      uuid: "shared-block",
      page: "Twin",
      pageKind: "page",
      path: "pages/duplicates/Twin.md",
    });

    expect(rightSidebar()).toEqual([{
      kind: "block",
      uuid: "shared-block",
      page: "Twin",
      pageKind: "page",
      path: "pages/duplicates/Twin.md",
      collapsed: false,
    }]);
  });

  it("rewrites duplicate restored journals panes to a previous page route", () => {
    const raw = JSON.stringify({
      tabs: journals().tabs,
      activeIndex: 0,
      layout: {
        kind: "split",
        dir: "row",
        ratio: 0.5,
        children: [
          { kind: "pane", paneId: "main", ...journals() },
          {
            kind: "pane",
            paneId: "pane-2",
            tabs: [
              {
                history: [
                  { kind: "page", name: "Previous", pageKind: "page" },
                  { kind: "journals" },
                ],
                pos: 1,
                pinned: false,
              },
            ],
            activeIndex: 0,
          },
        ],
      },
    });

    const parsed = parsePersistedSession(raw)!;

    expect(parsed.layout).toEqual({
      kind: "split",
      dir: "row",
      ratio: 0.5,
      children: [
        { kind: "pane", paneId: "main" },
        { kind: "pane", paneId: "pane-2" },
      ],
    });
    expect(parsed.snapshots.get("main")?.tabs[0].history[0]).toEqual({ kind: "journals" });
    expect(parsed.snapshots.get("pane-2")?.tabs[0].history[1]).toEqual({
      kind: "page",
      name: "Previous",
      pageKind: "page",
    });
  });
});
