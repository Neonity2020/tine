import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { SyncConflictMergeModal } from "./Settings";
import type { SyncConflict } from "../types";

// Concord P3 (ADR 0056): with a base in the ledger the conflict diff is 3-way
// and each non-conflicting row carries a suggestion. The modal must PRE-SELECT
// that side — the user's gesture becomes glance-and-confirm — while applying
// nothing without the explicit merge click. Fail-before: the pre-3-way modal
// defaulted every row to "mine", so no "Copy"/"Pull in" segment was ever active
// on open. The mock backend's syncConflictDiff serves the 3-way fixture.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
});

const conflict: SyncConflict = {
  path: "pages/Note.sync-conflict-20260817-120000-AAAAAAA.md",
  base_name: "Note",
  base_path: "pages/Note.md",
  kind: "page",
  tag: "sync-conflict-20260817-120000-AAAAAAA",
  preview: "TODO ship the beta by Thursday",
};

describe("SyncConflictMergeModal 3-way suggestions", () => {
  it("pre-selects each row's suggested side from the 3-way diff", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(
      () => <SyncConflictMergeModal conflict={conflict} onClose={() => {}} />,
      host
    );
    try {
      await flush();
      await flush();
      // Modified row suggesting "theirs" → the "Copy" segment is active on open.
      const modified = host.querySelector('.sync-merge-row[data-kind="modified"]');
      expect(modified, "modified row rendered").toBeTruthy();
      expect(modified!.querySelector(".sync-merge-seg.active")?.textContent).toBe("Copy");
      // Removed row suggesting "theirs" → "Pull in" is active.
      const removed = host.querySelector('.sync-merge-row[data-kind="removed"]');
      expect(removed!.querySelector(".sync-merge-seg.active")?.textContent).toBe("Pull in");
      // Added row suggesting "mine" → "Keep" stays active (the default side).
      const added = host.querySelector('.sync-merge-row[data-kind="added"]');
      expect(added!.querySelector(".sync-merge-seg.active")?.textContent).toBe("Keep");
    } finally {
      dispose();
    }
  });

  it("explains that suggestions come from the last-agreed version", async () => {
    const host = document.createElement("div");
    document.body.appendChild(host);
    const dispose = render(
      () => <SyncConflictMergeModal conflict={conflict} onClose={() => {}} />,
      host
    );
    try {
      await flush();
      await flush();
      expect(host.textContent).toContain("last version Tine and this file agreed on");
    } finally {
      dispose();
    }
  });
});
