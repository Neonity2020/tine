import { afterEach, describe, expect, it, vi } from "vitest";
import {
  __setBackendForTest,
  classifyNativeCallError,
  DirectSaveFailureError,
  ManagedActorRefusalError,
  type Backend,
  SaveConflictError,
} from "./backend";
import {
  dirtyPages,
  flushAll,
  flushPage,
  holdManagedMovePages,
  isSaveConflictFailure,
  isRetryableSaveFailure,
  markDirty,
  requireManagedRuntimeReopen,
  resetSaveState,
  saveFailureDisposition,
  scheduleLiveSaveConflictDraftRefresh,
  trackAssetWrite,
} from "./persistence";
import type { PageDto } from "./types";
import {
  registerLiveSaveConflict,
  setConflictQueue,
  setGraphMeta,
} from "./ui";

function deferred<T>(): { promise: Promise<T>; resolve: (value: T) => void; reject: (reason?: unknown) => void } {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

afterEach(() => {
  vi.useRealTimers();
  __setBackendForTest(null);
  setConflictQueue([]);
  setGraphMeta(null);
});

describe("live-save conflict capsule refresh", () => {
  it("debounces changed drafts independently and persists the latest one", async () => {
    vi.useFakeTimers();
    const store = vi.fn(async (_root: string, _capsule: unknown) => {});
    __setBackendForTest({
      storeConflictCapsule: store,
      loadConflictCapsules: async () => [],
      retireConflictCapsule: async () => {},
    } as unknown as Backend);
    setGraphMeta({ root: "/graphs/A" } as never);
    const page = {
      name: "Draft",
      path: "pages/Draft.md",
      format: "md",
      blocks: [{ id: "b1", raw: "registered draft", collapsed: false, children: [] }],
      rev: "rev-1",
    } as unknown as PageDto;
    await registerLiveSaveConflict(page, "rev-1", 1);

    scheduleLiveSaveConflictDraftRefresh({
      ...page,
      blocks: [{ id: "b1", raw: "intermediate draft", collapsed: false, children: [] }],
    });
    scheduleLiveSaveConflictDraftRefresh({
      ...page,
      blocks: [{ id: "b1", raw: "latest draft", collapsed: false, children: [] }],
    });

    expect(store).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(499);
    expect(store).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(store).toHaveBeenCalledTimes(2);
    expect(store.mock.calls.at(-1)?.[1]).toMatchObject({
      live: { page: { blocks: [{ raw: "latest draft" }] } },
    });
  });

  it("flushAll persists a pending capsule draft before resolving", async () => {
    // I-2: the close/switch barrier must not leave the crash-recovery capsule
    // trailing the in-memory draft by the debounce window (B3E review).
    vi.useFakeTimers();
    const store = vi.fn(async (_root: string, _capsule: unknown) => {});
    __setBackendForTest({
      storeConflictCapsule: store,
      loadConflictCapsules: async () => [],
      retireConflictCapsule: async () => {},
    } as unknown as Backend);
    setGraphMeta({ root: "/graphs/A" } as never);
    const page = {
      name: "Closing",
      path: "pages/Closing.md",
      format: "md",
      blocks: [{ id: "b1", raw: "registered draft", collapsed: false, children: [] }],
      rev: "rev-1",
    } as unknown as PageDto;
    await registerLiveSaveConflict(page, "rev-1", 1);
    scheduleLiveSaveConflictDraftRefresh({
      ...page,
      blocks: [{ id: "b1", raw: "draft typed just before close", collapsed: false, children: [] }],
    });
    expect(store).toHaveBeenCalledTimes(1);

    // No timer advance: the barrier itself must land the draft.
    await flushAll();

    expect(store).toHaveBeenCalledTimes(2);
    expect(store.mock.calls.at(-1)?.[1]).toMatchObject({
      live: { page: { blocks: [{ raw: "draft typed just before close" }] } },
    });
    await vi.advanceTimersByTimeAsync(600);
    expect(store).toHaveBeenCalledTimes(2);
  });
});

describe("asset write close barrier", () => {
  it("flushAll waits for a pending tracked asset write", async () => {
    const asset = deferred<string>();
    const tracked = trackAssetWrite(asset.promise);
    let flushed = false;

    const flush = flushAll().then((ok) => {
      flushed = true;
      return ok;
    });
    await Promise.resolve();
    expect(flushed).toBe(false);

    asset.resolve("saved.png");

    await expect(tracked).resolves.toBe("saved.png");
    await expect(flush).resolves.toBe(true);
    expect(flushed).toBe(true);
  });
});

describe("managed move persistence barrier", () => {
  it("refuses close and defers an ordinary page save until actor ownership releases", async () => {
    resetSaveState();
    const release = holdManagedMovePages(["Held page"]);
    markDirty("Held page");

    await expect(flushPage("Held page")).resolves.toBe(false);
    await expect(flushAll()).resolves.toBe(false);
    expect([...dirtyPages()]).toContain("Held page");

    release();
    await expect(flushAll()).resolves.toBe(true);
    expect([...dirtyPages()]).not.toContain("Held page");
  });

  it("fails closed after unresolved actor recovery until the graph is reopened", async () => {
    resetSaveState();
    requireManagedRuntimeReopen();
    await expect(flushAll()).resolves.toBe(false);

    resetSaveState();
    await expect(flushAll()).resolves.toBe(true);
  });
});

describe("save failure classification", () => {
  it("tags only the complete Direct Files conflict payload at the backend boundary", () => {
    const payload = JSON.stringify({
      kind: "save-conflict",
      reason_code: "conflict.base_rev",
      detail: { io_error_kind: "AlreadyExists", epoch: 17 },
    });
    expect(classifyNativeCallError(payload)).toMatchObject({ kind: "save-conflict", epoch: 17 });
    expect(classifyNativeCallError("conflict")).toBe("conflict");
    expect(classifyNativeCallError("conflict:17")).toBe("conflict:17");
    expect(classifyNativeCallError("ordinary prose says conflict:17")).toBe("ordinary prose says conflict:17");
  });

  it("recognizes only bounded content-conflict contracts", () => {
    expect(isSaveConflictFailure(new SaveConflictError(17))).toBe(true);
    expect(isSaveConflictFailure("managed.conflict: stale_base")).toBe(true);
    expect(isSaveConflictFailure(new ManagedActorRefusalError("managed.conflict"))).toBe(true);
    expect(isSaveConflictFailure(new DirectSaveFailureError("precheck.portable_collision", "AlreadyExists"))).toBe(false);
    expect(isSaveConflictFailure("ordinary prose says conflict or already exists")).toBe(false);
    expect(isSaveConflictFailure("conflict:17")).toBe(false);
  });

  // A non-retryable failure used to be retried twice more before toasting, and
  // each retry re-runs the whole pre-save check — on a large graph, the
  // expensive part — only to reach the same answer. GH #267's "about a minute
  // then a red toast" is that multiplier on top of an already slow check.
  it("does not retry failures a retry cannot change", () => {
    for (const code of [
      "precheck.symlink",
      "precheck.portable_collision",
      "precheck.resource_alias",
      "precheck.not_portable",
      "precheck.nofollow",
      "precheck.limit",
      "identity.owned_elsewhere",
      // A name collision is real, but no number of retries frees the name.
      "identity.name_taken",
      // Managed storage refused the save because the page moved underneath it.
      // This used to arrive as "conflict: {Reason}", which matched neither the
      // conflict banner (an exact comparison) nor any bounded code, so it
      // retried silently forever and the user could quit believing the page
      // had been written.
      "trusted_local.append_outcome_unknown",
    ]) {
      expect(isRetryableSaveFailure(
        code === "trusted_local.append_outcome_unknown"
          ? new ManagedActorRefusalError(code)
          : new DirectSaveFailureError(code, "Other"),
      )).toBe(false);
    }
    expect(isRetryableSaveFailure("managed.conflict: something specific")).toBe(false);
  });

  it("still retries failures that a later attempt can succeed at", () => {
    // The graph moved under the capture (a sync client mid-pull), or the file
    // was replaced and the watcher has not re-pinned its identity yet. Both
    // resolve on their own.
    expect(isRetryableSaveFailure(new DirectSaveFailureError("precheck.interrupted", "Interrupted"))).toBe(true);
    expect(isRetryableSaveFailure(new DirectSaveFailureError("identity.changed_since_load", "AlreadyExists"))).toBe(true);
    expect(
      isRetryableSaveFailure(new DirectSaveFailureError("conflict_retry.replace_pre_retirement", "WouldBlock"))
    ).toBe(true);
    expect(isRetryableSaveFailure(new DirectSaveFailureError("unknown", "StorageFull"))).toBe(true);
    expect(isRetryableSaveFailure(new Error("EBUSY"))).toBe(true);
  });

  it("classifies append uncertainty from the actor reason-code envelope before retry policy", () => {
    expect(
      saveFailureDisposition("trusted_local.append_outcome_unknown: storage receipt did not escape")
    ).toBe("ordinary");
    const actorFailure = new ManagedActorRefusalError("trusted_local.append_outcome_unknown");
    expect(saveFailureDisposition(actorFailure)).toBe("append_outcome_unknown");
    expect(isRetryableSaveFailure(actorFailure)).toBe(false);
    expect(
      saveFailureDisposition("ordinary failure mentions trusted_local.append_outcome_unknown in prose")
    ).toBe("ordinary");
    expect(
      saveFailureDisposition("ordinary prose (reason code: trusted_local.append_outcome_unknown)")
    ).toBe("ordinary");
    expect(
      isRetryableSaveFailure("ordinary failure mentions trusted_local.append_outcome_unknown in prose")
    ).toBe(true);
  });
});

// Direct Files data-safety audit, 2026-08-09, finding 6.
//
// `doSave` bailed out on a null DTO *before* removing the name from `dirty`, so a
// dirty name with no page behind it wedged `flushAll()` permanently: every later
// graph switch aborted with "Some pages couldn't be saved" and every window close
// offered to discard edits that were in fact already on disk.
describe("a dirty name with no page behind it does not wedge every flush", () => {
  it("lets flushAll succeed again", async () => {
    resetSaveState();
    markDirty("A page that is not in the store");
    expect([...dirtyPages()]).toContain("A page that is not in the store");

    // Before the fix all three of these resolved false, forever.
    expect(await flushAll()).toBe(true);
    expect([...dirtyPages()]).toHaveLength(0);
    expect(await flushAll()).toBe(true);
  });
});
