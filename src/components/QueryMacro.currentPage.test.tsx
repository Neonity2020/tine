import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { Block } from "./Block";
import { initParser } from "../render/parse";
import { backend } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { doc, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import { openPage, route } from "../router";
import { resetPaneLayoutToSingle } from "../panes";
import type { QueryExecution, RefGroup } from "../types";

// GH #301 (approved): a query whose text explicitly carries `<% current page %>`
// binds that marker to the FOCUSED pane's route page and re-runs when that page
// changes. Everything else stays exactly as today: authoring text untouched,
// no rerun for marker-less queries, coalesced on same-page transitions, and a
// stale asynchronous result must never overwrite the latest page's result.

let disposeKeys: (() => void) | null = null;
beforeAll(async () => {
  await initParser();
  disposeKeys = null;
});

afterEach(() => {
  disposeKeys?.();
  vi.restoreAllMocks();
  resetSharedQueryResultsForTests();
  resetStore();
  resetPaneLayoutToSingle({ tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }], activeIndex: 0 });
  localStorage.clear();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(node, root);
  return { root, dispose };
}

function page(roots: string[]): FeedPage {
  return { name: "Sheet", kind: "page", title: "Sheet", preBlock: null, roots, format: "md", readOnly: false, guide: false };
}

function node(id: string, raw: string, parent: string | null, children: string[] = []): StoreNode {
  return { id, raw, collapsed: false, parent, page: "Sheet", children };
}

function loadQueryDoc(queryRaw: string) {
  setDoc({
    byId: {
      query: node("query", queryRaw, null),
      todo: node("todo", "TODO Body", null),
      rowA: node("rowA", "RowA-Presented", null),
      rowB: node("rowB", "RowB-Presented", null),
    },
    pages: [page(["query", "todo"])],
    feed: ["Sheet"],
    loaded: true,
  });
}

function groupsFor(...ids: string[]): RefGroup[] {
  return [{
    page: "Sheet",
    kind: "page",
    blocks: ids.map((id) => ({ id, raw: doc.byId[id]?.raw ?? id, collapsed: false, children: [] })),
  }];
}

function searchFor(id: "rowA" | "rowB"): QueryExecution {
  return {
    hits: [{
      entity: "block",
      page: "Sheet",
      kind: "page",
      block: { id, raw: doc.byId[id].raw, collapsed: false, children: [] },
      display_text: doc.byId[id].raw,
      evidence: [],
    }],
    diagnostics: [],
    explanation: { branches: [] },
    cancelled: false,
  };
}

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

async function settle(): Promise<void> {
  await tick();
  await tick();
  await tick();
}

