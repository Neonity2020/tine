import { describe, expect, it, beforeEach, vi } from "vitest";

let existing: string[] = [];
const calls: string[][] = [];
let epoch = 1;
let inventory = 0;
let aliases = 0;
let queuedResponses: Promise<string[]>[] = [];

vi.mock("./backend", () => ({
  backend: () => ({
    existingPageNames: async (names: string[]) => {
      calls.push([...names]);
      const queued = queuedResponses.shift();
      if (queued) return queued;
      return names.filter((name) => existing.includes(name));
    },
  }),
}));
vi.mock("./ui", () => ({
  graphEpoch: () => epoch,
  pageInventoryRev: () => inventory,
  aliasRev: () => aliases,
}));

const { pageIsMissing, resetPageExistsBatch } = await import("./pageExistsBatch");

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

describe("pageExistsBatch", () => {
  beforeEach(() => {
    calls.length = 0;
    existing = [];
    epoch = 1;
    inventory = 0;
    aliases = 0;
    queuedResponses = [];
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

  describe("GH #355: page-inventory changes invalidate batched answers", () => {
    it("restyles a same-session page creation immediately (missing → existing)", async () => {
      pageIsMissing("Later Target");
      await settle();
      expect(pageIsMissing("Later Target")).toBe(true);

      // The physical inventory moves (savePage baseline-null create, external
      // add, etc.): the blank-page style must flip without a restart.
      inventory++;
      existing = ["Later Target"];
      pageIsMissing("Later Target");
      await settle();

      expect(pageIsMissing("Later Target")).toBe(false);
      expect(calls).toEqual([["Later Target"], ["Later Target"]]);
    });

    it("does not leave a stale positive result after the page disappears", async () => {
      existing = ["Gone Soon"];
      pageIsMissing("Gone Soon");
      await settle();
      expect(pageIsMissing("Gone Soon")).toBe(false);

      inventory++;
      existing = [];
      pageIsMissing("Gone Soon");
      await settle();

      expect(pageIsMissing("Gone Soon")).toBe(true);
      expect(calls).toEqual([["Gone Soon"], ["Gone Soon"]]);
    });

    it("keeps one name asked once per (graph × inventory) state", async () => {
      existing = ["A"];
      pageIsMissing("A");
      await settle();
      pageIsMissing("A");
      await settle();
      expect(calls).toEqual([["A"]]);

      inventory++;
      pageIsMissing("A");
      await settle();
      pageIsMissing("A");
      await settle();
      expect(calls).toEqual([["A"], ["A"]]);
    });

    it("ignores an older inventory response that arrives after creation", async () => {
      let resolveOld!: (names: string[]) => void;
      queuedResponses.push(new Promise<string[]>((resolve) => { resolveOld = resolve; }));
      pageIsMissing("Racing Target");
      await Promise.resolve();

      inventory++;
      existing = ["Racing Target"];
      pageIsMissing("Racing Target");
      await settle();
      expect(pageIsMissing("Racing Target")).toBe(false);

      resolveOld([]);
      await settle();
      expect(pageIsMissing("Racing Target")).toBe(false);
      expect(calls).toEqual([["Racing Target"], ["Racing Target"]]);
    });
  });

  describe("GH #484: alias changes invalidate batched answers", () => {
    // `existing_page_names` answers over page names UNION alias names, so an
    // alias edit changes the answer without touching the physical page
    // inventory: adding `alias:: page1` to page2 creates and deletes no file.
    // Keying the cache on the inventory alone therefore left every `[[page1]]`
    // painted as a dead link until the next restart.
    it("restyles a reference the moment its name becomes an alias (missing → existing)", async () => {
      pageIsMissing("page1");
      await settle();
      expect(pageIsMissing("page1")).toBe(true);

      // The user adds `alias:: page1` to page2. No file is created or removed,
      // so the page inventory does NOT move — only the alias map does.
      aliases++;
      existing = ["page1"];
      pageIsMissing("page1");
      await settle();

      expect(pageIsMissing("page1")).toBe(false);
      expect(calls).toEqual([["page1"], ["page1"]]);
    });

    it("does not leave a stale positive result after an alias is removed", async () => {
      existing = ["page1"];
      pageIsMissing("page1");
      await settle();
      expect(pageIsMissing("page1")).toBe(false);

      aliases++;
      existing = [];
      pageIsMissing("page1");
      await settle();

      expect(pageIsMissing("page1")).toBe(true);
      expect(calls).toEqual([["page1"], ["page1"]]);
    });

    it("keeps one name asked once per (graph × inventory × alias) state", async () => {
      existing = ["A"];
      pageIsMissing("A");
      await settle();
      pageIsMissing("A");
      await settle();
      expect(calls).toEqual([["A"]]);

      aliases++;
      pageIsMissing("A");
      await settle();
      pageIsMissing("A");
      await settle();
      expect(calls).toEqual([["A"], ["A"]]);
    });
  });
});
