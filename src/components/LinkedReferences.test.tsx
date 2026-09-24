import { afterEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { backend, QueryNotReadyError, QueryUnavailableError } from "../backend";
import type { BacklinkFilterContext, BacklinkFilterEntry, BlockDto, RefGroup } from "../types";
import { LinkedReferences } from "./LinkedReferences";
import { resetReferenceSectionState } from "../referenceSectionState";
import { bumpGraphEpoch, correctLaunchAnswers, setGraphMeta } from "../ui";
import { bumpGraphBinding } from "../persistence";
import { parseSearchQuery } from "../editor/searchQuery";
import { mockSearchMatches } from "../mockSearchQuery";

vi.mock("./LiveRefGroup", () => ({
  LiveRefGroup: (props: { blocks: BlockDto[]; showBreadcrumb?: boolean }) => (
    <div class="test-ref-group" data-show-breadcrumb={props.showBreadcrumb ? "true" : "false"}>
      {props.blocks.map((block) => block.id).join(",")}
    </div>
  ),
}));

const block = (id: string, raw: string, marker?: string, children: BlockDto[] = []): BlockDto => ({
  id,
  raw,
  marker,
  collapsed: false,
  children,
});

function tick(): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, 0));
}

function wait(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

type MockFilterEntry = Omit<BacklinkFilterEntry, "text_matches"> & { text: string };

function mockBacklinkFilterContext(
  entries: MockFilterEntry[],
  options: Pick<BacklinkFilterContext, "truncated"> = {}
) {
  return vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
    async (_name, _targets, search) => {
      const matcher = parseSearchQuery(search);
      return {
        ...options,
        search_error: matcher.kind === "invalid" ? matcher.error : undefined,
        entries: entries.map(({ text, ...entry }) => ({
          ...entry,
          text_matches: matcher.kind === "empty" || matcher.kind === "invalid"
            || mockSearchMatches(matcher, text),
        })),
      };
    }
  );
}

afterEach(() => {
  document.body.innerHTML = "";
  localStorage.clear();
  resetReferenceSectionState();
  setGraphMeta(null);
  vi.restoreAllMocks();
});

// GH #272: on a big graph the section would collapse itself while the user
// scrolled or expanded a group. The expand/collapse flag was component-local, so
// any remount of the subtree reset it — and above OG's 100-reference threshold
// "reset" means collapsed. Below the threshold the same remount is invisible,
// which is exactly the reporter's 48-fine / 173-broken split.
describe("Linked References section state survives a remount (GH #272)", () => {
  const bigResult = (): RefGroup[] => [{
    page: "Source",
    kind: "page",
    blocks: Array.from({ length: 173 }, (_, index) => block(`b${index}`, `[[Target]] ${index}`)),
  }];

  it("stays expanded when the component is destroyed and recreated", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(bigResult());
    const root = document.createElement("div");
    document.body.appendChild(root);

    let dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    // Above the threshold it starts collapsed, matching OG.
    expect(root.querySelector(".test-ref-group")).toBeNull();
    (root.querySelector(".references-header") as HTMLElement).click();
    await tick();
    expect(root.querySelector(".test-ref-group")).not.toBeNull();

    // Exactly what a transient re-render of the page subtree does.
    dispose();
    root.innerHTML = "";
    dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();

    expect(root.querySelector(".test-ref-group")).not.toBeNull();
    dispose();
  });

  it("still applies OG's collapse-above-100 default on a page the user has not touched", async () => {
    // Necessity guard: remembering the choice must not turn into "always open".
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(bigResult());
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Untouched" />, root);
    await tick();
    expect(root.querySelector(".test-ref-group")).toBeNull();
    dispose();
  });
});

