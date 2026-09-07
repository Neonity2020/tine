import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { Block } from "./Block";
import { ContextMenu } from "./ContextMenu";
import { initParser } from "../render/parse";
import { backend } from "../backend";
import { blockProperty, doc, resetStore, setDoc, setBlockProperty, undo, type FeedPage, type Node as StoreNode } from "../store";
import { route } from "../router";
import type { QueryExecution, QueryHit, RefGroup } from "../types";
import type { QueryReport, QueryResult } from "../editor/queryIr";
import { bumpDataRev, bumpGraphEpoch } from "../ui";
import { queryMacroExtent } from "../editor/queryMacro";
import { backendReadsQueries } from "../queryReadingsTestkit";
import { searchFilter } from "../editor/queryBuilder";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
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
  return {
    name: "Sheet",
    kind: "page",
    title: "Sheet",
    preBlock: null,
    roots,
    format: "md",
    readOnly: false,
    guide: false,
  };
}

function node(id: string, raw: string, parent: string | null, children: string[] = []): StoreNode {
  return { id, raw, collapsed: false, parent, page: "Sheet", children };
}

function queryGroups(ids: string[]): RefGroup[] {
  return [
    {
      page: "Sheet",
      kind: "page",
      blocks: ids.map((id) => ({
        id,
        raw: doc.byId[id].raw,
        collapsed: false,
        children: [],
        marker: doc.byId[id].raw.startsWith("TODO") ? "TODO" : undefined,
        properties: [["owner", "Martin"]],
      })),
    },
  ];
}

/** What `query_run` answers for a block-anchored query (§7.1). Execution goes
 *  through the ONE evaluator now — `run_query` and `run_advanced_query` cannot
 *  read TQL and are no longer on the render path — so this is what every result
 *  in this file is mocked as. `report` is the advanced ran/ignored answer, which
 *  now rides on the result rather than on a second command (M5). */
function blockResult(groups: RefGroup[], report?: Partial<QueryReport>): QueryResult {
  return {
    anchor: "block",
    groups,
    diagnostics: [],
    report: { ran: [], ignored: [], supported: true, ...report },
    total: groups.reduce((sum, group) => sum + group.blocks.length, 0),
    exceeded: false,
  };
}

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

async function settleQuery(): Promise<void> {
  await tick();
  await tick();
}

/** The Display panel, opened. A builder-backed query states its view there now
 *  — the header switcher would be a second control writing the same key — so
 *  the helper opens the sheet and the panel, in that order, exactly as a user
 *  would. Hosts with no builder (an advanced query, a friendly search) keep the
 *  header switcher, which is why both branches live here. */
async function openDisplay(root: HTMLElement): Promise<HTMLElement> {
  const gear = await vi.waitFor(() => {
    const found = root.querySelector<HTMLButtonElement>(".qs-gear");
    if (!found) throw new Error("the query sentence never appeared");
    return found;
  });
  if (!document.querySelector(".qs-sheet")) gear.click();
  const trigger = await vi.waitFor(() => {
    const found = document.querySelector<HTMLButtonElement>(".qd-trigger");
    if (!found) throw new Error("the Display control never appeared");
    return found;
  });
  if (!document.querySelector(".qd-panel")) trigger.click();
  return await vi.waitFor(() => {
    const panel = document.querySelector<HTMLElement>(".qd-panel");
    if (!panel) throw new Error("the Display panel never opened");
    return panel;
  });
}

async function clickView(
  root: HTMLElement,
  label: "Search" | "List" | "Table" | "Board",
): Promise<void> {
  const legacy = [...root.querySelectorAll(".query-view-switcher button")].find(
    (el) => el.textContent?.trim() === label
  ) as HTMLButtonElement | undefined;
  if (legacy) {
    legacy.click();
    return;
  }
  const panel = await openDisplay(root);
  const button = [...panel.querySelectorAll(".qd-view")].find(
    (el) => el.textContent?.trim() === label
  ) as HTMLButtonElement | undefined;
  if (!button) throw new Error(`missing query view button ${label}`);
  button.click();
}

async function activeView(root: HTMLElement): Promise<string | undefined> {
  const legacy = root.querySelector(".query-view-switcher button.active");
  if (legacy) return legacy.textContent?.trim();
  const panel = await openDisplay(root);
  return panel.querySelector(".qd-view.active")?.textContent?.trim();
}

