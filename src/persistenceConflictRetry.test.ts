import { beforeEach, describe, expect, it, vi } from "vitest";

// GH #254 increment 2, adversarial implementation verification, finding 3.
//
// A force ("Keep mine") consumes its one-shot authority BEFORE every fallible
// validation, by design — a failed or replayed attempt must not be able to
// reuse it. If the backend then cannot coherently observe the winner it mints
// nothing and returns a `conflict_retry.*` code, which is deliberately NOT
// banner class.
//
// But the page already had a banner, and the force spent its token. Leaving
// that banner up strands the user completely: "Keep mine" can never work again,
// the transient retry refuses to run while a page is conflicted, and the only
// live button — "Use disk version" — discards their unsaved edits.

const calls: { name: string; force: boolean }[] = [];
let nextResult: (() => Promise<string>) | null = null;

vi.mock("./store", () => ({
  doc: { loaded: true, pages: [] },
  pageByName: (name: string) => ({ name }),
  pageInstanceGeneration: () => 1,
  pageToDto: (name: string) => ({
    name, kind: "page", title: name, pre_block: null, blocks: [],
    format: "markdown", path: `pages/${name}.md`, guide: false, read_only: false,
  }),
}));

vi.mock("./backend", () => ({
  backend: () => ({
    savePage: (page: { name: string }, _baseRev: string | null, force: boolean) => {
      calls.push({ name: page.name, force });
      const result = nextResult;
      nextResult = null;
      return result ? result() : Promise.resolve("rev-after");
    },
  }),
}));

const conflicted = new Set<string>();
const toasts: string[] = [];
vi.mock("./ui", () => ({
  markConflict: (name: string) => conflicted.add(name),
  clearConflict: (name: string) => conflicted.delete(name),
  isConflicted: (name: string) => conflicted.has(name),
  conflicts: () => [...conflicted],
  bumpDataRev: () => {},
  bumpPageInventoryRev: () => {},
  pushToast: (message: string) => toasts.push(message),
}));

const { forceSave, markDirty, resetSaveState } = await import("./persistence");

describe("a tokenless force does not strand the page behind a spent banner", () => {
  beforeEach(() => {
    calls.length = 0;
    toasts.length = 0;
    conflicted.clear();
    nextResult = null;
    resetSaveState();
  });

  it("retracts the spent banner and lets the retry reach the backend", async () => {
    markDirty("Notes");
    conflicted.add("Notes"); // the banner the user is looking at
    nextResult = () => Promise.reject(new Error("conflict_retry.replace_pre_retirement: ..."));

    expect(await forceSave("Notes")).toBe(false);

    // The banner is gone: it stood on authority this attempt already spent.
    expect(conflicted.has("Notes")).toBe(false);

    // …and the retry actually calls the backend, which the conflicted gate
    // would otherwise forbid.
    await vi.waitFor(() => expect(calls.length).toBe(2));
    expect(calls[0]).toEqual({ name: "Notes", force: true });
    expect(calls[1].force).toBe(false);
  });

  it("re-raises a real conflict from the retry, with a fresh banner", async () => {
    markDirty("Notes");
    conflicted.add("Notes");
    nextResult = () => Promise.reject(new Error("conflict_retry.commit_recheck: ..."));

    await forceSave("Notes");
    expect(conflicted.has("Notes")).toBe(false);

    nextResult = () => Promise.reject(new Error("conflict"));
    await vi.waitFor(() => expect(calls.length).toBe(2));
    await vi.waitFor(() => expect(conflicted.has("Notes")).toBe(true));
  });

  // A banner-class conflict is unchanged: it mints authority, so its banner is
  // live and must stay up.
  it("leaves a banner-class conflict exactly as it was", async () => {
    markDirty("Notes");
    nextResult = () => Promise.reject(new Error("conflict"));

    expect(await forceSave("Notes")).toBe(false);

    expect(conflicted.has("Notes")).toBe(true);
    expect(calls.length).toBe(1);
  });
});
