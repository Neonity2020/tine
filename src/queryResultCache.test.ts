import { afterEach, describe, expect, it, vi } from "vitest";
import { resetSharedQueryResultsForTests, sharedQueryResult } from "./queryResultCache";
import { OperationCancelledError } from "./backend";

afterEach(resetSharedQueryResultsForTests);

describe("shared query results", () => {
  it("delivers the result to a remaining subscriber after another leaves", async () => {
    const a = new AbortController();
    const b = new AbortController();
    let shared!: AbortSignal;
    let finish!: (value: object) => void;
    const load = (signal: AbortSignal) => { shared = signal; return new Promise<object>(resolve => { finish = resolve; }); };
    const first = sharedQueryResult("graph", "q", load, a.signal);
    const second = sharedQueryResult("graph", "q", load, b.signal);
    const rejected = expect(first).rejects.toBeInstanceOf(OperationCancelledError);
    a.abort();
    await rejected;
    const value = { answer: true };
    finish(value);
    expect(await second).toBe(value);
    expect(shared.aborted).toBe(false);
  });

  it("cancels the shared load only when its last subscriber leaves", async () => {
    const a = new AbortController();
    const b = new AbortController();
    let shared!: AbortSignal;
    let finish!: (value: object) => void;
    const load = vi.fn((signal: AbortSignal) => {
      shared = signal;
      return new Promise<object>(resolve => { finish = resolve; });
    });
    const first = sharedQueryResult("graph", "q", load, a.signal);
    const second = sharedQueryResult("graph", "q", load, b.signal);
    const firstRejected = expect(first).rejects.toBeInstanceOf(OperationCancelledError);
    const secondRejected = expect(second).rejects.toBeInstanceOf(OperationCancelledError);
    a.abort();
    await firstRejected;
    expect(shared.aborted).toBe(false);
    b.abort();
    await secondRejected;
    expect(shared.aborted).toBe(true);
    expect(load).toHaveBeenCalledTimes(1);
    // A late completion cannot overwrite or remove a newer attempt's memo.
    const fresh = { fresh: true };
    expect(await sharedQueryResult("graph", "q", async () => fresh)).toBe(fresh);
    finish({ obsolete: true });
    await Promise.resolve();
    const unexpected = vi.fn(async () => ({}));
    expect(await sharedQueryResult("graph", "q", unexpected)).toBe(fresh);
    expect(unexpected).not.toHaveBeenCalled();
  });

  it("keeps an unowned metadata consumer alive when a cancellable subscriber leaves", async () => {
    const controller = new AbortController();
    let shared!: AbortSignal;
    let finish!: (value: object) => void;
    const load = (signal: AbortSignal) => {
      shared = signal;
      return new Promise<object>(resolve => { finish = resolve; });
    };
    const owned = sharedQueryResult("graph", "q", load, controller.signal);
    const metadata = sharedQueryResult("graph", "q", load);
    const rejected = expect(owned).rejects.toBeInstanceOf(OperationCancelledError);
    controller.abort();
    await rejected;
    expect(shared.aborted).toBe(false);
    const value = {};
    finish(value);
    expect(await metadata).toBe(value);
  });

  it("does not admit an already cancelled subscriber", async () => {
    const controller = new AbortController();
    controller.abort();
    const load = vi.fn(async () => ({}));
    await expect(sharedQueryResult("graph", "q", load, controller.signal)).rejects.toBeInstanceOf(OperationCancelledError);
    expect(load).not.toHaveBeenCalled();
  });

  it("coalesces identical concurrent pane requests and shares the DTO", async () => {
    const value = [{ page: "P", blocks: [] }];
    const load = vi.fn(async () => value);
    const [a, b] = await Promise.all([
      sharedQueryResult("graph-a:1", "simple:q:rev-2", load),
      sharedQueryResult("graph-a:1", "simple:q:rev-2", load),
    ]);
    expect(load).toHaveBeenCalledTimes(1);
    expect(a).toBe(b);
  });

  it("does not share across revisions or graph epochs", async () => {
    let call = 0;
    const load = vi.fn(async () => ({ call: ++call }));
    await sharedQueryResult("graph-a:1", "simple:q:rev-1", load);
    await sharedQueryResult("graph-a:1", "simple:q:rev-2", load);
    await sharedQueryResult("graph-a:2", "simple:q:rev-2", load);
    expect(load).toHaveBeenCalledTimes(3);
  });

  it("does not let a late old-graph result populate the new scope", async () => {
    let finishOld!: (value: { graph: string }) => void;
    const old = sharedQueryResult(
      "graph-a:1",
      "simple:q:rev-1",
      () => new Promise<{ graph: string }>((resolve) => { finishOld = resolve; }),
    );
    const fresh = { graph: "b" };
    await sharedQueryResult("graph-b:2", "simple:q:rev-1", async () => fresh);
    finishOld({ graph: "a" });
    await old;
    const load = vi.fn(async () => ({ graph: "b-second" }));
    const again = await sharedQueryResult("graph-b:2", "simple:q:rev-1", load);
    expect(load).not.toHaveBeenCalled();
    expect(again).toBe(fresh);
  });

  it("drops failed in-flight work so a retry can run", async () => {
    const fail = vi.fn(async () => { throw new Error("nope"); });
    await expect(sharedQueryResult("graph-a:1", "q", fail)).rejects.toThrow("nope");
    const ok = vi.fn(async () => ({ ok: true }));
    await expect(sharedQueryResult("graph-a:1", "q", ok)).resolves.toEqual({ ok: true });
    expect(ok).toHaveBeenCalledTimes(1);
  });
});