function presentedResultNumbers(
  root: HTMLElement,
  view: "Search" | "List" | "Table" | "Board"
): number[] {
  const selectors = {
    Search: ".query-search-hit",
    List: '.query-group [data-block-id^="todo-"]',
    Table: '.sheet-title-cell[data-block-id^="todo-"]',
    Board: '.sheet-board-card[data-block-id^="todo-"]',
  } as const;
  return [...root.querySelectorAll(selectors[view])].map((element) => {
    const match = /Result\s+(\d+)/.exec(element.textContent ?? "");
    if (!match) throw new Error(`${view} result did not expose its fixture identity: ${element.textContent}`);
    return Number(match[1]);
  });
}

function loadQueryDoc(queryRaw: string) {
  setDoc({
    byId: {
      query: node("query", queryRaw, null),
      todo: node("todo", "TODO From query\nowner:: Martin", null),
    },
    pages: [page(["query", "todo"])],
    feed: ["Sheet"],
    loaded: true,
  });
  vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["todo"])));
}

function loadAdvancedQueryDoc(queryRaw: string) {
  setDoc({
    byId: {
      query: node("query", queryRaw, null),
      todo: node("todo", "TODO From query\nowner:: Martin", null),
    },
    pages: [page(["query", "todo"])],
    feed: ["Sheet"],
    loaded: true,
  });
  // The advanced ran/ignored answer now rides on the run's own report (M5).
  vi.spyOn(backend(), "queryRun").mockResolvedValue(
    blockResult(queryGroups(["todo"]), { ran: ["task"], ignored: [], supported: true }),
  );
  // Whether a `{{query …}}` holds datalog is the ENGINE's reading, not a regex
  // over the text (§7.1) — so the test says the engine read datalog.
  const argument = queryMacroExtent(queryRaw)?.argument ?? "";
  backendReadsQueries({ [argument]: { form: argument, kind: "advanced" } });
}