describe("Linked References filters", () => {
  it("stays unmounted while loading and defaults a threshold-sized result to an unmounted body", async () => {
    let resolve!: (groups: RefGroup[]) => void;
    vi.spyOn(backend(), "getBacklinks").mockImplementation(
      () => new Promise<RefGroup[]>((done) => { resolve = done; })
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await tick();
    expect(root.querySelector(".linked-references")).toBeNull();

    resolve([{
      page: "Source",
      kind: "page",
      blocks: Array.from({ length: 100 }, (_, index) => block(`b${index}`, `[[Target]] ${index}`)),
    }]);
    await tick();
    await tick();
    expect(root.querySelector(".references-count")?.textContent).toBe("100");
    expect(root.querySelector(".test-ref-group")).toBeNull();

    root.querySelector<HTMLElement>(".references-header")!.click();
    expect(root.querySelector(".test-ref-group")).not.toBeNull();
    dispose();
  });

  it("renders a bounded bridge error instead of an empty panel", async () => {
    vi.spyOn(backend(), "getBacklinks").mockRejectedValue(new Error("result-too-large: 20001 matches"));
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await tick();
    await tick();
    expect(root.querySelector<HTMLElement>('[role="alert"]')?.textContent).toContain(
      "bounded result limit was exceeded"
    );
    dispose();
  });

  it("does not mislabel an ordinary backend failure as a bounded bridge error", async () => {
    vi.spyOn(backend(), "getBacklinks").mockRejectedValue(new Error("database unavailable"));
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await tick();
    await tick();
    const message = root.querySelector<HTMLElement>('[role="alert"]')?.textContent ?? "";
    expect(message).toContain("Couldn’t load references");
    expect(message).not.toContain("bounded result limit");
    dispose();
  });

  // GH #594 (index liveness L4): a failed index is named, with a way out,
  // instead of an empty panel or a load that never ends.
  it("shows a failed index with its code, Retry and the diagnostic report", async () => {
    vi.spyOn(backend(), "getBacklinks").mockRejectedValue(
      new QueryUnavailableError("index_failed", "The index couldn't be built.", "file_in_use")
    );
    const retry = vi.spyOn(backend(), "retryIndex").mockResolvedValue();
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await vi.waitFor(() => {
      expect(root.querySelector('[role="alert"]')?.textContent).toContain("code: file_in_use");
    });
    expect(root.querySelector(".index-failed-report")?.textContent).toBe("Create diagnostic report");
    root.querySelector<HTMLButtonElement>(".index-failed-retry")!.click();
    expect(retry).toHaveBeenCalledTimes(1);
    dispose();
  });

  it("says it is indexing while the index is not ready, then shows the references", async () => {
    // The index answers on the second ask; hold that answer so the waiting
    // state is observable rather than a race with the retry timer.
    const answer = deferred<RefGroup[]>();
    vi.spyOn(backend(), "getBacklinks")
      .mockRejectedValueOnce(new QueryNotReadyError("indexing"))
      .mockReturnValue(answer.promise);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await vi.waitFor(() => {
      expect(root.querySelector(".references-loading")?.textContent ?? "").toContain("indexing…");
    });
    answer.resolve([{ page: "Source", kind: "page", blocks: [block("one", "[[Target]]")] }]);
    await vi.waitFor(() => {
      expect(root.querySelector(".references-count")?.textContent).toBe("1");
    });
    expect(root.querySelector(".references-loading")).toBeNull();
    dispose();
  });

  it("requests ancestor context for every linked-reference hit", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([
      { page: "Journal", kind: "journal", blocks: [block("nested", "Nested [[Target]]")] },
    ]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);

    await tick();
    await tick();
    expect(root.querySelector(".test-ref-group")?.getAttribute("data-show-breadcrumb")).toBe("true");

    dispose();
  });

  it("never normalizes or matches the native search corpus in the frontend", async () => {
    const groups: RefGroup[] = [
      {
        page: "Journal",
        kind: "journal",
        blocks: [block("root", "Planning [[My Project]]")],
      },
    ];
    const indexedText = "UNIQUE INDEXED SEARCH CORPUS";
    const originalToLowerCase = String.prototype.toLowerCase;
    let corpusNormalizations = 0;
    vi.spyOn(String.prototype, "toLowerCase").mockImplementation(function (this: string) {
      if (String(this) === indexedText) corpusNormalizations += 1;
      return originalToLowerCase.call(this);
    });
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups);
    vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(async () => ({
      entries: [
        { page: "Journal", kind: "journal", block_id: "root", facets: [], text_matches: true },
      ],
    }));
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);

    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    await tick();
    expect(corpusNormalizations).toBe(0);

    const input = root.querySelector<HTMLInputElement>(".reference-filter-search")!;
    for (const query of ["unique", "indexed", "search corpus"]) {
      input.value = query;
      input.dispatchEvent(new InputEvent("input", { bubbles: true }));
      await wait(150);
      expect(root.querySelector(".references-count")?.textContent).toBe("1");
    }
    expect(corpusNormalizations).toBe(0);

    dispose();
  });

  it("keeps a backlink root when ephemeral content search matches only a descendant (GH #173)", async () => {
    const groups: RefGroup[] = [
      {
        page: "Journal",
        kind: "journal",
        blocks: [
          block("matching-root", "Planning [[My Project]]"),
          block("other-root", "Another [[My Project]] reference"),
        ],
      },
    ];
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups);
    mockBacklinkFilterContext([
      { page: "Journal", kind: "journal", block_id: "matching-root", text: "Planning\nA descendant carries the exact needle", facets: [] },
      { page: "Journal", kind: "journal", block_id: "other-root", text: "Another reference\nUnrelated descendant", facets: [] },
    ]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);

    await tick();
    await tick();
    const filterButton = root.querySelector<HTMLButtonElement>(
      'button[aria-label="Filter linked references"]'
    );
    expect(filterButton).not.toBeNull();
    filterButton!.click();
    await tick();

    const input = root.querySelector<HTMLInputElement>('.reference-filter-search');
    expect(input).not.toBeNull();
    input!.value = '"exact needle"';
    input!.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await wait(150);

    expect(root.querySelector(".reference-filter-summary")?.textContent).toContain("1 of 2");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("matching-root");

    root.querySelector<HTMLButtonElement>(".reference-filter-clear")!.click();
    await tick();
    expect(root.querySelector(".reference-filter-summary")?.textContent).toContain("2 of 2");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("matching-root,other-root");

    dispose();
  });

  it("includes task markers and references from child blocks", async () => {
    const groups: RefGroup[] = [
      {
        page: "Jul 10th, 2026",
        kind: "journal",
        blocks: [
          block("planning", "Planning [[My Project]] #fun #pin", undefined, [
            block("nested-todo", "TODO maybe not to be detected", "TODO"),
          ]),
        ],
      },
      {
        page: "Jul 9th, 2026",
        kind: "journal",
        blocks: [
          block("sync", "Sync [[My Project]] #fun", undefined, [
            block("nested-pin", "very important note #pin"),
          ]),
          block("direct-todo", "TODO should be detected [[My Project]]", "TODO"),
        ],
      },
    ];
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups);
    const context: MockFilterEntry[] = [
        { page: "Jul 10th, 2026", kind: "journal", block_id: "planning", text: "Planning", facets: ["fun", "pin", "TODO"] },
        { page: "Jul 9th, 2026", kind: "journal", block_id: "sync", text: "Sync", facets: ["fun", "pin"] },
        { page: "Jul 9th, 2026", kind: "journal", block_id: "direct-todo", text: "TODO should be detected", facets: ["TODO"] },
    ];
    mockBacklinkFilterContext(context);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);

    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    const chips = [...root.querySelectorAll<HTMLButtonElement>(".ref-filter-chip")].map((el) =>
      el.textContent?.replace(/\s+/g, " ").trim()
    );
    expect(chips).toContain("TODO 2");
    expect(chips).toContain("pin 2");

    dispose();
  });

  it("filters a backlink root when its descendant has the selected marker", async () => {
    const groups: RefGroup[] = [
      {
        page: "Journal",
        kind: "journal",
        blocks: [
          block("with-task", "Planning [[My Project]] #work", undefined, [
            block("task", "TODO nested task", "TODO"),
          ]),
          block("without-task", "Notes [[My Project]] #notes"),
        ],
      },
    ];
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups);
    mockBacklinkFilterContext([
      { page: "Journal", kind: "journal", block_id: "with-task", text: "Planning\nTODO nested task", facets: ["work", "TODO"] },
      { page: "Journal", kind: "journal", block_id: "without-task", text: "Notes", facets: ["notes"] },
    ]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);

    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    const todo = [...root.querySelectorAll<HTMLButtonElement>(".ref-filter-chip")].find((el) =>
      el.textContent?.includes("TODO")
    );
    expect(todo).toBeDefined();
    todo!.click();

    expect(root.querySelector(".references-count")?.textContent).toBe("1");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("with-task");

    dispose();
  });
});

