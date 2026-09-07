import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { backend } from "../backend";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import { clearTransientLayersForTest } from "../transientLayers";
import {
  QueryBuilder,
  requestQueryRegistryRefresh,
  resetQueryRegistryRevisionForTests,
  type BuilderSession,
} from "./QueryBuilder";
import { propertyFilter, propertyLeafTest, taskFilter } from "../editor/queryBuilder";
import type { Filter, Query, RegistryRow, RegistrySnapshot } from "../editor/queryIr";
import { buildVocabulary, novelKeyEntries } from "./QueryVocabularyPicker";
import { stubVocabularyGeometry } from "./QueryVocabularyPicker.test-helpers";

// **The one vocabulary picker, at the real mount** (SPEC §7.5, §6.4, I-13, I-22).
//
// The list is registry-backed, sectioned and VIRTUALIZED, so the things worth
// pinning are the ones a "does it render" test cannot see: that a key used once
// in a graph of hundreds is still REACHABLE, that its count and type are the
// registry's own and not invented, that the keyboard can walk to a row the
// scroll has not reached, and that none of it asks the graph anything per
// keystroke.
//
// The virtualizer measures a scroll element, and jsdom has no layout, so the
// picker's viewport geometry is stubbed by `QueryVocabularyPicker.test-helpers`
// (N2). Nothing in production branches on being under test; what the rows look
// like is proved by `scripts/shot-query-vocabulary.mjs`.

// `import.meta.url` is not a file URL under the jsdom runner, so the golden
// fixtures are read relative to the repository root vitest already runs in.
const FIXTURE_DIR = join(process.cwd(), "crates/tine-core/tests/fixtures/query-ir");
const fixture = <T,>(name: string): T =>
  JSON.parse(readFileSync(join(FIXTURE_DIR, `${name}.json`), "utf8"));

/** The golden registry snapshot Rust round-trips — `status` (42 blocks, 3
 *  pages, top values) and `size` (9 blocks, declared `number many`). Using the
 *  shared fixture means a registry field the engine changes shape on cannot
 *  drift silently into a hand-written double here. */
const GOLDEN: RegistrySnapshot = fixture("registry_snapshot");

/** A graph with `count` ordinary keys plus one rare key nobody would scroll to.
 *  Counts descend with the index, so `rare` sorts last in the list. */
function largeSnapshot(count: number): RegistrySnapshot {
  const rows: RegistryRow[] = [];
  for (let i = 0; i < count; i += 1) {
    rows.push({
      normalized_name: `bulk-${String(i).padStart(3, "0")}`,
      cardinality: "one",
      observed_type: "text",
      count_blocks: 1000 - i,
      count_pages: 0,
      mismatch_count: 0,
    });
  }
  rows.push({
    normalized_name: "eigenvalue",
    cardinality: "one",
    observed_type: "number",
    count_blocks: 1,
    count_pages: 0,
    mismatch_count: 0,
    top_values: [["1.61", 1]],
  });
  return { generation: 3, rows };
}

function session(filter: Filter, anchor: "block" | "page" = "block"): BuilderSession {
  return { query: { anchor, filter, source: { kind: "builder" } }, view: {} };
}

function mountBuilder(initial: BuilderSession) {
  const host = document.createElement("div");
  document.body.append(host);
  const [current, setCurrent] = createSignal<BuilderSession>(initial);
  const dispose = render(() => <QueryBuilder session={current} onChange={setCurrent} />, host);
  const open = (): HTMLElement => {
    const before = new Set(document.querySelectorAll<HTMLElement>(".qs-sheet"));
    host.querySelector<HTMLButtonElement>(".qs-gear")!.click();
    const sheet = [...document.querySelectorAll<HTMLElement>(".qs-sheet")].find(
      (el) => !before.has(el),
    );
    if (!sheet) throw new Error("the sheet did not open");
    return sheet;
  };
  return { host, open, session: current, dispose };
}

const settle = async () => {
  for (let i = 0; i < 8; i += 1) await new Promise((resolve) => setTimeout(resolve, 0));
};

