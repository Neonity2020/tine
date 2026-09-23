import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { ConflictOverview } from "./ConflictOverview";
import { __setBackendForTest, type Backend } from "../backend";
import * as ui from "../ui";
import { setConflictQueue, setSyncConflicts, setToasts } from "../ui";
import type { PaneRouter } from "../router";
import type { ConflictObject, SyncConflict } from "../types";

// GH #536: the `N conflicts` badge opens one overview of every page that needs
// a decision, rendered from the live queue and never written to the graph.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
  __setBackendForTest(null);
  setConflictQueue([]);
  setSyncConflicts([]);
  setToasts([]);
  vi.restoreAllMocks();
});

const syncCopy: ConflictObject = {
  id: "copy:pages/Alpha.sync-conflict-20260920-LAPTOP.md",
  source: "sync-copy",
  page_name: "Alpha",
  page_path: "pages/Alpha.md",
  kind: "page",
  sides: [
    { role: "mine", label: "This device", path: "pages/Alpha.md" },
    { role: "theirs", label: "LAPTOP", path: "pages/Alpha.sync-conflict-20260920-LAPTOP.md" },
  ],
  block_conflicts: 3,
};
const markers: ConflictObject = {
  id: "markers:journals/2026_09_20.md",
  source: "vcs-markers",
  page_name: "Sep 20th, 2026",
  page_path: "journals/2026_09_20.md",
  kind: "journal",
  sides: [
    { role: "mine", label: "HEAD" },
    { role: "theirs", label: "feature/x" },
  ],
  block_conflicts: null,
};
const orphan: SyncConflict = {
  path: "pages/Gone.sync-conflict-20260919-PHONE.md",
  base_name: "Gone",
  base_path: null,
  kind: "page",
  tag: "PHONE",
  preview: "- orphaned",
};

function mount(queue: ConflictObject[], copies: SyncConflict[], extra: Partial<Backend> = {}) {
  __setBackendForTest({
    conflictInventory: async () => ({
      sync_conflicts: copies,
      vcs_markers: [],
      queue,
    }),
    ...extra,
  } as unknown as Backend);
  setConflictQueue(queue);
  setSyncConflicts(copies);
  const router = { openPageTarget: vi.fn() } as unknown as PaneRouter;
  const host = document.createElement("div");
  document.body.appendChild(host);
  const dispose = render(() => <ConflictOverview router={router} />, host);
  return { host, router, dispose };
}

const rowFor = (host: HTMLElement, name: string) =>
  [...host.querySelectorAll<HTMLElement>(".conflict-overview-row")].find((row) =>
    row.querySelector(".conflict-overview-open")?.textContent === name,
  )!;

describe("the conflict overview", () => {
  it("lists every conflicted page by source with its block count, absent shown as —", async () => {
    const { host, dispose } = mount([syncCopy, markers], [orphan]);
    try {
      await flush();
      const groups = [...host.querySelectorAll("section.conflict-overview-group")].map((g) =>
        g.getAttribute("aria-label"),
      );
      expect(groups).toEqual(["Sync conflict copies", "Version-control merge markers"]);

      const alpha = rowFor(host, "Alpha");
      expect(alpha.querySelector(".conflict-overview-source")?.textContent).toBe("sync copy · LAPTOP");
      expect(alpha.querySelector(".conflict-overview-count")?.textContent).toBe("3 blocks");

      const day = rowFor(host, "Sep 20th, 2026");
      expect(day.querySelector(".conflict-overview-source")?.textContent).toBe("merge markers · HEAD vs feature/x");
      // Not computed is not zero.
      expect(day.querySelector(".conflict-overview-count")?.textContent).toBe("—");

      // A copy whose page is gone is not in the queue, but it still needs a
      // decision, and only this inventory can offer to discard it.
      const gone = rowFor(host, "Gone");
      expect(gone.textContent).toContain("its page no longer exists");
      expect(gone.querySelector("button")?.textContent).toContain("Discard copy");
    } finally {
      dispose();
    }
  });

  it("opens the exact conflicted file, or the right sidebar on shift-click", async () => {
    const sidebar = vi.spyOn(ui, "openPageInSidebar").mockImplementation(() => {});
    const { host, router, dispose } = mount([syncCopy, markers], []);
    try {
      await flush();
      rowFor(host, "Sep 20th, 2026").querySelector<HTMLButtonElement>(".conflict-overview-open")!.click();
      expect(router.openPageTarget).toHaveBeenCalledWith({
        name: "Sep 20th, 2026", pageKind: "journal", path: "journals/2026_09_20.md",
      });

      rowFor(host, "Alpha").querySelector(".conflict-overview-open")!
        .dispatchEvent(new MouseEvent("click", { bubbles: true, shiftKey: true }));
      expect(sidebar).toHaveBeenCalledWith({ name: "Alpha", pageKind: "page", path: "pages/Alpha.md" });
      expect(router.openPageTarget).toHaveBeenCalledTimes(1);
    } finally {
      dispose();
    }
  });

  it("discards a sync copy by its own file, after confirmation", async () => {
    const trashed: string[] = [];
    const { host, dispose } = mount([syncCopy], [], {
      confirm: async () => true,
      trashSyncConflict: async (path: string) => { trashed.push(path); },
    });
    try {
      await flush();
      const discard = [...rowFor(host, "Alpha").querySelectorAll("button")]
        .find((b) => b.textContent?.includes("Discard copy"))!;
      discard.click();
      await flush();
      await flush();
      expect(trashed).toEqual(["pages/Alpha.sync-conflict-20260920-LAPTOP.md"]);
    } finally {
      dispose();
    }
  });

  it("stays a valid page when nothing is left", async () => {
    const { host, dispose } = mount([], []);
    try {
      await flush();
      expect(host.querySelector(".conflict-overview-empty")?.textContent).toContain("No conflicts");
      expect(host.querySelector(".conflict-overview-row")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("drops a row as soon as the queue re-derives without it", async () => {
    const { host, dispose } = mount([syncCopy, markers], []);
    try {
      await flush();
      expect(rowFor(host, "Alpha")).toBeTruthy();
      setConflictQueue([markers]);
      await flush();
      expect(rowFor(host, "Alpha")).toBeUndefined();
      expect(rowFor(host, "Sep 20th, 2026")).toBeTruthy();
    } finally {
      dispose();
    }
  });
});
