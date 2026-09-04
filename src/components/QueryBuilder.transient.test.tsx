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
import { QueryBuilder } from "./QueryBuilder";

function mountBuilder(dsl = "(and (task TODO))") {
  const host = document.createElement("div");
  document.body.append(host);
  const [source, setSource] = createSignal(dsl);
  const dispose = render(() => <QueryBuilder dsl={source} onChange={setSource} />, host);
  return { host, source, dispose };
}

afterEach(() => {
  clearTransientLayersForTest();
  resetSharedQueryResultsForTests();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

beforeEach(() => {
  vi.spyOn(backend(), "queryFacets").mockResolvedValue([]);
});

describe("QueryBuilder transient ownership (post-GH #161)", () => {
  // Sharing across instances is proven separately, by the Harvest W4-P1 item 3
  // test below; this one pins the per-revision refresh for a single builder.
  it("fetches facets once and refreshes once per data revision", async () => {
    const facets = vi.mocked(backend().queryFacets);
    const { host, dispose } = mountBuilder();
    try {
      await Promise.resolve();
      expect(facets).toHaveBeenCalledTimes(1);

      host.querySelector<HTMLButtonElement>(".qb-add")!.click();
      [...host.querySelectorAll<HTMLButtonElement>(".qb-menu-item")]
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

  it("renders a bounded sentinel instead of recursing through a hostile query tree", () => {
    const depth = 65;
    const dsl = `${"(and ".repeat(depth)}[[Leaf]]${")".repeat(depth)}`;
    const { host, dispose } = mountBuilder(dsl);
    try {
      expect(host.querySelector(".qb-depth-limit")?.textContent).toContain("64");
      expect(host.querySelectorAll(".qb-group").length).toBeLessThanOrEqual(64);
    } finally {
      dispose();
    }
  });

  it("gives every popover family one Escape or Back rung above a lower owner without changing the DSL", () => {
    const { host, source, dispose } = mountBuilder();
    const original = source();
    const cases: Array<{ open: () => HTMLButtonElement; visible: string; reason: "escape" | "back" }> = [
      { open: () => host.querySelector<HTMLButtonElement>(".qb-chip")!, visible: ".qb-menu", reason: "escape" },
      { open: () => host.querySelector<HTMLButtonElement>(".qb-add")!, visible: ".qb-picker", reason: "back" },
      {
        open: () => [...host.querySelectorAll<HTMLButtonElement>(".qb-sort")]
          .find((button) => button.textContent?.trim() === "+ sort")!,
        visible: ".qb-sort-picker",
        reason: "escape",
      },
      {
        open: () => [...host.querySelectorAll<HTMLButtonElement>(".qb-sort")]
          .find((button) => button.textContent?.includes("summarize"))!,
        visible: ".qb-picker",
        reason: "back",
      },
    ];

    try {
      for (const testCase of cases) {
        const lower = vi.fn(() => true);
        const unregisterLower = registerTransientLayer({
          id: `query-builder-lower-${testCase.reason}-${testCase.visible}`,
          dismiss: lower,
        });
        testCase.open().click();
        expect(host.querySelector(testCase.visible)).not.toBeNull();

        expect(dismissTopTransient(testCase.reason)).toBe(true);
        expect(host.querySelector(testCase.visible)).toBeNull();
        expect(lower).not.toHaveBeenCalled();
        expect(source()).toBe(original);
        unregisterLower();
      }
    } finally {
      dispose();
    }
  });

  it("keeps two builder instances independent: a press in one closes only the other's menu", () => {
    // Reactivation of an older visible peer by an inside pointer is pinned
    // generically in transientRegistry.p1d1.lifecycle.test.tsx. What is specific
    // here is that two builders on one page own separate popover state, and that
    // a press inside one is an OUTSIDE press for the other (GH #472) — so they
    // cannot both stay open, and the one pressed survives.
    const first = mountBuilder("(and (task TODO))");
    const second = mountBuilder("(and (priority A))");
    try {
      first.host.querySelector<HTMLButtonElement>(".qb-chip")!.click();
      second.host.querySelector<HTMLButtonElement>(".qb-chip")!.click();
      expect(first.host.querySelector(".qb-menu")).not.toBeNull();
      expect(second.host.querySelector(".qb-menu")).not.toBeNull();

      first.host.querySelector(".qb-menu")!.dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
      expect(first.host.querySelector(".qb-menu")).not.toBeNull();
      expect(second.host.querySelector(".qb-menu")).toBeNull();

      expect(dismissTopTransient("escape")).toBe(true);
      expect(first.host.querySelector(".qb-menu")).toBeNull();
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
  // difference was that two of the four popovers had hand-rolled an
  // outside-click effect and two had not, so the cases below drive ALL FOUR
  // through the same gesture rather than only the reported one.
  const popovers: Array<{ name: string; open: (host: HTMLElement) => HTMLButtonElement; visible: string }> = [
    { name: "clause menu", open: (host) => host.querySelector<HTMLButtonElement>(".qb-chip")!, visible: ".qb-menu" },
    { name: "add-filter picker", open: (host) => host.querySelector<HTMLButtonElement>(".qb-add")!, visible: ".qb-picker" },
    {
      name: "sort popover",
      open: (host) => [...host.querySelectorAll<HTMLButtonElement>(".qb-sort")]
        .find((button) => button.textContent?.trim() === "+ sort")!,
      visible: ".qb-sort-picker",
    },
    {
      name: "summarize popover",
      open: (host) => [...host.querySelectorAll<HTMLButtonElement>(".qb-sort")]
        .find((button) => button.textContent?.includes("summarize"))!,
      visible: ".qb-picker",
    },
  ];

  // A press elsewhere in the document — the page background, another block.
  const pressOutside = (type: "mousedown" | "pointerdown") => {
    const elsewhere = document.createElement("div");
    document.body.append(elsewhere);
    elsewhere.dispatchEvent(new MouseEvent(type, { bubbles: true }));
    elsewhere.remove();
  };

  const expectAllClosedBy = (type: "mousedown" | "pointerdown") => {
    for (const popover of popovers) {
      const { host, source, dispose } = mountBuilder();
      const original = source();
      try {
        popover.open(host).click();
        expect(host.querySelector(popover.visible), `${popover.name} did not open`).not.toBeNull();

        pressOutside(type);

        expect(host.querySelector(popover.visible), `${popover.name} stayed open`).toBeNull();
        expect(source()).toBe(original);
      } finally {
        dispose();
      }
    }
  };

  // Both event types, because touch and pen deliver only the first and some
  // synthesized/compatibility paths only the second.
  it("closes each popover on an outside mousedown without changing the DSL", () => {
    expectAllClosedBy("mousedown");
  });

  it("closes each popover on an outside pointerdown without changing the DSL", () => {
    expectAllClosedBy("pointerdown");
  });

  it("keeps a popover open when the press lands inside it", () => {
    for (const popover of popovers) {
      const { host, dispose } = mountBuilder();
      try {
        popover.open(host).click();
        const panel = host.querySelector(popover.visible)!;
        panel.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
        expect(host.querySelector(popover.visible), `${popover.name} closed on an inside press`).not.toBeNull();
      } finally {
        dispose();
      }
    }
  });

  it("lets the trigger of an open popover toggle it shut instead of reopening it", () => {
    // The trigger must count as INSIDE: dismissing on its press would close the
    // popover, and the click that follows would immediately reopen it.
    for (const popover of popovers) {
      const { host, dispose } = mountBuilder();
      try {
        const trigger = popover.open(host);
        trigger.click();
        expect(host.querySelector(popover.visible)).not.toBeNull();

        trigger.dispatchEvent(new MouseEvent("mousedown", { bubbles: true }));
        expect(host.querySelector(popover.visible), `${popover.name} closed before its own click`).not.toBeNull();
        trigger.click();
        expect(host.querySelector(popover.visible), `${popover.name} did not toggle shut`).toBeNull();
      } finally {
        dispose();
      }
    }
  });
});

describe("QueryBuilder facet sharing (Harvest W4-P1 item 3)", () => {
  // Drive the production Property picker and read back the keys it offers, so a
  // "one call" bound cannot be met by starving four of the five builders.
  function propertyKeysOffered(host: HTMLElement): string[] {
    const add = host.querySelector<HTMLButtonElement>(".qb-add")!;
    add.click();
    [...host.querySelectorAll<HTMLButtonElement>(".qb-menu-item")]
      .find((button) => button.textContent === "Property")!
      .click();
    const keys = [...host.querySelectorAll<HTMLButtonElement>(".qb-value .qb-menu-item")].map(
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

    const builders = Array.from({ length: 5 }, () => mountBuilder("(and (task TODO))"));
    try {
      await Promise.resolve();
      await Promise.resolve();
      const mounted = facets.mock.calls.length;
      for (const builder of builders) {
        expect(propertyKeysOffered(builder.host)).toEqual(["revision-one"]);
      }

      // A new data revision: one fresh shared call, and every builder sees it.
      facets.mockClear();
      current = 1;
      setDataRev((revision) => revision + 1);
      await Promise.resolve();
      await Promise.resolve();
      const perRevision = facets.mock.calls.length;
      for (const builder of builders) {
        expect(propertyKeysOffered(builder.host)).toEqual(["revision-two"]);
      }

      // A graph switch: the shared scope changes, so one fresh call again.
      facets.mockClear();
      current = 2;
      bumpGraphEpoch();
      await Promise.resolve();
      await Promise.resolve();
      const perGraphScope = facets.mock.calls.length;
      for (const builder of builders) {
        expect(propertyKeysOffered(builder.host)).toEqual(["revision-three"]);
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
