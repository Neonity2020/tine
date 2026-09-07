import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { autocompleteFacets, backend } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { bumpGraphEpoch, setDataRev } from "../ui";
import {
  clearTransientLayersForTest,
  dismissTopTransient,
  registerTransientLayer,
} from "../transientLayers";
import { QueryBuilder, type BuilderSession } from "./QueryBuilder";
import {
  ADVANCED_PHRASE,
  MAX_QUERY_BUILDER_DEPTH,
  priorityFilter,
  taskFilter,
} from "../editor/queryBuilder";
import type { Filter } from "../editor/queryIr";

// The builder edits the IR now, so the harness hands it a `Filter` rather than a
// DSL string: there is no frontend parser left to turn text into a tree, and the
// popover behaviour under test never depended on the text form.
function session(filter: Filter): BuilderSession {
  return { query: { anchor: "block", filter, source: { kind: "builder" } }, view: {} };
}

function nested(depth: number, leaf: Filter): Filter {
  let filter = leaf;
  for (let level = 0; level < depth; level += 1) filter = { kind: "and", items: [filter] };
  return filter;
}

/** Mount one builder. At rest it draws ONE sentence inside `host`; `open()`
 *  presses its ⚙ and hands back the sheet, which is PORTALLED to <body> (the
 *  query block's own compositing layer is a containing block for `fixed`
 *  children, so the sheet cannot live inside it). Per-instance assertions
 *  therefore go through the returned sheet, not through `host`. */
function mountBuilder(filter: Filter = taskFilter(["TODO"])) {
  const host = document.createElement("div");
  document.body.append(host);
  const [current, setCurrent] = createSignal<BuilderSession>(session(filter));
  const dispose = render(
    () => <QueryBuilder session={current} onChange={setCurrent} />,
    host
  );
  let sheetEl: HTMLElement | null = null;
  const open = (): HTMLElement => {
    if (sheetEl?.isConnected) return sheetEl;
    const before = new Set(document.querySelectorAll<HTMLElement>(".qs-sheet"));
    host.querySelector<HTMLButtonElement>(".qs-gear")!.click();
    sheetEl =
      [...document.querySelectorAll<HTMLElement>(".qs-sheet")].find((el) => !before.has(el)) ?? null;
    if (!sheetEl) throw new Error("the sheet did not open");
    return sheetEl;
  };
  return { host, open, source: () => JSON.stringify(current()), dispose };
}

/** The popover families the sheet owns, each named by its trigger inside one
 *  open sheet. The GH #472 report was that ONE of them had hand-rolled its
 *  outside-click handling; the fix was to give them all the same one, so every
 *  case below drives all of them through the same gesture. */
const FAMILIES: Array<{ name: string; open: (sheet: HTMLElement) => HTMLButtonElement; visible: string }> = [
  { name: "anchor menu", open: (s) => s.querySelector<HTMLButtonElement>(".qs-anchor-button")!, visible: ".qs-menu" },
  { name: "row field menu", open: (s) => s.querySelector<HTMLButtonElement>(".qs-row .qs-field")!, visible: ".qs-menu" },
  { name: "row operator menu", open: (s) => s.querySelector<HTMLButtonElement>(".qs-row .qs-op")!, visible: ".qs-menu" },
  { name: "add-condition picker", open: (s) => s.querySelector<HTMLButtonElement>(".qs-add")!, visible: ".qs-menu" },
  {
    name: "sort popover",
    open: (s) => [...s.querySelectorAll<HTMLButtonElement>(".qb-sort")]
      .find((button) => button.textContent?.trim() === "+ sort")!,
    visible: ".qb-sort-picker",
  },
  {
    name: "summarize popover",
    open: (s) => [...s.querySelectorAll<HTMLButtonElement>(".qb-sort")]
      .find((button) => button.textContent?.includes("summarize"))!,
    visible: ".qb-picker",
  },
];

