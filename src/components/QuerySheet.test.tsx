import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { installMockQueryFixture } from "../mock";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { clearTransientLayersForTest, dismissTopTransient } from "../transientLayers";
import { QueryBuilder, type BuilderSession } from "./QueryBuilder";
import { encodePropertyLeaf, propertyFilter, taskFilter } from "../editor/queryBuilder";
import type { Filter, ParsedQuery, RegistrySnapshot } from "../editor/queryIr";

// **The sheet itself** (SPEC §7.2–§7.4): what the resting sentence expands into.
//
// The three things pinned here are the ones that have no other home. The
// popover ladder and the facet sharing live in `QueryBuilder.transient.test.tsx`
// with the rest of GH #472; the phrase lives in `queryBuilder.test.ts`.

function session(filter: Filter, anchor: "block" | "page" = "block"): BuilderSession {
  return { query: { anchor, filter, source: { kind: "builder" } }, view: {} };
}

function mountBuilder(filter: Filter, blockId?: string) {
  const host = document.createElement("div");
  document.body.append(host);
  const [current, setCurrent] = createSignal<BuilderSession>(session(filter));
  const dispose = render(
    () => <QueryBuilder session={current} onChange={setCurrent} blockId={blockId} />,
    host,
  );
  let sheetEl: HTMLElement | null = null;
  const open = (): HTMLElement => {
    if (sheetEl?.isConnected) return sheetEl;
    const before = new Set(document.querySelectorAll<HTMLElement>(".qs-sheet"));
    host.querySelector<HTMLButtonElement>(".qs-gear")!.click();
    sheetEl = [...document.querySelectorAll<HTMLElement>(".qs-sheet")].find((el) => !before.has(el)) ?? null;
    if (!sheetEl) throw new Error("the sheet did not open");
    return sheetEl;
  };
  return { host, open, session: current, dispose };
}

