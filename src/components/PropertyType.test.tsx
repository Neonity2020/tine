// **The property registry, made visible and writable (SPEC §6.3, §9 P2; T1).**
//
// P0 shipped the whole registry — observed type, cardinality, `mismatch_count`,
// and the declared override read off the key's own page — and until this packet
// no UI had ever called `query_registry`. What this file is the evidence for:
//
//  A1  The badge says what the key's type IS, and whether that is DECLARED or
//      merely observed. The two are different claims and must not read alike.
//  A2  A mismatch count is shown only against a declaration — never against an
//      observed majority, which is just what an untyped graph looks like.
//  A3  "declare type…" writes `tine.type::` on the key's page THROUGH the
//      ordinary page path (I-1), creating that page if it does not exist, and
//      on the page name the engine will actually bind: the row's
//      `normalized_name`, so a key authored `due date` declares on `due-date`.
//  A4  The registry is read when the picker opens and again after a declaration
//      lands — and on nothing else (I-13).
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import type { JSX } from "solid-js";
import { PropertyType, registryRowFor } from "./PropertyType";
import { QueryBuilder } from "./QueryBuilder";
import { initParser } from "../render/parse";
import { backend } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { doc, flushPage, readPageProperty, resetStore, setDoc, type FeedPage } from "../store";
import type { PageDto, SavePageResult } from "../types";
import type { BuilderSession } from "./QueryBuilder";
import type { RegistryRow, RegistrySnapshot } from "../editor/queryIr";

beforeAll(async () => {
  await initParser();
});

afterEach(() => {
  vi.restoreAllMocks();
  resetSharedQueryResultsForTests();
  resetStore();
  localStorage.clear();
  document.body.innerHTML = "";
});

function mount(node: () => JSX.Element): { root: HTMLDivElement; dispose: () => void } {
  const root = document.createElement("div");
  document.body.appendChild(root);
  return { root, dispose: render(node, root) };
}

const wait = (ms: number) => new Promise((resolve) => setTimeout(resolve, ms));
async function settle(): Promise<void> {
  for (let i = 0; i < 6; i++) await wait(0);
}

function row(over: Partial<RegistryRow> & { normalized_name: string }): RegistryRow {
  return {
    cardinality: "one",
    observed_type: "text",
    count_blocks: 3,
    count_pages: 0,
    mismatch_count: 0,
    ...over,
  };
}

// ---------------------------------------------------------------------------
// A1 / A2: the badge and the mismatch line
// ---------------------------------------------------------------------------

function showBadge(rows: RegistryRow[], key: string): { root: HTMLDivElement; dispose: () => void } {
  return mount(() => (
    <PropertyType propertyKey={key} rows={() => rows} onDeclarationWritten={() => {}} />
  ));
}

