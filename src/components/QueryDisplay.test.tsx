// **The inline Display panel states all six display facts** (P5B).
//
// Before this the inline surface could state one and a half of them. `+ sort`
// read `sort[0]` and wrote a ONE-element list back; `+ summarize` read
// `aggregates[0]` and did the same; the view kind had a separate switcher with
// its own writer, and the column list and the sample had no inline control at
// all. A note carrying two sorts or three aggregates lost the rest the first
// time anyone touched a pill.
//
// This file is the evidence that the panel edits the six as what they are — two
// enums, three ORDERED lists and a number — and that every change leaves through
// the one writer the host owns.

import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { createSignal, type JSX } from "solid-js";
import { QueryDisplay, displayFieldEntries, readSample, MAX_SAMPLE } from "./QueryDisplay";
import { initParser } from "../render/parse";
import { resetStore } from "../store";
import type { RegistryRow, ViewSettings } from "../editor/queryIr";
import type { RegistryAccess } from "./QuerySheet";
import { stubVocabularyGeometry } from "./QueryVocabularyPicker.test-helpers";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetStore();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

function registryRow(name: string, blocks: number): RegistryRow {
  return {
    normalized_name: name,
    display_name: name,
    count_blocks: blocks,
    count_pages: 0,
    observed_type: "text",
    declared_type: null,
    top_values: [],
  } as unknown as RegistryRow;
}

const ROWS = [registryRow("cost", 12), registryRow("severity", 4)];

function harness(initial: ViewSettings, rows = ROWS, retainedAggregates?: readonly string[]) {
  const [view, setView] = createSignal<ViewSettings>(initial);
  const applied: ViewSettings[] = [];
  const registry: RegistryAccess = {
    rows: () => rows,
    pending: () => false,
    failure: () => null,
    unavailable: () => false,
    request: () => {},
    retry: () => {},
  };
  const mounted = mount(() => (
    <QueryDisplay
      control={{
        get view() {
          return view();
        },
        apply: (next) => {
          applied.push(next);
          setView(next);
        },
        retainedAggregates,
      }}
      registry={registry}
      formulas={() => ["effort"]}
    />
  ));
  const trigger = mounted.root.querySelector<HTMLButtonElement>(".qd-trigger")!;
  trigger.click();
  const panel = () => document.querySelector<HTMLElement>(".qd-panel")!;
  return { ...mounted, panel, applied, view, trigger };
}

const section = (panel: HTMLElement, title: string): HTMLElement => {
  const found = [...panel.querySelectorAll<HTMLElement>(".qd-section")].find(
    (element) => element.querySelector(".qd-section-title")?.textContent?.trim() === title,
  );
  if (!found) throw new Error(`no Display section titled ${title}`);
  return found;
};

const rowLabels = (panel: HTMLElement, title: string) =>
  [...section(panel, title).querySelectorAll(".qd-row-label")].map((el) => el.textContent?.trim());

describe("the view kind", () => {
  it("is stated by one control, and a switch to Board fills only an UNSET grouping", () => {
    const h = harness({});
    const board = [...h.panel().querySelectorAll<HTMLButtonElement>(".qd-view")].find(
      (el) => el.textContent?.trim() === "Board",
    )!;
    board.click();
    expect(h.applied.at(-1)).toEqual({ view: "board", group_by: "state" });
    h.dispose();
  });

  it("shows the existing Board default without writing it on open", () => {
    const h = harness({ view: "board" });
    try {
      expect(rowLabels(h.panel(), "Group by")).toEqual(["State"]);
      expect(h.applied).toEqual([]);
    } finally { h.dispose(); }
  });

  it("does not reinstate the task marker over an explicit clear", () => {
    // `""` is the user saying "no grouping". A default that spoke over it is
    // how switching to Board used to undo a choice.
    const h = harness({ group_by: "" });
    const board = [...h.panel().querySelectorAll<HTMLButtonElement>(".qd-view")].find(
      (el) => el.textContent?.trim() === "Board",
    )!;
    board.click();
    expect(h.applied.at(-1)).toEqual({ view: "board", group_by: "" });
    h.dispose();
  });

  it("renders List as the active view when the setting is absent", () => {
    const h = harness({});
    expect(h.panel().querySelector(".qd-view.active")?.textContent?.trim()).toBe("List");
    h.dispose();
  });
});

