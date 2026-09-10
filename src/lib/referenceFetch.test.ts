import { describe, expect, it } from "vitest";
import { createRoot } from "solid-js";
import { QueryNotReadyError } from "../backend";
import { createReferenceFetcher } from "./referenceFetch";
import type { ReferenceLoadError } from "./referenceLoadError";

function harness(currentName: () => string) {
  let dispose = () => {};
  const errors: (ReferenceLoadError | null)[] = [];
  const pending: (Error | null)[] = [];
  const fetcher = createRoot((d) => {
    dispose = d;
    return createReferenceFetcher({
      currentName,
      setLoadError: (error) => errors.push(error),
      setIndexPending: (error) => pending.push(error),
    });
  });
  return { fetcher, dispose, errors, pending };
}

describe("createReferenceFetcher", () => {
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

  it("still reports a real backend refusal", async () => {
    const { fetcher, dispose, errors } = harness(() => "Target");
    const rows = await fetcher("Target", async () => {
      throw new Error("This action is unavailable while Tine-managed storage is active.");
    });
    dispose();
    expect(rows).toEqual([]);
    expect(errors.at(-1)).toMatchObject({
      kind: "backend",
      detail: "This action is unavailable while Tine-managed storage is active.",
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
