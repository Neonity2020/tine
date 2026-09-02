import { afterEach, describe, expect, it, vi } from "vitest";
import { __setBackendForTest, type Backend } from "./backend";
import {
  clearLiveSaveConflict,
  conflictQueue,
  registerLiveSaveConflict,
  retireLiveSaveConflict,
  setConflictQueue,
  setGraphMeta,
  setToasts,
  toasts,
} from "./ui";
import type { PageDto } from "./types";

// Wave-2 review B3-1: `clearLiveSaveConflict` dropped the in-memory draft but
// never retired the app-private capsule, so a resolved draft resurrected after
// the next restart. Every clear path retires durably; a retirement that cannot
// be proven keeps the draft in the queue (awaited path) or tells the user
// (fire-and-forget path).

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

const page: PageDto = {
  name: "Draft",
  path: "pages/Draft.md",
  format: "md",
  blocks: [{ id: "b1", raw: "retained draft", children: [] }],
  rev: "rev-1",
} as unknown as PageDto;

function stubBackend(overrides: Partial<Backend>): { retire: ReturnType<typeof vi.fn> } {
  const retire = vi.fn(async () => {});
  __setBackendForTest({
    storeConflictCapsule: async () => {},
    loadConflictCapsules: async () => [],
    retireConflictCapsule: retire,
    ...overrides,
  } as unknown as Backend);
  return { retire };
}

afterEach(() => {
  __setBackendForTest(null);
  setConflictQueue([]);
  setGraphMeta(null);
  setToasts([]);
});

describe("live-save conflict retirement", () => {
  it("clearLiveSaveConflict retires the on-disk capsule for the cleared page", async () => {
    const { retire } = stubBackend({});
    setGraphMeta({ root: "/graphs/A" } as never);
    await registerLiveSaveConflict(page, "rev-1", 1);
    expect(conflictQueue().map((c) => c.page_name)).toEqual(["Draft"]);

    clearLiveSaveConflict("Draft");
    await flush();

    expect(conflictQueue()).toEqual([]);
    expect(retire).toHaveBeenCalledWith("/graphs/A", "Draft");
  });

  it("tells the user when a fire-and-forget retirement fails instead of silently resurrecting the draft", async () => {
    const { retire } = stubBackend({
      retireConflictCapsule: vi.fn(async () => {
        throw new Error("EIO");
      }),
    });
    void retire;
    setGraphMeta({ root: "/graphs/A" } as never);
    await registerLiveSaveConflict(page, "rev-1", 1);

    clearLiveSaveConflict("Draft");
    await flush();

    expect(conflictQueue()).toEqual([]);
    const toast = toasts().at(-1);
    expect(toast?.kind).toBe("error");
    expect(toast?.message).toContain("could not retire its restart-recovery copy");
  });

  it("retireLiveSaveConflict keeps the draft queued until the capsule is durably gone", async () => {
    stubBackend({
      retireConflictCapsule: vi.fn(async () => {
        throw new Error("EIO");
      }),
    });
    setGraphMeta({ root: "/graphs/A" } as never);
    await registerLiveSaveConflict(page, "rev-1", 1);

    await expect(retireLiveSaveConflict("Draft")).rejects.toThrow("EIO");
    expect(conflictQueue().map((c) => c.page_name)).toEqual(["Draft"]);
  });
});
