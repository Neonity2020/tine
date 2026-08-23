import { afterEach, beforeAll, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { initParser } from "../render/parse";
import { doc, resetStore, setDoc, type FeedPage, type Node as StoreNode } from "../store";
import { editingId } from "../editorController";
import { Block } from "./Block";
import { installKeybindings } from "../keybindings";

// GH #227 investigation: reporter saw "Add row" insert at the TOP of a table.
// Ordinary (unsorted) tables append at the bottom (covered elsewhere). This
// probes the remaining presentations a reporter can plausibly hit: an ACTIVE
// column sort (asc/desc), first/middle/last positions, and the empty table.

let disposeKeys: (() => void) | null = null;
beforeAll(async () => {
  await initParser();
  disposeKeys = installKeybindings()();
});

afterEach(() => {
  disposeKeys?.();
  disposeKeys = null;
  resetStore();
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

function loadTable(children: [string, string][]) {
  setDoc({
    byId: Object.fromEntries<StoreNode>([
      ["table", node("table", "Table\ntine.view:: table", null, children.map(([id]) => id))],
      ...children.map(([id, title]) => [id, node(id, title, "table")] as [string, StoreNode]),
    ]),
    pages: [page(["table"])],
    feed: ["Sheet"],
    loaded: true,
  });
}

function visibleTitles(root: HTMLElement): string[] {
  return [...root.querySelectorAll(".sheet-title-cell .sheet-cell-body")].map((c) => c.textContent?.trim() ?? "");
}

async function tick(): Promise<void> {
  await new Promise((r) => setTimeout(r, 0));
}

function addRow(root: HTMLElement) {
  (root.querySelector(".sheet-add-row-ghost") as HTMLButtonElement).click();
}

describe("table Add row position across presentations (GH #227)", () => {
  it("unsorted table with header+data rows appends the new row at the BOTTOM", async () => {
    loadTable([["r1", "First"], ["r2", "Second"], ["r3", "Third"]]);
    const { root, dispose } = mount(() => <Block id="table" />);
    addRow(root);
    await tick();
    const children = doc.byId.table.children;
    expect(children.slice(0, 3)).toEqual(["r1", "r2", "r3"]);
    expect(children).toHaveLength(4);
    expect(visibleTitles(root).slice(0, 3)).toEqual(["First", "Second", "Third"]);
    expect(visibleTitles(root)).toHaveLength(4);
    expect(editingId()).toBe(children[3]);
    dispose();
  });

  it("EMPTY table: the new row is the first (and only) row", async () => {
    loadTable([]);
    const { root, dispose } = mount(() => <Block id="table" />);
    addRow(root);
    await tick();
    expect(doc.byId.table.children).toHaveLength(1);
    dispose();
  });

  it("with an ACTIVE ascending title sort, the source order is preserved BUT the empty row lands at the sorted view's top", async () => {
    loadTable([["r1", "Beta"], ["r2", "Alpha"], ["r3", "Gamma"]]);
    const { root, dispose } = mount(() => <Block id="table" />);
    (root.querySelector(".sheet-title-header") as HTMLElement).click(); // ascending sort
    expect(visibleTitles(root)).toEqual(["Alpha", "Beta", "Gamma"]);
    addRow(root);
    await tick();
    // Under the sort contract "empty title sorts first", the new row is shown
    // FIRST — the reporter's apparent top-insertion is the sorted view, not a
    // write to the top of the source outline.
    expect(doc.byId.table.children.slice(0, 3)).toEqual(["r1", "r2", "r3"]);
    expect(visibleTitles(root)[0]).toBe("");
    // Once a real title is typed, the sort takes over naturally (existing
    // sort semantics, unchanged).
    dispose();
  });

  it("with an ACTIVE DESCENDING title sort, the empty row lands at the visible BOTTOM", async () => {
    loadTable([["r1", "Beta"], ["r2", "Alpha"], ["r3", "Gamma"]]);
    const { root, dispose } = mount(() => <Block id="table" />);
    const header = root.querySelector(".sheet-title-header") as HTMLElement;
    header.click();
    header.click(); // second click flips to descending
    expect(visibleTitles(root)).toEqual(["Gamma", "Beta", "Alpha"]);
    addRow(root);
    await tick();
    expect(doc.byId.table.children.slice(0, 3)).toEqual(["r1", "r2", "r3"]);
    expect(visibleTitles(root)[3]).toBe("");
    dispose();
  });
});
