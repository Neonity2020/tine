// The REAL paths go through the storage-authority front door (I-6, I-20).
//
// `storageDispatch.test.ts` proves the dispatcher branches correctly in
// isolation; this file proves the production call sites actually use it, at the
// layer where a user observes the failure:
//
//   - instrumentation gate: each semantic operation records a dispatch, with
//     the intent it stated, on the route its admission selects;
//   - capability gate: under a managed binding a cross-page move never reaches
//     the Direct persistence entry points (`backend().savePage`, dirty marks);
//   - staleness gate (I-20): a binding that changes between the intent's
//     capture and its async landing makes the operation refuse — it does NOT
//     fall back to the other backend.
//
// B1 is behaviour-preserving, so nothing here asserts a NEW outcome: every
// expectation is the behaviour master already had, now pinned to the front door.

import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from "vitest";
import { initParser } from "./render/parse";
import { backend } from "./backend";
import { managedStorageRuntime } from "./managedStorageRuntime";
import {
  lastStorageDispatch,
  resetStorageDispatchCounters,
  storageDispatchCounters,
} from "./storageDispatch";
import {
  __setStoreMutationObserverForTest,
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

const GENERATION = 7;

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

function managedWritable(generation = GENERATION): void {
  managedStorageRuntime.clear();
  managedStorageRuntime.bind(generation);
  managedStorageRuntime.receiveStatus({
    state: "active",
    runtime: null,
    can_activate: false,
    can_retry: false,
    can_cancel: false,
    cancel_reason: null,
    binding_generation: generation,
    application_page_admission: {
      binding_generation: generation,
      authority: "managed_writable",
      application_save_page_blocks: 511,
      application_page_request_text_bytes: 1_048_576,
      application_page_max_depth: 128,
    },
  } as any);
}

function managedUnavailable(): void {
  managedStorageRuntime.clear();
  managedStorageRuntime.bind(GENERATION, {
    binding_generation: GENERATION,
    authority: "managed_unavailable",
  });
}

function direct(): void {
  managedStorageRuntime.clear();
  managedStorageRuntime.bind(1, { binding_generation: 1, authority: "direct" });
}

/** Every Direct persistence entry point a managed binding must not reach. */
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
        "I-6: a managed-bound slot reached Direct persistence. Semantic storage "
        + "operations dispatch through src/storageDispatch.ts, whose managed arm "
        + "is the native request — see storageDispatch.test.ts.",
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

/** The managed actor, always refusing with a typed no-commit: the route is what
 *  is under test, not the actor's success path. */
function refusingActor() {
  return vi.spyOn(backend(), "moveManagedApplicationSubtrees").mockImplementation(
    async (_binding: any, request: any) => ({
      binding_generation: GENERATION,
      application_page_admission: managedStorageRuntime.snapshot().applicationPageAdmission!,
      outcome: { status: "no_commit", episode_id: request.episode_id, reason: "stale_source" },
    }) as any,
  );
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
  managedStorageRuntime.clear();
  setToasts([]);
  vi.restoreAllMocks();
});

