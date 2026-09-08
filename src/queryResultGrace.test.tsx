import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  QUERY_COMPLETION_GRACE_MS,
  createQueryRefreshRevision,
  noteQueryCompletionInteraction,
} from "./queryResultGrace";

afterEach(() => {
  vi.useRealTimers();
  document.body.innerHTML = "";
});

describe("query result completion grace", () => {
  it("coalesces to the latest data revision and releases it at two seconds", async () => {
    vi.useFakeTimers();
    let now = 0;
    const [revision, setRevision] = createSignal(0);
    const root = document.createElement("div");
    const dispose = render(() => {
      const visibleRevision = createQueryRefreshRevision({
        revision,
        identity: () => "graph-a/query-a/list",
        containsDisplayedBlock: (id) => id === "task-a",
        now: () => now,
      });
      return <span>{visibleRevision()}</span>;
    }, root);
    try {
      noteQueryCompletionInteraction("other-task", now);
      setRevision(1);
      expect(root.textContent).toBe("1");

      noteQueryCompletionInteraction("task-a", now);
      setRevision(2);
      setRevision(3);
      expect(root.textContent).toBe("1");

      now = QUERY_COMPLETION_GRACE_MS - 1;
      await vi.advanceTimersByTimeAsync(QUERY_COMPLETION_GRACE_MS - 1);
      expect(root.textContent).toBe("1");

      now = QUERY_COMPLETION_GRACE_MS;
      await vi.advanceTimersByTimeAsync(1);
      expect(root.textContent).toBe("3");
    } finally {
      dispose();
    }
  });

  it("cancels a hold when query identity changes and cancels its timer on unmount", async () => {
    vi.useFakeTimers();
    let now = 10;
    const [revision, setRevision] = createSignal(0);
    const [identity, setIdentity] = createSignal("graph-a/query-a/list");
    const root = document.createElement("div");
    const dispose = render(() => {
      const visibleRevision = createQueryRefreshRevision({
        revision,
        identity,
        containsDisplayedBlock: (id) => id === "task-a",
        now: () => now,
      });
      return <span>{visibleRevision()}</span>;
    }, root);

    noteQueryCompletionInteraction("task-a", now);
    setRevision(1);
    expect(root.textContent).toBe("0");
    expect(vi.getTimerCount()).toBe(1);
    setIdentity("graph-a/query-b/board");
    expect(root.textContent).toBe("1");
    expect(vi.getTimerCount()).toBe(0);

    noteQueryCompletionInteraction("task-a", now);
    setRevision(2);
    expect(vi.getTimerCount()).toBe(1);
    dispose();
    expect(vi.getTimerCount()).toBe(0);
    now += QUERY_COMPLETION_GRACE_MS;
    await vi.advanceTimersByTimeAsync(QUERY_COMPLETION_GRACE_MS);
  });
});
