import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

const inventory = vi.hoisted(() => ({
  copies: [] as unknown[],
  markers: [] as unknown[],
  queue: [] as unknown[],
}));

vi.mock("../backend", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../backend")>();
  return {
    ...actual,
    isTauri: () => false,
    backend: () => ({
      conflictInventory: async () => ({
        sync_conflicts: inventory.copies,
        vcs_markers: inventory.markers,
        queue: inventory.queue,
      }),
      listJournalConflicts: async () => [],
      listJournalFilenameMigrations: async () => [],
      confirm: async () => false,
      trashSyncConflict: async () => {},
    }),
  };
});

import { SettingsConflictPanels } from "./Settings";
import { closeSettings, openSettings, setConflictQueue, setSyncConflicts, setVcsMarkerConflicts, settingsOpen } from "../ui";
import { paneRouter, resetPaneLayoutToSingle } from "../panes";

// Concord P5 made Settings the conflict INVENTORY and the page the one
// resolution surface. GH #536 moved that inventory to the Conflicts overview,
// which the `N conflicts` badge opens: two lists over one queue drift. Settings
// keeps a pointer, and it must still appear for a copy whose page is gone,
// which the queue itself does not carry.

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  inventory.copies = [];
  inventory.markers = [];
  inventory.queue = [];
});

afterEach(() => {
  closeSettings();
  setSyncConflicts([]);
  setVcsMarkerConflicts([]);
  setConflictQueue([]);
  document.body.innerHTML = "";
});

function mount() {
  resetPaneLayoutToSingle({
    tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }],
    activeIndex: 0,
  });
  const root = document.createElement("div");
  document.body.append(root);
  openSettings("backups");
  const dispose = render(() => <SettingsConflictPanels />, root);
  return { root, dispose };
}

const buttons = (root: HTMLElement) => [...root.querySelectorAll("button")].map((b) => b.textContent ?? "");

describe("the Settings conflict inventory", () => {
  it("points to the Conflicts overview instead of keeping a second list", async () => {
    inventory.copies = [
      {
        path: "pages/Note.sync-conflict-20260818-101010-AAAAAAA.md",
        base_name: "Note",
        base_path: "pages/Note.md",
        kind: "page",
        tag: "sync-conflict-20260818-101010-AAAAAAA",
        preview: "TODO ship the beta",
      },
    ];
    inventory.queue = [
      {
        id: "copy:pages/Note.sync-conflict-20260818-101010-AAAAAAA.md",
        source: "sync-copy",
        page_name: "Note",
        page_path: "pages/Note.md",
        kind: "page",
        sides: [],
        block_conflicts: 2,
      },
    ];
    const { root, dispose } = mount();
    try {
      await tick();
      await tick();
      expect(buttons(root).some((b) => b.includes("Review in page"))).toBe(false);
      expect(buttons(root).some((b) => b.includes("Discard copy"))).toBe(false);
      [...root.querySelectorAll("button")].find((b) => b.textContent?.includes("Open conflicts"))!.click();
      await tick();
      expect(paneRouter("main").route()).toEqual({ kind: "conflicts" });
      expect(settingsOpen()).toBe(false);
      expect(document.querySelector(".sync-merge-overlay")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("still points there for a stray copy whose page is gone", async () => {
    inventory.copies = [
      {
        path: "pages/Gone.sync-conflict-20260818-101010-BBBBBBB.md",
        base_name: "Gone",
        base_path: null,
        kind: "page",
        tag: "sync-conflict-20260818-101010-BBBBBBB",
        preview: "orphaned copy",
      },
    ];
    const { root, dispose } = mount();
    try {
      await tick();
      await tick();
      // The queue omits it (there is no page to resolve at), so the pointer
      // must count it or the discard it needs would be unreachable.
      expect(root.textContent).toContain("1 item needs a decision");
      expect(buttons(root).some((b) => b.includes("Open conflicts"))).toBe(true);
    } finally {
      dispose();
    }
  });

  it("shows nothing when nothing needs a decision", async () => {
    const { root, dispose } = mount();
    try {
      await tick();
      await tick();
      expect(buttons(root).some((b) => b.includes("Open conflicts"))).toBe(false);
    } finally {
      dispose();
    }
  });
});
