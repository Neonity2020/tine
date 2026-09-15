// The REAL paths go through the storage-authority front door (I-6, I-20).
//
// `storageDispatch.test.ts` proves the dispatcher branches correctly in
// isolation; this file proves the production call sites actually use it, at the
// layer where a user observes the failure:
//
//   - instrumentation gate: each semantic operation records a dispatch, with
//     the intent it stated, on the route its admission selects;
//   - capability gate: with no admission published a cross-page move never
//     reaches the Direct persistence entry points (`backend().savePage`, dirty
//     marks).

import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from "vitest";
import { initParser } from "./render/parse";
import { backend } from "./backend";
import { graphBindingRuntime } from "./graphBindingRuntime";
import {
  lastStorageDispatch,
  resetStorageDispatchCounters,
  storageDispatchCounters,
} from "./storageDispatch";
import {
  __setStoreMutationObserverForTest,
  captureToPage,
  loadFeed,
  moveBlock,
  moveBlockFeed,
  moveBlocksRelative,
  moveSelectionItems,
  pageByName,
  resetStore,
  selectBlock,
  settleDirectMovesForTest,
} from "./store";
import { insertDroppedFiles } from "./filedrop";
import { carryDay } from "./carry";
import { journalTitle } from "./journal";
import { clearConflict, setToasts, toasts } from "./ui";
import { resetPaneLayoutToSingle } from "./panes";
import type { BlockDto, PageDto } from "./types";

function page(
  name: string,
  path: string,
  rev: string,
  blocks: BlockDto[],
  kind: "page" | "journal" = "page",
): PageDto {
  return { name, kind, title: name, pre_block: null, blocks, path, rev };
}

function block(id: string, raw: string): BlockDto {
  return { id, raw, collapsed: false, children: [] };
}

function direct(): void {
  graphBindingRuntime.clear();
  graphBindingRuntime.bind(1, { binding_generation: 1 });
}

/** No admission published: the graph is still opening or switching. */
function unavailable(): void {
  graphBindingRuntime.clear();
}

const UNAVAILABLE_TOAST = "Can't move between pages while the graph is changing.";

/** Every Direct persistence entry point an unadmitted slot must not reach. */
function watchDirectPersistence() {
  const save = vi.spyOn(backend(), "savePage");
  const counts = { dirtyMarks: 0, publications: 0 };
  __setStoreMutationObserverForTest((observation) => {
    if (observation.kind === "dirty") counts.dirtyMarks++;
    else if (observation.kind === "publication") counts.publications++;
  });
  return {
    counts,
    assertUnreached() {
      expect(
        { saves: save.mock.calls.length, dirtyMarks: counts.dirtyMarks },
        "I-6: a slot with no admission reached Direct persistence. Semantic storage "
        + "operations dispatch through src/storageDispatch.ts — see storageDispatch.test.ts.",
      ).toEqual({ saves: 0, dirtyMarks: 0 });
    },
  };
}

/** Two ordinary named pages: one source root, one destination root. */
async function loadTwoPages(): Promise<void> {
  clearConflict("Source");
  clearConflict("Destination");
  await loadFeed([
    page("Source", "pages/source.md", "source-r1", [block("source", "source")]),
    page("Destination", "pages/destination.md", "destination-r1", [block("target", "target")]),
  ]);
}

/** Two journal days, so the feed-boundary move shapes have somewhere to go. */
async function loadTwoDays(): Promise<void> {
  clearConflict("Sep 1st, 2026");
  clearConflict("Sep 2nd, 2026");
  await loadFeed([
    page("Sep 2nd, 2026", "journals/2026_09_02.md", "d2-r1", [block("newer", "newer")], "journal"),
    page("Sep 1st, 2026", "journals/2026_09_01.md", "d1-r1", [block("older", "older")], "journal"),
  ]);
}

beforeAll(() => initParser());

beforeEach(() => {
  resetStore();
  resetPaneLayoutToSingle({
    tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }],
    activeIndex: 0,
  });
  resetStorageDispatchCounters();
  setToasts([]);
  direct();
});

afterEach(() => {
  __setStoreMutationObserverForTest(null);
  graphBindingRuntime.clear();
  setToasts([]);
  vi.restoreAllMocks();
});

