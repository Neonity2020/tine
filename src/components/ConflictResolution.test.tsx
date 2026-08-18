import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { PageConflictResolution } from "./ConflictResolution";
import { __setBackendForTest, type Backend } from "../backend";
import { conflictQueue, setConflictQueue } from "../ui";
import type { ConflictObject, MarkerConflictDiff, SyncConflictDiff } from "../types";

// Concord P4 (L4 + L5). Fail-before: nothing in Tine could resolve a
// VCS-marker conflict at all — a marker-bearing page showed a banner telling the
// user to go and fix it in another tool, and a conflict copy could only be
// merged from a Settings modal. These assert the in-page surface: the sides the
// artifact itself named, a suggested resolution pre-selected from the markers'
// own common ancestor, keep-both as the no-loss fallback, and an apply that goes
// through the guarded backend path with the file's own base_rev.

const flush = () => new Promise((resolve) => setTimeout(resolve, 0));

afterEach(() => {
  document.body.innerHTML = "";
  __setBackendForTest(null);
  setConflictQueue([]);
});

const view = (text: string) => ({ uuid: "", text, child_count: 0 });

const markerObject: ConflictObject = {
  id: "markers:pages/Merged.md",
  source: "vcs-markers",
  page_name: "Merged",
  page_path: "pages/Merged.md",
  kind: "page",
  sides: [
    { role: "mine", label: "HEAD" },
    { role: "theirs", label: "feature" },
    { role: "base", label: "Common ancestor" },
  ],
  block_conflicts: 2,
  markers: ["<<<<<<<", "|||||||", "=======", ">>>>>>>"],
};

/** A 3-way marker diff: one row only we changed, one only they changed, and one
 *  both changed (so it has no suggestion and falls back to keep-both). */
const markerDiff: MarkerConflictDiff = {
  mine_label: "HEAD",
  theirs_label: "feature",
  regions: 1,
  diff: {
    base_rev: "marker-file-rev",
    conflict_rev: "marker-file-rev",
    rows: [
      { id: "0", kind: "unchanged", mine: view("shared top"), theirs: view("shared top"), children: [] },
      {
        id: "1",
        kind: "modified",
        mine: view("alpha edited here"),
        theirs: view("alpha"),
        children: [],
        verdict: "mine-only",
        suggestion: "mine",
      },
      {
        id: "2",
        kind: "modified",
        mine: view("beta"),
        theirs: view("beta edited there"),
        children: [],
        verdict: "theirs-only",
        suggestion: "theirs",
      },
      {
        id: "3",
        kind: "modified",
        mine: view("gamma my way"),
        theirs: view("gamma their way"),
        children: [],
        verdict: "both-changed",
      },
    ],
    mine_pre: null,
    theirs_pre: null,
    pre_differs: false,
    blocks_identical: false,
    three_way: true,
  },
};

function stubBackend(overrides: Partial<Backend>): void {
  __setBackendForTest({
    vcsMarkerConflictDiff: async () => markerDiff,
    syncConflictDiff: async () => null,
    resolveVcsMarkerConflict: async () => {},
    resolveSyncConflict: async () => {},
    listSyncConflicts: async () => [],
    listVcsMarkerConflicts: async () => [],
    conflictQueue: async () => [],
    ...overrides,
  } as unknown as Backend);
}

function mount(conflict: ConflictObject): { host: HTMLElement; dispose: () => void } {
  const host = document.createElement("div");
  document.body.appendChild(host);
  const dispose = render(() => <PageConflictResolution conflict={conflict} />, host);
  return { host, dispose };
}