describe("A1: the badge distinguishes what was DECLARED from what was observed", () => {
  it("shows the observed type, marked observed, when nobody has declared one", () => {
    const { root, dispose } = showBadge([row({ normalized_name: "cost", observed_type: "number" })], "cost");
    try {
      const badge = root.querySelector(".qb-prop-type-badge")!;
      expect(badge.textContent).toContain("number");
      expect(badge.textContent).toContain("observed");
      expect(badge.classList.contains("is-declared")).toBe(false);
    } finally {
      dispose();
    }
  });

  it("shows the DECLARED type when there is one, and says so", () => {
    const { root, dispose } = showBadge(
      [row({ normalized_name: "cost", observed_type: "text", declared: ["number", "one"] })],
      "cost",
    );
    try {
      const badge = root.querySelector(".qb-prop-type-badge")!;
      // The declaration wins for coercion (`Registry::effective_type`), so it is
      // what the badge must show — showing the observed majority beside a
      // declaration would tell the user the opposite of what the engine does.
      expect(badge.textContent).toContain("number");
      expect(badge.textContent).not.toContain("text");
      expect(badge.textContent).toContain("declared");
      expect(badge.classList.contains("is-declared")).toBe(true);
    } finally {
      dispose();
    }
  });

  it("spells a many-valued key `list of …`", () => {
    const { root, dispose } = showBadge(
      [row({ normalized_name: "tags", observed_type: "ref", cardinality: "many" })],
      "tags",
    );
    try {
      expect(root.querySelector(".qb-prop-type-badge")!.textContent).toContain("list of ref");
    } finally {
      dispose();
    }
  });

  it("says `no observed values` and shows no type for a key the registry has never seen", () => {
    const { root, dispose } = showBadge([row({ normalized_name: "cost" })], "brand-new-key");
    try {
      expect(root.querySelector(".qb-prop-type-badge")).toBeNull();
      expect(root.textContent).toContain("no observed values");
      // No row means nothing to declare ON: the engine binds a declaration by
      // the row's own normalized name, and this module deliberately does not
      // invent one (D-14).
      expect(root.querySelector(".qb-prop-type-declare")).toBeNull();
    } finally {
      dispose();
    }
  });

  it("matches a picker key to its row without re-implementing property_key_norm", () => {
    const rows = [row({ normalized_name: "due-date" }), row({ normalized_name: "cost" })];
    expect(registryRowFor(rows, "due-date")?.normalized_name).toBe("due-date");
    // The engine folds space and underscore to `-` and lowercases; this MATCHES
    // those shapes rather than producing them.
    expect(registryRowFor(rows, "Due Date")?.normalized_name).toBe("due-date");
    expect(registryRowFor(rows, "due_date")?.normalized_name).toBe("due-date");
    expect(registryRowFor(rows, "  cost ")?.normalized_name).toBe("cost");
    expect(registryRowFor(rows, "nothing/like/this")).toBeUndefined();
    expect(registryRowFor(undefined, "cost")).toBeUndefined();
  });
});

describe("A2: a mismatch count is shown against a DECLARATION and nothing else", () => {
  it("names how many owners disagree with the declared type", () => {
    const { root, dispose } = showBadge(
      [row({ normalized_name: "cost", declared: ["number", "one"], mismatch_count: 2 })],
      "cost",
    );
    try {
      // `mismatch_count` counts OWNERS — blocks and pages alike — not atoms.
      expect(root.querySelector(".qb-prop-type-mismatch")!.textContent)
        .toBe("2 blocks or pages don't match number");
    } finally {
      dispose();
    }
  });

  it("stays silent when nothing disagrees, and when the type is only observed", () => {
    const clean = showBadge(
      [row({ normalized_name: "cost", declared: ["number", "one"], mismatch_count: 0 })],
      "cost",
    );
    expect(clean.root.querySelector(".qb-prop-type-mismatch")).toBeNull();
    clean.dispose();

    // The registry computes `mismatch_count` against the EFFECTIVE type, which
    // without a declaration is the observed majority. "3 blocks don't match the
    // majority" is not a defect the user can act on — it is what an untyped
    // graph looks like — so it is noise, and is not shown.
    const observed = showBadge(
      [row({ normalized_name: "cost", observed_type: "number", mismatch_count: 3 })],
      "cost",
    );
    expect(observed.root.querySelector(".qb-prop-type-mismatch")).toBeNull();
    observed.dispose();
  });
});

// ---------------------------------------------------------------------------
// A3 / A4: declaring, through the ordinary page path
// ---------------------------------------------------------------------------

function emptyGraph(): void {
  setDoc({ byId: {}, pages: [], feed: [], loaded: true });
}

function existingPage(name: string, preBlock: string | null): FeedPage {
  return {
    name, kind: "page", title: name, preBlock,
    roots: [], format: "md", readOnly: false, guide: false,
  };
}

