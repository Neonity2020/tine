import { afterEach, describe, expect, it } from "vitest";
import { __setBackendForTest, type Backend } from "./backend";
import type { JournalConflict } from "./types";
import { journalConflicts, refreshJournalConflicts, setJournalConflicts } from "./ui";

// GH #543, audit R6-02: `list_journal_conflicts` walks journals/, so it runs
// off the main thread and overlapping refreshes (graph open, a title-format
// change, a resolution) can answer out of order. The newest one must win.
describe("refreshJournalConflicts", () => {
  afterEach(() => {
    __setBackendForTest(null);
    setJournalConflicts([]);
  });

  it("keeps the newest refresh's answer when an older one finishes last", async () => {
    const day = { date: "2026-09-22" } as unknown as JournalConflict;
    const replies: Array<(value: JournalConflict[]) => void> = [];
    __setBackendForTest({
      listJournalConflicts: () => new Promise<JournalConflict[]>((resolve) => replies.push(resolve)),
    } as unknown as Backend);
    const older = refreshJournalConflicts();
    const newer = refreshJournalConflicts();
    replies[1]([]); // the duplicate day was just resolved
    await newer;
    replies[0]([day]); // a scan begun before the resolution
    await older;
    expect(journalConflicts()).toEqual([]);
  });
});