describe("cross-page move dispatch — the four move shapes", () => {
  describe("moveBlock (drag across pages)", () => {
    it("routes managed, states its intent, and never reaches Direct persistence", async () => {
      await loadTwoPages();
      managedWritable();
      const actor = refusingActor();
      const persistence = watchDirectPersistence();

      await moveBlock("source", null, 1, "Destination");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "managed",
        request: { sourcePages: ["Source"], destinationPage: "Destination", roots: ["source"] },
      });
      expect(actor).toHaveBeenCalledTimes(1);
      persistence.assertUnreached();
      expect(pageByName("Source")!.roots).toEqual(["source"]);
    });

    it("routes unavailable, refuses with the shared toast, and mutates nothing", async () => {
      await loadTwoPages();
      managedUnavailable();
      const persistence = watchDirectPersistence();

      await moveBlock("source", null, 1, "Destination");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
      expect(toasts().map((toast) => toast.message)).toEqual([
        "Can't move between pages while managed storage is changing state.",
      ]);
      persistence.assertUnreached();
      expect(pageByName("Source")!.roots).toEqual(["source"]);
    });

    it("routes direct and runs the Direct choreography", async () => {
      await loadTwoPages();
      const save = vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      await moveBlock("source", null, 1, "Destination");
      await settleDirectMovesForTest();

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
      expect(pageByName("Source")!.roots).toEqual([]);
      expect(pageByName("Destination")!.roots).toEqual(["target", "source"]);
      expect(save).toHaveBeenCalled();
    });

    it("does not dispatch at all for a same-page reorder", async () => {
      await loadFeed([
        page("Source", "pages/source.md", "source-r1", [block("a", "a"), block("b", "b")]),
      ]);
      await moveBlock("b", null, 0, "Source");
      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 0 });
    });
  });

  describe("moveBlocksRelative (selection dropped next to a target)", () => {
    it("routes managed and never reaches Direct persistence", async () => {
      await loadTwoPages();
      managedWritable();
      const actor = refusingActor();
      const persistence = watchDirectPersistence();

      await moveBlocksRelative(["source"], "target", "after");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "managed",
        request: { sourcePages: ["Source"], destinationPage: "Destination", roots: ["source"] },
      });
      expect(actor).toHaveBeenCalledTimes(1);
      persistence.assertUnreached();
    });

    it("routes unavailable and refuses", async () => {
      await loadTwoPages();
      managedUnavailable();
      const persistence = watchDirectPersistence();

      expect(await moveBlocksRelative(["source"], "target", "after")).toBe(false);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
      expect(toasts().map((toast) => toast.message)).toEqual([
        "Can't move between pages while managed storage is changing state.",
      ]);
      persistence.assertUnreached();
    });

    it("routes direct and runs the Direct choreography", async () => {
      await loadTwoPages();
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      expect(await moveBlocksRelative(["source"], "target", "after")).toBe(true);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
      expect(pageByName("Destination")!.roots).toEqual(["target", "source"]);
      expect(pageByName("Source")!.roots).toEqual([]);
    });

    it("does not dispatch at all for a same-page relative move", async () => {
      await loadFeed([
        page("Source", "pages/source.md", "source-r1", [block("a", "a"), block("b", "b")]),
      ]);
      expect(await moveBlocksRelative(["a"], "b", "after")).toBe(true);
      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 0 });
    });
  });

  describe("moveBlockFeed (single block across a journal-day boundary)", () => {
    it("routes managed and never reaches Direct persistence", async () => {
      await loadTwoDays();
      managedWritable();
      const actor = refusingActor();
      const persistence = watchDirectPersistence();

      expect(await moveBlockFeed("newer", 1)).toBe("none");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "managed",
        request: {
          sourcePages: ["Sep 2nd, 2026"],
          destinationPage: "Sep 1st, 2026",
          roots: ["newer"],
        },
      });
      expect(actor).toHaveBeenCalledTimes(1);
      persistence.assertUnreached();
    });

    it("routes unavailable and refuses", async () => {
      await loadTwoDays();
      managedUnavailable();
      const persistence = watchDirectPersistence();

      expect(await moveBlockFeed("newer", 1)).toBe("none");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
      expect(toasts().map((toast) => toast.message)).toEqual([
        "Can't move between pages while managed storage is changing state.",
      ]);
      persistence.assertUnreached();
    });

    it("routes direct and crosses the day boundary", async () => {
      await loadTwoDays();
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      expect(await moveBlockFeed("newer", 1)).toBe("crossed");

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
      expect(pageByName("Sep 1st, 2026")!.roots).toEqual(["newer", "older"]);
    });
  });

  describe("moveSelectionItems (whole selection across a journal-day boundary)", () => {
    it("routes managed and never reaches Direct persistence", async () => {
      await loadTwoDays();
      selectBlock("newer");
      managedWritable();
      const actor = refusingActor();
      const persistence = watchDirectPersistence();

      await moveSelectionItems(1);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
      expect(lastStorageDispatch("cross-page-move")).toEqual({
        operation: "cross-page-move",
        route: "managed",
        request: {
          sourcePages: ["Sep 2nd, 2026"],
          destinationPage: "Sep 1st, 2026",
          roots: ["newer"],
        },
      });
      expect(actor).toHaveBeenCalledTimes(1);
      persistence.assertUnreached();
    });

    it("routes unavailable and refuses", async () => {
      await loadTwoDays();
      selectBlock("newer");
      managedUnavailable();
      const persistence = watchDirectPersistence();

      await moveSelectionItems(1);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
      expect(toasts().map((toast) => toast.message)).toEqual([
        "Can't move between pages while managed storage is changing state.",
      ]);
      persistence.assertUnreached();
    });

    it("routes direct and crosses the day boundary", async () => {
      await loadTwoDays();
      selectBlock("newer");
      vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

      await moveSelectionItems(1);

      expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
      expect(pageByName("Sep 1st, 2026")!.roots).toEqual(["newer", "older"]);
    });
  });
});

