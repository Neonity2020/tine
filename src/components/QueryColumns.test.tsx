// **Columns are not schema** (P5A).
//
// `tine.fields::` used to carry two unrelated things on a query block: the typed
// sheet schema, and the list of columns to show. Two live producers wrote it
// from opposite directions and each destroyed the other's value —
//
//   * `Macro.tsx::writeViewProperties` wrote `(view.columns ?? []).join(";")`
//     into `tine.fields` on EVERY save, so any filter edit replaced a declared
//     `cost=number;severity=text` schema with a bare list, or removed it;
//   * `SheetTable.tsx::writeSchemaFields` wrote `serializeFields(...)` into the
//     same key from `declareField` / `reorderFieldHeader`, so declaring a schema
//     replaced the block's column list.
//
// The visible columns now live in `tine.columns::` and the typed schema stays in
// `tine.fields::`. This file is the evidence for both clobber directions, for
// the reader's precedence at the renderer, and for the bytes an authored note
// keeps in both formats.
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { Block } from "./Block";
import { ContextMenu } from "./ContextMenu";
import { SheetTable } from "./SheetTable";
import { initParser } from "../render/parse";
import { backend } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import {
  blockProperty,
  doc,
  redo,
  resetStore,
  setDoc,
  undo,
  type FeedPage,
  type Node as StoreNode,
} from "../store";
import type { Format } from "../render/ast";
import type { BlockDto, RefGroup } from "../types";
import type { ParsedQuery, ViewSettings } from "../editor/queryIr";
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

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
async function settle(): Promise<void> {
  for (let i = 0; i < 4; i += 1) await tick();
}

function page(roots: string[], format: Format = "md"): FeedPage {
  return {
    name: "Sheet", kind: "page", title: "Sheet", preBlock: null,
    roots, format, readOnly: false, guide: false,
  };
}

function node(id: string, raw: string): StoreNode {
  return { id, raw, collapsed: false, parent: null, page: "Sheet", children: [] };
}

/** Result rows as the BACKEND ships them: a `BlockDto` with its properties
 *  already computed off the Rust lsdoc projection, which is what the query table
 *  renders from. Not a hand-built row record. */
function resultBlock(id: string, raw: string, properties: [string, string][]): BlockDto {
  return { id, raw, collapsed: false, children: [], properties };
}

function resultGroups(): RefGroup[] {
  return [{
    page: "Tracker",
    kind: "page",
    blocks: [
      resultBlock("r1", "Refresh Guide examples", [["cost", "10"], ["severity", "high"], ["owner", "Avery"]]),
      resultBlock("r2", "Publish the demo", [["cost", "4"], ["severity", "low"], ["owner", "Jules"]]),
    ],
  }];
}

function load(raw: string, format: Format = "md"): void {
  setDoc({
    byId: { query: node("query", raw) },
    pages: [page(["query"], format)],
    feed: ["Sheet"],
    loaded: true,
  });
}

/** A parse answer for arbitrary pane text: a `raw` capsule, which is what a
 *  reader that retained text without interpreting it honestly returns. Same
 *  shape the P4 seam suite uses. */
function parsedAs(text: string): ParsedQuery {
  return {
    query: {
      anchor: "block",
      filter: { kind: "raw", text, diagnostic_kind: "not_applicable" },
      diagnostics: [],
      source: { kind: "tql", original: text, og_options: "" },
    },
    view: {},
  } as unknown as ParsedQuery;
}

async function openSheet(root: HTMLElement): Promise<HTMLElement> {
  const gear = await vi.waitFor(() => {
    const found = root.querySelector<HTMLButtonElement>(".qs-gear");
    if (!found) throw new Error("the query sentence never appeared");
    return found;
  });
  if (!document.querySelector(".qs-sheet")) gear.click();
  return await vi.waitFor(() => {
    const sheet = document.querySelector<HTMLElement>(".qs-sheet");
    if (!sheet) throw new Error("the sheet never opened");
    return sheet;
  });
}

/** Drive one save through the text pane: open the sheet, type valid text, click
 *  "Save query text". The session's VIEW is the one the block's own parse
 *  returned, which is what `stubSave` fixes. */
async function saveThroughPane(root: HTMLElement, text: string): Promise<void> {
  const sheet = await openSheet(root);
  const input = await vi.waitFor(() => {
    const found = sheet.querySelector<HTMLTextAreaElement>(".query-text-pane-input");
    if (!found) throw new Error("the pane has no input");
    return found;
  });
  input.value = text;
  input.dispatchEvent(new Event("input", { bubbles: true }));
  const save = await vi.waitFor(() => {
    const button = document.querySelector<HTMLButtonElement>(".query-text-pane-save");
    if (!button || button.disabled) throw new Error("save is not enabled yet");
    return button;
  });
  save.click();
  await settle();
}

