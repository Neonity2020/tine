// Concord P2 (GH #337 / spec L6): the external revision. A VCS checkout,
// branch switch, or first big sync under a running Tine used to reach the
// frontend as N `graph-changed` events — N synchronous dataRev bumps and up to
// N `getPage` IPCs. The watcher now coalesces a cycle that changed more than
// its bulk threshold into ONE `graph-changed-bulk` event; these tests pin the
// frontend half of that contract.
import { afterEach, describe, expect, it, vi } from "vitest";
import { backend } from "./backend";
import { handleGraphChange, handleGraphChangedBulk } from "./App";
import { resetPaneLayoutToSingle } from "./panes";
import {
  doc,
  pageByName,
  reloadPage,
  resetStore,
  setDoc,
  type FeedPage,
  type Node as StoreNode,
} from "./store";
import { endEdit, startEditing } from "./editorController";
import { clearConflict, dataRev, isConflicted, pageInventoryRev, setToasts, toasts } from "./ui";
import { markDirty, resetSaveState } from "./persistence";
import { managedStorageRuntime } from "./managedStorageRuntime";

const COUNT = 40; // comfortably above the backend's 32-page bulk threshold

afterEach(() => {
  vi.restoreAllMocks();
  managedStorageRuntime.clear();
  resetSaveState();
  resetStore();
  setToasts([]);
  resetPaneLayoutToSingle({ tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }], activeIndex: 0 });
});

function page(name: string, kind: "page" | "journal", roots: string[]): FeedPage {
  return { name, kind, title: name, preBlock: null, roots, format: "md", readOnly: false, guide: false };
}

function node(id: string, pageName: string): StoreNode {
  return { id, raw: "stale in memory", collapsed: false, parent: null, page: pageName, children: [] };
}

function diskDto(name: string, kind: "page" | "journal" = "page") {
  return {
    name,
    kind,
    title: name,
    pre_block: null,
    rev: "rev-2",
    blocks: [{ id: `${name}-b1`, raw: "fresh from disk", collapsed: false, children: [] }],
  };
}

function bulkNames(): string[] {
  return Array.from({ length: COUNT }, (_, index) => `Bulk ${index}`);
}

function loadPages(names: string[]) {
  const byId: Record<string, StoreNode> = {};
  const pages: FeedPage[] = [];
  for (const name of names) {
    const id = `${name}-old`;
    byId[id] = node(id, name);
    pages.push(page(name, "page", [id]));
  }
  setDoc({ byId, pages, feed: [], loaded: true });
}

function changesFor(names: string[]) {
  return names.map((name) => ({ name, kind: "page" as const, created: false, removed: false }));
}

// The pre-coalescing delivery shape, kept as the documented baseline: below the
// backend threshold nothing changes, and each per-page event still costs one
// dataRev bump and (for a loaded page) one getPage fetch. This is the N× storm
// the bulk event exists to avoid.
describe("per-page delivery (below the bulk threshold)", () => {
  it("costs one dataRev bump and one fetch per event", async () => {
    const names = bulkNames();
    loadPages(names);
    const getPage = vi
      .spyOn(backend(), "getPage")
      .mockImplementation((name) => Promise.resolve(diskDto(name)));
    const revBefore = dataRev();

    for (const c of changesFor(names)) await handleGraphChange(c);

    expect(getPage).toHaveBeenCalledTimes(COUNT);
    expect(dataRev() - revBefore).toBe(COUNT);
  });
});

