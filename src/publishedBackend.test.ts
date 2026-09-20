import { describe, expect, it, vi } from "vitest";

const searchBridge = vi.hoisted(() => {
  const foldText = (text: string) => text
    .toLowerCase()
    .normalize("NFKD")
    .replace(/\p{Mn}/gu, "")
    .normalize("NFC");
  const searchFold = vi.fn(foldText);
  const searchFoldMap = vi.fn((text: string) => {
    let offset = 0;
    let folded = "";
    const sources: { start: number; end: number }[] = [];
    for (const scalar of text) {
      const start = offset;
      offset += scalar.length;
      const part = foldText(scalar);
      folded += part;
      for (const _output of part) sources.push({ start, end: offset });
    }
    return { text: folded, sources };
  });
  const searchMatchBatch = vi.fn((_query: string, texts: string[]): { matches: boolean[]; search_error: string | null } => ({
    matches: texts.map(() => true),
    search_error: null,
  }));
  return { searchFold, searchFoldMap, searchMatchBatch };
});

vi.mock("./render/parse", () => searchBridge);
import {
  PUBLISHED_QUERY_REASON,
  publishedBackend,
  publishedSnapshotUrl,
  stableJson,
  validateSnapshot,
  viewKey,
  type PublishedSnapshot,
} from "./publishedBackend";
import { PublishedExportReadOnlyError, QueryUnavailableError } from "./backend";
import { queryParsedDisplaySettings } from "./editor/queryDisplayDraft";
import type { ParsedQuery, QueryResult } from "./editor/queryIr";

// The snapshot backend answers exactly what the engine baked (spec §5.2): a
// query is looked up by the same key Macro.tsx sends, never re-run.

const parsed = (marker: string): ParsedQuery =>
  ({ query: { kind: "fixture", marker, nested: { b: 1, a: 2 } }, view: { kind: "list" } }) as unknown as ParsedQuery;

const result = (label: string): QueryResult =>
  ({ anchor: "block", groups: [{ page: label, kind: "page", blocks: [] }], total: 1, matched_total: 1 }) as unknown as QueryResult;

function fixture(): PublishedSnapshot {
  return {
    schema: 1,
    name: "Open tasks",
    exported_at: "2026-09-14T10:00:00Z",
    home: "Open tasks (export)",
    pages: [
      {
        name: "Open tasks (export)",
        kind: "page",
        title: "Open tasks (export)",
        pre_block: null,
        blocks: [{ id: "h1", raw: "{{query (task TODO)}}", collapsed: false, children: [] }],
        rev: null,
        format: "md",
        read_only: true,
        path: "",
      },
      {
        name: "Dashboard",
        kind: "page",
        title: "Dashboard",
        pre_block: null,
        blocks: [
          {
            id: "d1",
            raw: "{{query (and (task TODO) <% current page %>)}}\nid:: 11111111-1111-1111-1111-111111111111",
            collapsed: false,
            children: [{ id: "d2", raw: "child ((22222222-2222-2222-2222-222222222222))", collapsed: false, children: [] }],
          },
        ],
        rev: null,
        format: "md",
        read_only: true,
        path: "pages/Dashboard.md",
      },
      {
        name: "Sep 13th, 2026",
        kind: "journal",
        title: "Sep 13th, 2026",
        pre_block: null,
        blocks: [{ id: "j1", raw: "TODO call [[Dashboard]]", collapsed: false, children: [] }],
        rev: null,
        format: "md",
        read_only: true,
        path: "journals/2026_09_13.md",
      },
    ],
    entries: [
      { name: "Open tasks (export)", kind: "page", date_key: null, path: "" },
      { name: "Dashboard", kind: "page", date_key: null, path: "pages/Dashboard.md" },
      { name: "Sep 13th, 2026", kind: "journal", date_key: 20260913, path: "journals/2026_09_13.md" },
    ],
    backlinks: {
      Dashboard: [{ page: "Sep 13th, 2026", kind: "journal", path: "journals/2026_09_13.md", blocks: [{ id: "j1", raw: "TODO call [[Dashboard]]", collapsed: false, children: [] }] }],
    },
    block_ref_counts: { "11111111-1111-1111-1111-111111111111": 1 },
    aliases: [["dash", "Dashboard"]],
    icons: { Dashboard: "📊" },
    queries: [
      {
        host: "Open tasks (export)",
        argument: "(task TODO)",
        dialect: "macro_query",
        properties: [],
        parsed: parsed("home"),
        execution: null,
        context: { current_page: "Open tasks (export)" },
        executed_context: { current_page: "Open tasks (export)" },
        view: { kind: "list" } as never,
        result: result("home-rows"),
      },
      {
        host: "Dashboard",
        argument: "(and (task TODO) <% current page %>)",
        dialect: "macro_query",
        properties: [["tine.view", "table"]],
        parsed: parsed("dashboard-authored"),
        execution: { argument: "(and (task TODO) [[Dashboard]])", parsed: parsed("dashboard-executed") },
        context: { current_page: "Dashboard" },
        executed_context: { current_page: "Dashboard" },
        view: { kind: "table" } as never,
        result: result("dashboard-rows"),
      },
    ],
  } as unknown as PublishedSnapshot;
}