/** Open the add-condition picker inside an already-open sheet. */
function openPicker(sheet: HTMLElement): HTMLElement {
  sheet.querySelector<HTMLButtonElement>(".qs-add")!.click();
  const menu = document.querySelector<HTMLElement>(".qs-menu.qs-vocab");
  if (!menu) throw new Error("the vocabulary picker did not open");
  return menu;
}

const filterInput = (menu: HTMLElement) => menu.querySelector<HTMLInputElement>(".qs-menu-filter")!;
const type = (menu: HTMLElement, needle: string) => {
  const input = filterInput(menu);
  input.value = needle;
  input.dispatchEvent(new Event("input", { bubbles: true }));
};
const optionFor = (menu: HTMLElement, key: string) =>
  menu.querySelector<HTMLButtonElement>(`.qs-vocab-option[data-vocabulary-key="${key}"]`);
const activeOption = (menu: HTMLElement) => {
  const id = filterInput(menu).getAttribute("aria-activedescendant");
  return id ? document.getElementById(id) : null;
};
const press = (menu: HTMLElement, key: string) =>
  menu.dispatchEvent(new KeyboardEvent("keydown", { key, bubbles: true }));

let restoreGeometry: (() => void) | null = null;

beforeEach(() => {
  restoreGeometry = stubVocabularyGeometry();
  resetQueryRegistryRevisionForTests();
  vi.spyOn(backend(), "queryFacets").mockResolvedValue([]);
  vi.spyOn(backend(), "printQuery").mockResolvedValue("@block and task is TODO");
});

afterEach(() => {
  restoreGeometry?.();
  restoreGeometry = null;
  clearTransientLayersForTest();
  resetSharedQueryResultsForTests();
  resetQueryRegistryRevisionForTests();
  vi.restoreAllMocks();
  document.body.replaceChildren();
});

describe("the vocabulary list is the registry's, ordered and scoped", () => {
  // The list opens on the key the graph actually uses. Alphabetical order — what
  // `query_facets` gave — put a key written once above one written four hundred
  // times, which is the ordering this packet exists to replace.
  it("orders properties by descending count with a normalized-name tie-break", () => {
    const rows: RegistryRow[] = [
      { normalized_name: "zeta", cardinality: "one", observed_type: "text", count_blocks: 7, count_pages: 0, mismatch_count: 0 },
      { normalized_name: "alpha", cardinality: "one", observed_type: "text", count_blocks: 7, count_pages: 0, mismatch_count: 0 },
      { normalized_name: "mid", cardinality: "one", observed_type: "text", count_blocks: 90, count_pages: 0, mismatch_count: 0 },
    ];
    const properties = buildVocabulary({ rows, anchor: "block" }).filter(
      (entry) => entry.section === "property",
    );
    expect(properties.map((entry) => entry.label)).toEqual(["mid", "alpha", "zeta"]);
    expect(properties.map((entry) => entry.count)).toEqual([90, 7, 7]);
  });

  // F5. A `@block` query counts BLOCKS for its own properties; the same key read
  // through the owning page is a different question with a different count, and
  // it is only offered on a block anchor.
  it("scopes counts and `throughPage` to the anchor", () => {
    const block = buildVocabulary({ rows: GOLDEN.rows, anchor: "block" });
    const own = block.find((entry) => entry.section === "property" && entry.label === "status")!;
    expect(own.count).toBe(42);
    expect(own.unit).toBe("blocks");
    expect(own.choice).toEqual({ kind: "property", key: "status", throughPage: false });

    const throughPage = block.find((entry) => entry.section === "page" && entry.label === "status")!;
    expect(throughPage.count).toBe(3);
    expect(throughPage.unit).toBe("pages");
    expect(throughPage.choice).toEqual({ kind: "property", key: "status", throughPage: true });

    const page = buildVocabulary({ rows: GOLDEN.rows, anchor: "page" });
    expect(page.find((entry) => entry.label === "status")!.count).toBe(3);
    expect(page.filter((entry) => entry.section === "page" && entry.choice.kind === "property")).toEqual([]);
  });

  // A count is authoritative or it is absent. A built-in has no registry
  // statistics; borrowing a same-named property's would be a number the user
  // could act on and the engine never said.
  it("never fabricates a count for a built-in", () => {
    const rows: RegistryRow[] = [
      { normalized_name: "task", cardinality: "one", observed_type: "text", count_blocks: 400, count_pages: 0, mismatch_count: 0 },
    ];
    const entries = buildVocabulary({ rows, anchor: "block" });
    for (const entry of entries.filter((e) => e.choice.kind === "builtin")) {
      expect(entry.count).toBeUndefined();
    }
    expect(entries.find((e) => e.section === "property" && e.label === "task")!.count).toBe(400);
  });

  // Observation and coercion are different things (§6.3), so both are labelled
  // and neither is a bare badge.
  it("labels the observed type and, separately, a declaration", () => {
    const entries = buildVocabulary({ rows: GOLDEN.rows, anchor: "block" });
    const size = entries.find((entry) => entry.section === "property" && entry.label === "size")!;
    expect(size.observed).toBe("list of number");
    expect(size.declared).toBe("list of number");
    const status = entries.find((entry) => entry.section === "property" && entry.label === "status")!;
    expect(status.observed).toBe("text");
    expect(status.declared).toBeUndefined();
    expect(status.preview).toBe("active · done");
  });

  it("offers an unmatched key honestly, and never beside a scope the graph has", () => {
    const [own, page] = novelKeyEntries(GOLDEN.rows, "budget", "block");
    expect(own.label).toBe('Use "budget" as a block property');
    expect(own.count).toBe(0);
    expect(own.unit).toBe("blocks");
    expect(own.choice).toEqual({ kind: "property", key: "budget", throughPage: false });
    expect(page.label).toBe('Use "budget" as a page property');
    expect(page.choice).toEqual({ kind: "property", key: "budget", throughPage: true });
    // `registryRowFor` is the existing matcher, so a spelling the engine
    // normalizes onto an existing row is NOT a new key — in either scope, since
    // `status` is written on blocks AND on pages in the golden snapshot.
    expect(novelKeyEntries(GOLDEN.rows, "status", "block")).toEqual([]);
    expect(novelKeyEntries(GOLDEN.rows, "  ", "block")).toEqual([]);
  });
});

