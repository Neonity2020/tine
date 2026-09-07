import { afterEach, describe, expect, it, vi } from "vitest";
import { classifyNativeCallError, OperationCancelledError, QueryNotReadyError, QueryUnavailableError } from "./backend";
import { runQueryWhenReady } from "./queryReadiness";
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

describe("owned query readiness", () => {
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
});