const load = () => Promise.resolve(fixture());

describe("published backend: the two query seams", () => {
  it("parseQuery matches the authored argument, the substituted execution text, and the host properties", async () => {
    const backend = publishedBackend(load);
    expect(await backend.parseQuery("(task TODO)", "macro_query", [])).toEqual(parsed("home"));
    expect(await backend.parseQuery("(and (task TODO) <% current page %>)", "macro_query", [["tine.view", "table"]])).toEqual(
      parsed("dashboard-authored"),
    );
    expect(await backend.parseQuery("(and (task TODO) [[Dashboard]])", "macro_query", [["TINE.VIEW", " table "]])).toEqual(
      parsed("dashboard-executed"),
    );
  });

  it("parseQuery refuses a query the export was not made with, by reason code", async () => {
    const backend = publishedBackend(load);
    const miss = backend.parseQuery("(task DOING)", "macro_query", []);
    await expect(miss).rejects.toBeInstanceOf(QueryUnavailableError);
    await expect(miss).rejects.toMatchObject({ reasonCode: PUBLISHED_QUERY_REASON });
    // Same text, other dialect: a different query.
    await expect(backend.parseQuery("(task TODO)", "macro_tql", [])).rejects.toBeInstanceOf(QueryUnavailableError);
    // Same text, other properties: a different view, baked separately.
    await expect(backend.parseQuery("(task TODO)", "macro_query", [["tine.view", "table"]])).rejects.toBeInstanceOf(
      QueryUnavailableError,
    );
  });

  it("queryRun matches the IR by stable key and the context by current page", async () => {
    const backend = publishedBackend(load);
    const reordered = { nested: { a: 2, b: 1 }, marker: "dashboard-executed", kind: "fixture" } as never;
    expect(await backend.queryRun(reordered, {} as never, { current_page: "Dashboard" })).toEqual(result("dashboard-rows"));
    expect(await backend.queryRun(parsed("dashboard-authored").query, {} as never, { current_page: "Dashboard" })).toEqual(
      result("dashboard-rows"),
    );
    expect(await backend.queryRun(parsed("home").query, {} as never, { current_page: "Open tasks (export)" })).toEqual(
      result("home-rows"),
    );
    // The same IR under another page is another execution the export lacks.
    await expect(backend.queryRun(parsed("home").query, {} as never, { current_page: "Dashboard" })).rejects.toMatchObject({
      reasonCode: PUBLISHED_QUERY_REASON,
    });
    // A run asked with no page at all (a query in a sheet cell carries no host
    // page) gets the one record of that query; an unknown IR is still refused.
    expect(await backend.queryRun(parsed("home").query, {} as never)).toEqual(result("home-rows"));
    await expect(backend.queryRun({ kind: "fixture", marker: "never baked" } as never, {} as never)).rejects.toBeInstanceOf(
      QueryUnavailableError,
    );
  });

  it("queryRun tells two identical queries on one page apart by the view they were baked under", async () => {
    const two = fixture();
    const plain = two.queries[1];
    two.queries.push(
      { ...plain, properties: [["tine.sample", "1"]], view: { view: "table", sample: 1 } as never, result: result("sampled") },
      { ...plain, properties: [["tine.sample", "3"]], view: { view: "table", sample: 3 } as never, result: result("three") },
    );
    const backend = publishedBackend(() => Promise.resolve(two));
    const query = parsed("dashboard-executed").query;
    expect(await backend.queryRun(query, { sample: 1, view: "table" } as never, { current_page: "Dashboard" })).toEqual(result("sampled"));
    expect(await backend.queryRun(query, { view: "table", sample: 3 } as never, { current_page: "Dashboard" })).toEqual(result("three"));
    // A view the export never ran under still gets this page's baked answer
    // (a later view change is refused; the export shows what it baked).
    expect(await backend.queryRun(query, { view: "board" } as never, { current_page: "Dashboard" })).toEqual(result("dashboard-rows"));
  });

  it("matches a view the engine wrote densely against the sparse view the app resolves for it", async () => {
    // The producer (`anchored_view` in Rust) serializes `ViewSettings` with
    // its list fields always present; the consumer (`queryParsedDisplaySettings`
    // → `queryDisplaySettings`) omits every field a scoped draft did not state.
    // The two must select the same record, or a scoped `tine.block-sample::`
    // twin would silently answer with its unsampled sibling's rows.
    const two = fixture();
    const plain = two.queries[1];
    const parsedScoped = {
      ...parsed("dashboard-executed"),
      view: { view: "list", sort: [], columns: [], aggregates: [] },
      block_display: { sample: 2 },
    };
    two.queries.push({
      ...plain,
      parsed: parsedScoped as never,
      properties: [["tine.block-sample", "2"]],
      view: { view: "list", sort: [], columns: [], aggregates: [], sample: 2 } as never,
      result: result("two-of-four"),
    });
    const backend = publishedBackend(() => Promise.resolve(two));
    const asked = queryParsedDisplaySettings(parsedScoped as never, "block");
    expect(asked).toEqual({ view: "list", sample: 2 });
    expect(await backend.queryRun(parsed("dashboard-executed").query, asked, { current_page: "Dashboard" })).toEqual(result("two-of-four"));
    expect(viewKey({ view: "list", sort: [], columns: [], aggregates: [], sample: 2 })).toBe(viewKey({ sample: 2, view: "list" }));
    expect(viewKey({ view: "list", columns: ["page"] })).not.toBe(viewKey({ view: "list" }));
  });

  it("answers only the assets it copied: a path that leaves assets/ is refused, not fetched", async () => {
    const backend = publishedBackend(load);
    const fetched: string[] = [];
    const realFetch = globalThis.fetch;
    globalThis.fetch = (async (url: string) => {
      fetched.push(String(url));
      return { ok: true, arrayBuffer: async () => new Uint8Array([1]).buffer };
    }) as unknown as typeof fetch;
    try {
      expect(await backend.streamAsset("talk.mp3")).toBe("../assets/talk.mp3");
      expect(await backend.streamAsset("2026/a b.png")).toBe("../assets/2026/a%20b.png");
      // What the static copier accepts (`AssetSink::asset_relative`) is
      // answered under the path it copied to: a colon in a name is a name,
      // `.` and empty steps collapse the way `Path::components` collapses them.
      expect(await backend.streamAsset("x:y.png")).toBe("../assets/x%3Ay.png");
      expect(await backend.streamAsset("data:plot.png")).toBe("../assets/data%3Aplot.png");
      expect(await backend.streamAsset("nested//a.png")).toBe("../assets/nested/a.png");
      expect(await backend.streamAsset("nested/./a.png")).toBe("../assets/nested/a.png");
      expect(await backend.streamAsset(".hidden.png")).toBe("../assets/.hidden.png");
      // The copier drops a query or fragment before copying; the app asks for
      // the file it copied, not for `a.png?v=1`.
      expect(await backend.streamAsset("a.png?v=1")).toBe("../assets/a.png");
      expect(await backend.streamAsset("a.png#crop")).toBe("../assets/a.png");
      // A leading `.` step is the one `Path::components` keeps, and the copier
      // refuses it; so does the app.
      for (const name of ["../../private.png", "assets/../../x", "/etc/passwd", "https://example.test/x", "a\\b.png", ".", "", "./", "./x.png", "?v=1"]) {
        expect(await backend.streamAsset(name), name).toBe("");
        expect(await backend.readAsset(name), name).toEqual(new Uint8Array());
      }
      expect(await backend.readAsset("talk.mp3")).toEqual(new Uint8Array([1]));
      expect(fetched).toEqual(["../assets/talk.mp3"]);
    } finally {
      globalThis.fetch = realFetch;
    }
  });

  it("returns copies, so a consumer mutating a result cannot corrupt the snapshot", async () => {
    const backend = publishedBackend(load);
    const first = (await backend.queryRun(parsed("home").query, {} as never, { current_page: "Open tasks (export)" })) as unknown as {
      groups: unknown[];
    };
    first.groups.length = 0;
    const second = (await backend.queryRun(parsed("home").query, {} as never, { current_page: "Open tasks (export)" })) as unknown as {
      groups: unknown[];
    };
    expect(second.groups).toHaveLength(1);
  });

  it("answers the explicitly routed Ctrl-K consumer and refuses a query-language search", async () => {
    const backend = publishedBackend(load);
    const answer = await backend.runGraphSearch("dash", 10, 10, "quick-switch", false, undefined, undefined, "ctrl_k");
    expect(answer.hits.map((hit) => hit.entity)).toEqual(["page", "block"]);
    expect(answer.hits[0]).toMatchObject({ entity: "page", display_text: "Dashboard", match_class: "prefix" });
    expect(answer.hits[1]).toMatchObject({ entity: "block", page: "Sep 13th, 2026", display_text: "TODO call [[Dashboard]]" });
    expect(answer.has_more).toEqual({ pages: false, blocks: false });
    // Scoped to one page: only that page's blocks, no page rows.
    const scoped = await backend.runGraphSearch("child", 10, 10, "quick-switch:current-page", false, { name: "Dashboard", pageKind: "page", path: "pages/Dashboard.md" }, undefined, "ctrl_k");
    expect(scoped.hits.map((hit) => (hit.entity === "block" ? hit.block.id : hit.entity))).toEqual(["d2"]);
    // An alias row names the page it stands for.
    expect((await backend.runGraphSearch("dash", 10, 0, "quick-switch", false, undefined, undefined, "ctrl_k")).hits[0]).toMatchObject({ entity: "page", page: { name: "Dashboard" } });
    expect((await backend.runGraphSearch("", 10, 10, "quick-switch", false, undefined, undefined, "ctrl_k")).hits).toEqual([]);
    // The query workspace and inline friendly search are query-language runs.
    await expect(backend.runGraphSearch("(task TODO)", 10, 10, "query-workspace:1:materialize")).rejects.toMatchObject({ reasonCode: PUBLISHED_QUERY_REASON });
    await expect(backend.runGraphSearch("dash", 10, 10)).rejects.toBeInstanceOf(QueryUnavailableError);
  });
});