function stubSave(view: ViewSettings, printed = "(task DONE)"): void {
  vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult(resultGroups()));
  vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(true);
  vi.spyOn(backend(), "printQuery").mockResolvedValue(printed);
  vi.spyOn(backend(), "parseQuery").mockImplementation(async (text: string) => ({
    query: parsedAs(text).query,
    view,
  }));
}

describe("clobber direction 1: a query save must not touch the typed schema", () => {
  it("keeps a declared tine.fields schema across an unrelated filter save", async () => {
    // FAIL-BEFORE: `writeViewProperties` wrote the view's columns into
    // `tine.fields`, so this save replaced `cost=number;severity=text` with the
    // bare list — or, with no columns, deleted the schema outright.
    load('{{query (task TODO)}}\ntine.fields:: cost=number;severity=text');
    stubSave({ columns: ["cost"], sort: [["cost", "desc"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.sort")).toBe("cost desc"));
      expect(blockProperty("query", "tine.fields")).toBe("cost=number;severity=text");
      expect(blockProperty("query", "tine.columns")).toBe("cost");
    } finally {
      dispose();
    }
  });

  it("preserves widths, filter, formulas and unknown keys through a save", async () => {
    load([
      "{{query (task TODO)}}",
      "tine.fields:: cost=number",
      "tine.table-widths:: cost=120",
      "tine.header:: true",
      "tine.filter:: cost > 1",
      "tine.formula.effort:: cost * 2",
      "reviewed-by:: Martin",
    ].join("\n"));
    stubSave({ sample: 5 });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.sample")).toBe("5"));
      expect(blockProperty("query", "tine.fields")).toBe("cost=number");
      expect(blockProperty("query", "tine.table-widths")).toBe("cost=120");
      expect(blockProperty("query", "tine.header")).toBe("true");
      expect(blockProperty("query", "tine.filter")).toBe("cost > 1");
      expect(blockProperty("query", "tine.formula.effort")).toBe("cost * 2");
      expect(blockProperty("query", "reviewed-by")).toBe("Martin");
    } finally {
      dispose();
    }
  });

  it("retires a PROVEN legacy bare list when the save states new columns, in one undo unit", async () => {
    load('{{query (task TODO)}}\ntine.fields:: cost;severity');
    stubSave({ columns: ["cost"] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const before = doc.byId.query.raw;
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.columns")).toBe("cost"));
      // Clearing must not be able to resurrect the old list, so the list the new
      // key replaces does not survive beside it.
      expect(blockProperty("query", "tine.fields")).toBeNull();
      // ONE undo unit for the whole save: the macro rewrite, the new key and the
      // retirement come back together.
      undo();
      expect(doc.byId.query.raw).toBe(before);
      redo();
      expect(blockProperty("query", "tine.columns")).toBe("cost");
    } finally {
      dispose();
    }
  });

  it("does not migrate a legacy list on an unrelated filter edit", async () => {
    load('{{query (task TODO)}}\ntine.fields:: cost;severity');
    // The legacy list IS what this block currently spells for columns, so the
    // effective columns are unchanged and there is nothing to write.
    stubSave({ columns: ["cost", "severity"], sample: 3 });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.sample")).toBe("3"));
      expect(blockProperty("query", "tine.fields")).toBe("cost;severity");
      expect(blockProperty("query", "tine.columns")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("persists an OG-only grouping and aggregate that the reprint is about to drop", async () => {
    // The corrected baseline: not "did the user touch grouping", but "does the
    // property already spell it". `og_view` re-emits only sort-by and sample.
    load('{{query (task TODO) (group-by status) (aggregate count)}}');
    // What the engine hands back for that directive on a LIST face: the ordinary
    // property `status`, spelled canonically (P5B).
    stubSave({ group_by: "prop:status", aggregates: [["", "count"]] }, "(task DONE)");

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.group-field")).toBe("prop:status"));
      expect(blockProperty("query", "tine.col-aggregates")).toBe("count");
    } finally {
      dispose();
    }
  });

  it("edits recognized aggregate segments while keeping a table-only one verbatim", async () => {
    load('{{query (task TODO)}}\ntine.col-aggregates:: count;estimate=median');
    stubSave({ aggregates: [["", "count"], ["hours", "sum"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() =>
        expect(blockProperty("query", "tine.col-aggregates")).toBe("count;estimate=median;hours=sum"),
      );
    } finally {
      dispose();
    }
  });

  it("writes nothing at all when every persisted fact already agrees", async () => {
    load('{{query (task TODO)}}\ntine.sort:: cost desc\ntine.fields:: cost=number');
    stubSave({ sort: [["cost", "desc"]] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(doc.byId.query.raw).toContain("{{query (task DONE)}}"));
      expect(doc.byId.query.raw).toBe(
        "{{query (task DONE)}}\ntine.sort:: cost desc\ntine.fields:: cost=number",
      );
    } finally {
      dispose();
    }
  });

  it("keeps a query block's bytes identical in ORG too", async () => {
    load(
      ":PROPERTIES:\n:tine.fields: cost=number\n:END:\n{{query (task TODO)}}",
      "org",
    );
    stubSave({ columns: ["cost"] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      await saveThroughPane(root, "-- task DONE");
      await vi.waitFor(() => expect(blockProperty("query", "tine.columns")).toBe("cost"));
      // The org drawer is the property home in both directions: the schema is
      // still a drawer line, and the new key joined it rather than being
      // appended as visible body text.
      expect(blockProperty("query", "tine.fields")).toBe("cost=number");
      expect(doc.byId.query.raw).toContain(":tine.columns: cost");
      expect(doc.byId.query.raw).not.toContain("tine.columns::");
    } finally {
      dispose();
    }
  });

  it("renders a query without writing anything (I-4)", async () => {
    load('{{query (task TODO)}}\ntine.fields:: cost;severity\ntine.view:: table');
    stubSave({ columns: ["cost", "severity"] });

    const { root, dispose } = mount(() => <Block id="query" />);
    try {
      const before = doc.byId.query.raw;
      await vi.waitFor(() => expect(root.querySelector(".query-block")).not.toBeNull());
      await settle();
      expect(doc.byId.query.raw).toBe(before);
    } finally {
      dispose();
    }
  });
});

describe("clobber direction 2: declaring a schema must not destroy the column list", () => {
  function mountTable(raw: string) {
    setDoc({
      byId: { query: node("query", raw) },
      pages: [page(["query"])],
      feed: ["Sheet"],
      loaded: true,
    });
    return mount(() => (
      <>
        <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} />
        <ContextMenu />
      </>
    ));
  }

  /** Declare a schema through the field header's own context menu — the route
   *  the user actually takes. */
  const declareThroughHeader = (root: HTMLElement, label: string): void => {
    const header = [...root.querySelectorAll<HTMLElement>(".sheet-field-header")]
      .find((el) => (el.textContent ?? "").trim() === label);
    expect(header, `a header for ${label}`).toBeTruthy();
    header!.dispatchEvent(
      new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 20, clientY: 30 }),
    );
    const item = [...document.querySelectorAll<HTMLElement>(".ctx-item")]
      .find((el) => (el.textContent ?? "").trim() === "Declare field (text)");
    expect(item, "a Declare field action").toBeTruthy();
    item!.click();
  };

  it("does not read a pre-split bare list as a declared (empty) schema", () => {
    // FAIL-BEFORE: `schemaHome` was non-null whenever the property existed, so
    // a bare list parsed to an EMPTY schema — every column a stray, header
    // reordering dead, and the next schema write silently replacing the list.
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.fields:: cost;severity');
    try {
      expect(root.querySelectorAll(".sheet-col-stray")).toHaveLength(0);
    } finally {
      dispose();
    }
  });

  it("keeps an ordinary children table's existing schema interpretation", () => {
    const owner = node("query", "Rows\ntine.fields:: cost");
    owner.children = ["child"];
    const child = node("child", "Work\ncost:: 5");
    child.parent = "query";
    setDoc({ byId: { query: owner, child }, pages: [page(["query"])], feed: ["Sheet"], loaded: true });
    const { root, dispose } = mount(() => <SheetTable ownerId="query" rowSource="children" />);
    try {
      expect(root.querySelectorAll(".sheet-col-stray").length).toBeGreaterThan(0);
      expect(blockProperty("query", "tine.fields")).toBe("cost");
      expect(blockProperty("query", "tine.columns")).toBeNull();
    } finally { dispose(); }
  });

  it("keeps a typed schema readable as a schema", () => {
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.fields:: cost=number');
    try {
      // `severity` and `owner` are observed but not declared: with a real schema
      // home they ARE strays, which is the pre-existing behaviour this change
      // must not disturb.
      expect(root.querySelectorAll(".sheet-col-stray").length).toBeGreaterThan(0);
    } finally {
      dispose();
    }
  });

  it("rescues a legacy list into tine.columns when a schema is declared over it", () => {
    // FAIL-BEFORE: `writeSchemaFields` replaced the bare list with the declared
    // schema, and the note's column choice was simply gone.
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.fields:: cost;severity');
    try {
      const before = doc.byId.query.raw;
      declareThroughHeader(root, "cost");

      // The declaration landed in the schema key…
      expect(blockProperty("query", "tine.fields")).toContain("cost=text");
      // …and the column list it displaced moved to the new key rather than
      // being overwritten.
      expect(blockProperty("query", "tine.columns")).toBe("cost;severity");
      // ONE undo unit: the rescue and the declaration are one user action, and
      // `declareFreshSchema` + the write used to push two separate entries.
      undo();
      expect(doc.byId.query.raw).toBe(before);
    } finally {
      dispose();
    }
  });

  it("undo restores both pages when schema declaration also rescues query columns", () => {
    const raw = "{{query (task TODO)}}\ntine.fields:: cost;severity";
    const schema = { ...page([]), name: "Schema", title: "Schema", preBlock: "tine.fields:: severity=text" };
    setDoc({ byId: { query: node("query", raw) }, pages: [page(["query"]), schema], feed: ["Sheet"], loaded: true });
    const { root, dispose } = mount(() => <><SheetTable ownerId="query" rowSource="query" schemaPage="Schema" groups={resultGroups()} /><ContextMenu /></>);
    try {
      declareThroughHeader(root, "cost");
      expect(blockProperty("query", "tine.columns")).toBe("cost;severity");
      undo();
      expect(doc.byId.query.raw).toBe(raw);
      expect(doc.pages.find(p => p.name === "Schema")?.preBlock).toBe("tine.fields:: severity=text");
    } finally { dispose(); }
  });

  it("never overwrites a PRESENT columns property with a legacy rescue", () => {
    const { root, dispose } = mountTable(
      '{{query (task TODO)}}\ntine.columns:: owner\ntine.fields:: cost;severity',
    );
    try {
      declareThroughHeader(root, "owner");
      // Its explicit presence wins: the rescue must not replace the user's own
      // stated selection with the retired list.
      expect(blockProperty("query", "tine.columns")).toBe("owner");
      expect(blockProperty("query", "tine.fields")).toContain("owner=text");
    } finally {
      dispose();
    }
  });
});

describe("the query table shows the selected columns", () => {
  function mountTable(raw: string) {
    setDoc({
      byId: { query: node("query", raw) },
      pages: [page(["query"])],
      feed: ["Sheet"],
      loaded: true,
    });
    return mount(() => <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} />);
  }

  const headerLabels = (root: HTMLElement): string[] =>
    [...root.querySelectorAll<HTMLElement>(".sheet-field-header")]
      .map((el) => (el.textContent ?? "").trim());

  it("renders exactly the selected columns, in the selected order", () => {
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.columns:: severity;cost');
    try {
      expect(headerLabels(root)).toEqual(["severity", "cost"]);
    } finally {
      dispose();
    }
  });

  it("renders a selected column no row carries, as empty cells", () => {
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.columns:: cost;absent');
    try {
      expect(headerLabels(root)).toEqual(["cost", "absent"]);
      const cells = [...root.querySelectorAll<HTMLElement>(".sheet-cell")]
        .filter((el) => el.classList.contains("sheet-field-cell"));
      expect(cells.length).toBeGreaterThan(0);
    } finally {
      dispose();
    }
  });

  it("applies the selection AFTER the schema lookup, so declared types survive", () => {
    const { root, dispose } = mountTable(
      '{{query (task TODO)}}\ntine.columns:: cost\ntine.fields:: cost=number;severity=text',
    );
    try {
      expect(headerLabels(root)).toEqual(["cost"]);
      // The hidden `severity` declaration is still in the note: hiding a column
      // is not deleting its definition.
      expect(blockProperty("query", "tine.fields")).toBe("cost=number;severity=text");
    } finally {
      dispose();
    }
  });

  it("reads a pre-split bare list as the selection when the new key is absent", () => {
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.fields:: owner;cost');
    try {
      expect(headerLabels(root)).toEqual(["owner", "cost"]);
      expect(doc.byId.query.raw).toContain("tine.fields:: owner;cost");
    } finally {
      dispose();
    }
  });

  it("falls back to the DEFAULT columns for a present-but-empty selection, never to the legacy list", () => {
    const { root, dispose } = mountTable(
      '{{query (task TODO)}}\ntine.columns::\ntine.fields:: owner',
    );
    try {
      const labels = headerLabels(root);
      expect(labels).toContain("cost");
      expect(labels).toContain("severity");
      expect(labels).toContain("owner");
    } finally {
      dispose();
    }
  });

  it("deduplicates repeated names without rewriting the note", () => {
    const { root, dispose } = mountTable('{{query (task TODO)}}\ntine.columns:: cost;cost');
    try {
      expect(headerLabels(root)).toEqual(["cost"]);
      expect(doc.byId.query.raw).toContain("tine.columns:: cost;cost");
    } finally {
      dispose();
    }
  });
});