describe("the picker at the sheet's own mount", () => {
  it("reveals a rare key by substring, with its registry count and type", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(largeSnapshot(400));
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      // 400 keys: the rare one is nowhere near the mounted window.
      expect(optionFor(menu, "eigenvalue")).toBeNull();

      type(menu, "eigen");
      const row = optionFor(menu, "eigenvalue")!;
      expect(row).toBeTruthy();
      expect(row.textContent).toContain("eigenvalue");
      expect(row.textContent).toContain("observed number");
      expect(row.textContent).toContain("1 block");
      expect(row.textContent).toContain("1.61");
      // The needle matched the KEY, not a label the frontend spelled.
      expect(menu.querySelectorAll(".qs-vocab-option").length).toBeLessThan(20);
    } finally {
      builder.dispose();
    }
  });

  // I-22 with a keyboard: windowing that cannot be walked is a list with an
  // unreachable tail. `aria-activedescendant` must name a MOUNTED option at
  // every step, which is why the active row is mounted even before the scroll
  // reaches it.
  it("walks the keyboard to a distant entry and keeps aria-activedescendant mounted", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(largeSnapshot(120));
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      const first = activeOption(menu);
      expect(first).toBeTruthy();
      expect(first!.isConnected).toBe(true);

      const seen = new Set<string>();
      for (let step = 0; step < 40; step += 1) {
        press(menu, "ArrowDown");
        const active = activeOption(menu);
        expect(active, `step ${step} left aria-activedescendant unmounted`).toBeTruthy();
        expect(active!.isConnected).toBe(true);
        expect(active!.getAttribute("aria-selected")).toBe("true");
        seen.add(active!.getAttribute("data-vocabulary-key") ?? "");
      }
      // It moved through rows, and it moved past the seven built-ins into the
      // registry's own keys.
      expect(seen.size).toBeGreaterThan(20);
      expect([...seen].some((key) => key.startsWith("bulk-"))).toBe(true);
    } finally {
      builder.dispose();
    }
  });

  // Found by a native journey that asked for a built-in BY NAME and got
  // nothing: this attribute answered `builtin:content` for one kind of row and
  // `status` for the other, so half the vocabulary was unaddressable through
  // the one hook that exists for naming a row.
  it("names every row by the same rule, built-in or property", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      const content = optionFor(menu, "content");
      expect(content, "a built-in is addressable by its own name").toBeTruthy();
      expect(content!.getAttribute("data-section")).toBe("builtin");
      expect(optionFor(menu, "builtin:content")).toBeNull();
      expect(optionFor(menu, "status")?.getAttribute("data-section")).toBe("property");
    } finally {
      builder.dispose();
    }
  });

  it("picks a novel key and writes the user's own spelling into the IR", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      type(menu, "café-size");
      const novel = novelOption(menu, "café-size", false)!;
      expect(novel.textContent).toContain('Use "café-size" as a block property');
      expect(novel.textContent).toContain("0 blocks today");
      // …and the same typed key is offered as the owning PAGE's property, which
      // is the second field kind the two-stage chooser used to expose.
      expect(novelOption(menu, "café-size", true)!.textContent).toContain("0 pages today");
    } finally {
      builder.dispose();
    }
  });

  // I-13. The registry is read once when the sheet opens. Typing in the picker
  // filters a list the host already holds; it is not a query.
  it("asks the graph nothing per keystroke", async () => {
    const registry = vi.spyOn(backend(), "queryRegistry").mockResolvedValue(largeSnapshot(200));
    const facets = vi.spyOn(backend(), "queryFacets").mockResolvedValue([]);
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      const registryCalls = registry.mock.calls.length;
      const facetCalls = facets.mock.calls.length;
      expect(registryCalls).toBe(1);

      for (const needle of ["b", "bu", "bul", "bulk", "bulk-0", "bulk-01"]) type(menu, needle);
      await settle();
      expect(registry.mock.calls.length).toBe(registryCalls);
      expect(facets.mock.calls.length).toBe(facetCalls);
    } finally {
      builder.dispose();
    }
  });

  // §7.5's anti-Jira property, at the REAL mount: "every query the parser
  // accepts renders in the builder, invalid ones included". The golden wire
  // fixtures are the widest IR the two sides agree on, so mounting each of them
  // — with the golden registry behind the picker — is the strongest statement
  // available that no accepted query can reach a sheet that throws or blanks.
  it("mounts every golden query fixture, opens its sheet and its picker, and draws rows", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    for (const name of ["query", "query_page_anchor"]) {
      const query = fixture<Query>(name);
      const builder = mountBuilder({ query, view: {} });
      try {
        expect(builder.host.querySelector(".qs-sentence")).toBeTruthy();
        const sheet = builder.open();
        await settle();
        expect(sheet.querySelectorAll(".qs-row").length).toBeGreaterThan(0);

        const menu = openPicker(sheet);
        expect(menu.querySelectorAll(".qs-vocab-option").length).toBeGreaterThan(0);
        // The registry's keys are offered under the anchor the fixture declares.
        type(menu, "status");
        expect(optionFor(menu, "status"), `${name} hid the registry's own key`).toBeTruthy();
      } finally {
        builder.dispose();
        document.body.replaceChildren();
      }
    }
  });

  // Changing a row's field and adding a condition were two controls asking one
  // question. They are one control now, and the row's current field is MARKED in
  // it — a picker that does not say where you are is a picker you can only add
  // with.
  it("uses the same picker to change a row's field, marking the current one", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    const builder = mountBuilder(session(propertyFilter("status", "active")));
    try {
      const sheet = builder.open();
      await settle();
      sheet.querySelector<HTMLButtonElement>(".qs-row .qs-field")!.click();
      const menu = document.querySelector<HTMLElement>(".qs-menu.qs-vocab")!;
      expect(menu).toBeTruthy();
      const current = menu.querySelector<HTMLElement>(".qs-vocab-option.current");
      expect(current?.getAttribute("data-vocabulary-key")).toBe("status");
      expect(current?.getAttribute("data-section")).toBe("property");

      type(menu, "size");
      optionFor(menu, "size")!.click();
      const filter = builder.session().query.filter;
      expect(JSON.stringify(filter)).toContain("size");
      expect(JSON.stringify(filter)).not.toContain('"status"');
    } finally {
      builder.dispose();
    }
  });
});