describe("query `<% current page %>` dispatch to the focused pane (GH #301)", () => {
  it("substitutes the focused page at execution time, keeps the authored text literal, and re-runs on navigation — coalescing identical transitions", async () => {
    loadQueryDoc("{{query (and (page <% current page %>) (task TODO))}}");
    const runQuery = vi.spyOn(backend(), "runQuery").mockImplementation(async () => groupsFor("todo"));
    openPage("Focus A", "page");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settle();
      const callsA = runQuery.mock.calls.map((c) => String(c[0]));
      expect(callsA).toHaveLength(1);
      expect(callsA[0]).toContain("[[Focus A]]");
      expect(callsA[0]).not.toContain("<%");
      // Authoring text keeps the literal dyvar (execution-only substitution).
      expect(doc.byId.query.raw).toContain("<% current page %>");
      await vi.waitFor(() => expect(root.textContent).toContain("TODO Body")); // result presented

      openPage("Focus B", "page");
      await settle();
      const callsB = runQuery.mock.calls.map((c) => String(c[0]));
      expect(callsB).toHaveLength(2);
      expect(callsB[1]).toContain("[[Focus B]]");
      await vi.waitFor(() => expect(root.textContent).toContain("TODO Body"));

      // Same-page "navigation" (block anchor / reopen): no rerun.
      openPage("Focus B", "page");
      await settle();
      expect(runQuery).toHaveBeenCalledTimes(2);
    } finally {
      dispose();
    }
  });

  it("does NOT rerun a marker-less query on navigation (no global rerun)", async () => {
    loadQueryDoc("{{query (task TODO)}}");
    const runQuery = vi.spyOn(backend(), "runQuery").mockImplementation(async () => groupsFor("plain"));
    openPage("Focus A", "page");
    const { dispose } = mount(() => <Block id="query" />);
    try {
      await settle();
      openPage("Focus B", "page");
      await settle();
      expect(runQuery).toHaveBeenCalledTimes(1);
    } finally {
      dispose();
    }
  });

  it("advanced queries substitute at execution too, while its owner-page param stays the query block's page (the :query-page binding)", async () => {
    loadQueryDoc("{{query [:find (pull ?b [*]) :in $ ?cp :where [?b :block/refs ?cp]] :inputs [<% current page %>]}}");
    const runAdvancedQuery = vi.spyOn(backend(), "runAdvancedQuery").mockImplementation(async (q: string, pageParam?: string) => ({
      groups: groupsFor(`advanced ${q} @ ${pageParam}`),
      ran: [],
      ignored: [],
      supported: true,
    }));
    vi.spyOn(backend(), "runQuery").mockImplementation(async () => groupsFor("fallback"));
    openPage("Focus A", "page");
    const { dispose } = mount(() => <Block id="query" />);
    try {
      await settle();
      const calls = runAdvancedQuery.mock.calls;
      expect(calls).toHaveLength(1);
      expect(String(calls[0][0])).toContain("[[Focus A]]");
      expect(String(calls[0][0])).not.toContain("<%");
      expect(calls[0][1]).toBe("Sheet"); // owner-page param unchanged
    } finally {
      dispose();
    }
  });

  it("a stale async result from the previous page never overwrites the latest page's result", async () => {
    loadQueryDoc("{{query (and (page <% current page %>) (task TODO))}}");
    const deferred = new Map<string, (groups: RefGroup[]) => void>();
    const runQuery = vi.spyOn(backend(), "runQuery").mockImplementation(
      (q: string) =>
        new Promise<RefGroup[]>((resolve) => {
          deferred.set(String(q), resolve);
        }),
    );
    openPage("Focus A", "page");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await vi.waitFor(() => expect(runQuery).toHaveBeenCalledTimes(1), { timeout: 2000 });
      openPage("Focus B", "page");
      await vi.waitFor(() => expect(runQuery).toHaveBeenCalledTimes(2), { timeout: 2000 });
      // The latest page's fetch resolves FIRST; the stale fetch resolves after.
      deferred.get(String(runQuery.mock.calls[1][0]))!(groupsFor("rowB"));
      await settle();
      deferred.get(String(runQuery.mock.calls[0][0]))!(groupsFor("rowA"));
      await settle();
      expect(root.textContent).toContain("RowB-Presented");
      expect(root.textContent).not.toContain("RowA-Presented");
    } finally {
      dispose();
    }
  });

  it("a stale friendly-search completion cannot overwrite the latest search presentation", async () => {
    loadQueryDoc('{{query (search "<% current page %>")}}\ntine.view:: search');
    const deferred = new Map<string, (result: QueryExecution) => void>();
    const runGraphSearch = vi.spyOn(backend(), "runGraphSearch").mockImplementation(
      (query: string) => new Promise<QueryExecution>((resolve) => deferred.set(query, resolve)),
    );
    openPage("Focus A", "page");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await vi.waitFor(() => expect(runGraphSearch).toHaveBeenCalledTimes(1));
      openPage("Focus B", "page");
      await vi.waitFor(() => expect(runGraphSearch).toHaveBeenCalledTimes(2));
      deferred.get(String(runGraphSearch.mock.calls[1][0]))!(searchFor("rowB"));
      await settle();
      deferred.get(String(runGraphSearch.mock.calls[0][0]))!(searchFor("rowA"));
      await settle();
      expect(root.textContent).toContain("RowB-Presented");
      expect(root.textContent).not.toContain("RowA-Presented");
    } finally {
      dispose();
    }
  });

  it("with no page in the focused route (the journals feed), the dyvar is left alone verbatim", async () => {
    loadQueryDoc("{{query (and (page <% current page %>) (task TODO))}}");
    const runQuery = vi.spyOn(backend(), "runQuery").mockImplementation(async (q: string) => groupsFor(q));
    // route starts at journals: no focused page.
    expect(route().kind).toBe("journals");
    const { dispose } = mount(() => <Block id="query" />);
    try {
      await settle();
      const calls = runQuery.mock.calls.map((c) => String(c[0]));
      expect(calls).toHaveLength(1);
      expect(calls[0]).toContain("<% current page %>");
      // Navigating from journals to a real page NOW makes it live.
      openPage("Later Page", "page");
      await settle();
      expect(runQuery).toHaveBeenCalledTimes(2);
      expect(String(runQuery.mock.calls[1][0])).toContain("[[Later Page]]");
    } finally {
      dispose();
    }
  });
});
