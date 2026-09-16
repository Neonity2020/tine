// The durable-step ORDER of a Direct cross-page move (packet B2, I-3/I-2).
//
// `docs/contracts/direct-move-recovery.md` §3 names four durable steps and
// states they happen in this order:
//
//   1. commit the record   (`beginDirectCrossPageMove`)
//   2. write the destination
//   3. write each source, in record order
//   4. retire the record   (`finishDirectCrossPageMove`)
//
// The Rust side proves that a graph cut between any two of those steps
// converges (`crates/tine-core/src/direct_move_recovery_tests.rs`). That proof
// is only worth anything if the frontend actually EMITS them in that order: a
// record committed after the sources were written would describe a move that
// already happened, and one committed after the destination landed is a
// documented — and separately proven — benign window, not the contract.
//
// So this file pins the sequence at the boundary where it is decided, for every
// cross-page shape the app has: a drag, a multi-source drop, a one-day carry and
// an N-day carry. It is the file the contract and `src/store.ts` cite by name.

import { describe, it, expect, beforeAll, beforeEach, afterEach, vi } from "vitest";
import { initParser } from "./render/parse";
import { backend, SaveConflictError } from "./backend";
import { graphBindingRuntime } from "./graphBindingRuntime";
import { resetStorageDispatchCounters } from "./storageDispatch";
import {
  flushAll,
  flushPage,
  isDirty,
  loadFeed,
  moveBlock,
  moveBlocksRelative,
  moveSelectionItems,
  pageByName,
  resetStore,
  selectBlock,
  setRaw,
  settleDirectMovesForTest,
  undo,
} from "./store";
import { carryDay, carryDaysBack } from "./carry";
import { journalTitle } from "./journal";
import { clearConflict, setToasts } from "./ui";
import { resetPaneLayoutToSingle } from "./panes";
import type { BlockDto, PageDto } from "./types";

const MOVE_ID = "move-under-test";

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

/** One ordered log of every durable step the move emits, in emission order. */
type Step = ["begin", string, string[]] | ["save", string] | ["finish", string];

function recordSteps(beginReturns: string | null = MOVE_ID) {
  const steps: Step[] = [];
  vi.spyOn(backend(), "beginDirectCrossPageMove").mockImplementation(
    async (destination: PageDto, sources: PageDto[]) => {
      steps.push(["begin", destination.name, sources.map((source) => source.name)]);
      return beginReturns;
    },
  );
  vi.spyOn(backend(), "savePage").mockImplementation(async (dto: PageDto) => {
    steps.push(["save", dto.name]);
    return { revision: `${dto.rev ?? "r"}-next` } as any;
  });
  vi.spyOn(backend(), "finishDirectCrossPageMove").mockImplementation(async (moveId: string) => {
    steps.push(["finish", moveId]);
    return true;
  });
  return steps;
}

/** Saves of the same page collapse: the contract constrains the ORDER pages are
 *  first written in, not how many times the debounce coalesces a save. */
function order(steps: Step[]): Step[] {
  const seen = new Set<string>();
  return steps.filter((step) => {
    if (step[0] !== "save") return true;
    if (seen.has(step[1])) return false;
    seen.add(step[1]);
    return true;
  });
}

async function loadTwoPages(): Promise<void> {
  clearConflict("Source");
  clearConflict("Destination");
  await loadFeed([
    page("Source", "pages/source.md", "source-r1", [block("source-a", "a"), block("source-b", "b")]),
    page("Destination", "pages/destination.md", "destination-r1", [block("target", "target")]),
  ]);
}

/** Today plus `count` preceding days, each holding one unfinished task. */
async function loadCarryDays(count: number): Promise<{ today: string; days: string[] }> {
  const today = journalTitle(new Date());
  const base = new Date();
  const days: string[] = [];
  for (let i = 1; i <= count; i++) {
    const day = new Date(base);
    day.setDate(day.getDate() - i);
    days.push(journalTitle(day));
  }
  const dtos = [
    page(today, "journals/today.md", "today-r1", [block("today-root", "")], "journal"),
    ...days.map((name, i) =>
      page(name, `journals/back-${i + 1}.md`, `back-${i + 1}-r1`, [block(`task-${i + 1}`, "TODO carry me")], "journal"),
    ),
  ];
  for (const dto of dtos) clearConflict(dto.name);
  await loadFeed(dtos);
  return { today, days };
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
  graphBindingRuntime.clear();
  graphBindingRuntime.bind(1, { binding_generation: 1 });
});

afterEach(() => {
  graphBindingRuntime.clear();
  setToasts([]);
  vi.restoreAllMocks();
});

