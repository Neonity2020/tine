import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { ConflictQueueBadge } from "./Sidebar";
import { __setBackendForTest, type Backend } from "../backend";
import {
  advanceConflictCursor,
  conflictQueue,
  refreshConflictQueueIfTouched,
  refreshSyncConflicts,
  resetConflictCursor,
  setConflictQueue,
} from "../ui";
import type { ConflictObject } from "../types";

// Concord L3: the badge is the whole global surface for conflicts — calm,
// non-blocking, and derived. Fail-before: there was no queue at all; conflicts
// were only discoverable by opening Settings → Backups & recovery.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
  __setBackendForTest(null);
  setConflictQueue([]);
  resetConflictCursor();
});

const object = (id: string, name: string): ConflictObject => ({
  id,
  source: "vcs-markers",
  page_name: name,
  page_path: `pages/${name}.md`,
  kind: "page",
  sides: [
    { role: "mine", label: "HEAD" },
    { role: "theirs", label: "feature" },
  ],
  block_conflicts: 1,
});

describe("the conflict queue badge", () => {
  it("is absent when nothing needs a decision", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <ConflictQueueBadge />, host);
    try {
      await flush();
      expect(host.querySelector(".conflict-queue-badge")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("counts the queue and never opens anything by itself", async () => {
    setConflictQueue([object("markers:a", "Alpha"), object("markers:b", "Beta")]);
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <ConflictQueueBadge />, host);
    try {
      await flush();
      const badge = host.querySelector(".conflict-queue-badge")!;
      expect(badge.textContent).toContain("2 conflicts");
      // Calm: a badge, not a modal — nothing overlays the app.
      expect(document.querySelector(".sync-merge-overlay")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("walks the queue on repeated activation instead of parking on the first", () => {
    setConflictQueue([object("markers:a", "Alpha"), object("markers:b", "Beta")]);
    expect(advanceConflictCursor()?.page_name).toBe("Alpha");
    expect(advanceConflictCursor()?.page_name).toBe("Beta");
    expect(advanceConflictCursor()?.page_name).toBe("Alpha");
    setConflictQueue([]);
    expect(advanceConflictCursor()).toBeUndefined();
  });

  it("is refreshed from the derived backend queue, and empties safely on failure", async () => {
    const queue = [object("markers:a", "Alpha")];
    __setBackendForTest({
      listSyncConflicts: async () => [],
      listVcsMarkerConflicts: async () => [],
      conflictQueue: async () => queue,
    } as unknown as Backend);
    await refreshSyncConflicts();
    expect(conflictQueue().map((c) => c.id)).toEqual(["markers:a"]);

    __setBackendForTest({
      listSyncConflicts: async () => [],
      listVcsMarkerConflicts: async () => [],
      conflictQueue: vi.fn(async () => {
        throw new Error("backend gone");
      }),
    } as unknown as Backend);
    await refreshSyncConflicts();
    expect(conflictQueue()).toEqual([]);
  });
});

describe("the queue after an external change", () => {
  it("re-derives only when the change touched something queued", async () => {
    const conflictQueueFn = vi.fn(async () => [] as ConflictObject[]);
    __setBackendForTest({
      listSyncConflicts: async () => [],
      listVcsMarkerConflicts: async () => [],
      conflictQueue: conflictQueueFn,
    } as unknown as Backend);

    // Empty queue: an external change must cost nothing at all.
    await refreshConflictQueueIfTouched([{ name: "Anything", kind: "page" }]);
    expect(conflictQueueFn).not.toHaveBeenCalled();

    // Queued but untouched: still nothing.
    setConflictQueue([object("markers:a", "Alpha")]);
    await refreshConflictQueueIfTouched([{ name: "Beta", kind: "page" }]);
    expect(conflictQueueFn).not.toHaveBeenCalled();

    // Touched — e.g. git finished the merge outside Tine — so the queue is
    // re-derived and the resolved page drops out of it.
    await refreshConflictQueueIfTouched([{ name: "Alpha", kind: "page" }]);
    expect(conflictQueueFn).toHaveBeenCalledTimes(1);
    expect(conflictQueue()).toEqual([]);
  });
});