describe("grouping", () => {
  it("shows the resolved field, and can clear it explicitly", () => {
    const h = harness({ group_by: "prop:status" });
    expect(section(h.panel(), "Group by").querySelector(".qd-row-label")?.textContent).toBe("status");
    const none = [...section(h.panel(), "Group by").querySelectorAll<HTMLButtonElement>(".qd-row-btn")]
      .find((el) => el.textContent?.trim() === "None")!;
    none.click();
    expect(h.applied.at(-1)?.group_by).toBe("");
    h.dispose();
  });

  it("distinguishes nothing-said from an explicit clear in what it shows", () => {
    const unset = harness({});
    expect(section(unset.panel(), "Group by").querySelector(".qd-row-label")?.textContent).toBe("Not set");
    unset.dispose();
    const cleared = harness({ group_by: "" });
    expect(section(cleared.panel(), "Group by").querySelector(".qd-row-label")?.textContent).toBe(
      "No grouping",
    );
    cleared.dispose();
  });
});

describe("the three ordered lists", () => {
  it("shows every sort, not just the first", () => {
    const h = harness({ sort: [["priority", "asc"], ["cost", "desc"]] });
    expect(rowLabels(h.panel(), "Sort")).toEqual(["priority", "cost"]);
    h.dispose();
  });

  it("reorders a sort without dropping its neighbours", () => {
    const h = harness({ sort: [["priority", "asc"], ["cost", "desc"], ["page", "asc"]] });
    const down = [...section(h.panel(), "Sort").querySelectorAll<HTMLButtonElement>(".qd-row-btn")].find(
      (el) => el.title === "Move down",
    )!;
    down.click();
    expect(h.applied.at(-1)?.sort).toEqual([["cost", "desc"], ["priority", "asc"], ["page", "asc"]]);
    h.dispose();
  });

  it("flips one sort's direction and leaves the others alone", () => {
    const h = harness({ sort: [["priority", "asc"], ["cost", "desc"]] });
    const flip = [...section(h.panel(), "Sort").querySelectorAll<HTMLButtonElement>(".qd-row-btn")].find(
      (el) => el.title === "Ascending",
    )!;
    flip.click();
    expect(h.applied.at(-1)?.sort).toEqual([["priority", "desc"], ["cost", "desc"]]);
    h.dispose();
  });

  it("removes one sort and keeps the rest in order", () => {
    const h = harness({ sort: [["priority", "asc"], ["cost", "desc"], ["page", "asc"]] });
    const remove = section(h.panel(), "Sort").querySelector<HTMLButtonElement>(".qd-row-remove")!;
    remove.click();
    expect(h.applied.at(-1)?.sort).toEqual([["cost", "desc"], ["page", "asc"]]);
    h.dispose();
  });

  it("shows every aggregate, repeats and the keyless whole-result count included", () => {
    const h = harness({
      aggregates: [["", "count"], ["cost", "sum"], ["cost", "avg"], ["cost", "sum"]],
    });
    expect(rowLabels(h.panel(), "Summarize")).toEqual([
      "Whole result",
      "cost",
      "cost",
      "cost",
    ]);
    h.dispose();
  });

  it("cycles ONE aggregate's function and preserves the list's order and repeats", () => {
    const h = harness({ aggregates: [["", "count"], ["cost", "sum"], ["cost", "sum"]] });
    const buttons = [...section(h.panel(), "Summarize").querySelectorAll<HTMLButtonElement>(".qd-row-btn")]
      .filter((el) => el.title === "Change function");
    buttons[1].click();
    expect(h.applied.at(-1)?.aggregates).toEqual([["", "count"], ["cost", "avg"], ["cost", "sum"]]);
    h.dispose();
  });

  // **The sheet footer's own functions ride in the same property** (§5). The
  // merge preserves them byte for byte — but a panel that lists only the three
  // the query understands makes that preservation invisible, and an author who
  // cannot see `estimate=median` has no reason to believe it is still there.
  it("states the aggregate segments it keeps but cannot edit", () => {
    // FAIL-BEFORE: the panel rendered the query's aggregates only, so the
    // retained sheet-only entries were kept in the bytes and shown nowhere.
    const h = harness({ aggregates: [["cost", "sum"]] }, ROWS, ["estimate=median", "owner=distinct"]);
    try {
      const note = section(h.panel(), "Summarize").querySelector(".qd-retained")?.textContent ?? "";
      expect(note).toContain("estimate=median");
      expect(note).toContain("owner=distinct");
      expect(note.toLowerCase()).toContain("kept");
      // Retained metadata is READ-ONLY here: it offers no control of its own.
      expect(section(h.panel(), "Summarize").querySelectorAll(".qd-retained button").length).toBe(0);
      // And showing it changes nothing about the note.
      expect(h.applied).toEqual([]);
    } finally { h.dispose(); }
  });

  it("says nothing about retained aggregates when there are none", () => {
    const h = harness({ aggregates: [["cost", "sum"]] });
    try {
      expect(h.panel().querySelector(".qd-retained")).toBeNull();
    } finally { h.dispose(); }
  });

  it("appends a whole-result count with no field", () => {
    const h = harness({ aggregates: [["cost", "sum"]] });
    const add = [...section(h.panel(), "Summarize").querySelectorAll<HTMLButtonElement>(".qd-add")].find(
      (el) => el.textContent?.trim() === "+ count",
    )!;
    add.click();
    expect(h.applied.at(-1)?.aggregates).toEqual([["cost", "sum"], ["", "count"]]);
    h.dispose();
  });

  it("shows the column list in order and says when there is none", () => {
    const none = harness({});
    expect(section(none.panel(), "Columns").querySelector(".qd-empty")?.textContent).toBe(
      "All fields the rows carry",
    );
    none.dispose();
    const h = harness({ columns: ["cost", "state", "severity"] });
    expect(rowLabels(h.panel(), "Columns")).toEqual(["cost", "State", "severity"]);
    h.dispose();
  });

  it("reorders columns through the one writer", () => {
    const h = harness({ columns: ["cost", "severity", "state"] });
    const downs = [...section(h.panel(), "Columns").querySelectorAll<HTMLButtonElement>(".qd-row-btn")]
      .filter((el) => el.title === "Move down");
    downs[0].click();
    expect(h.applied.at(-1)?.columns).toEqual(["severity", "cost", "state"]);
    h.dispose();
  });
});

