import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { QueryNotReadyError } from "../backend";
import { bumpGraphBinding } from "../persistence";
import { createReferenceFetcher, referenceRead } from "./referenceFetch";
import type { ReferenceLoadError } from "./referenceLoadError";

function harness(currentName: () => string) {
  let dispose = () => {};
  const errors: (ReferenceLoadError | null)[] = [];
  const pending: (Error | null)[] = [];
  const fetchRead = createRoot((d) => {
    dispose = d;
    return createReferenceFetcher({
      currentRead: () => referenceRead(currentName()),
      setLoadError: (error) => errors.push(error),
      setIndexPending: (error) => pending.push(error),
    });
  });
  const fetcher = <T>(name: string, load: () => Promise<T[]>) => fetchRead(referenceRead(name), load);
  return { fetcher, dispose, errors, pending };
}

describe("createReferenceFetcher", () => {
  // GH #543, audit R5-04: the eager first attempt of a read the pane has
  // routed away from settles after the new page's read answered. Its
  // readiness refusal must not mark the new panel failed.
  it("a superseded first read cannot mark the new panel failed", async () => {
    let showing = "Old";
    const { fetcher, dispose, errors } = harness(() => showing);
    let rejectOld!: (reason: unknown) => void;
    const old = fetcher("Old", () => new Promise<string[]>((_resolve, reject) => { rejectOld = reject; }));
    showing = "New";
    expect(await fetcher("New", async () => ["current reference"])).toEqual(["current reference"]);
    rejectOld(new QueryNotReadyError("indexing"));
    expect(await old).toEqual([]);
    dispose();
    expect(errors.at(-1)).toBeNull();
  });

  it("a superseded first read's real failure does not mark the new panel failed either", async () => {
    let showing = "Old";
    const { fetcher, dispose, errors } = harness(() => showing);
    let rejectOld!: (reason: unknown) => void;
    const old = fetcher("Old", () => new Promise<string[]>((_resolve, reject) => { rejectOld = reject; }));
    showing = "New";
    expect(await fetcher("New", async () => ["current reference"])).toEqual(["current reference"]);
    rejectOld(new Error("backend exploded"));
    expect(await old).toEqual([]);
    dispose();
    expect(errors.at(-1)).toBeNull();
  });

  it("waits out an indexing projection instead of surfacing an error", async () => {
    const { fetcher, dispose, errors, pending } = harness(() => "Target");
    let attempts = 0;
    const rows = await fetcher("Target", async () => {
      attempts += 1;
      if (attempts < 3) throw new QueryNotReadyError("indexing");
      return ["a", "b"];
    });
    dispose();
    expect(rows).toEqual(["a", "b"]);
    expect(attempts).toBe(3);
    // Never classified as a load failure: the banner must stay away.
    expect(errors.every((error) => error === null)).toBe(true);
    expect(pending.some((error) => error instanceof QueryNotReadyError)).toBe(true);
  });

  it("stops retrying when the panel is disposed", async () => {
    const { fetcher, dispose } = harness(() => "Target");
    let attempts = 0;
    const promise = fetcher("Target", async () => {
      attempts += 1;
      throw new QueryNotReadyError("indexing");
    });
    // Let the first attempt refuse, then tear the section down.
    await new Promise((resolve) => setTimeout(resolve, 10));
    dispose();
    await expect(promise).resolves.toEqual([]);
    const settled = attempts;
    await new Promise((resolve) => setTimeout(resolve, 250));
    expect(attempts).toBe(settled);
  });

  it("stops retrying when the pane routes to another page", async () => {
    let showing = "Target";
    const { fetcher, dispose } = harness(() => showing);
    const promise = fetcher("Target", async () => {
      throw new QueryNotReadyError("indexing");
    });
    await new Promise((resolve) => setTimeout(resolve, 10));
    showing = "Somewhere Else";
    await expect(promise).resolves.toEqual([]);
    dispose();
  });

  // GH #543, audit R6-06: a panel that stays mounted across a rebind (the
  // right sidebar) must not keep the previous binding's read alive.
  it("stops retrying, and drops the answer, when the graph is rebound", async () => {
    const { fetcher, dispose, errors } = harness(() => "Target");
    let attempts = 0;
    const promise = fetcher("Target", async () => {
      attempts += 1;
      throw new QueryNotReadyError("indexing");
    });
    await new Promise((resolve) => setTimeout(resolve, 10));
    bumpGraphBinding();
    await expect(promise).resolves.toEqual([]);
    const settled = attempts;
    await new Promise((resolve) => setTimeout(resolve, 250));
    expect(attempts).toBe(settled);
    dispose();
    expect(errors.every((error) => error === null)).toBe(true);
  });

  it("still reports a real backend refusal", async () => {
    const { fetcher, dispose, errors } = harness(() => "Target");
    const rows = await fetcher("Target", async () => {
      throw new Error("Reference queries are unavailable for this graph.");
    });
    dispose();
    expect(rows).toEqual([]);
    expect(errors.at(-1)).toMatchObject({
      kind: "backend",
      detail: "Reference queries are unavailable for this graph.",
    });
  });
});

describe("referenceIndexPendingMessage", () => {
  it("states the panel's own subject, and keeps the rebuild distinction", async () => {
    const { referenceIndexPendingMessage } = await import("./referenceFetch");
    expect(referenceIndexPendingMessage(null)).toBeNull();
    expect(referenceIndexPendingMessage(new QueryNotReadyError("indexing"))).toBe("indexing…");
    expect(referenceIndexPendingMessage(new QueryNotReadyError("pending_edits"))).toBe("indexing…");
    expect(referenceIndexPendingMessage(new QueryNotReadyError("busy"))).toBe("indexing…");
    expect(referenceIndexPendingMessage(new QueryNotReadyError("recovering"))).toBe(
      "rebuilding the index…"
    );
  });
});
