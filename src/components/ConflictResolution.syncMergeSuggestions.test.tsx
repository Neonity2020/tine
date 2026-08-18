import { afterEach, describe, expect, it } from "vitest";
import { render } from "solid-js/web";
import { PageConflictResolution } from "./ConflictResolution";
import type { ConflictObject } from "../types";

// Concord P3 (ADR 0056): with a base in the ledger the conflict diff is 3-way
// and each non-conflicting row carries a suggestion. The surface must PRE-SELECT
// that side — the user's gesture becomes glance-and-confirm — while applying
// nothing without the explicit click.
//
// Re-pointed in P5. This covered the Settings merge modal, which no longer
// exists: resolution lives at the page, Settings keeps the inventory. The
// assertions are unchanged in substance — every row KIND (modified / added /
// removed) opens on its suggested side, and the surface says where suggestions
// come from — only the surface and its segment wording moved. The mock backend's
// `syncConflictDiff` still serves the same 3-way fixture.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
});

const conflict: ConflictObject = {
  id: "copy:pages/Note.sync-conflict-20260817-120000-AAAAAAA.md",
  source: "sync-copy",
  page_name: "Note",
  page_path: "pages/Note.md",
  kind: "page",
  sides: [
    { role: "mine", label: "This device", path: "pages/Note.md" },
    {
      role: "theirs",
      label: "sync-conflict-20260817-120000-AAAAAAA",
      path: "pages/Note.sync-conflict-20260817-120000-AAAAAAA.md",
    },
  ],
  block_conflicts: 3,
};

function mount(): { host: HTMLElement; dispose: () => void } {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const dispose = render(() => <PageConflictResolution conflict={conflict} />, host);
  return { host, dispose };
}

describe("3-way suggestions in the in-page resolver", () => {
  it("pre-selects each row's suggested side from the 3-way diff", async () => {
    const { host, dispose } = mount();
    try {
      await flush();
      await flush();
      const active = (kind: string) =>
        host
          .querySelector(`.sync-merge-row[data-kind="${kind}"]`)!
          .querySelector(".sync-merge-seg.active")!
          .getAttribute("data-decision");
      expect(active("modified")).toBe("theirs"); // suggestion: theirs
      expect(active("removed")).toBe("theirs"); // suggestion: theirs → pull it in
      expect(active("added")).toBe("mine"); // suggestion: mine → keep it
    } finally {
      dispose();
    }
  });

  it("explains that suggestions come from the last-agreed version", async () => {
    const { host, dispose } = mount();
    try {
      await flush();
      await flush();
      expect(host.textContent).toContain("last version both sides agreed on");
    } finally {
      dispose();
    }
  });
});