describe("the four durable steps, in contract order", () => {
  it("a drag across pages: record, destination, source, retire", async () => {
    await loadTwoPages();
    const steps = recordSteps();

    await moveBlock("source-a", null, 1, "Destination");
    await settleDirectMovesForTest();

    expect(order(steps)).toEqual([
      ["begin", "Destination", ["Source"]],
      ["save", "Destination"],
      ["save", "Source"],
      ["finish", MOVE_ID],
    ]);
    // …and the move really happened, so the order above is the order of a move
    // that landed, not of one that refused early.
    expect(pageByName("Destination")!.roots).toContain("source-a");
  });

  it("a multi-root selection dropped next to a target keeps the same order", async () => {
    await loadTwoPages();
    const steps = recordSteps();

    await moveBlocksRelative(["source-a", "source-b"], "target", "after");
    await settleDirectMovesForTest();

    expect(order(steps)).toEqual([
      ["begin", "Destination", ["Source"]],
      ["save", "Destination"],
      ["save", "Source"],
      ["finish", MOVE_ID],
    ]);
    expect(pageByName("Source")!.roots).toEqual([]);
  });

  it("a selection carried across the journal-feed day boundary keeps the same order", async () => {
    clearConflict("Sep 1st, 2026");
    clearConflict("Sep 2nd, 2026");
    await loadFeed([
      page("Sep 2nd, 2026", "journals/2026_09_02.md", "d2-r1", [block("newer", "newer")], "journal"),
      page("Sep 1st, 2026", "journals/2026_09_01.md", "d1-r1", [block("older", "older")], "journal"),
    ]);
    selectBlock("newer");
    const steps = recordSteps();

    await moveSelectionItems(1);
    await settleDirectMovesForTest();

    expect(order(steps)).toEqual([
      ["begin", "Sep 1st, 2026", ["Sep 2nd, 2026"]],
      ["save", "Sep 1st, 2026"],
      ["save", "Sep 2nd, 2026"],
      ["finish", MOVE_ID],
    ]);
  });

  it("a one-day carry emits the same four steps", async () => {
    const { today, days } = await loadCarryDays(1);
    const steps = recordSteps();

    await carryDay(days[0]);
    await settleDirectMovesForTest();

    expect(order(steps)).toEqual([
      ["begin", today, [days[0]]],
      ["save", today],
      ["save", days[0]],
      ["finish", MOVE_ID],
    ]);
  });

  it("an N-day carry names every source in the record, before any of them is written", async () => {
    const { today, days } = await loadCarryDays(3);
    const steps = recordSteps();

    await carryDaysBack(3);
    await settleDirectMovesForTest();

    const emitted = order(steps);
    expect(emitted[0]).toEqual(["begin", today, days]);
    expect(emitted[1]).toEqual(["save", today]);
    // Every source is written after the destination and before the retire; their
    // relative order among themselves is not a contract (they are independent
    // removals, and recovery classifies each participant on its own).
    expect(new Set(emitted.slice(2, -1))).toEqual(new Set(days.map((day) => ["save", day] as Step)));
    expect(emitted[emitted.length - 1]).toEqual(["finish", MOVE_ID]);
  });
});

describe("a carry whose destination save conflicts", () => {
  // Carry is a cross-page move, so it needs the same barrier as the other four
  // shapes (audit C#1): while today's write is not durable, nothing may save a
  // source day's post-removal state. `carryUnfinished` leaves the source days
  // clean and `carry.ts` marks them only after today lands, but nothing HELD
  // them, so an unrelated edit to a source day while today sat conflicted wrote
  // the carried task out of the only file that still had it.
  it("does not write the task out of its source day when a later edit saves that day", async () => {
    const today = journalTitle(new Date());
    const day = new Date();
    day.setDate(day.getDate() - 1);
    const source = journalTitle(day);
    for (const name of [today, source]) clearConflict(name);
    await loadFeed([
      page(today, "journals/today.md", "today-r1", [block("today-root", "")], "journal"),
      page(source, "journals/back-1.md", "back-1-r1", [
        block("carried-task", "TODO carry me"),
        block("staying-note", "a note that stays"),
      ], "journal"),
    ]);
    vi.spyOn(backend(), "beginDirectCrossPageMove").mockResolvedValue(MOVE_ID);
    vi.spyOn(backend(), "finishDirectCrossPageMove").mockResolvedValue(true);
    const sourceWrites: string[][] = [];
    vi.spyOn(backend(), "savePage").mockImplementation(async (dto: PageDto) => {
      // Syncthing delivered a newer today: every write of today is refused.
      if (dto.name === today) throw new SaveConflictError(7);
      if (dto.name === source) sourceWrites.push(dto.blocks.map((b) => b.raw));
      return { revision: `${dto.rev ?? "r"}-next` } as any;
    });

    await carryDay(source);
    expect(pageByName(today)!.roots).toContain("carried-task");

    // An unrelated edit to the source day, flushed as the debounce would.
    setRaw("staying-note", "a note that stays, edited");
    await flushPage(source);

    // Today is not on disk, so every write of the source must still hold the task…
    for (const raws of sourceWrites) expect(raws).toContain("TODO carry me");
    // …and the edit itself is written or still pending, never dropped.
    expect(sourceWrites.some((raws) => raws.includes("a note that stays, edited")) || isDirty(source)).toBe(true);
  });
});

