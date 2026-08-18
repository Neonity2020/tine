import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";

const inventory = vi.hoisted(() => ({
  copies: [] as unknown[],
  markers: [] as unknown[],
}));

vi.mock("../backend", async (importOriginal) => {
  const actual = await importOriginal<typeof import("../backend")>();
  return {
    ...actual,
    isTauri: () => false,
    backend: () => ({
      listSyncConflicts: async () => inventory.copies,
      listVcsMarkerConflicts: async () => inventory.markers,
      conflictQueue: async () => [],
      listJournalConflicts: async () => [],
      listJournalFilenameMigrations: async () => [],
      confirm: async () => false,
      trashSyncConflict: async () => {},
    }),
  };
});

import { SettingsConflictPanels } from "./Settings";
import { closeSettings, openSettings, setSyncConflicts, setVcsMarkerConflicts, settingsOpen } from "../ui";
import { paneRouter, resetPaneLayoutToSingle } from "../panes";

// Concord P5: the Settings conflict surfaces are the INVENTORY, not a second
// resolution UI. The block-level merge modal that used to live here is gone —
// two surfaces over the same data drift, and these two already opened with
// DIFFERENT defaults. What Settings keeps is exactly what the page cannot do:
// list what exists (including a copy whose winner page is gone), discard a copy,
// and send the user to the page where resolution happens.

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

beforeEach(() => {
  inventory.copies = [];
  inventory.markers = [];
});

afterEach(() => {
  closeSettings();
  setSyncConflicts([]);
  setVcsMarkerConflicts([]);
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

describe("the Settings conflict inventory", () => {
  it("sends a conflict copy to its page instead of opening a merge modal", async () => {
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
    const { root, dispose } = mount();
    try {
      await tick();
      const review = [...root.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("Review in page")
      )!;
      expect(review).toBeTruthy();
      review.click();
      await tick();
      // Resolution happens at the page — addressed by its exact FILE, so a
      // duplicate-day journal cannot resolve to the canonical one instead.
      const route = paneRouter("main").route();
      expect(route).toMatchObject({ kind: "page", name: "Note", path: "pages/Note.md" });
      expect(settingsOpen()).toBe(false);
      // ...and no modal was opened anywhere.
      expect(document.querySelector(".sync-merge-overlay")).toBeNull();
      expect(document.querySelector(".sync-merge-modal")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("keeps the one thing the page cannot do: a stray whose page is gone, and discard", async () => {
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
      // No page exists to resolve it at, so no in-page affordance is offered...
      expect(
        [...root.querySelectorAll("button")].some((b) => b.textContent?.includes("Review in page"))
      ).toBe(false);
      expect(root.textContent).toContain("no longer exists");
      // ...but discarding it is still reachable, which is why this panel stays.
      expect(
        [...root.querySelectorAll("button")].some((b) => b.textContent?.includes("Discard copy"))
      ).toBe(true);
    } finally {
      dispose();
    }
  });

  it("offers the same in-page route for a marker-bearing file", async () => {
    inventory.markers = [
      { path: "pages/Merged.md", name: "Merged", kind: "page", markers: ["<<<<<<<", ">>>>>>>"] },
    ];
    const { root, dispose } = mount();
    try {
      await tick();
      [...root.querySelectorAll("button")]
        .find((b) => b.textContent?.includes("Review in page"))!
        .click();
      await tick();
      expect(paneRouter("main").route()).toMatchObject({
        kind: "page",
        name: "Merged",
        path: "pages/Merged.md",
      });
    } finally {
      dispose();
    }
  });
});
