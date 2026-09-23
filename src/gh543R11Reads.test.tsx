// GH #543, indexing audit round 11: index-backed reads the frontend issues
// while a whole-graph pass runs. The pass is modelled by a command that never
// answers, which is how a read waits in the backend until the pass ends.
import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { __setBackendForTest, backend } from "./backend";
import { mockBackend } from "./mock";
import { bumpDataRev, bumpGraphEpoch, pruneSidebarBlocks, rightSidebar, setRightSidebar } from "./ui";
import { resolveBlockBatched } from "./resolveBatch";
import { BlockReferences } from "./components/BlockReferences";

const settle = async (n = 5) => {
  for (let i = 0; i < n; i++) await new Promise((r) => setTimeout(r, 0));
};
const pass = () => new Promise<never>(() => {});
let dispose: (() => void) | null = null;
afterEach(() => {
  dispose?.();
  dispose = null;
  vi.restoreAllMocks();
  setRightSidebar([]);
  // Each test's parked reads belong to its own graph epoch.
  bumpGraphEpoch();
});

describe("dataRev-keyed reads during an index pass (R11-09)", () => {
  it("block references keep one read in flight across saves", async () => {
    __setBackendForTest(mockBackend());
    let inFlight = 0;
    let peak = 0;
    const referrers = vi.spyOn(backend(), "getBlockReferrers").mockImplementation(async () => {
      inFlight++;
      peak = Math.max(peak, inFlight);
      return pass();
    });
    dispose = render(() => <BlockReferences id="b-r11-1" />, document.createElement("div"));
    await settle();
    for (let i = 0; i < 6; i++) {
      bumpDataRev();
      await settle(2);
    }
    expect(referrers.mock.calls.length, "block_referrers commands").toBe(1);
    expect(peak).toBe(1);
  });

  it("a visible block ref re-resolved after each save sends one batch at a time", async () => {
    __setBackendForTest(mockBackend());
    const batches = vi.spyOn(backend(), "resolveBlocks").mockImplementation(async () => pass());
    void resolveBlockBatched("65f0c0de-0000-4000-8000-00000000aaaa");
    await settle();
    for (let i = 0; i < 6; i++) {
      bumpDataRev();
      void resolveBlockBatched("65f0c0de-0000-4000-8000-00000000aaaa");
      await settle(2);
    }
    expect(batches.mock.calls.length, "resolve_blocks commands").toBe(1);
  });

  it("a batch superseded while it queued answers its waiters at the newest revision", async () => {
    __setBackendForTest(mockBackend());
    let release!: () => void;
    const first = new Promise<void>((resolve) => (release = resolve));
    const batches = vi.spyOn(backend(), "resolveBlocks").mockImplementation(async (ids) => {
      if (batches.mock.calls.length === 1) await first;
      return ids.map((id) => ({ page: `answer-${batches.mock.calls.length}`, kind: "page", blocks: [{ id }] }) as never);
    });
    void resolveBlockBatched("65f0c0de-0000-4000-8000-00000000bbbb");
    await settle();
    bumpDataRev();
    const waiter = resolveBlockBatched("65f0c0de-0000-4000-8000-00000000bbbb");
    await settle();
    bumpDataRev();
    const newest = resolveBlockBatched("65f0c0de-0000-4000-8000-00000000bbbb");
    await settle();
    release();
    const [stale, fresh] = await Promise.all([waiter, newest]);
    expect(batches.mock.calls.length).toBe(2);
    expect(stale?.page).toBe("answer-2");
    expect(fresh?.page).toBe("answer-2");
  });

  it("a read for a new graph epoch does not wait behind one parked on the old graph", async () => {
    __setBackendForTest(mockBackend());
    const batches = vi.spyOn(backend(), "resolveBlocks").mockImplementation(async (ids) => {
      if (batches.mock.calls.length === 1) return pass();
      return ids.map((id) => ({ page: "new graph", kind: "page", blocks: [{ id }] }) as never);
    });
    void resolveBlockBatched("65f0c0de-0000-4000-8000-00000000cccc");
    await settle();
    bumpGraphEpoch();
    const answer = await Promise.race([
      resolveBlockBatched("65f0c0de-0000-4000-8000-00000000cccc"),
      settle(20).then(() => "still waiting"),
    ]);
    expect(answer).toMatchObject({ page: "new graph" });
  });
});

describe("launch sidebar prune (R11-10)", () => {
  it("resolves every restored pin in one command and keeps pins a failed read cannot judge", async () => {
    __setBackendForTest(mockBackend());
    const single = vi.spyOn(backend(), "resolveBlock").mockResolvedValue(null);
    const many = vi.spyOn(backend(), "resolveBlocks").mockImplementation(async (ids) =>
      ids.map((id, i) => (i === 0 ? null : ({ page: "Alpha", kind: "page", blocks: [{ id }] } as never))),
    );
    const pins = Array.from({ length: 5 }, (_, n) => ({
      kind: "block" as const,
      uuid: `65f0c0de-0000-4000-8000-00000000000${n}`,
      page: "Alpha",
      pageKind: "page" as const,
    }));
    setRightSidebar(pins);
    await pruneSidebarBlocks();
    expect(single).not.toHaveBeenCalled();
    expect(many).toHaveBeenCalledTimes(1);
    expect(many.mock.calls[0][0]).toHaveLength(5);

    many.mockRejectedValueOnce(new Error("rebinding"));
    await pruneSidebarBlocks();
    expect(
      pins.slice(1).every((pin) => JSON.stringify(rightSidebar()).includes(pin.uuid)),
      "a failed read removed pins",
    ).toBe(true);
  });
});
