import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { ConflictQueueBadge } from "./Sidebar";
import * as routerModule from "../router";
import { __setBackendForTest, type Backend } from "../backend";
import {
  conflictQueue,
  refreshConflictQueueIfTouched,
  refreshSyncConflicts,
  setConflictQueue,
  settleArtifactConflict,
  setToasts,
  toasts,
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
  setToasts([]);
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

  it("opens the conflict overview, whatever the count", async () => {
    // GH #536: the badge used to walk to the next conflicted page, so with many
    // pages there was no view of what was left. It now opens one overview.
    setConflictQueue([object("markers:a", "Alpha"), object("markers:b", "Beta")]);
    const opened: string[] = [];
    const spy = vi.spyOn(routerModule, "openConflicts").mockImplementation(() => { opened.push("conflicts"); });
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(() => <ConflictQueueBadge />, host);
    try {
      await flush();
      const badge = host.querySelector<HTMLButtonElement>(".conflict-queue-badge")!;
      badge.click();
      badge.click();
      expect(opened).toEqual(["conflicts", "conflicts"]);
    } finally {
      dispose();
      spy.mockRestore();
    }
  });

  it("is refreshed from the derived backend queue, and empties safely on failure", async () => {
    const queue = [object("markers:a", "Alpha")];
    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue,
      }),
    } as unknown as Backend);
    await refreshSyncConflicts();
    expect(conflictQueue().map((c) => c.id)).toEqual(["markers:a"]);

    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue: await (vi.fn(async () => {
        throw new Error("backend gone");
      }))(),
      }),
    } as unknown as Backend);
    await refreshSyncConflicts();
    expect(conflictQueue()).toEqual([]);
  });
});

describe("the queue after an external change", () => {
  it("surfaces a newly arrived sync copy once without auto-navigating", async () => {
    const arrived: ConflictObject = {
      id: "copy:pages/Alpha.sync-conflict-20260822-120000-PHONE.md",
      source: "sync-copy",
      page_name: "Alpha",
      page_path: "pages/Alpha.md",
      kind: "page",
      sides: [
        { role: "mine", label: "This device", path: "pages/Alpha.md" },
        { role: "theirs", label: "Phone", path: "pages/Alpha.sync-conflict-20260822-120000-PHONE.md" },
      ],
      block_conflicts: 1,
    };
    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue: [arrived],
      }),
    } as unknown as Backend);

    await refreshSyncConflicts("new");
    expect(conflictQueue().map((conflict) => conflict.id)).toEqual([arrived.id]);
    expect(toasts()).toHaveLength(1);
    expect(toasts()[0].message).toContain("new sync conflict");
    expect(toasts()[0].sticky).toBe(true);

    await refreshSyncConflicts("new");
    expect(toasts()).toHaveLength(1);

    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue: [],
      }),
    } as unknown as Backend);
    await refreshSyncConflicts();
    expect(conflictQueue()).toEqual([]);
    expect(toasts()).toEqual([]);
  });

  it("does not resurrect a resolved conflict from an older inventory refresh", async () => {
    const arrived: ConflictObject = {
      id: "copy:pages/Alpha.sync-conflict-20260822-120000-PHONE.md",
      source: "sync-copy",
      page_name: "Alpha",
      page_path: "pages/Alpha.md",
      kind: "page",
      sides: [
        { role: "mine", label: "This device", path: "pages/Alpha.md" },
        { role: "theirs", label: "Phone", path: "pages/Alpha.sync-conflict-20260822-120000-PHONE.md" },
      ],
      block_conflicts: 1,
    };
    setConflictQueue([arrived]);
    let publishOlder!: (queue: ConflictObject[]) => void;
    const older = new Promise<ConflictObject[]>((resolve) => { publishOlder = resolve; });
    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue: older,
      }),
      // Full-suite graph setup may finish its independent warm-cache probe while
      // this deliberately delayed inventory is active. Keep that unrelated
      // callback inside the backend contract instead of leaking an unhandled
      // missing-method rejection from this race fixture.
      warmDone: async () => false,
    } as unknown as Backend);

    const refresh = refreshSyncConflicts();
    await flush();
    settleArtifactConflict(arrived.id);
    expect(conflictQueue()).toEqual([]);
    publishOlder([arrived]);
    await refresh;

    expect(conflictQueue()).toEqual([]);
  });

  it("re-derives only when the change touched something queued", async () => {
    const conflictQueueFn = vi.fn(async () => [] as ConflictObject[]);
    __setBackendForTest({
      conflictInventory: async () => ({
        sync_conflicts: [],
        vcs_markers: [],
        queue: await (conflictQueueFn)(),
      }),
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