// GH #273: positive include chips OR (a backlink stays when ANY included
// page/tag is present); excludes stay cumulative, zero positives is
// unconstrained, and the text filter stays conjunctive with the facet result.
describe("Linked References include chips OR (GH #273)", () => {
  const mk = (
    entries: { id: string; text: string; facets: string[] }[],
    page = "Jul 10th, 2026"
  ) => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([
      {
        page,
        kind: "journal",
        blocks: entries.map((entry) => block(entry.id, `${entry.text} [[My Project]]`)),
      },
    ]);
    mockBacklinkFilterContext(entries.map((entry) => ({
        page,
        kind: "journal" as const,
        block_id: entry.id,
        text: entry.text,
        facets: entry.facets,
      })));
  };

  async function mountFiltered(entries: { id: string; text: string; facets: string[] }[]) {
    mk(entries);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);
    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    return { root, dispose };
  }

  const chip = (root: HTMLElement, name: string) =>
    [...root.querySelectorAll<HTMLButtonElement>(".ref-filter-chip")].find((el) =>
      el.textContent?.replace(/\s+/g, " ").trim().startsWith(name)
    );

  const shownIds = (root: HTMLElement) =>
    [...root.querySelectorAll<HTMLElement>(".test-ref-group")]
      .map((el) => el.textContent)
      .filter(Boolean)
      .join("|");

  it("union-matches any included page/tag instead of requiring all of them", async () => {
    const { root, dispose } = await mountFiltered([
      { id: "bk-a", text: "Note one", facets: ["work"] },
      { id: "bk-b", text: "Note two", facets: ["fun"] },
      { id: "bk-c", text: "Note three", facets: ["other"] },
    ]);

    chip(root, "work")!.click();
    chip(root, "fun")!.click();

    expect(root.querySelector(".references-count")?.textContent).toBe("2");
    expect(shownIds(root)).toBe("bk-a,bk-b");
    dispose();
  });

  it("keeps exclude chips cumulative over the included union", async () => {
    const { root, dispose } = await mountFiltered([
      { id: "bk-a", text: "Note one", facets: ["work", "pin"] },
      { id: "bn", text: "Note two", facets: ["fun"] },
      { id: "bk-c", text: "Note three", facets: ["work"] },
    ]);

    chip(root, "work")!.click();
    chip(root, "fun")!.click();
    expect(root.querySelector(".references-count")?.textContent).toBe("3");

    // pin: off → include → exclude (clicks cycle)
    const pin = chip(root, "pin")!;
    pin.click();
    pin.click();

    expect(root.querySelector(".references-count")?.textContent).toBe("2");
    expect(shownIds(root)).toBe("bn,bk-c");
    dispose();
  });

  it("treats zero positive chips as unconstrained, as before", async () => {
    const { root, dispose } = await mountFiltered([
      { id: "bk-a", text: "Note one", facets: ["work", "pin"] },
      { id: "bn", text: "Note two", facets: ["fun"] },
      { id: "bk-c", text: "Note three", facets: ["work"] },
    ]);

    const pin = chip(root, "pin")!;
    pin.click(); // include
    pin.click(); // exclude

    expect(root.querySelector(".references-count")?.textContent).toBe("2");
    expect(shownIds(root)).toBe("bn,bk-c");
    dispose();
  });

  it("folds case when matching included names", async () => {
    const { root, dispose } = await mountFiltered([
      { id: "bk-a", text: "Note one", facets: ["WORK"] },
      { id: "bk-b", text: "Note two", facets: ["work"] },
    ]);

    chip(root, "WORK")!.click();

    expect(root.querySelector(".references-count")?.textContent).toBe("2");
    expect(shownIds(root)).toBe("bk-a,bk-b");
    dispose();
  });

  it("keeps the text filter conjunctive with the included union", async () => {
    const { root, dispose } = await mountFiltered([
      { id: "bk-a", text: "apple pie", facets: ["work"] },
      { id: "bk-b", text: "banana bread", facets: ["work"] },
    ]);

    chip(root, "work")!.click();
    expect(root.querySelector(".references-count")?.textContent).toBe("2");

    const search = root.querySelector<HTMLInputElement>('input[aria-label="Search linked reference text"]')!;
    search.value = "apple";
    search.dispatchEvent(new Event("input", { bubbles: true }));
    await wait(200);
    await tick();

    expect(root.querySelector(".references-count")?.textContent).toBe("1");
    expect(shownIds(root)).toBe("bk-a");
    dispose();
  });
});