describe("QueryMacro sheet integration", () => {
  it("shows bounded ancestor context for list-query hits", async () => {
    setDoc({
      byId: {
        query: node("query", "{{query (task TODO)}}", null),
        projects: node("projects", "Projects", null, ["tine"]),
        tine: node("tine", "Tine", "projects", ["todo"]),
        todo: node("todo", "TODO From query\nowner:: Martin", "tine"),
      },
      pages: [page(["query", "projects"])],
      feed: ["Sheet"],
      loaded: true,
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult([
      {
        page: "Sheet",
        kind: "page",
        blocks: [{
          id: "todo",
          raw: doc.byId.todo.raw,
          collapsed: false,
          children: [],
        }],
      },
    ]));

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      await vi.waitFor(() => expect(root.querySelector(".ref-breadcrumb")?.textContent ?? "").toContain("Projects"));
      expect(root.querySelectorAll(".ref-breadcrumb")).toHaveLength(1);
    } finally {
      dispose();
    }
  });

  it("retains a local query-tree disclosure across fresh result object identities", async () => {
    setDoc({
      byId: {
        query: node("query", "{{query (task LATER)}}", null),
        "hit-root": node("hit-root", "TODO Query hit", null, ["hit-child"]),
        "hit-child": node("hit-child", "Query child", "hit-root", ["hit-grandchild"]),
        "hit-grandchild": node("hit-grandchild", "Query grandchild", "hit-child"),
      },
      pages: [page(["query", "hit-root"])],
      feed: ["Sheet"],
      loaded: true,
    });
    const freshResult = (): RefGroup[] => [{
      page: "Sheet",
      kind: "page",
      blocks: [{ id: "hit-root", raw: "TODO Query hit", collapsed: false, children: [] }],
    }];
    const runQuery = vi.spyOn(backend(), "queryRun").mockImplementation(async () => blockResult(freshResult()));

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      await vi.waitFor(() => expect(root.textContent).toContain("Query child"));
      expect(root.textContent).not.toContain("Query grandchild");

      root.querySelector<HTMLElement>(
        '[data-block-id="hit-child"] > .block-main .collapse-toggle.has-children',
      )!.click();
      await vi.waitFor(() => expect(root.textContent).toContain("Query grandchild"));

      bumpDataRev();
      await vi.waitFor(() => expect(runQuery).toHaveBeenCalledTimes(2));
      await vi.waitFor(() => expect(root.textContent).toContain("Query grandchild"));
      expect(doc.byId["hit-child"].collapsed).toBe(false);
    } finally {
      dispose();
    }
  });

  it("reopens a materialized friendly search without exposing it as raw DSL", async () => {
    loadQueryDoc('{{query (search "alpha beta")}}\ntine.view:: search');
    // What a `(search …)` form MEANS is the engine's answer, not a regex here:
    // the chip is friendly because the IR carries a `content match` leaf.
    backendReadsQueries({
      '(search "alpha beta")': { form: '(search "alpha beta")', filter: searchFilter("alpha beta") },
    });
    const execution: QueryExecution = {
      hits: [{
        entity: "block",
        page: "Sheet",
        kind: "page",
        block: {
          id: "todo",
          raw: "TODO From query\nid:: todo-authored",
          collapsed: false,
          children: [],
          breadcrumb: [],
          properties: [["id", "todo-authored"]],
        },
        display_text: "alpha and beta",
        evidence: [{
          clause_id: 1,
          field: "visible_content",
          mode: "contains",
          spans: [{ start: 0, end: 5 }, { start: 10, end: 14 }],
        }],
      }],
      diagnostics: [],
      explanation: { branches: [] },
      cancelled: false,
    };
    const graphSearch = vi.spyOn(backend(), "runGraphSearch").mockResolvedValue(execution);

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    expect(await activeView(root)).toBe("Search");
    // The resting SENTENCE says it, and says it as words plus one soft value —
    // not as the DSL text the block happens to hold.
    expect(root.querySelector(".qs-sentence")?.textContent).toBe("Blocks where search: alpha beta");
    expect(root.querySelector(".qs-seg-value")?.textContent).toBe("alpha beta");
    expect(root.querySelector(".qs-seg-advanced")).toBeNull();
    expect([...root.querySelectorAll("mark")].map((mark) => mark.textContent)).toEqual(["alpha", "beta"]);
    expect(graphSearch).toHaveBeenCalledWith("alpha beta", 500, 5_000, "inline-query:query", false);
    root.querySelector<HTMLButtonElement>(".query-search-hit")!.click();
    expect(route()).toMatchObject({ kind: "page", name: "Sheet", pageKind: "page" });

    dispose();
  });

  it("keeps ordinary DSL query membership across Search, List, Table, and Board presentations", async () => {
    const ids = Array.from({ length: 9 }, (_, index) => `todo-${index + 1}`);
    setDoc({
      byId: {
        query: node(
          "query",
          "{{query (and (task TODO) (priority A) (not (page Templates)) (sort-by modified desc))}}\ntine.view:: search",
          null
        ),
        ...Object.fromEntries(ids.map((id, index) => [id, node(id, `TODO [#A] Result ${index + 1}`, null)])),
      },
      pages: [page(["query", ...ids])],
      feed: ["Sheet"],
      loaded: true,
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(ids)));
    const graphSearch = vi.spyOn(backend(), "runGraphSearch");

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    expect(await activeView(root)).toBe("Search");
    expect(root.querySelector(".query-count")?.textContent).toBe("9");
    expect(root.querySelectorAll(".query-search-results .query-search-hit")).toHaveLength(9);
    expect(root.querySelector(".query-search-hit")?.textContent).toContain("Result 1");
    expect(presentedResultNumbers(root, "Search")).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9]);
    expect(graphSearch).not.toHaveBeenCalled();

    for (const view of ["List", "Table", "Board", "Search"] as const) {
      await clickView(root, view);
      await settleQuery();
      expect(await activeView(root)).toBe(view);
      expect(root.querySelector(".query-count")?.textContent).toBe("9");
      expect(presentedResultNumbers(root, view)).toEqual([1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }
    expect(graphSearch).not.toHaveBeenCalled();

    dispose();
  });

  it("renders the query header and builder above a sheet-faced query exactly once", async () => {
    loadQueryDoc("{{query (todo TODO)}}\ntine.view:: table");

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    expect(root.querySelector(".query-header")).not.toBeNull();
    expect(root.querySelector(".qs-line")).not.toBeNull();
    // This block's text is mocked as unparsed, so the sentence reads it back as
    // the retained leaf it is — still one line, still not a chip bar.
    expect(root.querySelector(".qs-sentence")?.textContent).toBe("Blocks where (todo TODO)");
    // Exactly once: one sentence, one gear, one count — not one per face.
    expect(root.querySelectorAll(".qs-sentence")).toHaveLength(1);
    expect(root.querySelectorAll(".sheet-table")).toHaveLength(1);
    expect(root.querySelectorAll(".query-table")).toHaveLength(0);
    expect(root.textContent).toContain("From query");

    dispose();
  });

  it("applies the Sheets formula filter to query-sourced Table and Board faces", async () => {
    setDoc({
      byId: {
        query: node(
          "query",
          "{{query (and (todo TODO) \"score\")}}\ntine.view:: table\ntine.fields:: points=number\ntine.filter:: points > 2",
          null
        ),
        low: node("low", "TODO Low score\npoints:: 1", null),
        high: node("high", "TODO High score\npoints:: 3", null),
      },
      pages: [page(["query", "low", "high"])],
      feed: ["Sheet"],
      loaded: true,
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["low", "high"])));

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    expect(await activeView(root)).toBe("Table");
    expect(
      [...root.querySelectorAll(".sheet-title-cell .sheet-cell-body")].map((cell) => cell.textContent?.trim())
    ).toEqual(["High score"]);

    await clickView(root, "Board");
    await settleQuery();
    expect(await activeView(root)).toBe("Board");
    expect([...root.querySelectorAll(".sheet-board-card-title")].map((card) => card.textContent?.trim())).toEqual([
      "High score",
    ]);
    // View switching must retain the coarse query and the formula refinement.
    expect(blockProperty("query", "tine.filter")).toBe("points > 2");
    // Execution goes through the ONE evaluator, carrying the query the engine
    // read — not a string the frontend re-printed (I-12).
    expect(vi.mocked(backend().queryRun).mock.calls[0][0].source).toMatchObject({
      kind: "og",
      original: '(and (todo TODO) "score")',
    });

    dispose();
  });

  it("persists List, Table, and Board through tine.view properties with one undo unit per switch", async () => {
    loadQueryDoc("{{query (todo TODO)}}");
    const originalRaw = doc.byId.query.raw;

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    expect(await activeView(root)).toBe("List");

    await clickView(root, "Table");
    expect(await activeView(root)).toBe("Table");
    expect(blockProperty("query", "tine.view")).toBe("table");
    expect(doc.byId.query.raw).toBe("{{query (todo TODO)}}\ntine.view:: table");
    undo();
    expect(doc.byId.query.raw).toBe(originalRaw);
    expect(await activeView(root)).toBe("List");

    await clickView(root, "Table");
    await clickView(root, "Board");
    expect(await activeView(root)).toBe("Board");
    expect(blockProperty("query", "tine.view")).toBe("board");
    // The Board's default grouping is written under the QUERY-owned key, whose
    // value is a canonical field id — so `state` here is the task marker and
    // could not be mistaken for an ordinary property of the same name (P5B).
    expect(blockProperty("query", "tine.group-field")).toBe("state");
    expect(blockProperty("query", "tine.group-by")).toBeNull();
    undo();
    expect(blockProperty("query", "tine.view")).toBe("table");
    expect(blockProperty("query", "tine.group-field")).toBeNull();

    await clickView(root, "Board");
    await clickView(root, "List");
    expect(await activeView(root)).toBe("List");
    expect(blockProperty("query", "tine.view")).toBeNull();
    expect(blockProperty("query", "tine.group-field")).toBe("state");
    undo();
    expect(blockProperty("query", "tine.view")).toBe("board");
    expect(blockProperty("query", "tine.group-field")).toBe("state");

    dispose();
  });

  // Found by `scripts/e2e-query-display.mjs` on real WebKit, where a press is a
  // POINTER sequence and not a bare `click()`: the panel is portalled to <body>,
  // the sheet's outside-pointer check looked for an open popover UNDER its own
  // element, and so every press in the panel read as a press outside the sheet.
  // The sheet closed, the panel went with it, and the control's own click never
  // landed — the whole panel was unusable with a real pointer while every
  // `click()`-driven test passed.
  it("holds the sheet still under a press inside the portalled Display panel", async () => {
    loadQueryDoc("{{query (todo TODO)}}");
    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();
    const panel = await openDisplay(root);

    const board = [...panel.querySelectorAll<HTMLButtonElement>(".qd-view")].find(
      (el) => el.textContent?.trim() === "Board",
    )!;
    // The press first, exactly as a pointer delivers it, and only then the
    // click: the bug was that nothing survived in between.
    for (const type of ["pointerdown", "mousedown"] as const) {
      board.dispatchEvent(new MouseEvent(type, { bubbles: true, composed: true }));
    }
    expect(document.querySelector(".qs-sheet")).not.toBeNull();
    expect(document.querySelector(".qd-panel")).not.toBeNull();

    board.click();
    await vi.waitFor(() => expect(blockProperty("query", "tine.view")).toBe("board"));
    dispose();
  });

  it("does not clobber an existing board grouping when switching to Board", async () => {
    loadQueryDoc("{{query (todo TODO)}}\ntine.group-by:: tags");

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    await clickView(root, "Board");

    expect(blockProperty("query", "tine.view")).toBe("board");
    // A legacy key that already answers is a STATEMENT, so the Board default
    // does not speak over it — `state` is nowhere here.
    //
    // What the switch DOES do is pin the meaning the block had. A bare
    // `tine.group-by:: tags` on a LIST is the ordinary property `tags`, which is
    // what the list grouper has always read; the same token on a Board would be
    // the tags facet. So the switch writes the list reading canonically and
    // retires the ambiguous key, rather than letting the new view silently
    // reinterpret it.
    expect(blockProperty("query", "tine.group-field")).toBe("prop:tags");
    expect(blockProperty("query", "tine.group-by")).toBeNull();

    dispose();
  });

  it("switching to Board leaves an explicit no-grouping alone", async () => {
    // A PRESENT but empty `tine.group-field` is the user saying "no grouping".
    // The Board default only fills the silence, so it must not speak over this.
    loadQueryDoc("{{query (todo TODO)}}\ntine.group-field:: ");

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    await clickView(root, "Board");

    expect(blockProperty("query", "tine.view")).toBe("board");
    expect(blockProperty("query", "tine.group-field")).toBe("");
    dispose();
  });

  it("keeps the task-marker default on a board whose grouping nothing states", async () => {
    // ADR 0030, kept alive across the P5B grouping split. A note authored as
    // `tine.view:: board` with no grouping ANYWHERE has always shown a
    // task-marker board — the default fills the silence. `unset` and an explicit
    // clear are two different answers, and only the clear is one ungrouped
    // column; collapsing them would silently un-group every existing board.
    setDoc({
      byId: {
        query: node("query", "{{query (todo TODO)}}\ntine.view:: board", null),
        todo: node("todo", "TODO From query\nowner:: Martin", null),
      },
      pages: [page(["query", "todo"])],
      feed: ["Sheet"],
      loaded: true,
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["todo"])));

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();

      const headings = await vi.waitFor(() => {
        const found = [...root.querySelectorAll(".sheet-board-header span:first-child")].map((el) =>
          el.textContent?.trim(),
        );
        if (!found.length) throw new Error("the board never rendered");
        return found;
      });
      expect(headings).toContain("TODO");
      expect(headings).not.toContain("All results");
      // Reading is not writing: the default is applied by the renderer, and the
      // note gains no property from being looked at (I-4).
      expect(blockProperty("query", "tine.group-field")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("opens Display without the filter sheet and reads its registry only on demand", async () => {
    bumpGraphEpoch();
    loadQueryDoc("{{query (todo TODO)}}");
    const registry = vi.spyOn(backend(), "queryRegistry");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      expect(document.querySelector('.qs-sheet[aria-label="Query filter"]')).toBeNull();
      expect(registry).not.toHaveBeenCalled();
      const trigger = root.querySelector<HTMLButtonElement>(".qd-trigger");
      expect(trigger).not.toBeNull();
      trigger!.click();
      await vi.waitFor(() => expect(document.querySelector(".qd-panel")).not.toBeNull());
      await vi.waitFor(() => expect(registry).toHaveBeenCalledTimes(1));
      expect(document.querySelector('.qs-sheet[aria-label="Query filter"]')).toBeNull();
    } finally { dispose(); }
  });

  it("does not undo a display edit with the next click made before the re-parse", async () => {
    // FAIL-BEFORE (I-20): every display surface renders from the ENGINE's last
    // reading, and the engine re-reads asynchronously. Two clicks inside one
    // parse round-trip therefore both start from the reading that predates the
    // first — and a write set computed against the block's properties from that
    // stale reading restates the fact the first click just changed, undoing it.
    //
    // Here: clear the grouping, then switch to Board without waiting. The
    // switch's untouched grouping is the pre-clear one, and the save baseline
    // called that a disagreement with the empty `tine.group-field` and wrote the
    // old grouping straight back.
    loadQueryDoc("{{query (todo TODO)}}\ntine.group-by:: state");

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      const panel = await openDisplay(root);

      const clear = [...panel.querySelectorAll<HTMLButtonElement>(".qd-row-btn")].find(
        (button) => button.textContent?.trim() === "None",
      )!;
      const board = [...panel.querySelectorAll<HTMLButtonElement>(".qd-view")].find(
        (button) => button.textContent?.trim() === "Board",
      )!;
      // Two clicks, no await between them — the parse cannot have answered.
      clear.click();
      board.click();

      expect(blockProperty("query", "tine.view")).toBe("board");
      // The explicit clear survives, and the ambiguous legacy key stays retired.
      expect(blockProperty("query", "tine.group-field")).toBe("");
      expect(blockProperty("query", "tine.group-by")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("keeps both aggregate additions made before the re-parse", async () => {
    loadQueryDoc("{{query (todo TODO)}}");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      const panel = await openDisplay(root);
      const add = [...panel.querySelectorAll<HTMLButtonElement>("button")].find(
        (button) => button.textContent?.trim() === "+ count",
      )!;
      add.click();
      add.click();
      expect(blockProperty("query", "tine.col-aggregates")).toBe("count;count");
    } finally { dispose(); }
  });

  it("shows the sheet-only aggregate segments it retains, and keeps them on an edit", async () => {
    // FAIL-BEFORE: `tine.col-aggregates` is shared ground (contract §5). The
    // save merges rather than rewrites, so a table-only `estimate=median`
    // survived — but no surface said so, and the panel that DOES list the
    // aggregates listed only the three the query reader owns. Retention the
    // author cannot see is indistinguishable from loss.
    loadQueryDoc("{{query (todo TODO)}}\ntine.col-aggregates:: estimate=median");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      const panel = await openDisplay(root);
      // Read from the block's OWN bytes: the query reader never returns this
      // segment, so no reading of the view could have produced it.
      expect(panel.querySelector(".qd-retained")?.textContent).toContain("estimate=median");

      const add = [...panel.querySelectorAll<HTMLButtonElement>("button")].find(
        (button) => button.textContent?.trim() === "+ count",
      )!;
      add.click();
      // The unrelated edit appends; the retained segment keeps its text and its
      // place, and the panel still says it is there.
      expect(blockProperty("query", "tine.col-aggregates")).toBe("estimate=median;count");
      await vi.waitFor(() =>
        expect(document.querySelector(".qd-retained")?.textContent).toContain("estimate=median"),
      );
    } finally { dispose(); }
  });

  it("preserves a grouping clear when a sample save starts before the re-parse", async () => {
    loadQueryDoc("{{query (todo TODO)}}\ntine.group-by:: state");
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(todo TODO)");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      const panel = await openDisplay(root);
      const clear = [...panel.querySelectorAll<HTMLButtonElement>(".qd-row-btn")].find(
        (button) => button.textContent?.trim() === "None",
      )!;
      const sample = panel.querySelector<HTMLInputElement>(".qd-sample")!;
      clear.click();
      sample.value = "2";
      sample.dispatchEvent(new Event("input", { bubbles: true }));
      sample.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
      await vi.waitFor(() => expect(blockProperty("query", "tine.sample")).toBe("2"));
      expect(blockProperty("query", "tine.group-field")).toBe("");
      expect(blockProperty("query", "tine.group-by")).toBeNull();
    } finally { dispose(); }
  });

  it.each(["property", "graph"])("does not overwrite a %s change while the printer is pending", async (change) => {
    loadQueryDoc("{{query (todo TODO)}}");
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    let finish!: (text: string) => void;
    const printed = new Promise<string>((resolve) => { finish = resolve; });
    const printer = vi.spyOn(backend(), "printQuery").mockReturnValue(printed);
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await settleQuery();
      const panel = await openDisplay(root);
      const sample = panel.querySelector<HTMLInputElement>(".qd-sample")!;
      sample.value = "2";
      sample.dispatchEvent(new Event("input", { bubbles: true }));
      sample.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
      await vi.waitFor(() => expect(printer).toHaveBeenCalled());
      if (change === "property") setBlockProperty("query", "owner", "Later edit");
      else bumpGraphEpoch();
      finish("(todo TODO)");
      await vi.waitFor(() => expect(root.querySelector(".query-print-refused")?.textContent).toContain("changed while saving"));
      if (change === "property") expect(blockProperty("query", "owner")).toBe("Later edit");
      expect(blockProperty("query", "tine.sample")).toBeNull();
    } finally { finish("(todo TODO)"); dispose(); }
  });

  it("collapses a query sheet face while keeping the query controls visible", async () => {
    loadQueryDoc("{{query (todo TODO)}}\ntine.view:: table");

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();
    expect(root.querySelectorAll(".sheet-table")).toHaveLength(1);

    (root.querySelector(".query-collapse") as HTMLElement).click();

    expect(root.querySelector(".query-header")).not.toBeNull();
    expect(root.querySelector(".qs-line")).not.toBeNull();
    expect(root.querySelectorAll(".sheet-table")).toHaveLength(0);

    dispose();
  });

  it("keeps identical query collapse overrides isolated by block identity", async () => {
    setDoc({
      byId: {
        q1: node("q1", "{{query (todo TODO)}}", null),
        q2: node("q2", "{{query (todo TODO)}}", null),
        todo: node("todo", "TODO From query", null),
      },
      pages: [page(["q1", "q2", "todo"])], feed: ["Sheet"], loaded: true,
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["todo"])));
    const { root, dispose } = mount(() => <><Block id="q1" /><Block id="q2" /></>);
    await settleQuery();
    const toggles = root.querySelectorAll<HTMLElement>(".query-collapse");
    toggles[0].click();
    expect(toggles[0].classList.contains("collapsed")).toBe(true);
    expect(toggles[1].classList.contains("collapsed")).toBe(false);
    dispose();
  });

  it("persists an explicit expanded override over source collapsed true", async () => {
    loadQueryDoc("{{query (todo TODO) {:collapsed? true}}}");
    backendReadsQueries({ "(todo TODO) {:collapsed? true}": { form: "(todo TODO)", opts: "{:collapsed? true}" } });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["todo"])));
    const first = mount(() => <Block id="query" />);
    await settleQuery();
    const toggle = first.root.querySelector(".query-collapse") as HTMLElement;
    expect(toggle.classList.contains("collapsed")).toBe(true);
    toggle.click();
    expect(toggle.classList.contains("collapsed")).toBe(false);
    first.dispose();
    document.body.innerHTML = "";

    const second = mount(() => <Block id="query" />);
    await settleQuery();
    expect((second.root.querySelector(".query-collapse") as HTMLElement).classList.contains("collapsed")).toBe(false);
    second.dispose();
  });

  it("keeps legacy :table-view? rendering read-only when no tine.view is set", async () => {
    loadQueryDoc("{{query (todo TODO) {:table-view? true}}}");
    backendReadsQueries({ "(todo TODO) {:table-view? true}": { form: "(todo TODO)", opts: "{:table-view? true}" } });

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    expect(await activeView(root)).toBe("List");
    expect(blockProperty("query", "tine.view")).toBeNull();
    expect(root.querySelectorAll(".query-table")).toHaveLength(1);
    expect(root.querySelectorAll(".sheet-table")).toHaveLength(0);

    dispose();
  });

  // The `⚙ advanced` / `← Simple` pair is gone with the frontend's datalog
  // converters (§9 P0-ts). What must still hold is that an authored advanced
  // query renders its ran/ignored note and does NOT show the chip bar — the
  // builder edits a filter, and converting authored datalog into one is out of
  // scope (§4.3.1, Q13).
  it("renders an advanced query's report without offering the filter builder", async () => {
    loadAdvancedQueryDoc('{{query [:find (pull ?b [*]) :where (task ?b "TODO")]}}');

    const { root, dispose } = mount(() => (
      <>
        <Block id="query" />
        <ContextMenu />
      </>
    ));
    await settleQuery();

    await vi.waitFor(() => expect(root.querySelector(".query-adv-note")?.textContent ?? "").toContain("ran: task"));
    // An advanced (datalog) query has no sentence and no sheet to offer.
    expect(root.querySelector(".qs-line")).toBeNull();
    expect(root.querySelector(".qs-sheet")).toBeNull();
    // …and the count stays in the header, where it has always been.
    expect(root.querySelector(".query-header .query-count")).not.toBeNull();
    expect([...root.querySelectorAll("button")].some((el) => el.textContent?.trim() === "← Simple")).toBe(false);

    dispose();
  });
});