describe("published backend: pages and references", () => {
  it("serves pages by name and alias, and an absent page as an empty read-only page", async () => {
    const backend = publishedBackend(load);
    expect((await backend.getPage("dashboard", "page"))?.path).toBe("pages/Dashboard.md");
    expect((await backend.getPage("dash", "page"))?.name).toBe("Dashboard");
    const absent = await backend.getPage("Private", "page");
    expect(absent).toMatchObject({ name: "Private", kind: "page", blocks: [], read_only: true, path: "" });
    expect((await backend.getPageByPath("journals/2026_09_13.md"))?.name).toBe("Sep 13th, 2026");
    expect(await backend.getPageByPath("pages/Private.md")).toBeNull();
    expect(await backend.existingPageNames(["Dashboard", "Private", "dash"])).toEqual(["Dashboard", "dash"]);
  });

  it("lists the home page first and feeds the journals that were baked", async () => {
    const backend = publishedBackend(load);
    const entries = await backend.listPages();
    expect(entries[0].name).toBe("Open tasks (export)");
    const feed = await backend.journalFeedPage(10, null);
    expect(feed.pages.map((page) => page.name)).toEqual(["Sep 13th, 2026"]);
    expect(feed.done).toBe(true);
    expect(await backend.journalContentDays()).toEqual([20260913]);
  });

  it("answers backlinks, block refs and previews from the snapshot", async () => {
    const backend = publishedBackend(load);
    expect((await backend.getBacklinks("dashboard"))[0].page).toBe("Sep 13th, 2026");
    expect(await backend.getBacklinks("Private")).toEqual([]);
    expect(await backend.getBlockRefCounts()).toEqual({ "11111111-1111-1111-1111-111111111111": 1 });
    const resolved = await backend.resolveBlock("11111111-1111-1111-1111-111111111111");
    expect(resolved?.page).toBe("Dashboard");
    expect(resolved?.blocks[0].children).toEqual([]);
    const preview = await backend.previewBlock("d1", 1);
    expect(preview?.group.blocks[0].children).toEqual([]);
    expect(preview?.truncated).toBe(1);
    const referrers = await backend.getBlockReferrers("22222222-2222-2222-2222-222222222222");
    expect(referrers[0].blocks[0].id).toBe("d2");
    expect(referrers[0].blocks[0].breadcrumb).toEqual(["{{query (and (task TODO) <% current page %>)}}"]);
  });

  it("builds linked-filter entries only from requested snapshot roots and preserves stale roots", async () => {
    const snapshot = fixture();
    snapshot.backlinks.Dashboard = [{
      page: "Journal",
      kind: "journal",
      blocks: [
        {
          id: "match",
          raw: "Root café [[Dashboard]]",
          collapsed: false,
          tags: ["Work"],
          children: [{ id: "match-child", raw: "TODO descendant 😀", collapsed: false, marker: "TODO", children: [] }],
        },
        { id: "miss", raw: "Draft note [[Dashboard]]", collapsed: false, children: [] },
      ],
    }];
    const backend = publishedBackend(() => Promise.resolve(snapshot));
    const targets = [
      { page: "journal", kind: "journal" as const, block_id: "match" },
      { page: "Journal", kind: "journal" as const, block_id: "miss" },
      { page: "Journal", kind: "journal" as const, block_id: "stale" },
    ];

    searchBridge.searchMatchBatch.mockReturnValueOnce({ matches: [true, false], search_error: null });
    const context = await backend.getBacklinkFilterContext("dashboard", targets, "café OR 😀 -draft");

    expect(searchBridge.searchMatchBatch).toHaveBeenLastCalledWith(
      "café OR 😀 -draft",
      ["Root café [[Dashboard]]\nTODO descendant 😀", "Draft note [[Dashboard]]"],
    );
    expect(context).toEqual({
      entries: [
        { page: "Journal", kind: "journal", block_id: "match", facets: ["Work", "TODO"], text_matches: true },
        { page: "Journal", kind: "journal", block_id: "miss", facets: [], text_matches: false },
      ],
      truncated: true,
    });
  });

  it("passes regex through to the native batch matcher and keeps empty/invalid searches visible", async () => {
    const snapshot = fixture();
    const backend = publishedBackend(() => Promise.resolve(snapshot));
    const targets = [{ page: "Sep 13th, 2026", kind: "journal" as const, block_id: "j1" }];

    searchBridge.searchMatchBatch.mockReturnValueOnce({ matches: [true], search_error: null });
    expect((await backend.getBacklinkFilterContext("Dashboard", targets, "/TODO.*Dashboard/")).entries[0].text_matches).toBe(true);
    expect(searchBridge.searchMatchBatch).toHaveBeenLastCalledWith("/TODO.*Dashboard/", ["TODO call [[Dashboard]]"]);

    searchBridge.searchMatchBatch.mockReturnValueOnce({ matches: [true], search_error: null });
    expect(await backend.getBacklinkFilterContext("Dashboard", targets, "")).toMatchObject({
      entries: [{ text_matches: true }],
      truncated: false,
    });

    searchBridge.searchMatchBatch.mockReturnValueOnce({ matches: [true], search_error: "unclosed character class" });
    expect(await backend.getBacklinkFilterContext("Dashboard", targets, "/[/")).toMatchObject({
      entries: [{ text_matches: true }],
      search_error: "unclosed character class",
      truncated: false,
    });
  });

  it("searches raw text and switches pages by substring", async () => {
    const backend = publishedBackend(load);
    const hits = await backend.search("call", 10);
    expect(hits.map((group) => group.page)).toEqual(["Sep 13th, 2026"]);
    expect(await backend.search("", 10)).toEqual([]);
    expect((await backend.quickSwitch("dash", 10)).map((entry) => entry.name)).toEqual(["Dashboard"]);
    expect(await backend.pageIcons(["Dashboard", "Private"])).toEqual({ Dashboard: "📊" });
    expect(await backend.pageAliases()).toEqual([["dash", "Dashboard"]]);
  });

  it("uses the search fold for text but retains the separate page-identity fold", async () => {
    const snapshot = fixture();
    snapshot.pages.push({
      name: "café",
      kind: "page",
      title: "café",
      pre_block: null,
      blocks: [{ id: "accent", raw: "Příliš café", collapsed: false, children: [] }],
      path: "pages/café.md",
    });
    snapshot.entries.push({ name: "café", kind: "page", date_key: null, path: "pages/café.md" });
    const backend = publishedBackend(() => Promise.resolve(snapshot));

    expect((await backend.search("cafe", 10)).map((group) => group.page)).toContain("café");
    expect((await backend.quickSwitch("cafe", 10)).map((entry) => entry.name)).toContain("café");
    expect((await backend.getPage("cafe", "page"))?.path).toBe("");
    expect((await backend.getPage("café", "page"))?.path).toBe("pages/café.md");
  });

  it("maps compatibility-folded evidence back to original emoji-aware UTF-16 offsets", async () => {
    const snapshot = fixture();
    snapshot.entries.push({ name: "😀 𝐀lpha", kind: "page", date_key: null, path: "pages/math.md" });
    const backend = publishedBackend(() => Promise.resolve(snapshot));

    const answer = await backend.runGraphSearch("𝐀", 20, 0, "quick-switch", false, undefined, undefined, "ctrl_k");
    const hit = answer.hits.find((candidate) => candidate.entity === "page" && candidate.page.name === "😀 𝐀lpha");
    expect(hit?.evidence).toEqual([
      { clause_id: 0, field: "page_name", mode: "contains", spans: [{ start: 3, end: 5 }] },
    ]);
  });

  it("does not turn a nonempty fold-erased literal into match-all", async () => {
    const backend = publishedBackend(load);
    const erased = "\u0301";
    expect(await backend.search(erased, 10)).toEqual([]);
    expect(await backend.quickSwitch(erased, 10)).toEqual([]);
    expect((await backend.runGraphSearch(erased, 10, 10, "quick-switch", false, undefined, undefined, "ctrl_k")).hits).toEqual([]);
    expect(await backend.quickSwitch("", 10)).toHaveLength(fixture().entries.length);
  });

  it("loads the export as a graph whose home is the baked home page", async () => {
    const backend = publishedBackend(load);
    const loaded = await backend.loadGraph("");
    expect(loaded.kind).toBe("loaded");
    if (loaded.kind === "loaded") {
      expect(loaded.meta.root).toBe("Open tasks");
      expect(loaded.meta.default_home).toBe("Open tasks (export)");
      expect(loaded.meta.favorites).toEqual([]);
    }
    expect(await backend.startupGraphPath()).toBe("Open tasks");
  });
});

