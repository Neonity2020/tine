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
import { backend } from "./backend";
import { managedStorageRuntime } from "./managedStorageRuntime";
import { resetStorageDispatchCounters } from "./storageDispatch";
import {
  loadFeed,
  moveBlock,
  moveBlocksRelative,
  moveSelectionItems,
  pageByName,
  resetStore,
  selectBlock,
  settleDirectMovesForTest,
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
  managedStorageRuntime.clear();
  managedStorageRuntime.bind(1, { binding_generation: 1, authority: "direct" });
});

afterEach(() => {
  managedStorageRuntime.clear();
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
