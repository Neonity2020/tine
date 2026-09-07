// The seam between the block and the ONE query engine (SPEC §4.3.1, §7.1;
// I-9, I-12, I-20, I-4).
//
// Three things this file is the evidence for:
//
//  B1  A `{{tine-query …}}` block RENDERS ROWS. Before this packet the macro
//      called `run_query`, which cannot read TQL, so a TQL block drew its
//      header, its count and its controls and then nothing — and
//      `query_explain_empty` was decoded and never rendered, so the one moment a
//      user most needs to know WHICH conjunct emptied the query said only "No
//      results".
//  B4  The text pane. Debounced, last-good rows stay VISIBLE AND GREYED behind
//      the parser's own message, and a late answer for text the user has since
//      retyped is dropped rather than rendered (I-20).
//  B5  The save path, and the one caller entitled to `NotApplicable` (Q3): the
//      macro name is chosen from `query_og_expressible`, an OG refusal is
//      answered by switching dialect, and any OTHER refusal writes nothing and
//      shows the printer's own message (I-9).
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { Block } from "./Block";
import { initParser } from "../render/parse";
import { backend, QueryPrintRefusedError } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { blockProperty, doc, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import type { RefGroup } from "../types";
import type { ExplainEmptyResult, ParsedQuery, Query, ViewSettings } from "../editor/queryIr";
import { blockRunResult } from "../queryReadingsTestkit";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetSharedQueryResultsForTests();
  resetStore();
  localStorage.clear();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
const tick = () => wait(0);
async function settle(): Promise<void> {
  await tick();
  await tick();
  await tick();
}

function page(roots: string[], readOnly = false): FeedPage {
  return {
    name: "Sheet", kind: "page", title: "Sheet", preBlock: null,
    roots, format: "md", readOnly, guide: false,
  };
}

function node(id: string, raw: string): StoreNode {
  return { id, raw, collapsed: false, parent: null, page: "Sheet", children: [] };
}

function groups(): RefGroup[] {
  return [{
    page: "Sheet",
    kind: "page",
    blocks: [{ id: "todo", raw: "TODO A tracked row", collapsed: false, children: [] }],
  }];
}

function load(raw: string, { readOnly = false }: { readOnly?: boolean } = {}): void {
  setDoc({
    byId: { query: node("query", raw), todo: node("todo", "TODO A tracked row") },
    pages: [page(["query", "todo"], readOnly)],
    feed: ["Sheet"],
    loaded: true,
  });
}

/** A `{{tine-query …}}` block: TQL, which ONLY the IR evaluator can read. */
const TQL_MACRO = "{{tine-query -- task TODO}}";

describe("B1: a TQL block executes through query_run", () => {
  it("renders its rows, and hands the evaluator the IR rather than a printed string", async () => {
    load(TQL_MACRO);
    const run = vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      // The rows are the assertion: the legacy evaluator cannot read TQL, so a
      // block that renders its header and count but no rows is the exact defect.
      await vi.waitFor(() => expect(root.textContent).toContain("A tracked row"));
      expect(root.querySelector(".query-count")?.textContent).toBe("1");

      const [query, view] = run.mock.calls[0];
      expect(query.source).toMatchObject({ kind: "tql", original: "-- task TODO" });
      expect(view).toEqual({});
    } finally {
      dispose();
    }
  });

  it("renders query_explain_empty when the run comes back empty (Q14, N19)", async () => {
    load(TQL_MACRO);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult([]));
    const explain: ExplainEmptyResult = {
      rows: [
        { conjunct: "task = TODO", alone: 12, without: 0 },
        { conjunct: "page = [[Nowhere]]", alone: 0, without: 12 },
      ],
      diagnostics: [],
      report: { ran: [], ignored: [], supported: true },
    };
    const explainEmpty = vi.spyOn(backend(), "queryExplainEmpty").mockResolvedValue(explain);

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await vi.waitFor(() => expect(root.querySelector(".query-empty")).not.toBeNull());
      // Asked only once a run has actually come back empty — an ordinary query
      // still costs one command.
      expect(explainEmpty).not.toHaveBeenCalled();

      root.querySelector<HTMLButtonElement>(".query-why-empty")!.click();
      await vi.waitFor(() => expect(root.querySelector(".query-why-empty-panel")).not.toBeNull());
      const text = root.querySelector(".query-why-empty-panel")!.textContent ?? "";
      // The conjunct that matches nothing ALONE is the one that emptied it.
      expect(text).toContain("page = [[Nowhere]]");
      expect(text).toContain("task = TODO");
      expect(explainEmpty).toHaveBeenCalledTimes(1);
    } finally {
      dispose();
    }
  });
});

