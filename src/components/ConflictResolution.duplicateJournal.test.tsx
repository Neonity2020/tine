import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { PageConflictResolution } from "./ConflictResolution";
import { __setBackendForTest, type Backend } from "../backend";
import { resetStore } from "../store";
import {
  setConflictQueue,
  setGraphMeta,
  setJournalConflicts,
  setToasts,
} from "../ui";
import type { ConflictObject, JournalConflict, SyncConflictDiff } from "../types";

// Item 5. Fail-before: a duplicate journal day was not a conflict object at
// all, so it had no badge, no dock entry and no in-page surface — its only
// global signal was a sticky startup toast pointing at Settings. These assert
// the surface that replaced it, and in particular that Merge is IMPLICIT: the
// panel offers row-by-row choices rather than a bare file list, so keep-both
// reproduces what Settings' Merge did by concatenation.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
  __setBackendForTest(null);
  setConflictQueue([]);
  setJournalConflicts([]);
  setGraphMeta(null);
  setToasts([]);
  resetStore();
});

const view = (text: string) => ({ uuid: "", text, child_count: 0 });

const day: ConflictObject = {
  id: "journal:journals/2026_06_26.md",
  source: "duplicate-journal",
  page_name: "Friday, 26-06-2026",
  page_path: "journals/2026_06_26.md",
  kind: "journal",
  sides: [
    { role: "mine", label: "2026_06_26.md", path: "journals/2026_06_26.md" },
    { role: "theirs", label: "Friday, 26-06-2026.md", path: "journals/Friday, 26-06-2026.md" },
  ],
  block_conflicts: 2,
};

const inventory: JournalConflict[] = [
  {
    title: "Friday, 26-06-2026",
    files: [
      { name: "2026_06_26.md", path: "journals/2026_06_26.md", preview: "morning notes", canonical: true },
      { name: "Friday, 26-06-2026.md", path: "journals/Friday, 26-06-2026.md", preview: "evening notes", canonical: false },
    ],
  },
];

/** Disjoint content: every row is one-sided, the shape where keep-both is a
 *  plain fold and nothing can be lost. */
const disjointDiff = {
  base_rev: "canonical-rev",
  conflict_rev: "stray-rev",
  rows: [
    { id: "0", kind: "removed" as const, mine: view("morning notes"), theirs: null, children: [] },
    { id: "1", kind: "added" as const, mine: null, theirs: view("evening notes"), children: [] },
  ],
  pre_differs: false,
  blocks_identical: false,
  three_way: false,
} as unknown as SyncConflictDiff;

function stub(overrides: Partial<Backend>): void {
  __setBackendForTest({
    duplicateJournalDiff: async () => disjointDiff,
    resolveDuplicateJournalDay: async () => ({ name: "Friday, 26-06-2026", blocks: [], rev: "r" }),
    listSyncConflicts: async () => [],
    listVcsMarkerConflicts: async () => [],
    listJournalConflicts: async () => inventory,
    conflictQueue: async () => [],
    confirm: async () => true,
    trashJournalFile: async () => {},
    renameFileToPage: async () => {},
    readJournalFile: async () => "",
    ...overrides,
  } as unknown as Backend);
}

function mount(conflict: ConflictObject): { host: HTMLElement; dispose: () => void } {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const dispose = render(() => <PageConflictResolution conflict={conflict} />, host);
  return { host, dispose };
}

describe("a duplicate journal day resolves at the page", () => {
  it("names both files as the two sides", async () => {
    stub({});
    setJournalConflicts(inventory);
    const { host, dispose } = mount(day);
    try {
      await flush();
      await flush();
      const legend = host.querySelector(".page-conflict-legend")!;
      expect(legend.textContent).toContain("2026_06_26.md");
      expect(legend.textContent).toContain("Friday, 26-06-2026.md");
    } finally {
      dispose();
    }
  });

  it("offers row-by-row choices, so Merge is implicit rather than a separate action", async () => {
    stub({});
    setJournalConflicts(inventory);
    const { host, dispose } = mount(day);
    try {
      await flush();
      await flush();
      // Real decision rows — not the bare file list the original spec proposed.
      expect(host.querySelectorAll(".sync-merge-row").length).toBeGreaterThan(0);
      expect(host.textContent).toContain("morning notes");
      expect(host.textContent).toContain("evening notes");
    } finally {
      dispose();
    }
  });

  it("still offers the per-file actions, Rename included", async () => {
    stub({});
    setJournalConflicts(inventory);
    const { host, dispose } = mount(day);
    try {
      await flush();
      await flush();
      const rows = host.querySelectorAll("[data-journal-conflict]");
      expect(rows.length).toBe(2);
      const labels = Array.from(host.querySelectorAll(".journal-conflict-actions button")).map(
        (b) => b.textContent?.trim()
      );
      expect(labels).toContain("Open");
      expect(labels).toContain("Rename…");
      expect(labels).toContain("Trash");
    } finally {
      dispose();
    }
  });

  it("trashes a file through the recoverable backend path, after confirming", async () => {
    const trashJournalFile = vi.fn(async () => {});
    const confirm = vi.fn(async () => true);
    stub({ trashJournalFile, confirm } as unknown as Partial<Backend>);
    setJournalConflicts(inventory);
    const { host, dispose } = mount(day);
    try {
      await flush();
      await flush();
      const strayRow = host.querySelector(
        '[data-journal-conflict="journals/Friday, 26-06-2026.md"]'
      )!;
      const trash = Array.from(strayRow.querySelectorAll("button")).find(
        (b) => b.textContent?.trim() === "Trash"
      )!;
      trash.dispatchEvent(new MouseEvent("click", { bubbles: true }));
      await flush();
      await flush();
      expect(confirm).toHaveBeenCalled();
      expect(trashJournalFile).toHaveBeenCalledWith("Friday, 26-06-2026.md");
    } finally {
      dispose();
    }
  });

  it("explains a cross-format pair instead of offering choices it cannot apply", async () => {
    stub({ duplicateJournalDiff: async () => null } as unknown as Partial<Backend>);
    setJournalConflicts(inventory);
    const { host, dispose } = mount(day);
    try {
      await flush();
      await flush();
      expect(host.textContent).toContain("different formats");
      // The file rows are still the whole affordance in that case.
      expect(host.querySelectorAll("[data-journal-conflict]").length).toBe(2);
    } finally {
      dispose();
    }
  });
});