// ---------------------------------------------------------------------------
// Correction pass (manager review, 2026-09-07)
// ---------------------------------------------------------------------------

/** The last leaf the builder committed, unwrapped from the `and` root. */
function lastLeaf(current: BuilderSession): Filter {
  const filter = current.query.filter;
  return filter.kind === "and" && filter.items.length
    ? filter.items[filter.items.length - 1]
    : filter;
}

const novelOption = (menu: HTMLElement, key: string, throughPage: boolean) =>
  menu.querySelector<HTMLButtonElement>(
    `.qs-vocab-option[data-section="novel"][data-vocabulary-key="${key}"][data-through-page="${throughPage}"]`,
  );

describe("a key stays authorable in BOTH scopes", () => {
  /** `owner` is written on pages only — the shape that became unreachable as a
   *  block property when the two-stage chooser was replaced. */
  const PAGE_ONLY: RegistryRow[] = [
    { normalized_name: "owner", cardinality: "one", observed_type: "text", count_blocks: 0, count_pages: 12, mismatch_count: 0 },
  ];

  it("offers the block scope for a key the graph has only on pages", () => {
    const offers = novelKeyEntries(PAGE_ONLY, "owner", "block");
    const own = offers.find((entry) => entry.choice.kind === "property" && !entry.choice.throughPage)!;
    expect(own, "a page-only key was unauthorable as a block property").toBeTruthy();
    expect(own.choice).toEqual({ kind: "property", key: "owner", throughPage: false });
    expect(own.count).toBe(0);
    expect(own.unit).toBe("blocks");
    // The page scope already lists it, so it is not offered a second time.
    expect(offers.some((entry) => entry.choice.kind === "property" && entry.choice.throughPage)).toBe(false);
  });

  it("offers both scopes for a key the graph does not have at all", () => {
    const offers = novelKeyEntries(GOLDEN.rows, "budget", "block");
    expect(offers.map((entry) => (entry.choice as { throughPage: boolean }).throughPage)).toEqual([false, true]);
    expect(offers.map((entry) => entry.count)).toEqual([0, 0]);
    expect(offers.map((entry) => entry.unit)).toEqual(["blocks", "pages"]);
  });

  it("offers exactly one own choice on a page anchor", () => {
    const offers = novelKeyEntries(GOLDEN.rows, "budget", "page");
    expect(offers).toHaveLength(1);
    expect(offers[0].choice).toEqual({ kind: "property", key: "budget", throughPage: false });
    expect(offers[0].unit).toBe("pages");
    // `status` is already a page property, so there is nothing left to offer.
    expect(novelKeyEntries(GOLDEN.rows, "status", "page")).toEqual([]);
  });

  it("claims no count at all while the registry has not arrived", () => {
    const offers = novelKeyEntries(undefined, "budget", "block");
    expect(offers.length).toBeGreaterThan(0);
    for (const offer of offers) {
      expect(offer.count, "a loading registry is not a known-empty graph").toBeUndefined();
    }
  });

  it("adds a page-property condition for a typed key, through encodePropertyLeaf", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      type(menu, "budget");
      const offer = novelOption(menu, "budget", true);
      expect(offer, "no page-property offer for a typed key").toBeTruthy();
      offer!.click();
      const editor = document.querySelector<HTMLElement>(".qs-value-editor")!;
      expect(editor.textContent).toContain("budget");
      const input = editor.querySelector<HTMLInputElement>("input.qs-input")!;
      input.value = "high";
      input.dispatchEvent(new Event("input", { bubbles: true }));
      editor.querySelector<HTMLButtonElement>(".qs-commit")!.click();
      const test = propertyLeafTest(lastLeaf(builder.session()))!;
      expect(test.key).toBe("budget");
      expect(test.throughPage).toBe(true);
      expect(test.values).toEqual(["high"]);
    } finally {
      builder.dispose();
    }
  });

  it("changes a row's field to a page property for a typed key", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue(GOLDEN);
    const builder = mountBuilder(session(propertyFilter("status", "active")));
    try {
      const sheet = builder.open();
      await settle();
      sheet.querySelector<HTMLButtonElement>(".qs-row .qs-field")!.click();
      const menu = document.querySelector<HTMLElement>(".qs-menu.qs-vocab")!;
      type(menu, "budget");
      const offer = novelOption(menu, "budget", true);
      expect(offer, "the field menu offered no page scope for a typed key").toBeTruthy();
      offer!.click();
      const test = propertyLeafTest(lastLeaf(builder.session()))!;
      expect(test.key).toBe("budget");
      expect(test.throughPage).toBe(true);
    } finally {
      builder.dispose();
    }
  });

  it("re-authors a page-only key as a block property from the field menu", async () => {
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue({ generation: 1, rows: PAGE_ONLY });
    const builder = mountBuilder(session(propertyFilter("status", "active")));
    try {
      const sheet = builder.open();
      await settle();
      sheet.querySelector<HTMLButtonElement>(".qs-row .qs-field")!.click();
      const menu = document.querySelector<HTMLElement>(".qs-menu.qs-vocab")!;
      type(menu, "owner");
      const offer = novelOption(menu, "owner", false);
      expect(offer, "a page-only key could not be used as a block property").toBeTruthy();
      expect(offer!.textContent).toContain("0 blocks");
      offer!.click();
      const test = propertyLeafTest(lastLeaf(builder.session()))!;
      expect(test.key).toBe("owner");
      expect(test.throughPage).toBe(false);
    } finally {
      builder.dispose();
    }
  });
});