// GH #173 follow-up. The reporter asked that typing into the reference-text
// field also narrow the list he picks facets FROM ("an additional primary
// filter before making a selection"). The reference list itself already
// narrowed; the facet chips were computed over the UNFILTERED groups, so they
// and their counts never moved while he typed. OG's equivalent field
// ("Search in linked pages") narrows exactly that list.
describe("Linked References facet chips follow the text query (GH #173)", () => {
  const twoFacetedRoots = (): RefGroup[] => [
    {
      page: "Journal",
      kind: "journal",
      blocks: [block("alpha-root", "Planning [[My Project]]"), block("beta-root", "Other [[My Project]]")],
    },
  ];
  const facetedContext = (): MockFilterEntry[] => [
      { page: "Journal", kind: "journal", block_id: "alpha-root", text: "Planning the needle", facets: ["Alpha"] },
      { page: "Journal", kind: "journal", block_id: "beta-root", text: "Other unrelated", facets: ["Beta"] },
  ];

  const openFilter = async (root: HTMLElement) => {
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    await tick();
  };
  const type = async (root: HTMLElement, query: string) => {
    const input = root.querySelector<HTMLInputElement>(".reference-filter-search")!;
    input.value = query;
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await wait(150);
  };
  const chipNames = (root: HTMLElement) =>
    [...root.querySelectorAll(".ref-filter-chip")].map((chip) => chip.textContent!.trim());
  const visibleIds = (root: HTMLElement) =>
    [...root.querySelectorAll<HTMLElement>(".test-ref-group")]
      .map((el) => el.textContent)
      .filter(Boolean)
      .join("|");

  it("drops facets whose only backlinks no longer match the typed text", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(twoFacetedRoots());
    mockBacklinkFilterContext(facetedContext());
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);
    await tick();
    await tick();
    await openFilter(root);

    expect(chipNames(root)).toEqual(["Alpha 1", "Beta 1"]);

    await type(root, "needle");
    expect(visibleIds(root)).toBe("alpha-root");
    expect(chipNames(root)).toEqual(["Alpha 1"]);

    await type(root, "");
    expect(chipNames(root)).toEqual(["Alpha 1", "Beta 1"]);
    dispose();
  });

  // NOTE: this one PASSES before the fix (chips were computed over every group,
  // so an active chip was trivially present). It is a guard against the
  // regression the fix could introduce, not a repro of the reported defect.
  it("keeps an already-selected facet visible so it can still be cleared", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(twoFacetedRoots());
    mockBacklinkFilterContext(facetedContext());
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);
    await tick();
    await tick();
    await openFilter(root);

    const beta = [...root.querySelectorAll<HTMLButtonElement>(".ref-filter-chip")]
      .find((chip) => chip.textContent!.includes("Beta"))!;
    beta.click();
    await tick();

    await type(root, "needle");
    // Beta no longer matches the text, but it is an ACTIVE filter: hiding its
    // chip would strand the user with an unclearable filter and zero results.
    const names = chipNames(root);
    expect(names.some((name) => name.startsWith("Beta"))).toBe(true);
    expect(names.some((name) => name.startsWith("Alpha"))).toBe(true);
    expect(
      root.querySelector<HTMLButtonElement>(".ref-filter-chip.f-in")?.textContent
    ).toContain("Beta");
    dispose();
  });
});