afterEach(() => {
  clearTransientLayersForTest();
  resetSharedQueryResultsForTests();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

beforeEach(() => {
  vi.spyOn(backend(), "queryFacets").mockResolvedValue([]);
  // The text pane's contents are PRINTED BY RUST (I-12); these tests are about
  // the popovers above it, so the printer is stubbed rather than exercised.
  vi.spyOn(backend(), "printQuery").mockResolvedValue("(and (task TODO))");
});

describe("QueryBuilder transient ownership (post-GH #161)", () => {
  // Sharing across instances is proven separately, by the Harvest W4-P1 item 3
  // test below; this one pins the per-revision refresh for a single builder.
  it("asks for no facets at rest, once when the sheet opens, and once per data revision", async () => {
    const facets = vi.mocked(backend().queryFacets);
    const { open, dispose } = mountBuilder();
    try {
      await Promise.resolve();
      // A page of RESTING sentences is the common case, and it costs the graph
      // nothing: the facets are the editor's vocabulary, not the sentence's.
      expect(facets).toHaveBeenCalledTimes(0);

      const sheet = open();
      await Promise.resolve();
      expect(facets).toHaveBeenCalledTimes(1);

      sheet.querySelector<HTMLButtonElement>(".qs-add")!.click();
      [...sheet.querySelectorAll<HTMLButtonElement>(".qs-option")]
        .find((button) => button.textContent === "Property")!
        .click();
      await Promise.resolve();
      expect(facets).toHaveBeenCalledTimes(1);

      setDataRev((revision) => revision + 1);
      await Promise.resolve();
      expect(facets).toHaveBeenCalledTimes(2);
    } finally {
      dispose();
    }
  });

  it("renders a bounded ⟨advanced⟩ chip instead of recursing through a hostile query tree", () => {
    // 64 levels still PARSE (`QUERY_NESTING_MAX`); what is bounded here is the
    // drawing. Both the sentence and the rows stop at the rendering cap, so a
    // query written by outside content cannot make either of them big (I-22).
    const depth = 64;
    const { host, open, dispose } = mountBuilder(nested(depth, taskFilter(["TODO"])));
    try {
      const sentence = host.querySelector(".qs-sentence")!;
      expect(sentence.textContent).toContain(ADVANCED_PHRASE);
      expect(sentence.querySelectorAll(".qs-seg").length).toBeLessThanOrEqual(8);

      const sheet = open();
      expect(sheet.querySelectorAll(".qs-row-advanced").length).toBe(1);
      expect(sheet.querySelectorAll(".qs-group").length).toBeLessThanOrEqual(
        MAX_QUERY_BUILDER_DEPTH,
      );
      expect(sheet.querySelectorAll(".qs-row").length).toBeLessThanOrEqual(
        MAX_QUERY_BUILDER_DEPTH + 1,
      );
    } finally {
      dispose();
    }
  });

  it("gives every popover family one Escape or Back rung above the sheet, and the sheet one above a lower owner", () => {
    // Three rungs, in order: the menu, then the sheet, then whatever owned the
    // screen before either — and no rung edits the query.
    for (const [index, family] of FAMILIES.entries()) {
      const reason = index % 2 === 0 ? "escape" : "back";
      const { open, source, dispose } = mountBuilder();
      const original = source();
      const lower = vi.fn(() => true);
      const unregisterLower = registerTransientLayer({
        id: `query-builder-lower-${index}`,
        dismiss: lower,
      });
      try {
        const sheet = open();
        family.open(sheet).click();
        expect(sheet.querySelector(family.visible), `${family.name} did not open`).not.toBeNull();

        expect(dismissTopTransient(reason)).toBe(true);
        expect(sheet.querySelector(family.visible), `${family.name} survived its own rung`).toBeNull();
        expect(sheet.isConnected, "the sheet went with the menu").toBe(true);
        expect(lower).not.toHaveBeenCalled();

        expect(dismissTopTransient(reason)).toBe(true);
        expect(document.querySelector(".qs-sheet")).toBeNull();
        expect(lower).not.toHaveBeenCalled();
        expect(source()).toBe(original);
      } finally {
        unregisterLower();
        dispose();
      }
    }
  });

  it("keeps two builder instances independent: a press in one closes only the other's menu", () => {
    // Reactivation of an older visible peer by an inside pointer is pinned
    // generically in transientRegistry.p1d1.lifecycle.test.tsx. What is specific
    // here is that two builders on one page own separate sheets and separate
    // popover state, and that a press inside one is an OUTSIDE press for the
    // other (GH #472) — so they cannot both stay open, and the one pressed
    // survives.
    const first = mountBuilder(taskFilter(["TODO"]));
    const second = mountBuilder(priorityFilter(["A"]));
    try {
      const firstSheet = first.open();
      const secondSheet = second.open();
      firstSheet.querySelector<HTMLButtonElement>(".qs-row .qs-field")!.click();
      secondSheet.querySelector<HTMLButtonElement>(".qs-row .qs-field")!.click();
      expect(firstSheet.querySelector(".qs-menu")).not.toBeNull();
      expect(secondSheet.querySelector(".qs-menu")).not.toBeNull();

      firstSheet.querySelector(".qs-menu")!.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
      expect(firstSheet.querySelector(".qs-menu")).not.toBeNull();
      expect(secondSheet.querySelector(".qs-menu")).toBeNull();

      expect(dismissTopTransient("escape")).toBe(true);
      expect(firstSheet.querySelector(".qs-menu")).toBeNull();
    } finally {
      first.dispose();
      second.dispose();
    }
  });
});

describe("GH #472: every Query Builder popover closes on an outside press", () => {
  // The reported failure: the leftmost (clause) menu stayed open when the user
  // clicked away — "the menu even stays open after clicking into and editing a
  // different block" — while the sort popover next to it closed correctly. The
  // difference was that two of the popovers had hand-rolled an outside-click
  // effect and two had not, so the cases below drive ALL of them through the
  // same gesture rather than only the reported one. The sheet added a rung
  // under them, so an outside press now takes the menu FIRST and the sheet on
  // the press after — never both at once.
  const popovers = FAMILIES;

  // A press elsewhere in the document — the page background, another block.
  const pressOutside = (type: "mousedown" | "pointerdown") => {
    const elsewhere = document.createElement("div");
    document.body.append(elsewhere);
    elsewhere.dispatchEvent(new MouseEvent(type, { bubbles: true }));
    elsewhere.remove();
  };

  const expectAllClosedBy = (type: "mousedown" | "pointerdown") => {
    for (const popover of popovers) {
      const { open, source, dispose } = mountBuilder();
      const original = source();
      try {
        const sheet = open();
        popover.open(sheet).click();
        expect(sheet.querySelector(popover.visible), `${popover.name} did not open`).not.toBeNull();

        pressOutside(type);

        expect(sheet.querySelector(popover.visible), `${popover.name} stayed open`).toBeNull();
        expect(sheet.isConnected, `${popover.name} took the sheet with it`).toBe(true);

        pressOutside(type);
        expect(document.querySelector(".qs-sheet"), "the sheet stayed open").toBeNull();
        expect(source()).toBe(original);
      } finally {
        dispose();
      }
    }
  };

  // Both event types, because touch and pen deliver only the first and some
  // synthesized/compatibility paths only the second.
  it("closes each popover on an outside mousedown without changing the query", () => {
    expectAllClosedBy("mousedown");
  });

  it("closes each popover on an outside pointerdown without changing the query", () => {
    expectAllClosedBy("pointerdown");
  });

  it("keeps a popover open when the press lands inside it", () => {
    for (const popover of popovers) {
      const { open, dispose } = mountBuilder();
      try {
        const sheet = open();
        popover.open(sheet).click();
        const panel = sheet.querySelector(popover.visible)!;
        panel.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
        expect(sheet.querySelector(popover.visible), `${popover.name} closed on an inside press`).not.toBeNull();
      } finally {
        dispose();
      }
    }
  });

  it("keeps the sheet open when a press lands on a CLOSED popover's trigger", () => {
    // The gesture every one of these popovers starts with: the sheet is open,
    // nothing else is, and the user presses `+ sort`. That pointerdown is
    // inside the sheet, so the sheet must not treat it as "the user pressed
    // somewhere else" — if it does, the sheet closes under the press and the
    // click that follows lands on nothing at all.
    for (const popover of popovers) {
      const { open, dispose } = mountBuilder();
      try {
        const sheet = open();
        const trigger = popover.open(sheet);
        trigger.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
        expect(
          document.querySelector(".qs-sheet"),
          `the sheet closed on the press that opens ${popover.name}`,
        ).not.toBeNull();
        trigger.click();
        expect(sheet.querySelector(popover.visible), `${popover.name} never opened`).not.toBeNull();
      } finally {
        dispose();
      }
    }
  });

  it("lets the trigger of an open popover toggle it shut instead of reopening it", () => {
    // The trigger must count as INSIDE: dismissing on its press would close the
    // popover, and the click that follows would immediately reopen it.
    for (const popover of popovers) {
      const { open, dispose } = mountBuilder();
      try {
        const sheet = open();
        const trigger = popover.open(sheet);
        trigger.click();
        expect(sheet.querySelector(popover.visible)).not.toBeNull();

        trigger.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
        expect(sheet.querySelector(popover.visible), `${popover.name} closed before its own click`).not.toBeNull();
        trigger.click();
        expect(sheet.querySelector(popover.visible), `${popover.name} did not toggle shut`).toBeNull();
      } finally {
        dispose();
      }
    }
  });
});

describe("QueryBuilder facet sharing (Harvest W4-P1 item 3)", () => {
  // Drive the production Property picker and read back the keys it offers, so a
  // "one call" bound cannot be met by starving four of the five builders.
  function propertyKeysOffered(sheet: HTMLElement): string[] {
    const add = sheet.querySelector<HTMLButtonElement>(".qs-add")!;
    add.click();
    [...sheet.querySelectorAll<HTMLButtonElement>(".qs-option")]
      .find((button) => button.textContent === "Property")!
      .click();
    const keys = [...sheet.querySelectorAll<HTMLButtonElement>(".qs-value-editor .qs-option")].map(
      (button) => button.textContent ?? ""
    );
    add.click(); // The trigger toggles: leave the picker closed for the next read.
    return keys;
  }

  it("issues one shared facets request per (graph scope, dataRev) for five mounted builders", async () => {
    const payloads: Array<[string, string[]][]> = [
      [["revision-one", ["r1"]]],
      [["revision-two", ["r2"]]],
      [["revision-three", ["r3"]]],
    ];
    let current = 0;
    const facets = vi.mocked(backend().queryFacets);
    facets.mockReset();
    facets.mockImplementation(async (autocomplete?: boolean) =>
      autocomplete ? [["autocomplete-only", ["a"]]] : payloads[current]
    );

    const builders = Array.from({ length: 5 }, () => mountBuilder(taskFilter(["TODO"])));
    try {
      await Promise.resolve();
      await Promise.resolve();
      // Five RESTING sentences ask nothing at all; the shared request is made
      // when the first sheet opens and served to the other four from the cache.
      expect(facets.mock.calls.length).toBe(0);
      const sheets = builders.map((builder) => builder.open());
      await Promise.resolve();
      await Promise.resolve();
      const mounted = facets.mock.calls.length;
      for (const sheet of sheets) {
        expect(propertyKeysOffered(sheet)).toEqual(["revision-one"]);
      }

      // A new data revision: one fresh shared call, and every builder sees it.
      facets.mockClear();
      current = 1;
      setDataRev((revision) => revision + 1);
      await Promise.resolve();
      await Promise.resolve();
      const perRevision = facets.mock.calls.length;
      for (const sheet of sheets) {
        expect(propertyKeysOffered(sheet)).toEqual(["revision-two"]);
      }

      // A graph switch: the shared scope changes, so one fresh call again.
      facets.mockClear();
      current = 2;
      bumpGraphEpoch();
      await Promise.resolve();
      await Promise.resolve();
      const perGraphScope = facets.mock.calls.length;
      for (const sheet of sheets) {
        expect(propertyKeysOffered(sheet)).toEqual(["revision-three"]);
      }

      // The autocomplete producer asks a DIFFERENT question and must not be
      // served from the builder's shared entry.
      facets.mockClear();
      expect(await autocompleteFacets()).toEqual([["autocomplete-only", ["a"]]]);
      const autocompleteCalls = facets.mock.calls.map(([flag]) => flag ?? false);

      // eslint-disable-next-line no-console -- the measurement IS the receipt.
      console.log(
        `w4_p1_query_facets builders=5 mounted=${mounted} perDataRev=${perRevision} ` +
          `perGraphScope=${perGraphScope} autocomplete=${JSON.stringify(autocompleteCalls)}`
      );

      expect({ mounted, perRevision, perGraphScope, autocompleteCalls }).toEqual({
        mounted: 1,
        perRevision: 1,
        perGraphScope: 1,
        autocompleteCalls: [true],
      });
    } finally {
      for (const builder of builders) builder.dispose();
    }
  });
});
