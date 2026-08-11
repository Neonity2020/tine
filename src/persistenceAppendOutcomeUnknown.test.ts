import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const saves: string[] = [];
const toasts: Array<{ message: string; tone: string }> = [];
let nextSave: () => Promise<string>;

vi.mock("./store", () => ({
  doc: { loaded: true, pages: [] },
  bumpEditGeneration: () => {},
  editorActivationFor: () => 1,
  peekPageInstanceGeneration: () => 1,
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
    activation: 1,
  }),
  sweepReplaceable: () => {},
}));

vi.mock("./backend", () => ({
  backend: () => ({
    savePage: (page: { name: string }) => {
      saves.push(page.name);
      return nextSave();
    },
  }),
}));

vi.mock("./ui", () => ({
  markConflict: () => {},
  clearConflict: () => {},
  isConflicted: () => false,
  conflicts: () => [],
  bumpDataRev: () => {},
  bumpPageInventoryRev: () => {},
  pushToast: (message: string, tone: string) => toasts.push({ message, tone }),
}));

const { dirtyPages, markDirty, resetSaveState } = await import("./persistence");

describe("managed append outcome uncertainty", () => {
  beforeEach(() => {
    saves.length = 0;
    toasts.length = 0;
    nextSave = () => Promise.resolve("saved");
    resetSaveState();
    vi.useFakeTimers();
  });

  afterEach(() => {
    vi.useRealTimers();
  });

  it("keeps drafts dirty and suppresses every automatic retry until reset", async () => {
    nextSave = () => Promise.reject(new Error(
      "sync actor refused application page intent at committing the semantic page transaction "
      + "(reason code: trusted_local.append_outcome_unknown)"
    ));
    markDirty("First");
    await vi.advanceTimersByTimeAsync(500);

    expect(saves).toEqual(["First"]);
    expect([...dirtyPages()]).toContain("First");
    expect(toasts).toEqual([{
      message: "Managed storage could not establish the append outcome. Reopen Tine before saving again.",
      tone: "error",
    }]);

    markDirty("Second");
    await vi.advanceTimersByTimeAsync(10_000);
    expect(saves).toEqual(["First"]);
    expect([...dirtyPages()]).toEqual(expect.arrayContaining(["First", "Second"]));
    expect(toasts).toHaveLength(1);

    resetSaveState();
    nextSave = () => Promise.resolve("after-reopen");
    markDirty("Second");
    await vi.advanceTimersByTimeAsync(500);
    expect(saves).toEqual(["First", "Second"]);
  });

  it("continues to retry ordinary transient failures", async () => {
    let attempts = 0;
    nextSave = () => {
      attempts++;
      return attempts === 1 ? Promise.reject(new Error("EBUSY")) : Promise.resolve("saved");
    };
    markDirty("Transient");
    await vi.advanceTimersByTimeAsync(500);
    await vi.advanceTimersByTimeAsync(150);

    expect(saves).toEqual(["Transient", "Transient"]);
    expect([...dirtyPages()]).not.toContain("Transient");
  });
});
