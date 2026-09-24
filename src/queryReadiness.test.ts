import { afterEach, describe, expect, it, vi } from "vitest";
import { classifyNativeCallError, OperationCancelledError, QueryNotReadyError, QueryUnavailableError } from "./backend";
import { manualLifetime, runQueryWhenCurrent, runQueryWhenReady } from "./queryReadiness";
import { bumpGraphBinding } from "./persistence";
import { setGraphMeta, setGraphTransitioning } from "./ui";
import { resetSharedQueryResultsForTests, sharedQueryResult } from "./queryResultCache";

afterEach(() => { vi.useRealTimers(); resetSharedQueryResultsForTests(); });

function owner() {
  const controller = new AbortController();
  return { controller, signal: controller.signal, isCurrent: vi.fn(() => true), onPending: vi.fn() };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (error: unknown) => void;
  const promise = new Promise<T>((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

describe("query availability wire", () => {
  it.each(["indexing", "recovering", "pending_edits", "busy"])("classifies %s without prose matching", reason_code => {
    expect(classifyNativeCallError(JSON.stringify({ kind: "query-not-ready", reason_code })))
      .toBeInstanceOf(QueryNotReadyError);
  });
  it("leaves malformed pending messages terminal and preserves a typed permanent failure", () => {
    const malformed = '{"kind":"query-not-ready","reason_code":"failed"}';
    expect(classifyNativeCallError(malformed)).toBe(malformed);
    expect(classifyNativeCallError("Updating query results…")).toBe("Updating query results…");
    expect(classifyNativeCallError(JSON.stringify({ kind: "query-unavailable", reason_code: "projection.failed", detail: { message: "The query index could not be rebuilt." } })))
      .toMatchObject({ kind: "query-unavailable", reasonCode: "projection.failed", message: "The query index could not be rebuilt." });
  });
});

describe("searchIndexPendingMessage", () => {
  it("names a rebuild and collapses the rest, as the reference panel does", async () => {
    const { searchIndexPendingMessage } = await import("./queryReadiness");
    expect(searchIndexPendingMessage(null)).toBeNull();
    // A rebuild takes noticeably longer than a catch-up, so it is the one reason
    // worth distinguishing — the same split `referenceIndexPendingMessage` makes
    // and pins for its own surface. Without this, every reason read as indexing.
    expect(searchIndexPendingMessage(new QueryNotReadyError("recovering"))).toBe(
      "Rebuilding the search index — waiting for search to be ready…"
    );
    for (const reason of ["indexing", "pending_edits", "busy"] as const) {
      expect(searchIndexPendingMessage(new QueryNotReadyError(reason))).toBe(
        "Indexing — waiting for search to be ready…"
      );
    }
  });
});

describe("owned query readiness", () => {
  // GH #543, audit R5-04: the eager first attempt settles under the same
  // ownership rule as every retry. A caller no longer current gets
  // cancellation whatever the attempt returned.
  it("settles a superseded eager attempt as cancellation, whatever it returned", async () => {
    for (const settle of [
      (resolve: (v: string) => void) => resolve("stale reading"),
      (_resolve: unknown, reject: (e: unknown) => void) => reject(new QueryNotReadyError("indexing")),
      (_resolve: unknown, reject: (e: unknown) => void) => reject(new Error("real failure")),
    ]) {
      let current = true;
      const pending: unknown[] = [];
      let finish!: () => void;
      const run = runQueryWhenCurrent(
        manualLifetime().lifetime,
        () => new Promise<string>((resolve, reject) => { finish = () => settle(resolve, reject); }),
        () => current,
        (error: unknown) => pending.push(error),
      );
      current = false;
      finish();
      await expect(run).rejects.toBeInstanceOf(OperationCancelledError);
      expect(pending).toEqual([]);
    }
  });

  it("returns a current eager attempt's answer and rethrows its real failure", async () => {
    await expect(runQueryWhenCurrent(manualLifetime().lifetime, async () => "answer", () => true)).resolves.toBe("answer");
    const failure = new Error("real failure");
    await expect(runQueryWhenCurrent(manualLifetime().lifetime, async () => { throw failure; }, () => true)).rejects.toBe(failure);
  });

  it("backs off pending attempts and returns the actual answer", async () => {
    vi.useFakeTimers();
    const current = owner();
    const value = { total: 7 };
    const load = vi.fn().mockRejectedValueOnce(new QueryNotReadyError("indexing"))
      .mockRejectedValueOnce(new QueryNotReadyError("pending_edits")).mockResolvedValue(value);
    const result = runQueryWhenReady(load, current);
    await vi.advanceTimersByTimeAsync(99);
    expect(load).toHaveBeenCalledTimes(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(load).toHaveBeenCalledTimes(2);
    await vi.advanceTimersByTimeAsync(200);
    expect(await result).toBe(value);
    expect(current.onPending.mock.calls.map(([error]) => error?.reasonCode ?? null)).toEqual(["indexing", "pending_edits", null]);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("asks again within a quarter second once the index may be ready (GH #543)", async () => {
    vi.useFakeTimers();
    const current = owner();
    let ready = false;
    const load = vi.fn(async () => {
      if (!ready) throw new QueryNotReadyError("indexing");
      return "answer";
    });
    const result = runQueryWhenReady(load, current);
    await vi.advanceTimersByTimeAsync(5_000);
    const attempts = load.mock.calls.length;
    ready = true;
    await vi.advanceTimersByTimeAsync(250);
    expect(load.mock.calls.length).toBe(attempts + 1);
    expect(await result).toBe("answer");
  });

  it("never retries a permanent error or cancellation", async () => {
    vi.useFakeTimers();
    for (const error of [new QueryUnavailableError("projection.failed", "Failed"), new OperationCancelledError(), new Error("Updating query results…")]) {
      const load = vi.fn().mockRejectedValue(error);
      await expect(runQueryWhenReady(load, owner())).rejects.toBe(error);
      expect(load).toHaveBeenCalledTimes(1);
    }
    expect(vi.getTimerCount()).toBe(0);
  });

  it("aborts a pending timer without another attempt", async () => {
    vi.useFakeTimers();
    const current = owner();
    const load = vi.fn().mockRejectedValue(new QueryNotReadyError("busy"));
    const result = runQueryWhenReady(load, current);
    const rejected = expect(result).rejects.toBeInstanceOf(OperationCancelledError);
    await vi.advanceTimersByTimeAsync(0);
    current.controller.abort();
    await rejected;
    expect(vi.getTimerCount()).toBe(0);
    expect(load).toHaveBeenCalledTimes(1);
  });

  it("stops awaiting an in-flight attempt and ignores its late rejection", async () => {
    const current = owner();
    const pending = deferred<object>();
    const result = runQueryWhenReady(() => pending.promise, current);
    const rejected = expect(result).rejects.toBeInstanceOf(OperationCancelledError);
    await Promise.resolve();
    current.controller.abort();
    await rejected;
    pending.reject(new QueryNotReadyError("indexing"));
    await Promise.resolve();
    expect(current.onPending).not.toHaveBeenCalled();
  });

  it("rejects an old answer after source ABA without touching the successor status", async () => {
    let revision = 1;
    const current = { ...owner(), isCurrent: () => revision === 1 };
    const pending = deferred<object>();
    const result = runQueryWhenReady(() => pending.promise, current);
    const rejected = expect(result).rejects.toBeInstanceOf(OperationCancelledError);
    await Promise.resolve();
    revision += 2; // source A -> B -> A still changed ownership twice
    pending.resolve({ old: true });
    await rejected;
    expect(current.onPending).not.toHaveBeenCalled();
  });

  it("checks graph ownership again before starting a queued attempt", async () => {
    const current = owner();
    const load = vi.fn().mockResolvedValue({});
    const result = runQueryWhenReady(load, current);
    current.isCurrent.mockReturnValue(false);
    await expect(result).rejects.toBeInstanceOf(OperationCancelledError);
    expect(load).not.toHaveBeenCalled();
  });

  it("lets another consumer finish a shared attempt after one consumer leaves", async () => {
    const a = owner();
    const b = owner();
    const pending = deferred<object>();
    const load = vi.fn(() => pending.promise);
    const shared = () => sharedQueryResult("graph:1", "q", load);
    const first = runQueryWhenReady(shared, a);
    const second = runQueryWhenReady(shared, b);
    const rejected = expect(first).rejects.toBeInstanceOf(OperationCancelledError);
    await Promise.resolve();
    a.controller.abort();
    await rejected;
    const value = { current: true };
    pending.resolve(value);
    expect(await second).toBe(value);
    expect(load).toHaveBeenCalledTimes(1);
    expect(a.onPending).not.toHaveBeenCalled();
    expect(b.onPending).toHaveBeenLastCalledWith(null);
  });

  // GH #543, audits R12-06 and R13-01: whose is the retry? It ends with its
  // component and with its graph, and survives a reopen of the same graph.
  describe("the owner of a readiness retry", () => {
    const notReady = () => Promise.reject(new QueryNotReadyError("indexing"));
    afterEach(() => { vi.useRealTimers(); setGraphMeta(null); setGraphTransitioning(false); });

    it("stops retrying once its lifetime ends", async () => {
      vi.useFakeTimers();
      const { lifetime, end } = manualLifetime();
      const load = vi.fn(notReady);
      const run = runQueryWhenCurrent(lifetime, load);
      const settled = expect(run).rejects.toBeInstanceOf(OperationCancelledError);
      await vi.advanceTimersByTimeAsync(300);
      end();
      const atEnd = load.mock.calls.length;
      await vi.advanceTimersByTimeAsync(5000);
      await settled;
      expect(load.mock.calls.length - atEnd).toBe(0);
    });

    it("ends when the app switches to another graph", async () => {
      vi.useFakeTimers();
      setGraphMeta({ root: "/first" } as never);
      const load = vi.fn(notReady);
      const run = runQueryWhenCurrent(manualLifetime().lifetime, load);
      const settled = expect(run).rejects.toBeInstanceOf(OperationCancelledError);
      await vi.advanceTimersByTimeAsync(300);
      setGraphMeta({ root: "/second" } as never);
      bumpGraphBinding();
      const atSwitch = load.mock.calls.length;
      await vi.advanceTimersByTimeAsync(5000);
      await settled;
      expect(load.mock.calls.length - atSwitch, "the retry read the next graph").toBe(0);
    });

    it("reads the reopened graph when the same graph is reopened", async () => {
      vi.useFakeTimers();
      setGraphMeta({ root: "/first" } as never);
      let ready = false;
      const run = runQueryWhenCurrent(manualLifetime().lifetime, () => (ready ? Promise.resolve("answer") : notReady()));
      await vi.advanceTimersByTimeAsync(300);
      // A config.edn reopen: the binding moves, the root does not.
      bumpGraphBinding();
      ready = true;
      await vi.advanceTimersByTimeAsync(2000);
      await expect(run).resolves.toBe("answer");
    });

    it("takes no answer while a graph is being opened", async () => {
      vi.useFakeTimers();
      setGraphMeta({ root: "/first" } as never);
      setGraphTransitioning(true);
      const load = vi.fn(() => Promise.resolve("maybe the next graph's answer"));
      const run = runQueryWhenCurrent(manualLifetime().lifetime, load);
      let settled = false;
      void run.then(() => { settled = true; }, () => { settled = true; });
      await vi.advanceTimersByTimeAsync(2000);
      expect(settled).toBe(false);
      setGraphTransitioning(false);
      await vi.advanceTimersByTimeAsync(2000);
      await expect(run).resolves.toBe("maybe the next graph's answer");
    });
  });
});