/** Open the collapsed text pane, as a user clicking its disclosure does. */
async function openPane(root: HTMLElement): Promise<HTMLTextAreaElement> {
  const details = await vi.waitFor(() => {
    const found = root.querySelector<HTMLDetailsElement>(".query-text-pane-details");
    if (!found) throw new Error("the query text pane never appeared");
    return found;
  });
  details.open = true;
  details.dispatchEvent(new Event("toggle"));
  return await vi.waitFor(() => {
    const input = root.querySelector<HTMLTextAreaElement>(".query-text-pane-input");
    if (!input) throw new Error("the pane has no input");
    return input;
  });
}

function type(input: HTMLTextAreaElement, text: string): void {
  input.value = text;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

/** A parse answer for arbitrary pane text: a `raw` capsule, which is what a
 *  reader that retained text without interpreting it honestly returns. */
function parsedAs(text: string): ParsedQuery {
  return {
    query: {
      anchor: "block",
      filter: { kind: "raw", text, diagnostic_kind: "not_applicable" },
      diagnostics: [],
      source: { kind: "tql", original: text, og_options: "" },
    },
    view: {},
  };
}

describe("B4: the query text pane", () => {
  it("debounces: a burst of keystrokes asks the engine once, for the last text", async () => {
    load(TQL_MACRO);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "printQuery").mockResolvedValue("-- task TODO");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const input = await openPane(root);
      const parse = vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) =>
        parsedAs(text),
      );

      type(input, "-- task T");
      type(input, "-- task TO");
      type(input, "-- task TOD");
      type(input, "-- task DONE");
      expect(parse).not.toHaveBeenCalled();

      await wait(250);
      const paneCalls = parse.mock.calls.filter(([, dialect]) => dialect === "tql");
      expect(paneCalls).toHaveLength(1);
      expect(paneCalls[0][0]).toBe("-- task DONE");
    } finally {
      dispose();
    }
  });

  it("drops a late answer for text the user has since retyped (I-20)", async () => {
    load(TQL_MACRO);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "printQuery").mockResolvedValue("-- task TODO");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const input = await openPane(root);
      const settlers = new Map<string, { ok: (parsed: ParsedQuery) => void; fail: (e: Error) => void }>();
      vi.spyOn(backend(), "parseQuery").mockImplementation(
        (text: string) =>
          new Promise<ParsedQuery>((ok, fail) => settlers.set(text, { ok, fail })),
      );

      type(input, "-- task OLD");
      await wait(200);
      type(input, "-- task NEW");
      await wait(200);
      expect([...settlers.keys()]).toEqual(["-- task OLD", "-- task NEW"]);

      // The newest revision answers first; the stale one then fails. Rendering
      // that failure would read as "my correction was rejected".
      settlers.get("-- task NEW")!.ok(parsedAs("-- task NEW"));
      await settle();
      settlers.get("-- task OLD")!.fail(new Error("unbalanced parenthesis at 1:9"));
      await settle();

      expect(root.querySelector(".query-text-pane-error")).toBeNull();
      expect(root.querySelector(".query-block")?.classList.contains("query-stale")).toBe(false);
      expect(root.querySelector<HTMLButtonElement>(".query-text-pane-save")!.disabled).toBe(false);
    } finally {
      dispose();
    }
  });

  it("keeps the last-good rows visible and greyed under the parser's own message", async () => {
    load(TQL_MACRO);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "printQuery").mockResolvedValue("-- task TODO");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await vi.waitFor(() => expect(root.textContent).toContain("A tracked row"));
      const input = await openPane(root);
      vi.spyOn(backend(), "parseQuery").mockRejectedValue(
        new Error("expected a comparison after `where`"),
      );

      type(input, "-- task ");
      await vi.waitFor(() =>
        expect(root.querySelector(".query-text-pane-error")?.textContent)
          .toBe("expected a comparison after `where`"),
      );

      // Not blanked: these rows are the last reading that RAN.
      expect(root.textContent).toContain("A tracked row");
      expect(root.querySelector(".query-block")?.classList.contains("query-stale")).toBe(true);
      // …and no spinner outliving the response.
      expect(root.querySelector(".query-text-pane-pending")).toBeNull();
      expect(root.querySelector<HTMLButtonElement>(".query-text-pane-save")!.disabled).toBe(true);
      expect(root.querySelector(".query-text-pane-error")?.getAttribute("role")).toBe("alert");
    } finally {
      dispose();
    }
  });

  it("says the options map is edited elsewhere instead of asking the engine to parse one", async () => {
    load(TQL_MACRO);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "printQuery").mockResolvedValue("-- task TODO");
    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const input = await openPane(root);
      const parse = vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) =>
        parsedAs(text),
      );

      type(input, '-- task TODO {:title "Open"}');
      await wait(250);

      expect(root.querySelector(".query-text-pane-error")?.textContent).toContain("options map");
      // Splitting a macro argument is Rust's job and only Rust's — the pane makes
      // no claim about where the map starts, and asks nothing.
      expect(parse.mock.calls.filter(([, dialect]) => dialect === "tql")).toHaveLength(0);
    } finally {
      dispose();
    }
  });
});