describe("the sample", () => {
  it("reads an empty box as no limit and a number as a limit", () => {
    expect(readSample("")).toEqual({ kind: "unset" });
    expect(readSample("  ")).toEqual({ kind: "unset" });
    expect(readSample("20")).toEqual({ kind: "value", value: 20 });
  });

  it("treats zero as a real limit rather than as blank", () => {
    expect(readSample("0")).toEqual({ kind: "value", value: 0 });
  });

  it("refuses what the IR cannot carry", () => {
    expect(readSample("-1").kind).toBe("invalid");
    expect(readSample("1.5").kind).toBe("invalid");
    expect(readSample("lots").kind).toBe("invalid");
    expect(readSample(String(MAX_SAMPLE))).toEqual({ kind: "value", value: MAX_SAMPLE });
    expect(readSample(String(MAX_SAMPLE + 1)).kind).toBe("invalid");
  });

  it("says what a zero sample will do, before it is saved", () => {
    const h = harness({});
    const input = h.panel().querySelector<HTMLInputElement>(".qd-sample")!;
    input.value = "0";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    expect(section(h.panel(), "Sample").querySelector(".qd-note")?.textContent).toBe("No results (sample 0)");
    h.dispose();
  });

  it("does not write an invalid sample", () => {
    const h = harness({ sample: 20 });
    const input = h.panel().querySelector<HTMLInputElement>(".qd-sample")!;
    input.value = "nope";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("blur", { bubbles: true }));
    expect(h.applied).toEqual([]);
    expect(h.panel().querySelector(".qd-error")?.textContent).toContain("whole number");
    h.dispose();
  });

  it("clears the limit when the box is emptied", () => {
    const h = harness({ sample: 20 });
    const input = h.panel().querySelector<HTMLInputElement>(".qd-sample")!;
    input.value = "";
    input.dispatchEvent(new Event("input", { bubbles: true }));
    input.dispatchEvent(new Event("blur", { bubbles: true }));
    expect(h.applied.at(-1)?.sample).toBeUndefined();
    h.dispose();
  });
});