describe("shapes that must NOT compose a record", () => {
  it("a same-page reorder composes none: it is one ordinary save, already atomic", async () => {
    await loadTwoPages();
    const steps = recordSteps();

    await moveBlock("source-b", null, 0, "Source");
    await settleDirectMovesForTest();

    expect(steps.filter((step) => step[0] !== "save")).toEqual([]);
  });
});

describe("a record that could not be composed", () => {
  it("does not refuse the move, and retires nothing", async () => {
    await loadTwoPages();
    // `null` is what the native side answers when the app-private root is
    // unavailable. Contract §4: that is a lost recovery guarantee for this one
    // move, never a refusal — refusing would turn a missing private directory
    // into an unusable editor.
    const steps = recordSteps(null);

    await moveBlock("source-a", null, 1, "Destination");
    await settleDirectMovesForTest();

    expect(order(steps)).toEqual([
      ["begin", "Destination", ["Source"]],
      ["save", "Destination"],
      ["save", "Source"],
    ]);
    expect(pageByName("Destination")!.roots).toContain("source-a");
    expect(pageByName("Source")!.roots).toEqual(["source-b"]);
  });
});

describe("undo of a cross-page move", () => {
  // H2 (K6 move census §1.3). `applyEntry` restores both page snapshots, marks
  // both dirty, and `scheduleSave` writes them in parallel: no destination-first
  // order, no source barrier, no recovery record. Undoing a move A→B makes A the
  // page that GAINS the blocks, so if B's removal lands while A's save is
  // refused, the blocks are in neither file. The five forward shapes are all
  // held; undo is the sixth shape `docs/contracts/direct-move-recovery.md` §1
  // does not name.
  it("does not write the losing page while the gaining page's save is refused", async () => {
    await loadTwoPages();
    let refuseSource = false;
    const writes: string[] = [];
    vi.spyOn(backend(), "beginDirectCrossPageMove").mockResolvedValue(MOVE_ID);
    vi.spyOn(backend(), "finishDirectCrossPageMove").mockResolvedValue(true);
    vi.spyOn(backend(), "savePage").mockImplementation(async (dto: PageDto) => {
      if (refuseSource && dto.name === "Source") throw new SaveConflictError(7);
      writes.push(dto.name);
      return { revision: `${dto.rev ?? "r"}-next` } as any;
    });

    await moveBlock("source-a", null, 1, "Destination");
    await settleDirectMovesForTest();
    expect(pageByName("Destination")!.roots).toContain("source-a");

    // The user changes their mind. Source is now the gaining page, and an
    // external write has landed on it in the meantime, so its save is refused.
    refuseSource = true;
    writes.length = 0;
    undo();
    await flushAll();
    await settleDirectMovesForTest();

    expect(pageByName("Source")!.roots).toContain("source-a");
    // Destination is the losing page: its removal must not reach disk while the
    // block is not back in Source's file.
    expect(writes).not.toContain("Destination");
  });
});

describe("two cross-page selection moves in one burst", () => {
  // H3 (K6 move census §1.3). `moveSelectionItems` captures the page and the
  // root check BEFORE awaiting `feedNeighbor` and `prepareCrossPageSources`, and
  // `crossMoveBlocks` then pushes the ids into the target's roots
  // unconditionally. A second keypress arriving while the first is still
  // awaiting a real flush re-adds ids the target already holds, so the block
  // renders twice and serializes twice — a duplicate `id::` if it carries one.
  it("never leaves the same root twice in the target day", async () => {
    clearConflict("Sep 1st, 2026");
    clearConflict("Sep 2nd, 2026");
    await loadFeed([
      page("Sep 2nd, 2026", "journals/2026_09_02.md", "d2-r1", [block("newer", "newer")], "journal"),
      page("Sep 1st, 2026", "journals/2026_09_01.md", "d1-r1", [block("older", "older")], "journal"),
    ]);
    vi.spyOn(backend(), "beginDirectCrossPageMove").mockResolvedValue(MOVE_ID);
    vi.spyOn(backend(), "finishDirectCrossPageMove").mockResolvedValue(true);
    vi.spyOn(backend(), "savePage").mockImplementation(async (dto: PageDto) => {
      return { revision: `${dto.rev ?? "r"}-next` } as any;
    });
    // The source day is dirty, which is the ordinary state right after the user
    // stopped typing: the pre-flush then really awaits, and the repeated key
    // lands inside that window.
    setRaw("newer", "newer, edited");
    selectBlock("newer");

    const first = moveSelectionItems(1);
    const second = moveSelectionItems(1);
    await Promise.all([first, second]);
    await settleDirectMovesForTest();

    const roots = pageByName("Sep 1st, 2026")!.roots;
    expect(roots.filter((id) => id === "newer")).toHaveLength(1);
    expect(new Set(roots).size).toBe(roots.length);
  });
});