describe("in-page conflict resolution", () => {
  it("names the sides the marker file itself named", async () => {
    stubBackend({});
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      const legend = host.querySelector(".page-conflict-legend")!;
      expect(legend.textContent).toContain("HEAD");
      expect(legend.textContent).toContain("feature");
      // The third side is a first-class part of the object, not an assumption
      // that a conflict has exactly two sides.
      expect(legend.textContent).toContain("Common ancestor");
    } finally {
      dispose();
    }
  });

  it("pre-selects the suggested side, and keeps BOTH where no side is suggested", async () => {
    stubBackend({});
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      const active = (id: string) =>
        host
          .querySelector(`[data-row-id="${id}"]`)!
          .querySelector(".sync-merge-seg.active")!
          .getAttribute("data-decision");
      expect(active("1")).toBe("mine"); // suggestion: mine
      expect(active("2")).toBe("theirs"); // suggestion: theirs
      // Both sides moved away from the ancestor: no suggestion is possible, so
      // the no-loss default takes over instead of silently dropping a side.
      expect(active("3")).toBe("both");
    } finally {
      dispose();
    }
  });

  it("counts the regions needing a decision and offers navigation", async () => {
    stubBackend({});
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      expect(host.querySelector(".page-conflict-count")!.textContent).toBe("3 conflicts");
      expect(host.querySelectorAll(".page-conflict-nav button").length).toBe(2);
    } finally {
      dispose();
    }
  });

  it("applies through the guarded marker path with the file's own base_rev", async () => {
    const resolve = vi.fn(async () => {});
    stubBackend({ resolveVcsMarkerConflict: resolve as unknown as Backend["resolveVcsMarkerConflict"] });
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      expect(resolve).not.toHaveBeenCalled(); // nothing auto-applies
      const apply = [...host.querySelectorAll("button")].find((b) =>
        b.textContent?.includes("Apply resolution")
      )!;
      apply.click();
      await flush();
      await flush();
      expect(resolve).toHaveBeenCalledTimes(1);
      const [path, decisions, baseRev] = resolve.mock.calls[0] as unknown as [
        string,
        Record<string, string>,
        string,
      ];
      expect(path).toBe("pages/Merged.md");
      expect(baseRev).toBe("marker-file-rev");
      expect(decisions).toEqual({ "1": "mine", "2": "theirs", "3": "both" });
    } finally {
      dispose();
    }
  });

  it("routes a conflict copy through the existing resolve path, not the marker one", async () => {
    const copyDiff: SyncConflictDiff = {
      base_rev: "winner-rev",
      conflict_rev: "copy-rev",
      rows: [{ id: "0", kind: "modified", mine: view("mine"), theirs: view("theirs"), children: [] }],
      mine_pre: null,
      theirs_pre: null,
      pre_differs: false,
      blocks_identical: false,
    };
    const resolveCopy = vi.fn(async () => {});
    const resolveMarkers = vi.fn(async () => {});
    stubBackend({
      syncConflictDiff: (async () => copyDiff) as unknown as Backend["syncConflictDiff"],
      resolveSyncConflict: resolveCopy as unknown as Backend["resolveSyncConflict"],
      resolveVcsMarkerConflict: resolveMarkers as unknown as Backend["resolveVcsMarkerConflict"],
    });
    const copyObject: ConflictObject = {
      id: "copy:pages/Note.sync-conflict-20260817-101010-ABCDEFG.md",
      source: "sync-copy",
      page_name: "Note",
      page_path: "pages/Note.md",
      kind: "page",
      sides: [
        { role: "mine", label: "This device", path: "pages/Note.md" },
        {
          role: "theirs",
          label: "sync-conflict-20260817-101010-ABCDEFG",
          path: "pages/Note.sync-conflict-20260817-101010-ABCDEFG.md",
        },
      ],
      block_conflicts: 1,
    };
    const { host, dispose } = mount(copyObject);
    try {
      await flush();
      await flush();
      [...host.querySelectorAll("button")]
        .find((b) => b.textContent?.includes("Apply resolution"))!
        .click();
      await flush();
      await flush();
      expect(resolveMarkers).not.toHaveBeenCalled();
      expect(resolveCopy).toHaveBeenCalledTimes(1);
      const [winner, copy, , baseRev, conflictRev] = resolveCopy.mock
        .calls[0] as unknown as [string, string, unknown, string, string];
      expect(winner).toBe("pages/Note.md");
      expect(copy).toBe("pages/Note.sync-conflict-20260817-101010-ABCDEFG.md");
      expect(baseRev).toBe("winner-rev");
      expect(conflictRev).toBe("copy-rev");
    } finally {
      dispose();
    }
  });

  // Concord P5. The Settings modal was the only surface that let the user choose
  // what happens to the page's OWN properties when the two sides' pre-blocks
  // differ; the in-page resolver hardcoded "union". Retiring the modal without
  // this would have silently dropped a capability.
  it("offers the page-property choice the retired Settings modal used to own", async () => {
    const resolve = vi.fn(async () => {});
    const preDiff: MarkerConflictDiff = {
      ...markerDiff,
      diff: {
        ...markerDiff.diff,
        mine_pre: "alias:: here",
        theirs_pre: "alias:: there",
        pre_differs: true,
      },
    };
    stubBackend({
      vcsMarkerConflictDiff: (async () => preDiff) as unknown as Backend["vcsMarkerConflictDiff"],
      resolveVcsMarkerConflict: resolve as unknown as Backend["resolveVcsMarkerConflict"],
    });
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      const choice = host.querySelector<HTMLSelectElement>(".page-conflict-preblock-choice")!;
      // No-loss by default, consistent with this surface's row policy.
      expect(choice.value).toBe("union");
      expect([...choice.options].map((o) => o.value)).toEqual(["union", "mine", "theirs"]);

      choice.value = "mine";
      choice.dispatchEvent(new Event("change"));
      [...host.querySelectorAll("button")]
        .find((b) => b.textContent?.includes("Apply resolution"))!
        .click();
      await flush();
      await flush();
      const [, , , preChoice] = resolve.mock.calls[0] as unknown as [
        string,
        Record<string, string>,
        string,
        string,
      ];
      expect(preChoice).toBe("mine");
    } finally {
      dispose();
    }
  });

  it("hides the page-property choice when the two sides agree on them", async () => {
    stubBackend({});
    const { host, dispose } = mount(markerObject);
    try {
      await flush();
      await flush();
      expect(host.querySelector(".page-conflict-preblock-choice")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("warns quietly — never blocks — when the page is left unresolved", async () => {
    stubBackend({});
    setConflictQueue([markerObject]);
    const { dispose } = mount(markerObject);
    await flush();
    await flush();
    // Unmounting IS leaving the page. The object is still queued, so a note is
    // pushed; no dialog and no navigation veto exist anywhere in this path.
    dispose();
    await flush();
    expect(document.querySelector(".sync-merge-overlay")).toBeNull();
    expect(conflictQueue()).toHaveLength(1);
  });
});