const settle = async () => {
  for (let i = 0; i < 8; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

beforeEach(() => {
  vi.spyOn(backend(), "queryFacets").mockResolvedValue([["cost", ["10"]]]);
  vi.spyOn(backend(), "printQuery").mockResolvedValue("@block and cost > 10");
});

afterEach(() => {
  installMockQueryFixture(null);
  clearTransientLayersForTest();
  resetSharedQueryResultsForTests();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

describe("one block, two sheets", () => {
  // The same query block can be on screen twice — main pane and sidebar, or two
  // split panes. `transientLayers` keys by ID and a LATER registration REPLACES
  // an earlier one, so a layer id derived from the block would silently
  // unregister the first sheet and Escape would close the wrong one.
  it("gives each mount of one block its own layer, so Escape closes the sheet that is open", () => {
    const first = mountBuilder(taskFilter(["TODO"]), "block-42");
    const second = mountBuilder(taskFilter(["TODO"]), "block-42");
    try {
      const secondSheet = second.open();
      expect(document.querySelectorAll(".qs-sheet")).toHaveLength(1);

      expect(dismissTopTransient("escape")).toBe(true);
      expect(secondSheet.isConnected).toBe(false);
      expect(document.querySelectorAll(".qs-sheet")).toHaveLength(0);

      // The first mount was never registered, so its sentence is untouched and
      // still opens.
      const firstSheet = first.open();
      expect(firstSheet.isConnected).toBe(true);
      expect(dismissTopTransient("escape")).toBe(true);
      expect(document.querySelectorAll(".qs-sheet")).toHaveLength(0);
    } finally {
      first.dispose();
      second.dispose();
    }
  });
});

describe("a typed property leaf reopens as the row that wrote it", () => {
  it("shows the operator it was saved with, not a generic equality", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue({
      rows: [
        {
          normalized_name: "cost",
          display_name: "cost",
          observed_type: "number",
          declared_type: null,
          cardinality: "one",
          count: 3,
        },
      ],
      generation: 1,
    } as unknown as RegistrySnapshot);
    const leaf = encodePropertyLeaf({ id: "gt", key: "cost", values: ["10"], type: "number" })!;
    expect(leaf).toBeTruthy();
    const { open, dispose } = mountBuilder(leaf);
    try {
      const sheet = open();
      await settle();
      const row = sheet.querySelector(".qs-row")!;
      expect(row.querySelector(".qs-field")!.textContent).toContain("Property");
      expect(row.querySelector(".qs-op")!.textContent).toContain("is more than");
      expect(row.querySelector(".qs-property-key")!.textContent).toBe("cost");
      // The row is a listitem in the conditions listbox, and its menus are
      // listboxes of their own.
      expect(row.getAttribute("role")).toBe("listitem");
      row.querySelector<HTMLButtonElement>(".qs-op")!.click();
      const listbox = sheet.querySelector('[role="listbox"]')!;
      expect(listbox.getAttribute("aria-label")).toBe("Condition operator");
      expect(
        [...listbox.querySelectorAll(".qs-option")].map((option) => option.textContent),
      ).toContain("is at least");
    } finally {
      dispose();
    }
  });
});

describe("switching what the query selects re-validates it and says how much it costs", () => {
  const notApplicable = (): ParsedQuery => ({
    query: {
      anchor: "page",
      filter: {
        kind: "and",
        items: [
          { kind: "raw", text: "task = 'TODO'", diagnostic_kind: "not_applicable" },
          propertyFilter("owner", "Ada"),
        ],
      },
      diagnostics: [
        { kind: "not_applicable", message: "`task` does not apply to pages", disabled: false },
      ],
      source: { kind: "tql", original: "@page and task = 'TODO' and prop('owner') = 'Ada'" },
    },
    view: {},
  }) as unknown as ParsedQuery;

  it("counts the conditions that stop applying and offers a way out of each", async () => {
    // jsdom has no engine, so the round trip runs through the ONE mock-only
    // fixture seam (`mockQueryFixture.guard.test.ts` pins that it is mock-only).
    installMockQueryFixture({ print: "@page and task = 'TODO'", parse: notApplicable() });
    vi.spyOn(backend(), "printQuery").mockResolvedValue("@page and task = 'TODO'");
    vi.spyOn(backend(), "parseQuery").mockResolvedValue(notApplicable());

    const filter: Filter = {
      kind: "and",
      items: [taskFilter(["TODO"]), propertyFilter("owner", "Ada")],
    };
    const { open, session: current, dispose } = mountBuilder(filter);
    try {
      const sheet = open();
      sheet.querySelector<HTMLButtonElement>(".qs-anchor-button")!.click();
      [...sheet.querySelectorAll<HTMLButtonElement>(".qs-option")]
        .find((option) => option.textContent?.startsWith("pages"))!
        .click();
      await settle();

      const prompt = sheet.querySelector(".qs-anchor-prompt")!;
      expect(prompt.getAttribute("role")).toBe("alertdialog");
      // The number is the POINT: "1 of your 2 conditions", not "some".
      expect(prompt.textContent).toContain("1 of your 2 conditions");
      expect(prompt.textContent).toContain("task = 'TODO'");
      // Nothing is committed while the prompt is up.
      expect(current().query.anchor).toBe("block");

      prompt.querySelector<HTMLButtonElement>(".qs-commit")!.click();
      await settle();
      expect(current().query.anchor).toBe("page");
      // "Remove them" removed exactly the leaf that stopped applying.
      expect(JSON.stringify(current().query.filter)).not.toContain("not_applicable");
      expect(JSON.stringify(current().query.filter)).toContain("owner");
    } finally {
      dispose();
    }
  });

  it("cancels back to the query it had, with nothing changed", async () => {
    installMockQueryFixture({ print: "@page and task = 'TODO'", parse: notApplicable() });
    vi.spyOn(backend(), "printQuery").mockResolvedValue("@page and task = 'TODO'");
    vi.spyOn(backend(), "parseQuery").mockResolvedValue(notApplicable());

    const filter: Filter = {
      kind: "and",
      items: [taskFilter(["TODO"]), propertyFilter("owner", "Ada")],
    };
    const { open, session: current, dispose } = mountBuilder(filter);
    const before = JSON.stringify(current());
    try {
      const sheet = open();
      sheet.querySelector<HTMLButtonElement>(".qs-anchor-button")!.click();
      [...sheet.querySelectorAll<HTMLButtonElement>(".qs-option")]
        .find((option) => option.textContent?.startsWith("pages"))!
        .click();
      await settle();

      const actions = [...sheet.querySelectorAll<HTMLButtonElement>(".qs-anchor-prompt-actions button")];
      actions.find((button) => button.textContent === "Cancel")!.click();
      await settle();
      expect(sheet.querySelector(".qs-anchor-prompt")).toBeNull();
      expect(JSON.stringify(current())).toBe(before);
    } finally {
      dispose();
    }
  });
});