/** The dialects the SAVE path asked for. The pane prints too — that is how it
 *  shows the query's text — so the macro dialects are what identify a save. */
function savePrintDialects(print: { mock: { calls: unknown[][] } }): string[] {
  return print.mock.calls
    .map((call) => String(call[2]))
    .filter((dialect) => dialect === "og" || dialect === "tql_macro" || dialect === "advanced_macro");
}

/** Drive one save through the pane: type valid text, wait for its parse, click
 *  "Save query text". */
async function saveThroughPane(root: HTMLElement, text: string): Promise<void> {
  const input = await openPane(root);
  vi.spyOn(backend(), "parseQuery").mockImplementation(async (source: string) => parsedAs(source));
  type(input, text);
  const save = await vi.waitFor(() => {
    const button = root.querySelector<HTMLButtonElement>(".query-text-pane-save");
    if (!button || button.disabled) throw new Error("save is not enabled yet");
    return button;
  });
  save.click();
  await settle();
}

describe("B5: the save path chooses the name and answers NotApplicable", () => {
  it("keeps {{query}} for an OG-expressible edit, and an empty view adds no property line", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    const print = vi.spyOn(backend(), "printQuery").mockResolvedValue("(task DONE)");

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      expect(doc.byId.query.raw).toBe("{{query (task DONE)}}");
      expect(savePrintDialects(print)).toEqual(["og"]);
      // Every save now writes the six §7.6 view properties (T4) — but clearing a
      // property a block never had is the identity, so a view with nothing in it
      // still leaves the block's bytes as the macro line and nothing else.
      expect(doc.byId.query.raw).not.toContain("tine.");
    } finally {
      dispose();
    }
  });

  it("crosses a {{query}} block to {{tine-query}} when OG cannot express it, carrying the view (Y2)", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(false);
    const print = vi.spyOn(backend(), "printQuery").mockResolvedValue("-- task DONE");
    // The pane's parse answers with the session's own view, so state one worth
    // carrying: TQL text has nowhere to put it (§4.3 Y2).
    vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) => ({
      query: parsedAs(text).query,
      view: { sort: [["updated", "desc"]], sample: 20, group_by: "page", aggregates: [["", "count"]] },
    }));

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(doc.byId.query.raw).toContain("{{tine-query -- task DONE}}"));
      expect(savePrintDialects(print)).toEqual(["tql_macro"]);
      expect(blockProperty("query", "tine.sort")).toBe("updated desc");
      expect(blockProperty("query", "tine.sample")).toBe("20");
      expect(blockProperty("query", "tine.group-by")).toBe("page");
      // X3/W5: the whole-result count is a bare `count` segment, with no `=`.
      expect(blockProperty("query", "tine.col-aggregates")).toBe("count");
    } finally {
      dispose();
    }
  });

  it("answers a NotApplicable refusal by switching dialect, not by showing it", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    // `og_expressible` said yes; the printer refuses anyway. This is the ONE
    // caller entitled to see `NotApplicable`.
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    const print = vi.spyOn(backend(), "printQuery").mockImplementation(async (
      _query: Query,
      _view,
      dialect: string,
    ) => {
      if (dialect === "og") {
        throw new QueryPrintRefusedError("not_applicable", {
          kind: "not_applicable",
          message: "the OG DSL cannot express a regex comparison",
          suggestions: [],
          disabled: false,
        });
      }
      return "-- content ~ /x/"; // `tql_macro` for the save, `tql` for the pane
    });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- content ~ /x/");
      await vi.waitFor(() => expect(doc.byId.query.raw).toBe("{{tine-query -- content ~ /x/}}"));
      expect(savePrintDialects(print)).toEqual(["og", "tql_macro"]);
      // The user is never shown a refusal for a query Tine can perfectly well
      // store.
      expect(root.querySelector(".query-print-refused")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("writes nothing and renders the printer's own message for any other refusal (I-9, I-4)", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    // Only the MACRO print refuses. The pane keeps showing the query's text, or
    // there would be nothing to type into and the refusal would be untestable.
    vi.spyOn(backend(), "printQuery").mockImplementation(async (_query, _view, dialect: string) => {
      if (dialect === "tql") return "-- task TODO";
      throw new QueryPrintRefusedError("syntax", {
        kind: "syntax",
        message: "a `}}` in the query would end the macro",
        suggestions: [],
        disabled: false,
      });
    });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const before = doc.byId.query.raw;
      await saveThroughPane(root, "-- content = '}}'");
      expect(doc.byId.query.raw).toBe(before);
      const refusal = await vi.waitFor(() => {
        const found = root.querySelector(".query-print-refused");
        if (!found) throw new Error("no refusal was shown");
        return found;
      });
      expect(refusal.getAttribute("role")).toBe("alert");
      expect(refusal.textContent).toContain("a `}}` in the query would end the macro");
    } finally {
      dispose();
    }
  });
});