describe("stale binding across an async managed move (I-20)", () => {
  it("refuses the landing and does NOT fall back to the Direct backend", async () => {
    await loadTwoPages();
    managedWritable();
    const actor = vi.spyOn(backend(), "moveManagedApplicationSubtrees");
    const persistence = watchDirectPersistence();

    // The intent is captured synchronously against generation 7; the queued run
    // executes a microtask later. Rebinding in between is exactly the staleness
    // the binding_generation check exists for.
    const pending = moveBlock("source", null, 1, "Destination");
    managedWritable(GENERATION + 2);
    await pending;

    expect(actor).not.toHaveBeenCalled();
    persistence.assertUnreached();
    expect(pageByName("Source")!.roots).toEqual(["source"]);
    expect(pageByName("Destination")!.roots).toEqual(["target"]);
    // The route was still decided once, on the admission live at the time.
    expect(storageDispatchCounters("cross-page-move")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
  });
});

describe("dropped-file insertion dispatch", () => {
  async function loadDropTarget(): Promise<void> {
    await loadFeed([page("Drop", "pages/drop.md", "drop-r1", [block("target", "target")])]);
  }

  it("routes managed", async () => {
    await loadDropTarget();
    managedWritable();
    vi.spyOn(backend(), "importAsset").mockResolvedValue("../assets/a.png" as any);

    await insertDroppedFiles("target", ["/tmp/a.png"]);

    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
    expect(lastStorageDispatch("dropped-file-insertion")).toEqual({
      operation: "dropped-file-insertion",
      route: "managed",
      request: { afterId: "target", paths: ["/tmp/a.png"] },
    });
  });

  it("routes unavailable and reports through the bulk-insertion preflight", async () => {
    await loadDropTarget();
    managedUnavailable();
    const importAsset = vi.spyOn(backend(), "importAsset");

    await insertDroppedFiles("target", ["/tmp/a.png"]);

    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ managed: 0, direct: 0, unavailable: 1 });
    expect(importAsset).not.toHaveBeenCalled();
    expect(toasts().length).toBe(1);
  });

  it("routes direct", async () => {
    await loadDropTarget();
    vi.spyOn(backend(), "importAsset").mockResolvedValue("../assets/a.png" as any);

    await insertDroppedFiles("target", ["/tmp/a.png"]);

    expect(storageDispatchCounters("dropped-file-insertion")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
  });
});

describe("carry dispatch — B2 gave carry a route", () => {
  // B1 asserted the opposite of these: carry ran the Direct choreography under a
  // managed binding and B1 pinned that gap so B2 had to delete the assertion
  // deliberately. It is deleted here. The refusal is taken at the OPERATION
  // boundary, before the in-memory carry, so a managed binding is never left
  // holding a mutation storage did not accept.
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

  it("refuses under a managed binding: no Direct write, and memory is untouched", async () => {
    const yesterday = await loadCarryDays();
    managedWritable();
    const save = vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

    await carryDay(yesterday);

    expect(storageDispatchCounters("carry")).toEqual({ managed: 1, direct: 0, unavailable: 0 });
    expect(lastStorageDispatch("carry")).toEqual({
      operation: "carry",
      route: "managed",
      request: { destinationPage: journalTitle(new Date()), sourcePages: [yesterday] },
    });
    // I-6: a managed-bound slot must never reach Direct persistence.
    expect(save).not.toHaveBeenCalled();
    // And the refusal is taken BEFORE `carryUnfinished`, so the editor is not
    // left holding a move managed storage never accepted.
    expect(pageByName(yesterday)!.roots).toEqual(["task"]);
    expect(pageByName(journalTitle(new Date()))!.roots).not.toContain("task");
  });

  it("records the direct route under a Direct binding and really carries", async () => {
    const yesterday = await loadCarryDays();
    vi.spyOn(backend(), "savePage").mockResolvedValue(null as any);

    await carryDay(yesterday);

    expect(storageDispatchCounters("carry")).toEqual({ managed: 0, direct: 1, unavailable: 0 });
    expect(pageByName(journalTitle(new Date()))!.roots).toContain("task");
    expect(pageByName(yesterday)!.roots).toEqual([]);
  });
});