describe("cross-page move dispatch — the four move shapes", () => {
  describe("moveBlock (drag across pages)", () => {
    it("routes unavailable, refuses with the shared toast, and mutates nothing", async () => {
      await loadTwoPages();
      unavailable();
      const persistence = watchDirectPersistence();

      await moveBlock("source", null, 1, "Destination");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 1 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "unavailable",
        request: { sourcePages: ["Source"], destinationPage: "Destination", roots: ["source"] },
      });
      expect(toasts().map((toast) => toast.message)).toEqual([UNAVAILABLE_TOAST]);
      persistence.assertUnreached();
      expect(pageByName("Source")!.roots).toEqual(["source"]);
    });

    it("routes direct and runs the Direct choreography", async () => {
      await loadTwoPages();
      const save = vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      await moveBlock("source", null, 1, "Destination");
      await settleDirectMovesForTest();

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 1, unavailable: 0 });
      expect(pageByName("Source")!.roots).toEqual([]);
      expect(pageByName("Destination")!.roots).toEqual(["target", "source"]);
      expect(save).toHaveBeenCalled();
    });

    it("does not dispatch at all for a same-page reorder", async () => {
      await loadFeed([
        page("Source", "pages/source.md", "source-r1", [block("a", "a"), block("b", "b")]),
      ]);
      await moveBlock("b", null, 0, "Source");
      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 0 });
    });
  });

  describe("moveBlocksRelative (selection dropped next to a target)", () => {
    it("routes unavailable and refuses", async () => {
      await loadTwoPages();
      unavailable();
      const persistence = watchDirectPersistence();

      expect(await moveBlocksRelative(["source"], "target", "after")).toBe(false);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 1 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "unavailable",
        request: { sourcePages: ["Source"], destinationPage: "Destination", roots: ["source"] },
      });
      expect(toasts().map((toast) => toast.message)).toEqual([UNAVAILABLE_TOAST]);
      persistence.assertUnreached();
    });

    it("routes direct and runs the Direct choreography", async () => {
      await loadTwoPages();
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      expect(await moveBlocksRelative(["source"], "target", "after")).toBe(true);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 1, unavailable: 0 });
      expect(pageByName("Destination")!.roots).toEqual(["target", "source"]);
      expect(pageByName("Source")!.roots).toEqual([]);
    });

    it("does not dispatch at all for a same-page relative move", async () => {
      await loadFeed([
        page("Source", "pages/source.md", "source-r1", [block("a", "a"), block("b", "b")]),
      ]);
      expect(await moveBlocksRelative(["a"], "b", "after")).toBe(true);
      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 0 });
    });
  });

  describe("moveBlockFeed (single block across a journal-day boundary)", () => {
    it("routes unavailable and refuses", async () => {
      await loadTwoDays();
      unavailable();
      const persistence = watchDirectPersistence();

      expect(await moveBlockFeed("newer", 1)).toBe("none");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 1 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "unavailable",
        request: {
          sourcePages: ["Sep 2nd, 2026"],
          destinationPage: "Sep 1st, 2026",
          roots: ["newer"],
        },
      });
      expect(toasts().map((toast) => toast.message)).toEqual([UNAVAILABLE_TOAST]);
      persistence.assertUnreached();
    });

    it("routes direct and crosses the day boundary", async () => {
      await loadTwoDays();
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      expect(await moveBlockFeed("newer", 1)).toBe("crossed");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 1, unavailable: 0 });
      expect(pageByName("Sep 1st, 2026")!.roots).toEqual(["newer", "older"]);
    });
  });

  describe("moveSelectionItems (whole selection across a journal-day boundary)", () => {
    it("routes unavailable and refuses", async () => {
      await loadTwoDays();
      selectBlock("newer");
      unavailable();
      const persistence = watchDirectPersistence();

      await moveSelectionItems(1);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 0, unavailable: 1 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "unavailable",
        request: {
          sourcePages: ["Sep 2nd, 2026"],
          destinationPage: "Sep 1st, 2026",
          roots: ["newer"],
        },
      });
      expect(toasts().map((toast) => toast.message)).toEqual([UNAVAILABLE_TOAST]);
      persistence.assertUnreached();
    });

    it("routes direct and crosses the day boundary", async () => {
      await loadTwoDays();
      selectBlock("newer");
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      await moveSelectionItems(1);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ direct: 1, unavailable: 0 });
      expect(pageByName("Sep 1st, 2026")!.roots).toEqual(["newer", "older"]);
    });
  });
});