describe("one bulk epoch, one frontend notification", () => {
  it("bumps dataRev once, reloads only the routed page, and leaves the rest for lazy reload", async () => {
    const names = bulkNames();
    resetPaneLayoutToSingle({
      tabs: [{ history: [{ kind: "page", name: "Bulk 0", pageKind: "page" }], pos: 0, pinned: false }],
      activeIndex: 0,
    });
    loadPages(names);
    const getPage = vi
      .spyOn(backend(), "getPage")
      .mockImplementation((name) => Promise.resolve(diskDto(name)));
    const revBefore = dataRev();
    const inventoryBefore = pageInventoryRev();

    await handleGraphChangedBulk({ changes: changesFor(names) });

    // One epoch: one derived-view invalidation, not forty.
    expect(dataRev() - revBefore).toBe(1);
    // No creations/removals in this epoch — the rare inventory rev stays put.
    expect(pageInventoryRev()).toBe(inventoryBefore);
    // Only the visible page was fetched; the other 39 wait for navigation,
    // which always refetches from the backend's already-fresh cache.
    expect(getPage).toHaveBeenCalledTimes(1);
    expect(getPage).toHaveBeenCalledWith("Bulk 0", "page");
    expect(pageByName("Bulk 0")?.roots).toEqual(["Bulk 0-b1"]);
    expect(doc.byId[`${names[5]}-old`].raw).toBe("stale in memory");
    // The calm summary surface — a toast, never a dialog.
    expect(toasts().at(-1)?.message).toBe("40 pages updated externally");
    expect(toasts().at(-1)?.kind).toBe("info");
  });

  it("bumps the page inventory once when the epoch created or removed pages", async () => {
    const names = bulkNames();
    const changes = changesFor(names);
    changes[3] = { ...changes[3], created: true };
    changes[7] = { ...changes[7], created: true };
    const inventoryBefore = pageInventoryRev();

    await handleGraphChangedBulk({ changes });

    expect(pageInventoryRev()).toBe(inventoryBefore + 1);
  });

  it("defers the reload of a page being edited exactly like a single change", async () => {
    const names = bulkNames();
    loadPages(names);
    const edited = "Bulk 4";
    startEditing(`${edited}-old`);
    const getPage = vi
      .spyOn(backend(), "getPage")
      .mockImplementation((name) => Promise.resolve(diskDto(name)));

    await handleGraphChangedBulk({ changes: changesFor(names) });

    // Mid-edit: the bulk change must not yank the caret — the reload is
    // deferred through the same P1 machinery as a single watcher event.
    expect(getPage).not.toHaveBeenCalled();
    expect(doc.byId[`${edited}-old`].raw).toBe("stale in memory");

    endEdit("blur");

    await vi.waitFor(() =>
      expect(pageByName(edited)?.roots).toEqual([`${edited}-b1`])
    );
  });

  it("restarts a live journals feed once, not once per changed journal", async () => {
    const now = new Date();
    const feed = vi.spyOn(backend(), "journalFeedPage").mockResolvedValue({
      pages: [],
      next_before_day: null,
      done: true,
      as_of_day: now.getFullYear() * 10_000 + (now.getMonth() + 1) * 100 + now.getDate(),
    });
    const changes = Array.from({ length: COUNT }, (_, index) => ({
      name: `${index + 1}th July, 2030`,
      kind: "journal" as const,
      created: false,
      removed: false,
    }));

    await handleGraphChangedBulk({ changes });
    await Promise.resolve();

    expect(feed).toHaveBeenCalledTimes(1);
    expect(feed).toHaveBeenCalledWith(3, null);
  });

  it("counts pages that hit the conflict path in the summary", async () => {
    const names = bulkNames();
    const diverged = "Bulk 9";
    setDoc({ byId: {}, pages: [], feed: [], loaded: true });
    // A dirty page whose file the checkout replaced: baseline rev-1, disk rev-2.
    reloadPage({
      name: diverged,
      kind: "page",
      title: diverged,
      pre_block: null,
      rev: "rev-1",
      blocks: [{ id: "d1", raw: "typed by the user", collapsed: false, children: [] }],
    });
    markDirty(diverged);
    vi.spyOn(backend(), "getPage").mockImplementation((name) => Promise.resolve(diskDto(name)));
    // Only the backend's own guarded-save refusal may raise the banner.
    vi.spyOn(backend(), "savePage").mockRejectedValue(new Error("conflict:7"));

    try {
      await handleGraphChangedBulk({ changes: changesFor(names) });

      expect(isConflicted(diverged)).toBe(true);
      expect(toasts().at(-1)?.message).toBe(
        "40 pages updated externally · 1 conflict to review"
      );
    } finally {
      clearConflict(diverged);
    }
  });
});