describe("published backend: refusals and helpers", () => {
  it("refuses writes with the typed read-only error and never touches the snapshot", async () => {
    let loads = 0;
    const backend = publishedBackend(async () => {
      loads++;
      return fixture();
    });
    await expect(backend.savePage({} as never, null)).rejects.toBeInstanceOf(PublishedExportReadOnlyError);
    await expect(backend.deletePage("Dashboard", "page")).rejects.toMatchObject({ kind: "published-export-read-only" });
    expect(loads).toBe(0);
  });

  it("keeps session and app settings in memory only", async () => {
    const backend = publishedBackend(load);
    expect(await backend.loadSession()).toBeNull();
    await backend.saveSession("{}");
    expect(await backend.loadSession()).toBe("{}");
    expect(await backend.getAppBool("x", true)).toBe(true);
    await backend.setAppBool("x", false);
    expect(await backend.getAppBool("x", true)).toBe(false);
  });

  it("finds the snapshot URL through the published meta tag only", () => {
    const doc = (content: string | null) =>
      ({
        querySelector: () => (content === null ? null : { getAttribute: () => content }),
      }) as unknown as Document;
    expect(publishedSnapshotUrl(doc("snapshot.json"))).toBe("snapshot.json");
    expect(publishedSnapshotUrl(doc(" "))).toBeNull();
    expect(publishedSnapshotUrl(doc(null))).toBeNull();
    expect(publishedSnapshotUrl(undefined)).toBeNull();
  });

  it("validates the schema and required keys, and keys IR stably", () => {
    expect(() => validateSnapshot(fixture())).not.toThrow();
    expect(() => validateSnapshot({ ...fixture(), schema: 2 })).toThrow(/schema/);
    const missing = fixture() as unknown as Record<string, unknown>;
    delete missing.queries;
    expect(() => validateSnapshot(missing as unknown as PublishedSnapshot)).toThrow(/queries/);
    expect(stableJson({ b: [{ z: 1, y: undefined }], a: "x" })).toBe('{"a":"x","b":[{"z":1}]}');
  });
});