describe("bulk insertion dispatch", () => {
  async function loadBulkTarget(): Promise<void> {
    await loadFeed([page("Bulk", "pages/bulk.md", "bulk-r1", [block("target", "target")])]);
  }

  it("routes a real Direct capture through the bulk verb", async () => {
    await loadBulkTarget();
    const save = vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "direct-r2" });

    expect(await captureToPage("Bulk", "- direct child")).toBe(true);

    expect(storageDispatchCounters("bulk-insertion")).toEqual({ direct: 1, unavailable: 0 });
    expect(lastStorageDispatch("bulk-insertion")).toEqual({
      operation: "bulk-insertion",
      route: "direct",
      request: { targetId: "target", targetPageName: "Bulk" },
    });
    expect(pageByName("Bulk")!.roots).toHaveLength(2);
    expect(save).toHaveBeenCalledTimes(1);
  });

  it("routes an unavailable capture through the bulk verb and refuses without mutation", async () => {
    await loadBulkTarget();
    unavailable();
    const save = vi.spyOn(backend(), "savePage");

    expect(await captureToPage("Bulk", "- refused child")).toBe(false);

    expect(storageDispatchCounters("bulk-insertion")).toEqual({ direct: 0, unavailable: 1 });
    expect(lastStorageDispatch("bulk-insertion")).toEqual({
      operation: "bulk-insertion",
      route: "unavailable",
      request: { targetId: "target", targetPageName: "Bulk" },
    });
    expect(pageByName("Bulk")!.roots).toEqual(["target"]);
    expect(save).not.toHaveBeenCalled();
    expect(toasts()).toHaveLength(1);
  });
});

describe("dropped-file insertion dispatch", () => {
  async function loadDropTarget(): Promise<void> {
    await loadFeed([page("Drop", "pages/drop.md", "drop-r1", [block("target", "target")])]);
  }

  it("routes unavailable and reports through the bulk-insertion refusal", async () => {
    await loadDropTarget();
    unavailable();
    const importAsset = vi.spyOn(backend(), "importAsset");

    await insertDroppedFiles("target", ["/tmp/a.png"]);

    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ direct: 0, unavailable: 1 });
    expect(lastStorageDispatch("dropped-file-insertion")).toEqual({
      operation: "dropped-file-insertion",
      route: "unavailable",
      request: { afterId: "target", paths: ["/tmp/a.png"] },
    });
    expect(importAsset).not.toHaveBeenCalled();
    expect(toasts().length).toBe(1);
  });

  it("routes direct", async () => {
    await loadDropTarget();
    vi.spyOn(backend(), "importAsset").mockResolvedValue("../assets/a.png" as any);

    await insertDroppedFiles("target", ["/tmp/a.png"]);

    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ direct: 1, unavailable: 0 });
  });
});

describe("carry dispatch", () => {
  // The refusal is taken at the OPERATION boundary, before the in-memory
  // carry, so an unadmitted slot is never left holding a mutation storage did
  // not accept.
  async function loadCarryDays(): Promise<string> {
    const today = journalTitle(new Date());
    const yesterday = "Aug 31st, 2026";
    clearConflict(today);
    clearConflict(yesterday);
    await loadFeed([
      page(today, "journals/today.md", "today-r1", [block("today-root", "")], "journal"),
      page(yesterday, "journals/2026_08_31.md", "y-r1", [block("task", "TODO carry me")], "journal"),
    ]);
    return yesterday;
  }

  it("refuses with no admission: no Direct write, and memory is untouched", async () => {
    const yesterday = await loadCarryDays();
    unavailable();
    const save = vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

    await carryDay(yesterday);

    expect(storageDispatchCounters("carry")).toEqual({ direct: 0, unavailable: 1 });
    expect(lastStorageDispatch("carry")).toEqual({
      operation: "carry",
      route: "unavailable",
      request: { destinationPage: journalTitle(new Date()), sourcePages: [yesterday] },
    });
    expect(save).not.toHaveBeenCalled();
    expect(pageByName(yesterday)!.roots).toEqual(["task"]);
    expect(pageByName(journalTitle(new Date()))!.roots).not.toContain("task");
  });

  it("records the direct route under a Direct binding and really carries", async () => {
    const yesterday = await loadCarryDays();
    vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

    await carryDay(yesterday);

    expect(storageDispatchCounters("carry")).toEqual({ direct: 1, unavailable: 0 });
    expect(pageByName(journalTitle(new Date()))!.roots).toContain("task");
    expect(pageByName(yesterday)!.roots).toEqual([]);
  });
});