// GH #173 follow-up, second half. While the on-demand descendant index is in
// flight the component deliberately shows every group: the fallback corpus is a
// SUBSET of the native one, so dropping a root on a fallback miss could hide a
// real match. That is the right call — but the summary still read "N of N
// references", asserting a finished filter over an unfiltered list.
describe("Linked References filter summary is honest while indexing (GH #173)", () => {
  it("says filtering is pending instead of reporting a filtered count", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([
      {
        page: "Journal",
        kind: "journal",
        blocks: [block("one", "A [[My Project]]"), block("two", "B [[My Project]]")],
      },
    ]);
    // Never resolves: the resource stays in its loading state.
    vi.spyOn(backend(), "getBacklinkFilterContext").mockReturnValue(
      new Promise<BacklinkFilterContext>(() => {})
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);
    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    const input = root.querySelector<HTMLInputElement>(".reference-filter-search")!;
    input.value = "needle";
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await wait(150);

    const summary = root.querySelector(".reference-filter-summary")!.textContent!;
    expect(summary).toContain("Indexing");
    expect(summary).not.toContain("2 of 2");
    dispose();
  });
});

describe("Linked References native filter request identity", () => {
  const groups = (page: string, ids: string[]): RefGroup[] => [{
    page,
    kind: "page",
    blocks: ids.map((id) => block(id, `${id} [[Target]]`)),
  }];
  const context = (
    page: string,
    matches: Record<string, boolean>,
    extra: Partial<BacklinkFilterContext> = {}
  ): BacklinkFilterContext => ({
    entries: Object.entries(matches).map(([block_id, text_matches]) => ({
      page,
      kind: "page",
      block_id,
      facets: [block_id],
      text_matches,
    })),
    ...extra,
  });
  const type = async (root: HTMLElement, search: string) => {
    const input = root.querySelector<HTMLInputElement>(".reference-filter-search")!;
    input.value = search;
    input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    await wait(150);
  };

  it("keeps the settled list stable while a newer native match is pending", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups("Source", ["one", "two"]));
    const second = deferred<BacklinkFilterContext>();
    vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
      async (_name, _targets, search) => {
        if (search === "one") return context("Source", { one: true, two: false });
        if (search === "two") return second.promise;
        return context("Source", { one: true, two: true });
      }
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    await type(root, "one");
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");

    await type(root, "two");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    expect(root.querySelector(".reference-filter-summary")?.textContent).toContain("Indexing");

    second.resolve(context("Source", { one: false, two: true }));
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("two");
    dispose();
  });

  it("retries typed native readiness and publishes only the current request", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups("Source", ["one", "two"]));
    const getContext = vi.spyOn(backend(), "getBacklinkFilterContext")
      .mockRejectedValueOnce(new QueryNotReadyError("indexing"))
      .mockResolvedValue(context("Source", { one: true, two: false }));
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();

    await vi.waitFor(() => expect(getContext).toHaveBeenCalledTimes(2));
    await vi.waitFor(() => {
      expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    });
    expect(root.querySelector(".reference-filter-error")).toBeNull();
    dispose();
  });

  it("keeps a text-only filter applied across close, Escape, and reopen", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups("Source", ["one", "two"]));
    const getContext = vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
      async (_name, _targets, search) => search === "one"
        ? context("Source", { one: true, two: false })
        : context("Source", { one: true, two: true })
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    const toggle = root.querySelector<HTMLButtonElement>(
      'button[aria-label="Filter linked references"]'
    )!;
    toggle.click();
    await tick();
    await type(root, "one");
    await vi.waitFor(() => {
      expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    });
    const settledCalls = getContext.mock.calls.length;

    root.querySelector<HTMLInputElement>(".reference-filter-search")!
      .dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await tick();
    expect(root.querySelector(".reference-filter-panel")).toBeNull();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    expect(getContext).toHaveBeenCalledTimes(settledCalls);

    toggle.click();
    await tick();
    expect(root.querySelector<HTMLInputElement>(".reference-filter-search")?.value).toBe("one");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    expect(getContext).toHaveBeenCalledTimes(settledCalls);

    toggle.click();
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one");
    expect(getContext).toHaveBeenCalledTimes(settledCalls);
    dispose();
  });

  it("discards a late query reply after a newer query has settled", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups("Source", ["one", "two"]));
    const old = deferred<BacklinkFilterContext>();
    vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
      async (_name, _targets, search) => {
        if (search === "old") return old.promise;
        if (search === "new") return context("Source", { one: false, two: true });
        return context("Source", { one: true, two: true });
      }
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    await type(root, "old");
    await type(root, "new");
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("two");

    old.resolve(context("Source", { one: true, two: false }));
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("two");
    dispose();
  });

  it("does not land a late page/root reply on the replacement inventory", async () => {
    const first = deferred<BacklinkFilterContext>();
    vi.spyOn(backend(), "getBacklinks").mockImplementation(async (name) =>
      name === "First" ? groups("Old source", ["old-root"]) : groups("New source", ["new-root"])
    );
    vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
      async (name) => name === "First"
        ? first.promise
        : context("New source", { "new-root": true })
    );
    const [name, setName] = createSignal("First");
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name={name()} />, root);
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    setName("Second");
    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("new-root");

    first.resolve(context("Old source", { "old-root": false }));
    await tick();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("new-root");
    dispose();
  });

  it("renders native invalid, error, and truncation states without hiding roots", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(groups("Source", ["one", "two"]));
    vi.spyOn(backend(), "getBacklinkFilterContext").mockImplementation(
      async (_name, _targets, search) => {
        if (search === "fail") throw new Error("native failed");
        if (search === "/[/") {
          return context("Source", { one: true, two: true }, { search_error: "invalid regex" });
        }
        return context("Source", { one: true, two: true }, { truncated: search === "large" });
      }
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();

    await type(root, "/[/");
    await tick();
    expect(root.querySelector(".reference-filter-error")?.textContent).toContain("Invalid search");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one,two");

    await type(root, "large");
    await tick();
    expect(root.querySelector(".reference-filter-warning")?.textContent).toContain("searched partially");

    await type(root, "fail");
    await tick();
    expect(root.querySelector(".reference-filter-error")?.textContent).toContain("showing all references");
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one,two");

    await type(root, "");
    await tick();
    expect(root.querySelector(".reference-filter-error")).toBeNull();
    expect(root.querySelector(".test-ref-group")?.textContent).toBe("one,two");
    dispose();
  });
});

