// **The sheet offers ONE display control** (P5B, Q3).
//
// It used to offer three: `+ sort`, `+ summarize`, and — for hosts that opted
// in — the shared Display panel. The first two could each state a FRACTION of
// one display fact. `+ sort` read `sort[0]` and wrote a one-element list back;
// `+ summarize` read `aggregates[0]` and did the same. A note carrying two sorts
// or three aggregates lost the rest the first time anyone touched a pill, and a
// host that mounted the panel beside them had two controls writing the same
// keys with different ideas of how many entries there are.
//
// This file is the evidence that they are gone, that every host reaches the
// shared panel instead, and that the panel edits the whole list.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { initParser } from "../render/parse";
import { resetStore } from "../store";
import { clearTransientLayersForTest } from "../transientLayers";
import { QueryBuilder, type BuilderSession } from "./QueryBuilder";
import { stubVocabularyGeometry } from "./QueryVocabularyPicker.test-helpers";
import type { Filter, RegistrySnapshot } from "../editor/queryIr";
import { taskFilter } from "../editor/queryBuilder";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  clearTransientLayersForTest();
  resetStore();
  document.body.innerHTML = "";
});

function snapshot(keys: [string, number][]): RegistrySnapshot {
  return {
    generation: 1,
    rows: keys.map(([normalized_name, count_blocks]) => ({
      normalized_name,
      cardinality: "one" as const,
      observed_type: "text" as const,
      count_blocks,
      count_pages: 0,
      mismatch_count: 0,
    })),
  };
}

function session(filter: Filter, view: BuilderSession["view"] = {}): BuilderSession {
  return { query: { anchor: "block", filter, source: { kind: "builder" } }, view };
}

/** Mount one builder and open its sheet, which is PORTALLED to `<body>`. */
function mountSheet(initial: BuilderSession) {
  stubVocabularyGeometry();
  const host = document.createElement("div");
  document.body.append(host);
  const [current, setCurrent] = createSignal(initial);
  const dispose = render(
    () => <QueryBuilder session={current} onChange={setCurrent} />,
    host,
  );
  host.querySelector<HTMLButtonElement>(".qs-gear")!.click();
  const sheet = document.querySelector<HTMLElement>(".qs-sheet")!;
  return { host, sheet, current, dispose };
}

describe("QueryBuilder display controls", () => {
  it("q3_shared_display_retires_legacy_controls", () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(snapshot([["status", 4]]));
    // A view carrying TWO sorts and TWO aggregates — exactly what the retired
    // pills could not represent.
    const { sheet, current, dispose } = mountSheet(session(taskFilter(["TODO"]), {
      sort: [["page", "asc"], ["prop:status", "desc"]],
      aggregates: [["prop:hours", "sum"], ["prop:hours", "avg"]],
    }));
    try {
      const labels = [...sheet.querySelectorAll("button")].map((button) => button.textContent?.trim() ?? "");
      expect(labels.some((label) => label === "+ sort")).toBe(false);
      expect(labels.some((label) => label.includes("summarize"))).toBe(false);

      // The one control that is offered names itself for the family it edits and
      // opens a dialog, not an anonymous popover.
      const trigger = sheet.querySelector<HTMLButtonElement>(".qd-trigger")!;
      expect(trigger.getAttribute("aria-label")).toBe("Display blocks");
      expect(trigger.getAttribute("aria-haspopup")).toBe("dialog");
      trigger.click();
      const panel = document.querySelector<HTMLElement>(".qd-panel")!;
      expect(panel.getAttribute("role")).toBe("dialog");
      expect(panel.getAttribute("aria-label")).toBe("Block display");

      // It states the WHOLE list rather than its first entry: the summary counts
      // both sorts and both aggregates, and the panel draws a row for each.
      expect(trigger.textContent).toContain("2 sorts");
      expect(trigger.textContent).toContain("2 Σ");
      expect(current().view.sort).toHaveLength(2);
      expect(current().view.aggregates).toHaveLength(2);
    } finally {
      dispose();
    }
  });

  it("q3_display_edits_reach_a_host_with_no_inline_writer", () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(snapshot([["status", 4]]));
    // No `display` prop and no `inlineDisplay`: the workspace's own builder
    // mount. Before this it got the two retired pills and nothing else; the
    // shared panel now writes through the builder's session instead.
    const { sheet, current, dispose } = mountSheet(session(taskFilter(["TODO"]), { sort: [["page", "asc"]] }));
    try {
      sheet.querySelector<HTMLButtonElement>(".qd-trigger")!.click();
      const panel = document.querySelector<HTMLElement>(".qd-panel")!;
      const table = [...panel.querySelectorAll<HTMLButtonElement>(".qd-view")]
        .find((button) => button.textContent?.trim() === "Table")!;
      table.click();
      expect(current().view.view).toBe("table");
      // The edit changed the view and NOTHING else: the authored sort survives.
      expect(current().view.sort).toEqual([["page", "asc"]]);
    } finally {
      dispose();
    }
  });
});