/** §4.3 "Directive migration" (Q15, T4).
 *
 *  A block that STAYS `{{query}}` used to lose every view edit that OG cannot
 *  say and silently duplicate the ones it can: `writeViewProperties` fired only
 *  on the crossing to `{{tine-query}}`, and the OG printer re-emitted
 *  `(aggregate …)`/`(group-by …)` into the text. Now every save writes all six
 *  §7.6 properties for both macro names, aggregates and grouping leave the DSL
 *  text, and `sort-by`/`sample` stay in the text as well as in the properties
 *  because OG itself reads those.
 */
describe("B6: directive migration for blocks that stay {{query}}", () => {
  /** A parse whose view is `view`, so a save has something to persist. */
  function parseWithView(view: ViewSettings): void {
    vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) => ({
      query: parsedAs(text).query,
      view,
    }));
  }

  it("writes tine.col-aggregates on a NON-crossing save and leaves (aggregate …) out of the text", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task DONE)");
    parseWithView({ aggregates: [["", "count"], ["hours", "sum"]], group_by: "status" });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() =>
        expect(blockProperty("query", "tine.col-aggregates")).toBe("count;hours=sum"),
      );
      expect(blockProperty("query", "tine.group-by")).toBe("status");
      // The block stayed `{{query}}` — the migration is about WHERE the view
      // lives, not about the macro name.
      expect(doc.byId.query.raw).toContain("{{query (task DONE)}}");
      expect(doc.byId.query.raw).not.toContain("(aggregate");
      expect(doc.byId.query.raw).not.toContain("(group-by");
    } finally {
      dispose();
    }
  });

  it("keeps (sort-by a desc) in the OG text AND gains tine.sort:: a desc", async () => {
    load('{{query (task TODO) (sort-by a desc)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task TODO) (sort-by a desc)");
    parseWithView({ sort: [["a", "desc"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task TODO");
      await vi.waitFor(() => expect(blockProperty("query", "tine.sort")).toBe("a desc"));
      // Q15: OG reads `sort-by`, so it is re-emitted as well as persisted.
      expect(doc.byId.query.raw).toContain("(sort-by a desc)");
    } finally {
      dispose();
    }
  });

  it("drops tine.sort when the sort is removed, so the removal survives a reparse", async () => {
    load('{{query (task TODO)}}\ntine.sort:: a desc');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task DONE)");
    // The builder removed the sort: the session's view no longer carries one.
    parseWithView({});

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      expect(blockProperty("query", "tine.sort")).toBe("a desc");
      await saveThroughPane(root, "-- task DONE");
      // `tine.*` has ABSOLUTE precedence over the DSL text, so a stale property
      // line would put the removed sort straight back on the next parse.
      await vi.waitFor(() => expect(blockProperty("query", "tine.sort")).toBeNull());
      expect(doc.byId.query.raw).not.toContain("tine.sort");
    } finally {
      dispose();
    }
  });

  it("persists all six §7.6 fields, so the block re-parses to the saved view", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task DONE)");
    const saved: ViewSettings = {
      view: "table",
      sort: [["a", "desc"]],
      group_by: "status",
      columns: ["a", "b"],
      aggregates: [["hours", "sum"]],
      sample: 20,
    };
    parseWithView(saved);

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.view")).toBe("table"));
      // `tine.fields::` and `tine.view::` are the two the reader consumes and
      // nobody wrote: before T4 a saved column set or view kind was simply lost.
      expect(blockProperty("query", "tine.fields")).toBe("a;b");
      expect(blockProperty("query", "tine.sort")).toBe("a desc");
      expect(blockProperty("query", "tine.group-by")).toBe("status");
      expect(blockProperty("query", "tine.col-aggregates")).toBe("hours=sum");
      expect(blockProperty("query", "tine.sample")).toBe("20");
      // The properties the block now carries are exactly the ones the engine is
      // handed back on the next parse (§4.1 precedence merge).
      const properties = vi.mocked(backend().parseQuery).mock.calls.at(-1)![2] ?? [];
      const asMap = Object.fromEntries(properties);
      expect(asMap["tine.view"]).toBe("table");
      expect(asMap["tine.fields"]).toBe("a;b");
    } finally {
      dispose();
    }
  });

  it("leaves an untouched block byte-identical (no save, no property lines)", async () => {
    load('{{query (task TODO)}}');
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task TODO)");
    parseWithView({ sort: [["a", "desc"]], aggregates: [["", "count"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const before = doc.byId.query.raw;
      await vi.waitFor(() => expect(root.querySelector(".query-block")).not.toBeNull());
      await settle();
      // I-4: rendering a query is not editing it. Nothing is written until the
      // user saves.
      expect(doc.byId.query.raw).toBe(before);
    } finally {
      dispose();
    }
  });

  it("writes nothing at all when the page is not writable", async () => {
    load('{{query (task TODO)}}', { readOnly: true });
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(groups()));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
    vi.spyOn(backend(), "printQuery").mockResolvedValue("(task DONE)");
    parseWithView({ sort: [["a", "desc"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const before = doc.byId.query.raw;
      await saveThroughPane(root, "-- task DONE").catch(() => undefined);
      await settle();
      // The whole save is one undo unit now, and `withUndoUnit` returns without
      // running its body on a non-writable page — so the macro rewrite and the
      // property writes are refused TOGETHER rather than half-applied.
      expect(doc.byId.query.raw).toBe(before);
      expect(blockProperty("query", "tine.sort")).toBeNull();
    } finally {
      dispose();
    }
  });
});