/** Drive a declaration through the component and let the save land. */
async function declare(
  rows: RegistryRow[],
  key: string,
  choose: (root: HTMLElement) => HTMLButtonElement,
  onWritten = () => {},
): Promise<void> {
  const { root, dispose } = mount(() => (
    <PropertyType propertyKey={key} rows={() => rows} onDeclarationWritten={onWritten} />
  ));
  try {
    root.querySelector<HTMLButtonElement>(".qb-prop-type-declare")!.click();
    await settle();
    choose(root).click();
    await settle();
  } finally {
    dispose();
  }
}

const optionNamed = (root: HTMLElement, label: string) =>
  [...root.querySelectorAll<HTMLButtonElement>(".qb-prop-type-option")]
    .find((b) => b.textContent?.trim() === label)!;

describe("A3: declaring writes tine.type:: on the key page, through the ordinary page path", () => {
  it("creates the page when there is none, and its first save carries tine.type:: number", async () => {
    emptyGraph();
    // No page for this key yet — the common case, since Tine never creates key
    // pages on its own (§6.3).
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    const saved = vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "rev-1" } as SavePageResult);

    await declare([row({ normalized_name: "cost" })], "cost", (root) => optionNamed(root, "number"));

    expect(doc.pages.map((p) => p.name)).toContain("cost");
    expect(readPageProperty("cost", "tine.type")).toBe("number");

    await flushPage("cost");
    // I-1/I-4: the page is created and written by the SAME save path every other
    // page uses, and what lands is an ordinary Logseq property line.
    const dto = saved.mock.calls.at(-1)![0] as PageDto;
    expect(dto.name).toBe("cost");
    expect(dto.pre_block).toContain("tine.type:: number");
  });

  it("declares on the row's normalized_name, which is the page the engine binds", async () => {
    emptyGraph();
    const getPage = vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "rev-1" } as SavePageResult);

    // The user authored `due date`; the registry's row for it is `due-date`.
    await declare([row({ normalized_name: "due-date" })], "due date", (root) =>
      optionNamed(root, "date"),
    );

    // `build_registry` binds a declaration through `refs::page_key(normalized_name)`.
    // A page literally named `due date` would never bind, and the user would be
    // left with a declaration that silently does nothing.
    expect(getPage).toHaveBeenCalledWith("due-date", "page");
    expect(doc.pages.map((p) => p.name)).toEqual(["due-date"]);
    expect(readPageProperty("due-date", "tine.type")).toBe("date");
  });

  it("writes `list of ref` when the `list of` box is ticked", async () => {
    emptyGraph();
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "rev-1" } as SavePageResult);

    const { root, dispose } = mount(() => (
      <PropertyType propertyKey="tags" rows={() => [row({ normalized_name: "tags" })]} onDeclarationWritten={() => {}} />
    ));
    try {
      root.querySelector<HTMLButtonElement>(".qb-prop-type-declare")!.click();
      await settle();
      const box = root.querySelector<HTMLInputElement>(".qb-prop-type-listof input")!;
      box.checked = true;
      box.dispatchEvent(new Event("change", { bubbles: true }));
      await settle();
      optionNamed(root, "list of ref").click();
      await settle();
    } finally {
      dispose();
    }
    expect(readPageProperty("tags", "tine.type")).toBe("list of ref");
  });

  it("removes the declaration line entirely, leaving the page", async () => {
    setDoc({
      byId: {},
      pages: [existingPage("cost", "tine.type:: number\nicon:: 💰\n")],
      feed: ["cost"],
      loaded: true,
    });
    vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "rev-2" } as SavePageResult);

    const { root, dispose } = mount(() => (
      <PropertyType
        propertyKey="cost"
        rows={() => [row({ normalized_name: "cost", declared: ["number", "one"] })]}
        onDeclarationWritten={() => {}}
      />
    ));
    try {
      root.querySelector<HTMLButtonElement>(".qb-prop-type-declare")!.click();
      await settle();
      root.querySelector<HTMLButtonElement>(".qb-prop-type-remove")!.click();
      await settle();
    } finally {
      dispose();
    }
    expect(readPageProperty("cost", "tine.type")).toBeNull();
    // Only the declaration goes. The key page is an ordinary page and may hold
    // anything else the user put on it.
    expect(readPageProperty("cost", "icon")).toBe("💰");
  });

  it("offers `remove declaration` only when there IS one", async () => {
    const undeclared = showBadge([row({ normalized_name: "cost" })], "cost");
    undeclared.root.querySelector<HTMLButtonElement>(".qb-prop-type-declare")!.click();
    await settle();
    expect(undeclared.root.querySelector(".qb-prop-type-remove")).toBeNull();
    undeclared.dispose();
  });
});