describe("option ids are single valid ID references", () => {
  /** Keys a person can really write into a graph: spaces, a tab, quotes, `#`,
   *  `.`, non-ASCII, an emoji (a surrogate PAIR) and a LONE surrogate. */
  const HOSTILE = [
    "due date",
    "due  date",
    "a\tb",
    'quote"key',
    "a#b.c",
    "ünïcode ключ",
    "emoji \u{1f600} key",
    "\ud800lone",
  ];

  it("keeps aria-activedescendant a mounted, whitespace-free, unique IDREF", async () => {
    const rows: RegistryRow[] = HOSTILE.map((name, index) => ({
      normalized_name: name,
      cardinality: "one",
      observed_type: "text",
      count_blocks: 100 - index,
      count_pages: 0,
      mismatch_count: 0,
    }));
    vi.spyOn(backend(), "queryRegistry").mockResolvedValue({ generation: 1, rows });
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      const ids = new Map<string, string>();
      for (let step = 0; step < 30; step += 1) {
        press(menu, "ArrowDown");
        const id = filterInput(menu).getAttribute("aria-activedescendant");
        expect(id, `step ${step} had no active descendant`).toBeTruthy();
        // An IDREF is ONE id. Whitespace makes it a list, and a list is not
        // what `aria-activedescendant` accepts.
        expect(id!, `step ${step}: "${id}" is not a single ID reference`).toMatch(/^\S+$/);
        const element = document.getElementById(id!);
        expect(element, `step ${step}: ${id} names no mounted element`).toBeTruthy();
        const key = element!.getAttribute("data-vocabulary-key") ?? "";
        const section = element!.getAttribute("data-section") ?? "";
        const seen = ids.get(id!);
        if (seen !== undefined) expect(seen).toBe(`${section} ${key}`);
        ids.set(id!, `${section} ${key}`);
      }
      // Distinct rows keep distinct ids: identity is the key, never the index.
      expect(new Set(ids.values()).size).toBe(ids.size);
      expect(ids.size).toBeGreaterThan(5);
    } finally {
      builder.dispose();
    }
  });
});

