// Research fixture for GH #481: "Linked References" missing on journal pages.
//
// NOT a production change. Renders the real PageView on a routed JOURNAL page
// with the backend mocked at the IPC boundary, and compares it with an
// ordinary-page control:
//
//   1. a journal page route must mount the Linked References section and pass
//      the journal's display title to getBacklinks verbatim;
//   2. an ordinary page route must do the same (control);
//   3. a journal page with zero references renders nothing (the reporter's
//      expected "If there are 0 references do not show anything").
//
// Together with the Rust fixture (crates/tine-core/tests/
// issue481_journal_linked_references.rs, which proves the query layer answers
// journal targets), this pins every layer the reported failure could live in.

import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import type { JSX } from "solid-js";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { initParser } from "../render/parse";
import { resetStore } from "../store";
import { mainPaneRouter, resetTabsToJournals } from "../router";
import { setGraphMeta } from "../ui";
import { PageView } from "./Page";
import type { GraphMeta, PageDto, RefGroup } from "../types";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.clearAllTimers();
  vi.useRealTimers();
  vi.restoreAllMocks();
  resetStore();
  setGraphMeta(null);
  document.body.innerHTML = "";
  resetTabsToJournals();
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  const dispose = render(node, root);
  return { root, dispose };
}

async function flushMicrotasks() {
  for (let i = 0; i < 12; i++) await Promise.resolve();
}

function journalGraphMeta(): GraphMeta {
  return {
    root: "/tmp/tine-481-graph",
    journals_dir: "journals",
    pages_dir: "pages",
    preferred_workflow: "now",
    shortcuts: {},
    start_of_week: 6,
    block_hidden_properties: [],
    linked_references_collapsed_threshold: 100,
    default_journal_template: null,
    favorites: [],
    journal_page_title_format: "MMM do, yyyy",
    journal_file_name_format: "yyyy_MM_dd",
    preferred_format: "md",
    macros: {},
    enable_timetracking: true,
    show_brackets: true,
    logbook_with_second_support: true,
    logbook_enabled_in_timestamped_blocks: false,
    logbook_enabled_in_all_blocks: false,
    guide_announced: true,
  };
}

function refGroup(from: string, kind: "page" | "journal"): RefGroup {
  return {
    page: from,
    kind,
    blocks: [{ id: `${from}-b1`, raw: `mentions the target`, collapsed: false, children: [] }],
  };
}

describe("GH #481: Linked References on a routed journal page", () => {
  it("renders the section and queries by the journal's display title", async () => {
    setGraphMeta(journalGraphMeta());
    const journal: PageDto = {
      name: "Sep 20th, 2026",
      kind: "journal",
      title: "Sep 20th, 2026",
      pre_block: null,
      blocks: [{ id: "j-root", raw: "journal day under test", collapsed: false, children: [] }],
    };
    vi.spyOn(backend(), "getPage").mockResolvedValue(journal);
    const backlinks = vi
      .spyOn(backend(), "getBacklinks")
      .mockResolvedValue([refGroup("Notes", "page")]);
    vi.spyOn(backend(), "getUnlinkedRefs").mockResolvedValue([]);

    mainPaneRouter.openPage("Sep 20th, 2026", "journal", { inPlace: true });
    const { root, dispose } = mount(() => <PageView />);
    try {
      await flushMicrotasks();
      expect(backlinks).toHaveBeenCalledWith("Sep 20th, 2026");
      const section = root.querySelector<HTMLElement>(".linked-references");
      expect(section).not.toBeNull();
      expect(section!.textContent).toContain("Linked References");
      expect(section!.textContent).toContain("Notes");
    } finally {
      dispose();
    }
  });

  it("renders the section for an ordinary page (control)", async () => {
    setGraphMeta(journalGraphMeta());
    const target: PageDto = {
      name: "Target",
      kind: "page",
      title: "Target",
      pre_block: null,
      blocks: [{ id: "t-root", raw: "the target page", collapsed: false, children: [] }],
    };
    vi.spyOn(backend(), "getPage").mockResolvedValue(target);
    const backlinks = vi
      .spyOn(backend(), "getBacklinks")
      .mockResolvedValue([refGroup("Notes", "page")]);
    vi.spyOn(backend(), "getUnlinkedRefs").mockResolvedValue([]);

    mainPaneRouter.openPage("Target", "page", { inPlace: true });
    const { root, dispose } = mount(() => <PageView />);
    try {
      await flushMicrotasks();
      expect(backlinks).toHaveBeenCalledWith("Target");
      const section = root.querySelector<HTMLElement>(".linked-references");
      expect(section).not.toBeNull();
      expect(section!.textContent).toContain("Linked References");
      expect(section!.textContent).toContain("Notes");
    } finally {
      dispose();
    }
  });

  it("renders no section when the journal has zero references", async () => {
    setGraphMeta(journalGraphMeta());
    const journal: PageDto = {
      name: "Sep 21st, 2026",
      kind: "journal",
      title: "Sep 21st, 2026",
      pre_block: null,
      blocks: [{ id: "j2-root", raw: "quiet day", collapsed: false, children: [] }],
    };
    vi.spyOn(backend(), "getPage").mockResolvedValue(journal);
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([]);
    vi.spyOn(backend(), "getUnlinkedRefs").mockResolvedValue([]);

    mainPaneRouter.openPage("Sep 21st, 2026", "journal", { inPlace: true });
    const { root, dispose } = mount(() => <PageView />);
    try {
      await flushMicrotasks();
      expect(root.querySelector(".linked-references")).toBeNull();
    } finally {
      dispose();
    }
  });
});