// Each slot spells a field differently, and offering one a slot cannot carry
// would be a control that looks like it saved.
describe("the panel's field pickers", () => {
  // The vocabulary list is virtualized: it mounts what its viewport says fits,
  // and jsdom gives every element a height of zero.
  let restoreGeometry: (() => void) | null = null;
  beforeEach(() => {
    restoreGeometry = stubVocabularyGeometry();
  });
  afterEach(() => {
    restoreGeometry?.();
    restoreGeometry = null;
  });

  const openGroupPicker = (panel: HTMLElement): HTMLElement => {
    const change = [...section(panel, "Group by").querySelectorAll<HTMLButtonElement>(".qd-row-btn")].find(
      (el) => el.textContent?.trim() === "Change",
    )!;
    change.click();
    const picker = document.querySelector<HTMLElement>(".qd-field-picker");
    if (!picker) throw new Error("the Group by field picker did not open");
    return picker;
  };

  it("opens OUTSIDE the panel, because the panel is a scroll box", () => {
    // Nested inside it, the list laid its rows out at real positions, painted
    // nowhere and answered no click — all four field choices in the panel,
    // unusable, with a DOM that looked perfectly correct.
    const h = harness({});
    const picker = openGroupPicker(h.panel());
    expect(h.panel().contains(picker)).toBe(false);
    expect(picker.querySelectorAll(".qs-vocab-option").length).toBeGreaterThan(0);
    h.dispose();
  });

  it("names the panel as its parent, so a press inside it holds the panel still", () => {
    const h = harness({});
    const picker = openGroupPicker(h.panel());
    expect(picker.getAttribute("data-transient-parent")).toMatch(/^query-display:/);
    const option = picker.querySelector<HTMLButtonElement>('.qs-vocab-option[data-vocabulary-key="state"]')!;
    for (const type of ["pointerdown", "mousedown"] as const) {
      option.dispatchEvent(new MouseEvent(type, { bubbles: true, composed: true }));
    }
    expect(document.querySelector(".qd-panel")).not.toBeNull();
    expect(document.querySelector(".qd-field-picker")).not.toBeNull();
    h.dispose();
  });

  it("writes the field it was asked for and closes", () => {
    const h = harness({});
    const picker = openGroupPicker(h.panel());
    picker.querySelector<HTMLButtonElement>('.qs-vocab-option[data-vocabulary-key="prop:cost"]')!.click();
    expect(h.applied.at(-1)).toEqual({ group_by: "prop:cost" });
    expect(document.querySelector(".qd-field-picker")).toBeNull();
    h.dispose();
  });
});

