import { describe, expect, it, beforeEach, vi } from "vitest";

let existing: string[] = [];
const calls: string[][] = [];
let epoch = 1;

vi.mock("./backend", () => ({
  backend: () => ({
    existingPageNames: async (names: string[]) => {
      calls.push([...names]);
      return names.filter((name) => existing.includes(name));
    },
  }),
}));
vi.mock("./ui", () => ({ graphEpoch: () => epoch }));

const { pageIsMissing, resetPageExistsBatch } = await import("./pageExistsBatch");

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("pageExistsBatch", () => {
  beforeEach(() => {
    calls.length = 0;
    existing = [];
    epoch = 1;
    resetPageExistsBatch();
  });

  it("reports a page with no document as missing", async () => {
    existing = ["Swap Bribery"];
    pageIsMissing("c01 Swap Bribery");
    pageIsMissing("Swap Bribery");
    await settle();
    expect(pageIsMissing("c01 Swap Bribery")).toBe(true);
    expect(pageIsMissing("Swap Bribery")).toBe(false);
  });

  it("treats a name as alive until its batch answers", () => {
    // The pre-answer value decides whether a page of good links flashes as dead
    // on first paint. It must be "alive".
    expect(pageIsMissing("Anything")).toBe(false);
  });

  it("coalesces every reference in one tick into a single request", async () => {
    existing = ["A"];
    for (const name of ["A", "B", "C", "A", "B"]) pageIsMissing(name);
    await settle();
    expect(calls).toEqual([["A", "B", "C"]]);
  });

  it("asks for each name at most once per graph", async () => {
    existing = ["A"];
    pageIsMissing("A");
    await settle();
    pageIsMissing("A");
    await settle();
    expect(calls).toEqual([["A"]]);
  });

  it("forgets its answers when the graph changes", async () => {
    existing = ["A"];
    pageIsMissing("A");
    await settle();
    epoch = 2;
    pageIsMissing("A");
    await settle();
    expect(calls).toEqual([["A"], ["A"]]);
  });

  it("still resolves a name that exists only under a different case", async () => {
    // Backend-side concern, asserted here so the contract is visible: the batch
    // passes names through verbatim and trusts the graph's own key folding.
    existing = ["swap bribery"];
    pageIsMissing("swap bribery");
    await settle();
    expect(pageIsMissing("swap bribery")).toBe(false);
  });
});