// ---------------------------------------------------------------------------
// A4: how often the registry is read
// ---------------------------------------------------------------------------

function builderPage(): FeedPage {
  return {
    name: "Sheet", kind: "page", title: "Sheet", preBlock: null,
    roots: [], format: "md", readOnly: false, guide: false,
  };
}

const EMPTY_SESSION: BuilderSession = {
  query: {
    anchor: "block",
    filter: { kind: "and", items: [] },
    diagnostics: [],
    source: { kind: "builder" },
  },
  view: {},
};

describe("A4: the registry is read on picker open and after a declaration — and nowhere else (I-13)", () => {
  it("asks once when a property picker opens, and once more when a declaration lands", async () => {
    setDoc({ byId: {}, pages: [builderPage()], feed: ["Sheet"], loaded: true });
    const rows = [row({ normalized_name: "cost", observed_type: "number" })];
    const registry = vi
      .spyOn(backend(), "queryRegistry")
      .mockImplementation(async (): Promise<RegistrySnapshot> => ({ rows, generation: 1 }));
    vi.spyOn(backend(), "queryFacets").mockResolvedValue([["cost", ["10", "20"]]]);
    vi.spyOn(backend(), "getPage").mockResolvedValue(null);
    vi.spyOn(backend(), "savePage").mockResolvedValue({ revision: "rev-1" } as SavePageResult);

    const { root, dispose } = mount(() => (
      <QueryBuilder session={() => EMPTY_SESSION} onChange={() => {}} paneDialect="tql" />
    ));
    try {
      await settle();
      // Rendering the bar asks NOTHING: the registry is a graph-level table, and
      // a query block that is merely on screen has no reason to want it.
      expect(registry).not.toHaveBeenCalled();

      root.querySelector<HTMLButtonElement>(".qb-add")!.click();
      await settle();
      [...root.querySelectorAll<HTMLButtonElement>(".qb-menu-item")]
        .find((b) => b.textContent === "Property")!
        .click();
      await settle();
      expect(registry).toHaveBeenCalledTimes(1);

      // Typing a key filters the list from the facets already in hand. It must
      // not ask again — that is the per-keystroke whole-graph question I-13
      // exists to forbid.
      const input = root.querySelector<HTMLInputElement>(".qb-value .qb-input")!;
      for (const text of ["c", "co", "cos", "cost"]) {
        input.value = text;
        input.dispatchEvent(new Event("input", { bubbles: true }));
      }
      await settle();
      expect(registry).toHaveBeenCalledTimes(1);

      [...root.querySelectorAll<HTMLButtonElement>(".qb-value .qb-menu-item")]
        .find((b) => b.textContent === "cost")!
        .click();
      await settle();
      expect(registry).toHaveBeenCalledTimes(1);
      expect(root.querySelector(".qb-prop-type-badge")!.textContent).toContain("number");

      root.querySelector<HTMLButtonElement>(".qb-prop-type-declare")!.click();
      await settle();
      optionNamed(root, "text").click();
      await settle();
      // The row the badge reads is now stale, so exactly one more read.
      expect(registry).toHaveBeenCalledTimes(2);
      expect(readPageProperty("cost", "tine.type")).toBe("text");
    } finally {
      dispose();
    }
  });
});