describe("the display field vocabulary", () => {
  const entries = (slot: "group" | "sort" | "column" | "aggregate", search = "") =>
    displayFieldEntries({ slot, rows: ROWS, formulas: ["effort"], search }).map((entry) =>
      entry.choice.kind === "field" ? entry.choice.field : "",
    );

  it("offers every field identity for grouping, canonically", () => {
    expect(entries("group")).toEqual([
      "state",
      "priority",
      "scheduled",
      "deadline",
      "tags",
      "page",
      "prop:cost",
      "prop:severity",
      "formula:effort",
    ]);
  });

  it("offers sorting only by what the engine can order by", () => {
    // `state`, `tags`, the title and formulas are table-only arrangements.
    expect(entries("sort")).toEqual(["priority", "page", "scheduled", "deadline", "cost", "severity"]);
  });

  it("does not offer a property column whose token would select a builtin instead", () => {
    const entries = displayFieldEntries({
      slot: "column", rows: [registryRow("state", 3), registryRow("page", 2), registryRow("cost", 1)],
      formulas: [], search: "",
    });
    expect(entries.filter((entry) => entry.icon === "•").map((entry) => entry.label)).toEqual(["cost"]);
    expect(new Set(entries.map((entry) => entry.id)).size).toBe(entries.length);
  });

  it("explains which fields need default columns rather than a custom selection", () => {
    const h = harness({}, [registryRow("state", 3)]);
    try {
      const note = h.panel().querySelector(".qd-column-limits")?.textContent;
      expect(note).toContain("state (property)");
      expect(note).toContain("effort (formula)");
      expect(note).toContain("Use default columns");
    } finally { h.dispose(); }
  });

  it("offers column tokens, with no formula and no page breadcrumb", () => {
    expect(entries("column")).toEqual([
      "state",
      "priority",
      "scheduled",
      "deadline",
      "tags",
      "cost",
      "severity",
    ]);
  });

  it("offers only property keys to aggregate", () => {
    expect(entries("aggregate")).toEqual(["cost", "severity"]);
  });

  it("offers a property named like a builtin to aggregate, unlike a column", () => {
    // FAIL-BEFORE: the aggregate slot borrowed the COLUMNS grammar's reserved
    // builtin names, which say nothing about `tine.col-aggregates` keys — an
    // ordinary property named `state` is a perfectly ordinary thing to sum.
    const rows = [registryRow("state", 3), registryRow("page", 2), registryRow("cost", 1)];
    expect(
      displayFieldEntries({ slot: "aggregate", rows, formulas: [], search: "" }).map((entry) =>
        entry.choice.kind === "field" ? entry.choice.field : "",
      ),
    ).toEqual(["state", "page", "cost"]);
  });

  it("does not offer a property key the aggregate grammar cannot spell", () => {
    // FAIL-BEFORE: nothing filtered these, so picking one wrote a segment the
    // reader parses as another key, another function, or not at all.
    const rows = ["a;b", "a=b", "a\nb", "a\rb", "a\0b", "", " cost"].map((name) => registryRow(name, 1));
    expect(
      displayFieldEntries({ slot: "aggregate", rows: [...rows, registryRow("cost", 1)], formulas: [], search: "" })
        .map((entry) => (entry.choice.kind === "field" ? entry.choice.field : "")),
    ).toEqual(["cost"]);
  });

  it("filters by the typed search", () => {
    expect(entries("group", "cos")).toEqual(["prop:cost"]);
  });

  // **A page is not a block** (§7.6, Q3). The vocabulary above is the BLOCK
  // vocabulary, and every caller that does not say otherwise keeps it. A page
  // row has no task state, no priority, no planning dates and no formula, and
  // none of the writers can spell one for a page — so offering them would be
  // six controls that save a setting the row can never show.
  it("q3_page_vocabulary_excludes_block_fields", () => {
    // FAIL-BEFORE: `rowKind` did not exist, so a Pages section's panel offered
    // the block builtins and the block formula list.
    const pageRows = [
      { ...registryRow("cost", 12), count_pages: 3 },
      { ...registryRow("severity", 4), count_pages: 0 },
    ] as RegistryRow[];
    const page = (slot: "group" | "sort" | "column" | "aggregate") =>
      displayFieldEntries({ slot, rows: pageRows, formulas: ["effort"], search: "", rowKind: "page" })
        .map((entry) => (entry.choice.kind === "field" ? entry.choice.field : ""));

    // Grouping: the three page attributes, canonically, then the properties the
    // graph's PAGES actually carry. No `state`, no `priority`, no `formula:`.
    expect(page("group")).toEqual(["prop:name", "prop:kind", "prop:day", "prop:cost"]);
    expect(page("group").some((field) => field.startsWith("formula:"))).toBe(false);
    // Sorting: page-name ordering and the supported page attributes.
    expect(page("sort")).toEqual(["name", "kind", "day", "cost"]);
    // Columns: no `name` — that is the row's own link — and no block builtin.
    expect(page("column")).toEqual(["kind", "day", "cost"]);
    // Aggregates: literal property keys only, for both row kinds.
    expect(page("aggregate")).toEqual(["cost"]);

    // `severity` is observed on no page of this graph, so it is not a page
    // field. The BLOCK vocabulary is untouched by that judgement.
    expect(page("column")).not.toContain("severity");
    expect(
      displayFieldEntries({ slot: "column", rows: pageRows, formulas: [], search: "" })
        .map((entry) => (entry.choice.kind === "field" ? entry.choice.field : "")),
    ).toContain("severity");
  });

  it("q3_page_panel_names_its_own_trigger_and_dialog", () => {
    // Two panels can be mounted side by side on one mixed result, so "Display
    // settings" would name both of them and neither would say which section it
    // changes.
    const registry: RegistryAccess = {
      rows: () => ROWS, pending: () => false, failure: () => null,
      unavailable: () => false, request: () => {}, retry: () => {},
    };
    const mounted = mount(() => (
      <QueryDisplay rowKind="page" control={{ view: {}, apply: () => {} }} registry={registry} />
    ));
    try {
      const trigger = mounted.root.querySelector<HTMLButtonElement>(".qd-trigger")!;
      expect(trigger.getAttribute("aria-label")).toBe("Display pages");
      expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");
      trigger.click();
      const panel = document.querySelector<HTMLElement>(".qd-panel")!;
      expect(panel.getAttribute("role")).toBe("dialog");
      expect(panel.getAttribute("aria-label")).toBe("Page display");
    } finally {
      mounted.dispose();
    }
  });
});
