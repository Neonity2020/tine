import { beforeEach, describe, expect, it, vi } from "vitest";

// The cross-page move barrier is a data-loss guard, so its interaction with
// "keep mine" is worth driving through the real save path rather than a helper.
// persistence.ts depends on store/backend/ui only through narrow call-time
// seams, so mocking those three reaches `doSave` itself.

const saved: { name: string; force: boolean }[] = [];

vi.mock("./store", () => ({
  doc: { loaded: true, pages: [] },
  pageByName: (name: string) => ({ name }),
  pageInstanceGeneration: () => 1,
  pageToDto: (name: string) => ({
    name,
    kind: "page",
    title: name,
    pre_block: null,
    blocks: [],
    format: "markdown",
    path: `pages/${name}.md`,
    guide: false,
    read_only: false,
  }),
}));

vi.mock("./backend", () => ({
  backend: () => ({
    savePage: (page: { name: string }, _baseRev: string | null, force: boolean) => {
      saved.push({ name: page.name, force });
      return Promise.resolve("rev-after");
    },
  }),
}));

const conflicted = new Set<string>();
vi.mock("./ui", () => ({
  markConflict: (name: string) => conflicted.add(name),
  isConflicted: (name: string) => conflicted.has(name),
  conflicts: () => [...conflicted],
  bumpDataRev: () => {},
  bumpPageInventoryRev: () => {},
  pushToast: () => {},
}));

const { forceSave, holdSourcesForDest, markDirty, flushPage } = await import("./persistence");

describe("cross-page move barrier vs keep-mine", () => {
  beforeEach(() => {
    saved.length = 0;
    conflicted.clear();
  });

  // Before GH-audit N1 was fixed, force-save could never reach the backend, so
  // `force` bypassing this barrier was latent. It is live now: a page can be
  // BOTH a source of an in-flight cross-page move and conflicted, and the two
  // rules guard different things. Overriding the conflict must not also
  // override the barrier — that writes the moved block out of existence in the
  // source before it is durable in the destination.
  it("defers a keep-mine on a held source, then applies it on release", async () => {
    holdSourcesForDest("Dest", ["Source"]);
    conflicted.add("Source");

    const overridden = await forceSave("Source");

    expect(overridden).toBe(false);
    expect(saved).toEqual([]);

    // The destination write lands; its sources are freed.
    markDirty("Dest");
    await flushPage("Dest");

    // Give the re-issued forced save its microtask.
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(saved.map((entry) => entry.name)).toContain("Source");
    expect(saved.find((entry) => entry.name === "Source")?.force).toBe(true);
  });
});