// GH #475. The copy button sits at the section's right edge, which in a flex row
// means being the LAST control: Unlinked References has copy alone there, so
// Linked References must not put its filter after it. This is the ordering half
// of the fix; the layout itself is measured in a real engine by
// scripts/shot-reference-header-align.mjs, since jsdom applies no layout.
describe("Linked References header control order (GH #475)", () => {
  it("renders the copy button last, on the same edge as Unlinked References", async () => {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue([
      { page: "Source", kind: "page", blocks: [block("b1", "[[Target]] one")] },
    ] as RefGroup[]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    try {
      await vi.waitFor(() => {
        expect(root.querySelector(".references-header .reference-export-toggle")).not.toBeNull();
      });
      const controls = [...root.querySelectorAll<HTMLButtonElement>(".references-header > button")]
        .map((button) => button.className.split(" ")[0]);
      expect(controls).toEqual(["reference-filter-toggle", "reference-export-toggle"]);
    } finally {
      dispose();
    }
  });
});

// GH #479. Tine implemented OG's rule — a page opens its Linked References
// collapsed once the TOTAL backlink count reaches the threshold — but wired the
// threshold to a constant 100 and never read
// `:ref/linked-references-collapsed-threshold` from config.edn. The reporter's
// case is 0, which the Logseq discussion that produced the key uses to mean
// "always collapsed"; it must not be mistaken for "unset".
describe("Linked References honor :ref/linked-references-collapsed-threshold (GH #479)", () => {
  const backlinks = (count: number): RefGroup[] => [{
    page: "Source",
    kind: "page",
    blocks: Array.from({ length: count }, (_, index) => block(`b${index}`, `[[Target]] ${index}`)),
  }];

  async function mountWithThreshold(count: number, threshold?: number) {
    vi.spyOn(backend(), "getBacklinks").mockResolvedValue(backlinks(count));
    setGraphMeta(
      threshold === undefined
        ? ({ root: "/graphs/A" } as never)
        : ({ root: "/graphs/A", linked_references_collapsed_threshold: threshold } as never),
    );
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    await tick();
    return { root, dispose };
  }

  it("starts collapsed at a threshold of 0, however few backlinks there are", async () => {
    const { root, dispose } = await mountWithThreshold(3, 0);
    expect(root.querySelector(".test-ref-group")).toBeNull();
    // Still the user's to open — this changes the default, not the control.
    (root.querySelector(".references-header") as HTMLElement).click();
    await tick();
    expect(root.querySelector(".test-ref-group")).not.toBeNull();
    dispose();
  });

  it("starts expanded below a configured threshold and collapsed at it", async () => {
    const below = await mountWithThreshold(4, 5);
    expect(below.root.querySelector(".test-ref-group")).not.toBeNull();
    below.dispose();

    const at = await mountWithThreshold(5, 5);
    expect(at.root.querySelector(".test-ref-group")).toBeNull();
    at.dispose();
  });

  it("falls back to OG's 100 when the graph does not set the key", async () => {
    const under = await mountWithThreshold(99);
    expect(under.root.querySelector(".test-ref-group")).not.toBeNull();
    under.dispose();

    const over = await mountWithThreshold(100);
    expect(over.root.querySelector(".test-ref-group")).toBeNull();
    over.dispose();
  });
});

// GH #543, audit R6-06: the right sidebar mounts this panel once and keeps it
// across a same-root rebind (backup restore, journal format change). Keyed on
// the page name alone, it neither refetched nor rejected the old binding's
// answer.
describe("LinkedReferences across a rebind of the same page", () => {
  const group = (id: string): RefGroup => ({ page: `Page ${id}`, kind: "page", blocks: [block(id, `${id} [[Target]]`)] });

  it("refetches when the graph is rebound", async () => {
    const calls = vi.spyOn(backend(), "getBacklinks").mockResolvedValue([group("old")]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    try {
      await vi.waitFor(() => expect(root.textContent).toContain("old"));
      calls.mockResolvedValue([group("new")]);
      bumpGraphBinding();
      bumpGraphEpoch();
      await vi.waitFor(() => expect(root.textContent).toContain("new"));
      expect(calls.mock.calls.length).toBeGreaterThan(1);
    } finally {
      dispose();
    }
  });

  it("does not publish an answer that was asked of the previous binding", async () => {
    const first = deferred<RefGroup[]>();
    vi.spyOn(backend(), "getBacklinks").mockImplementationOnce(() => first.promise)
      .mockResolvedValue([group("new")]);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    try {
      await wait(10);
      bumpGraphBinding();
      bumpGraphEpoch();
      first.resolve([group("preRebind")]);
      await vi.waitFor(() => expect(root.textContent).toContain("new"));
      expect(root.textContent).not.toContain("preRebind");
    } finally {
      dispose();
    }
  });
});

// Launch design D4 (GH #550): during the launch index check the panel may be
// answered from the index as the last session left it. When the check lands
// (`warm-cache-done` → `correctLaunchAnswers`) the panel asks again and shows
// the corrected answer, without the user touching anything.
describe("Linked References after the launch index check", () => {
  it("asks again and shows the corrected answer", async () => {
    let answer: RefGroup[] = [{ page: "Source", kind: "page", blocks: [block("stored", "[[Target]] stored")] }];
    const backlinks = vi.spyOn(backend(), "getBacklinks").mockImplementation(async () => answer);
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="Target" />, root);
    try {
      await vi.waitFor(() => expect(root.querySelector(".test-ref-group")?.textContent).toBe("stored"));
      const asked = backlinks.mock.calls.length;
      answer = [{ page: "Source", kind: "page", blocks: [block("fresh", "[[Target]] fresh")] }];
      correctLaunchAnswers();
      await vi.waitFor(() => expect(root.querySelector(".test-ref-group")?.textContent).toBe("fresh"));
      expect(backlinks.mock.calls.length).toBeGreaterThan(asked);
    } finally {
      dispose();
    }
  });
});
