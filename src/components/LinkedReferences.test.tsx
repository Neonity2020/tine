import { afterEach, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import type { BacklinkFilterContext, BlockDto, RefGroup } from "../types";
import { LinkedReferences } from "./LinkedReferences";
import { resetReferenceSectionState } from "../referenceSectionState";

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

afterEach(() => {
  document.body.innerHTML = "";
  localStorage.clear();
  resetReferenceSectionState();
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

  it("normalizes each native search corpus once instead of once per search evaluation", async () => {
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
    vi.spyOn(backend(), "getBacklinkFilterContext").mockResolvedValue({
      entries: [
        { page: "Journal", kind: "journal", block_id: "root", text: indexedText, facets: [] },
      ],
    });
    const root = document.createElement("div");
    document.body.appendChild(root);
    const dispose = render(() => <LinkedReferences name="My Project" />, root);

    await tick();
    await tick();
    root.querySelector<HTMLButtonElement>('button[aria-label="Filter linked references"]')!.click();
    await tick();
    await tick();
    expect(corpusNormalizations).toBe(1);

    const input = root.querySelector<HTMLInputElement>(".reference-filter-search")!;
    for (const query of ["unique", "indexed", "search corpus"]) {
      input.value = query;
      input.dispatchEvent(new InputEvent("input", { bubbles: true }));
      await wait(150);
      expect(root.querySelector(".references-count")?.textContent).toBe("1");
    }
    expect(corpusNormalizations).toBe(1);

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
    vi.spyOn(backend(), "getBacklinkFilterContext").mockResolvedValue({
      entries: [
        { page: "Journal", kind: "journal", block_id: "matching-root", text: "Planning\nA descendant carries the exact needle", facets: [] },
        { page: "Journal", kind: "journal", block_id: "other-root", text: "Another reference\nUnrelated descendant", facets: [] },
      ],
    });
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
    const context: BacklinkFilterContext = {
      entries: [
        { page: "Jul 10th, 2026", kind: "journal", block_id: "planning", text: "Planning", facets: ["fun", "pin", "TODO"] },
        { page: "Jul 9th, 2026", kind: "journal", block_id: "sync", text: "Sync", facets: ["fun", "pin"] },
        { page: "Jul 9th, 2026", kind: "journal", block_id: "direct-todo", text: "TODO should be detected", facets: ["TODO"] },
      ],
    };
    vi.spyOn(backend(), "getBacklinkFilterContext").mockResolvedValue(context);
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
    vi.spyOn(backend(), "getBacklinkFilterContext").mockResolvedValue({
      entries: [
        { page: "Journal", kind: "journal", block_id: "with-task", text: "Planning\nTODO nested task", facets: ["work", "TODO"] },
        { page: "Journal", kind: "journal", block_id: "without-task", text: "Notes", facets: ["notes"] },
      ],
    });
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
    vi.spyOn(backend(), "getBacklinkFilterContext").mockResolvedValue({
      entries: entries.map((entry) => ({
        page,
        kind: "journal",
        block_id: entry.id,
        text: entry.text,
        facets: entry.facets,
      })),
    });
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