// GH #469. `{{query "xyz"}}` matched its own block, because the block's own text
// contains `xyz` — so the query listed the page it lives on, which renders the
// query again, which lists the page again. OG removes exactly the host block
// from every result set for this reason, and says so at
// frontend/components/query/result.cljs (6e7afa8e): "exclude the current one,
// otherwise it'll loop forever".
describe("a query never returns its own block (GH #469)", () => {
  function loadSelfMatching(queryRaw: string) {
    setDoc({
      byId: {
        query: node("query", queryRaw, null),
        todo: node("todo", "TODO From query\nowner:: Martin", null),
      },
      pages: [page(["query", "todo"])],
      feed: ["Sheet"],
      loaded: true,
    });
  }

  it("drops the host block from a simple DSL query's results", async () => {
    loadSelfMatching('{{query "From query"}}\ntine.view:: list');
    // The backend answers honestly: the host block's own text matches too.
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockResult(queryGroups(["query", "todo"])));

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    const listed = [...root.querySelectorAll(".query-group [data-block-id]")]
      .map((el) => el.getAttribute("data-block-id"));
    expect(listed).toContain("todo");
    expect(listed).not.toContain("query");
    // The count the user reads must agree with what is shown.
    expect(root.querySelector(".query-count")?.textContent).toBe("1");
    dispose();
  });

  it("drops the host block from an advanced query's results", async () => {
    loadSelfMatching('{{query {:query [:find (pull ?b [*]) :where [?b :block/content "x"]]}}}\ntine.view:: list');
    // A form that is ITSELF one map stays whole: `split_trailing_map` only splits
    // a map that FOLLOWS a nonempty form (§4.3.1).
    backendReadsQueries({
      '{:query [:find (pull ?b [*]) :where [?b :block/content "x"]]}': {
        form: '{:query [:find (pull ?b [*]) :where [?b :block/content "x"]]}',
        kind: "advanced",
      },
    });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(
      blockResult(queryGroups(["query", "todo"]), { ran: ["content"] }),
    );

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    const listed = [...root.querySelectorAll(".query-group [data-block-id]")]
      .map((el) => el.getAttribute("data-block-id"));
    expect(listed).toContain("todo");
    expect(listed).not.toContain("query");
    dispose();
  });

  it("drops the host block from a full-text search query's hits", async () => {
    loadSelfMatching('{{query (search "From query")}}\ntine.view:: search');
    const hit = (id: string, raw: string): QueryHit => ({
      entity: "block" as const,
      page: "Sheet",
      kind: "page" as const,
      block: { id, raw, collapsed: false, children: [], breadcrumb: [], properties: [] },
      display_text: raw,
      evidence: [{ clause_id: 1, field: "visible_content" as const, mode: "contains" as const, spans: [{ start: 0, end: 4 }] }],
    });
    const execution: QueryExecution = {
      hits: [hit("query", '{{query (search "From query")}}'), hit("todo", "TODO From query")],
      diagnostics: [],
      explanation: { branches: [] },
      cancelled: false,
    };
    vi.spyOn(backend(), "runGraphSearch").mockResolvedValue(execution);

    const { root, dispose } = mount(() => <Block id="query" />);
    await settleQuery();

    const hits = [...root.querySelectorAll(".query-search-hit")].map((el) => el.textContent ?? "");
    expect(hits).toHaveLength(1);
    expect(hits[0]).toContain("TODO From query");
    dispose();
  });
});