describe("a pending registry is not a known-empty graph", () => {
  it("shows a compact loading line, keeps the built-ins, and resolves into the graph's keys", async () => {
    let land: (snapshot: RegistrySnapshot) => void = () => {};
    vi.spyOn(backend(), "queryRegistry").mockReturnValue(
      new Promise<RegistrySnapshot>((resolve) => {
        land = resolve;
      }),
    );
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      const menu = openPicker(sheet);
      expect(menu.querySelector(".qs-vocab-pending"), "no loading indication").toBeTruthy();
      // Built-ins need no registry, so they stay usable while it is in flight.
      expect(optionFor(menu, "content")).toBeTruthy();
      // …and nothing claims the graph has no properties.
      expect(menu.querySelector('.qs-vocab-option[data-section="property"]')).toBeNull();

      // A key typed WHILE the read is in flight is still offered in both
      // scopes — nobody yet knows which the graph has — and neither row claims
      // a count, because 0 would be a statement about a graph not yet read.
      type(menu, "status");
      const own = novelOption(menu, "status", false);
      const page = novelOption(menu, "status", true);
      expect(own, "a typed key was unauthorable while the registry loaded").toBeTruthy();
      expect(page).toBeTruthy();
      expect(own!.textContent).not.toMatch(/\d+ (blocks?|pages?)/);
      expect(page!.textContent).not.toMatch(/\d+ (blocks?|pages?)/);
      type(menu, "");

      land(GOLDEN);
      await settle();
      expect(menu.querySelector(".qs-vocab-pending")).toBeNull();
      expect(optionFor(menu, "status")).toBeTruthy();
    } finally {
      builder.dispose();
    }
  });

  it("suspends a property row's operator and value while a declaration refresh is in flight", async () => {
    const snapshots: ((snapshot: RegistrySnapshot) => void)[] = [];
    vi.spyOn(backend(), "queryRegistry").mockImplementation(
      () => new Promise<RegistrySnapshot>((resolve) => snapshots.push(resolve)),
    );
    const builder = mountBuilder(session(propertyFilter("size", "3")));
    try {
      const sheet = builder.open();
      await settle();
      snapshots[0](GOLDEN);
      await settle();
      const op = () => sheet.querySelector<HTMLButtonElement>(".qs-row .qs-op")!;
      const value = () => sheet.querySelector<HTMLInputElement>(".qs-property-value input.qs-input")!;
      expect(op().disabled).toBe(false);
      expect(value().disabled).toBe(false);

      // A declaration lands: the rows the operators were computed from are now
      // stale, and the new ones have not arrived.
      requestQueryRegistryRefresh();
      await settle();
      expect(op().disabled, "the operator menu used a stale declaration").toBe(true);
      expect(value().disabled, "the value editor used a stale declaration").toBe(true);
      // The draft the user can see is still the row's own.
      expect(value().value).toBe("3");

      snapshots[1](GOLDEN);
      await settle();
      expect(op().disabled).toBe(false);
      expect(value().disabled).toBe(false);
    } finally {
      builder.dispose();
    }
  });

  it("keeps the chosen key and the typed draft while the new registry is awaited", async () => {
    const snapshots: ((snapshot: RegistrySnapshot) => void)[] = [];
    vi.spyOn(backend(), "queryRegistry").mockImplementation(
      () => new Promise<RegistrySnapshot>((resolve) => snapshots.push(resolve)),
    );
    const builder = mountBuilder(session(taskFilter(["TODO"])));
    try {
      const sheet = builder.open();
      await settle();
      snapshots[0](GOLDEN);
      await settle();
      const menu = openPicker(sheet);
      optionFor(menu, "status")!.click();
      const editor = () => document.querySelector<HTMLElement>(".qs-value-editor")!;
      const input = editor().querySelector<HTMLInputElement>("input.qs-input")!;
      input.value = "active";
      input.dispatchEvent(new Event("input", { bubbles: true }));

      requestQueryRegistryRefresh();
      await settle();
      expect(editor().textContent).toContain("status");
      expect(editor().querySelector(".qs-registry-pending"), "no pending note").toBeTruthy();
      const commit = editor().querySelector<HTMLButtonElement>(".qs-commit")!;
      expect(commit.disabled, "committed against a registry that had not landed").toBe(true);
      commit.click();
      expect(builder.session().query.filter).toEqual(taskFilter(["TODO"]));

      snapshots[1](GOLDEN);
      await settle();
      const resumed = editor().querySelector<HTMLButtonElement>(".qs-commit")!;
      expect(resumed.disabled).toBe(false);
      expect(editor().querySelector<HTMLInputElement>("input.qs-input")!.value).toBe("active");
      resumed.click();
      const test = propertyLeafTest(lastLeaf(builder.session()))!;
      expect(test.key).toBe("status");
      expect(test.values).toEqual(["active"]);
    } finally {
      builder.dispose();
    }
  });
});
