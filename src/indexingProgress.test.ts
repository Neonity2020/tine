import { describe, expect, it } from "vitest";
import type { IndexingProgress } from "./backend";
import { followIndexingProgress, indexingProgressLabel, type IndexingProgressDeps } from "./indexingProgress";

function scripted(polls: (IndexingProgress | null)[], opts: { warmAfterPoll?: number; epochChangesAfterPoll?: number } = {}) {
  let clock = 0;
  let polled = 0;
  let epoch = 1;
  let warmResolve: (ready: boolean) => void = () => {};
  const warm = new Promise<boolean>((resolve) => { warmResolve = resolve; });
  const deps: IndexingProgressDeps = {
    epoch: () => epoch,
    async progress() {
      const next = polls[Math.min(polled, polls.length - 1)];
      polled += 1;
      if (polled === opts.warmAfterPoll) warmResolve(true);
      if (polled === opts.epochChangesAfterPoll) epoch = 2;
      return next;
    },
    warmDone: () => warm,
    now: () => clock,
    async sleep(ms) { clock += ms; await Promise.resolve(); },
  };
  return { deps, polled: () => polled };
}

const building = (done: number): IndexingProgress => ({ phase: "indexing", done, total: 10_000 });

describe("indexing progress", () => {
  it("labels each pass with its page count", () => {
    expect(indexingProgressLabel({ phase: "checking", done: 3200, total: 10000 }))
      .toBe(`Checking search index · ${(3200).toLocaleString()} / ${(10000).toLocaleString()} pages`);
    expect(indexingProgressLabel({ phase: "reading", done: 1, total: 2 })).toBe("Reading pages · 1 / 2 pages");
    expect(indexingProgressLabel({ phase: "indexing", done: 0, total: 0 })).toBe("Building search index…");
  });

  it("keeps following a build that outlives the warm and hides once it ends", async () => {
    // The warm finishes on the first poll while the fresh build runs on.
    const { deps, polled } = scripted([building(0), building(5000), null, building(9000), null, null], { warmAfterPoll: 1 });
    const published: (IndexingProgress | null)[] = [];
    await followIndexingProgress(1, (p) => published.push(p), deps);
    // One empty poll between passes does not end it; two in a row do.
    expect(polled()).toBe(6);
    expect(published).toContainEqual(building(9000));
    expect(published.at(-1)).toBeNull();
  });

  it("does not flash on a graph that finishes quickly", async () => {
    const { deps } = scripted([building(1), null, null], { warmAfterPoll: 1 });
    const published: (IndexingProgress | null)[] = [];
    await followIndexingProgress(1, (p) => published.push(p), deps);
    expect(published.every((p) => p === null)).toBe(true);
  });

  it("stops when another graph is opened", async () => {
    const { deps, polled } = scripted([building(1)], { epochChangesAfterPoll: 3 });
    const published: (IndexingProgress | null)[] = [];
    await followIndexingProgress(1, (p) => published.push(p), deps);
    expect(polled()).toBe(3);
    expect(published.at(-1)).toBeNull();
  });
});

describe("indexing progress without a graph", () => {
  it("gives up when every poll fails", async () => {
    let polls = 0;
    const published: (IndexingProgress | null)[] = [];
    await followIndexingProgress(1, (p) => published.push(p), {
      epoch: () => 1,
      async progress() { polls += 1; throw new Error("no graph"); },
      warmDone: () => new Promise(() => {}),
      now: () => 0,
      async sleep() {},
    });
    expect(polls).toBe(10);
    expect(published.at(-1)).toBeNull();
  });
});
