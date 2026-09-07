// **One writer for a query's display, wherever the change is made** (P5B).
//
// The query table's header sorts, its column order and its aggregate footer, and
// the query board's grouping dropdown and context menu, all used to reach for a
// property of their own — the SHEET's. So a query board grouped by
// `tine.group-by` while the query saved `tine.group-field`, a header reorder
// rewrote `tine.fields` (the typed schema) instead of the column list, and the
// footer offered seventeen functions of which the query reader understands
// three.
//
// Every one of them now hands the change back through the host's
// `QueryDisplayControl` / `QueryGroupingControl`, which runs the same
// `queryViewPropertyPatch` a filter save runs. This file is the evidence, and it
// also pins that an ordinary CHILDREN-backed sheet is untouched by all of it.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { createSignal, type JSX } from "solid-js";
import { SheetTable } from "./SheetTable";
import { SheetBoard } from "./SheetBoard";
import { ContextMenu } from "./ContextMenu";
import { initParser } from "../render/parse";
import { blockProperty, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import { setToasts, toasts } from "../ui";
import type { BlockDto, RefGroup } from "../types";
import type { ViewSettings } from "../editor/queryIr";
import type { QueryDisplayControl } from "../editor/queryViewProperties";
import { queryColumnName, type FieldId, type QueryGroupingControl } from "../sheet/fields";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
  setToasts([]);
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

function page(roots: string[]): FeedPage {
  return {
    name: "Sheet", kind: "page", title: "Sheet", preBlock: null,
    roots, format: "md", readOnly: false, guide: false,
  };
}

function node(id: string, raw: string, children: string[] = []): StoreNode {
  return { id, raw, collapsed: false, parent: null, page: "Sheet", children };
}

function resultBlock(id: string, raw: string, properties: [string, string][]): BlockDto {
  return { id, raw, collapsed: false, children: [], properties };
}

function resultGroups(): RefGroup[] {
  return [{
    page: "Tracker",
    kind: "page",
    blocks: [
      resultBlock("r1", "TODO Refresh the Guide", [["cost", "10"], ["status", "open"]]),
      resultBlock("r2", "DONE Publish the demo", [["cost", "4"], ["status", "done"]]),
    ],
  }];
}

function load(raw: string): void {
  setDoc({ byId: { query: node("query", raw) }, pages: [page(["query"])], feed: ["Sheet"], loaded: true });
}

/** A host that records what the surface asked it to save, exactly as `Macro`
 *  would receive it. */
function displayHarness(initial: ViewSettings) {
  const [view, setView] = createSignal<ViewSettings>(initial);
  const applied: ViewSettings[] = [];
  const control: QueryDisplayControl = {
    get view() {
      return view();
    },
    apply: (next) => {
      applied.push(next);
      setView(next);
    },
  };
  return { control, applied, view };
}

function pointer(type: string, x: number, y: number): Event {
  return new MouseEvent(type, { bubbles: true, cancelable: true, button: 0, clientX: x, clientY: y });
}

/** The header drag, as `SheetTable.test.tsx` drives it: jsdom has no layout, so
 *  the drop target is stated through `elementFromPoint`. */
function dragFieldHeader(source: HTMLElement, target: HTMLElement, before = true): void {
  const elementFromPoint = document.elementFromPoint;
  document.elementFromPoint = () => target;
  try {
    source.dispatchEvent(pointer("pointerdown", 0, 0));
    const x = before ? -10 : 10;
    window.dispatchEvent(pointer("pointermove", x, 0));
    window.dispatchEvent(pointer("pointerup", x, 0));
  } finally {
    document.elementFromPoint = elementFromPoint;
  }
}

const headers = (root: HTMLElement) =>
  [...root.querySelectorAll<HTMLElement>(".sheet-field-header")];

/** The Block column. Sorting by it is a real thing to want and a thing the note
 *  cannot carry, which is exactly the case the label exists for. */
const titleHeader = (root: HTMLElement) =>
  root.querySelector<HTMLElement>(".sheet-title-header")!;

const headerFor = (root: HTMLElement, field: string) => {
  const found = headers(root).find((element) => element.dataset.sheetField === field);
  if (!found) throw new Error(`no header for ${field}`);
  return found;
};

describe("the query table's header sort", () => {
  it("routes a backend-sortable column to the SAVED sort", () => {
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table" });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      headerFor(root, "prop:cost").click();
      expect(h.applied.at(-1)?.sort).toEqual([["cost", "asc"]]);
      headerFor(root, "prop:cost").click();
      expect(h.applied.at(-1)?.sort).toEqual([["cost", "desc"]]);
      // Third click clears it, the same three-step cycle a local sort has.
      headerFor(root, "prop:cost").click();
      expect(h.applied.at(-1)?.sort).toEqual([]);
    } finally {
      dispose();
    }
  });

  it("keeps a sort the note cannot carry local, and says so", () => {
    // FAIL-BEFORE: every header sort was local and silent, so a query table
    // sorted by `cost` looked saved and came back unsorted.
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table" });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      titleHeader(root).click();
      expect(h.applied).toEqual([]);
      const label = root.querySelector(".sheet-table-only-sort");
      expect(label?.textContent).toContain("Table-only sort: Title");
      // And it can be dropped from the label itself.
      (label as HTMLElement).click();
      expect(root.querySelector(".sheet-table-only-sort")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("drops a table-only arrangement when a NEW result revision arrives", () => {
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table" });
    const [groups, setGroups] = createSignal(resultGroups());
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={groups()} queryDisplay={h.control} />
    ));
    try {
      titleHeader(root).click();
      expect(root.querySelector(".sheet-table-only-sort")).not.toBeNull();
      setGroups(resultGroups());
      expect(root.querySelector(".sheet-table-only-sort")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("drops a table-only arrangement when a sort is SAVED", () => {
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table" });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      titleHeader(root).click();
      expect(root.querySelector(".sheet-table-only-sort")).not.toBeNull();
      headerFor(root, "prop:cost").click();
      expect(root.querySelector(".sheet-table-only-sort")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("leaves a query table with NO display control sorted as the user left it", () => {
    // FAIL-BEFORE: the revision reset keyed off `rowSource`, so the tag page's
    // reference table — query-sourced, with no saved sort to be second to — lost
    // the user's column sort every time its references refreshed.
    load("{{query (task TODO)}}\ntine.view:: table");
    const [groups, setGroups] = createSignal(resultGroups());
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={groups()} />
    ));
    try {
      headerFor(root, "prop:cost").click();
      expect(headerFor(root, "prop:cost").textContent).toContain("▲");
      setGroups(resultGroups());
      expect(headerFor(root, "prop:cost").textContent).toContain("▲");
    } finally {
      dispose();
    }
  });

  it("leaves an ordinary children table's local sort exactly as it was", () => {
    const owner = node("table", "Rows\ntine.view:: table", ["c1", "c2"]);
    setDoc({
      byId: {
        table: owner,
        c1: { ...node("c1", "TODO One\ncost:: 3"), parent: "table" },
        c2: { ...node("c2", "DONE Two\ncost:: 1"), parent: "table" },
      },
      pages: [page(["table"])],
      feed: ["Sheet"],
      loaded: true,
    });
    const { root, dispose } = mount(() => <SheetTable ownerId="table" rowSource="children" />);
    try {
      headerFor(root, "prop:cost").click();
      // No query control: no label, no property write, just the local sort.
      expect(root.querySelector(".sheet-table-only-sort")).toBeNull();
      expect(blockProperty("table", "tine.sort")).toBeNull();
      expect(headerFor(root, "prop:cost").textContent).toContain("▲");
    } finally {
      dispose();
    }
  });
});

describe("the query table's aggregate footer", () => {
  it("offers the QUERY's three functions, not the sheet's seventeen", () => {
    // FAIL-BEFORE: the cell wrote through `setColumnAggregate`, whose vocabulary
    // has fourteen names the query reader does not understand and no `avg`.
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table", aggregates: [["cost", "sum"]] });
    const { root, dispose } = mount(() => (
      <>
        <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
        <ContextMenu />
      </>
    ));
    try {
      const cell = root.querySelector<HTMLButtonElement>(".sheet-aggregate-value")!;
      // The number is the QUERY summary's: 10 + 4.
      expect(cell.textContent?.trim()).toBe("14");
      cell.click();
      const labels = [...document.querySelectorAll<HTMLElement>(".ctx-item")].map((element) =>
        (element.textContent ?? "").replace("✓ ", "").trim(),
      );
      expect(labels).toEqual(["None", "Count", "Sum", "Average"]);
      const average = [...document.querySelectorAll<HTMLElement>(".ctx-item")].find(
        (element) => (element.textContent ?? "").trim() === "Average",
      )!;
      average.click();
      expect(h.applied.at(-1)?.aggregates).toEqual([["cost", "avg"]]);
      // The sheet's own writer was never reached.
      expect(blockProperty("query", "tine.col-aggregates")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("edits one column's function IN PLACE, keeping the list's order", () => {
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({
      view: "table",
      aggregates: [["", "count"], ["cost", "sum"], ["status", "count"]],
    });
    const { root, dispose } = mount(() => (
      <>
        <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
        <ContextMenu />
      </>
    ));
    try {
      const cells = [...root.querySelectorAll<HTMLButtonElement>(".sheet-aggregate-value")];
      cells[0].click();
      [...document.querySelectorAll<HTMLElement>(".ctx-item")]
        .find((element) => (element.textContent ?? "").trim() === "Count")!
        .click();
      expect(h.applied.at(-1)?.aggregates).toEqual([
        ["", "count"],
        ["cost", "count"],
        ["status", "count"],
      ]);
    } finally {
      dispose();
    }
  });

  it("aggregates a property named like a builtin, which the COLUMNS grammar reserves", () => {
    // FAIL-BEFORE: the footer asked the columns grammar whether it could spell
    // the key, and that grammar reserves the six builtin names — so a note
    // saying `state=count`, about an ordinary property named `state`, rendered
    // no footer cell at all and could not be edited or removed from the table.
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table", aggregates: [["state", "count"]] });
    const groups: RefGroup[] = [{
      page: "Tracker",
      kind: "page",
      blocks: [
        resultBlock("r1", "TODO Refresh the Guide", [["state", "open"], ["cost", "10"]]),
        resultBlock("r2", "DONE Publish the demo", [["state", "done"], ["cost", "4"]]),
      ],
    }];
    const { root, dispose } = mount(() => (
      <>
        <SheetTable ownerId="query" rowSource="query" groups={groups} queryDisplay={h.control} />
        <ContextMenu />
      </>
    ));
    try {
      const cells = [...root.querySelectorAll<HTMLButtonElement>(".sheet-aggregate-value")];
      expect(cells.map((cell) => cell.textContent?.trim())).toEqual(["2"]);
      cells[0].click();
      [...document.querySelectorAll<HTMLElement>(".ctx-item")]
        .find((element) => (element.textContent ?? "").trim() === "Sum")!
        .click();
      // The key stays the LITERAL property name; the task marker keeps no key.
      expect(h.applied.at(-1)?.aggregates).toEqual([["state", "sum"]]);
    } finally {
      dispose();
    }
  });
});

describe("the query table's header reorder", () => {
  it("moves a visible column through tine.columns, never tine.fields", () => {
    // FAIL-BEFORE: a query header drag ran `writeSchemaFields`, which rewrites
    // `tine.fields` — the TYPED SCHEMA — and says nothing about visible order.
    load("{{query (task TODO)}}\ntine.view:: table\ntine.columns:: cost;status");
    const h = displayHarness({ view: "table", columns: ["cost", "status"] });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      dragFieldHeader(headerFor(root, "prop:status"), headerFor(root, "prop:cost"), true);
      expect(h.applied.at(-1)?.columns).toEqual(["status", "cost"]);
      expect(blockProperty("query", "tine.fields")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("refuses to state an order it cannot spell, rather than dropping a column", () => {
    // FAIL-BEFORE: the reorder wrote the REPRESENTABLE names only, and
    // `tine.columns` is a complete selection — so stating `status;cost;page`
    // around a formula column did not "keep its place", it HID it. The column
    // grammar has no token for a formula (P5A, unchanged in B), so the honest
    // answer is to leave the order alone and say why.
    //
    // No `tine.columns`, so every field the rows carry is visible — the formula
    // column included, which is the case this pins.
    load("{{query (task TODO)}}\ntine.view:: table\ntine.formula.effort:: cost * 2");
    const h = displayHarness({ view: "table" });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      // A formula column has no `tine.columns` token, so it is not draggable…
      expect(headerFor(root, "formula:effort").classList.contains("sheet-header-draggable")).toBe(false);
      // …and dragging the columns around it saves nothing at all.
      dragFieldHeader(headerFor(root, "prop:status"), headerFor(root, "prop:cost"), true);
      expect(h.applied).toEqual([]);
      expect(toasts().map((toast) => toast.message).join(" ")).toContain("computed column");
      // The column is still on screen, which is what "kept its place" has to
      // mean for the sentence to be true.
      expect(headerFor(root, "formula:effort")).toBeTruthy();
    } finally {
      dispose();
    }
  });

  it("keeps every visible column when an order IS stated", () => {
    // The complement: with no unrepresentable field in sight the order is saved,
    // and the saved list still covers every column that was on screen — a
    // reorder is an ORDER change, never a visibility change.
    load("{{query (task TODO)}}\ntine.view:: table");
    const h = displayHarness({ view: "table" });
    const { root, dispose } = mount(() => (
      <SheetTable ownerId="query" rowSource="query" groups={resultGroups()} queryDisplay={h.control} />
    ));
    try {
      const before = headers(root).map((header) => header.dataset.sheetField);
      dragFieldHeader(headerFor(root, "prop:status"), headerFor(root, "prop:cost"), true);
      const saved = h.applied.at(-1)?.columns ?? [];
      expect(saved).toEqual(["status", "cost", "page"]);
      expect([...saved].sort()).toEqual([...before.map((field) => queryColumnName(field as FieldId)!)].sort());
    } finally {
      dispose();
    }
  });
});

describe("the query board's grouping", () => {
  function boardHarness(field: FieldId | null, cleared = field === null) {
    const [current, setCurrent] = createSignal<FieldId | null>(field);
    const [isCleared, setCleared] = createSignal(cleared);
    const picked: (FieldId | null)[] = [];
    const control: QueryGroupingControl = {
      get field() {
        return current();
      },
      get cleared() {
        return isCleared();
      },
      options: ["state", "priority", "tags", "prop:status"],
      set: (next) => {
        picked.push(next);
        setCurrent(next);
        setCleared(next === null);
      },
    };
    return { control, picked, current };
  }

  it("tells an explicit clear from a grouping nothing states", () => {
    // FAIL-BEFORE: both arrived as `field: null` and both drew ONE ungrouped
    // column — so an ordinary `tine.view:: board` note with no grouping key,
    // which ADR 0030 has always drawn as a task-marker board, lost its columns.
    load("{{query (task TODO)}}\ntine.view:: board");
    const h = boardHarness(null, false);
    const { root, dispose } = mount(() => (
      <SheetBoard ownerId="query" rowSource="query" groups={resultGroups()} queryGrouping={h.control} />
    ));
    try {
      expect(root.textContent).not.toContain("All results");
      // The default the silence is filled with, shown as the current choice.
      expect(root.querySelector<HTMLSelectElement>(".sheet-board-groupby")!.value).toBe("state");
    } finally {
      dispose();
    }
  });

  it("routes the toolbar dropdown to the query's writer", () => {
    load("{{query (task TODO)}}\ntine.view:: board");
    const h = boardHarness("state");
    const { root, dispose } = mount(() => (
      <SheetBoard ownerId="query" rowSource="query" groups={resultGroups()} queryGrouping={h.control} />
    ));
    try {
      const select = root.querySelector<HTMLSelectElement>(".sheet-board-groupby")!;
      select.value = "prop:status";
      select.dispatchEvent(new Event("change", { bubbles: true }));
      expect(h.picked).toEqual(["prop:status"]);
      // The sheet's own `tine.group-by` writer was never reached.
      expect(blockProperty("query", "tine.group-by")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("offers an explicit no-grouping, which renders ONE column", () => {
    load("{{query (task TODO)}}\ntine.view:: board");
    const h = boardHarness("state");
    const { root, dispose } = mount(() => (
      <SheetBoard ownerId="query" rowSource="query" groups={resultGroups()} queryGrouping={h.control} />
    ));
    try {
      const select = root.querySelector<HTMLSelectElement>(".sheet-board-groupby")!;
      expect([...select.options].some((option) => option.value === "")).toBe(true);
      select.value = "";
      select.dispatchEvent(new Event("change", { bubbles: true }));
      expect(h.picked).toEqual([null]);
      const columns = root.querySelectorAll(".sheet-board-column");
      expect(columns).toHaveLength(1);
      expect(columns[0].textContent).toContain("All results");
    } finally {
      dispose();
    }
  });

  it("routes the CONTEXT MENU to the same writer as the dropdown", () => {
    // FAIL-BEFORE: the menu called `setBoardGroupBy`, which writes the sheet's
    // `tine.group-by`, while the dropdown beside it wrote the query's key.
    load("{{query (task TODO)}}\ntine.view:: board");
    const h = boardHarness("state");
    const { root, dispose } = mount(() => (
      <>
        <SheetBoard ownerId="query" rowSource="query" groups={resultGroups()} queryGrouping={h.control} />
        <ContextMenu />
      </>
    ));
    try {
      root.querySelector(".sheet-board")!.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 }),
      );
      const item = [...document.querySelectorAll<HTMLElement>(".ctx-submenu-menu .ctx-item")].find(
        (element) => (element.textContent ?? "").replace("✓ ", "").trim() === "status",
      );
      expect(item, "a Group by → status action").toBeTruthy();
      item!.click();
      expect(h.picked).toEqual(["prop:status"]);
      expect(blockProperty("query", "tine.group-by")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("offers No grouping in the context menu too", () => {
    load("{{query (task TODO)}}\ntine.view:: board");
    const h = boardHarness("state");
    const { root, dispose } = mount(() => (
      <>
        <SheetBoard ownerId="query" rowSource="query" groups={resultGroups()} queryGrouping={h.control} />
        <ContextMenu />
      </>
    ));
    try {
      root.querySelector(".sheet-board")!.dispatchEvent(
        new MouseEvent("contextmenu", { bubbles: true, clientX: 10, clientY: 10 }),
      );
      const item = [...document.querySelectorAll<HTMLElement>(".ctx-submenu-menu .ctx-item")].find(
        (element) => (element.textContent ?? "").replace("✓ ", "").trim() === "No grouping",
      );
      expect(item, "a Group by → No grouping action").toBeTruthy();
      item!.click();
      expect(h.picked).toEqual([null]);
    } finally {
      dispose();
    }
  });

  it("leaves an ordinary children board writing its own tine.group-by", () => {
    const owner = node("board", "Board\ntine.view:: board\ntine.group-by:: state", ["c1"]);
    setDoc({
      byId: { board: owner, c1: { ...node("c1", "TODO One\nstatus:: open"), parent: "board" } },
      pages: [page(["board"])],
      feed: ["Sheet"],
      loaded: true,
    });
    const { root, dispose } = mount(() => <SheetBoard ownerId="board" rowSource="children" groupBy="state" />);
    try {
      const select = root.querySelector<HTMLSelectElement>(".sheet-board-groupby")!;
      // A children board cannot be ungrouped, so it is not offered.
      expect([...select.options].some((option) => option.value === "")).toBe(false);
      select.value = "priority";
      select.dispatchEvent(new Event("change", { bubbles: true }));
      expect(blockProperty("board", "tine.group-by")).toBe("priority");
    } finally {
      dispose();
    }
  });
});
