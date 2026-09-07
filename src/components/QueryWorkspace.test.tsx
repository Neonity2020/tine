import { afterEach, describe, expect, it, vi } from "vitest";
import { createSignal } from "solid-js";
import { render } from "solid-js/web";
import type { PaneRouter, QueryRoute } from "../router";
import type { PageDto, QueryExecution } from "../types";
import {
  clearTransientLayersForTest,
  dismissTopTransient,
  registerTransientLayer,
} from "../transientLayers";
import {
  QueryWorkspace,
  materializeQueryWorkspace,
  type MaterializeQueryDependencies,
  type QueryWorkspaceDependencies,
} from "./QueryWorkspace";
import { pageInventoryRev, setGraphMeta } from "../ui";
import { backend, SaveConflictError } from "../backend";

afterEach(() => {
  clearTransientLayersForTest();
  setGraphMeta(null);
  document.body.innerHTML = "";
});

/** A promise the test releases by hand, so a real await can be held open across
 *  a user edit instead of being simulated by a callback. */
function gate(): { wait: Promise<void>; open: () => void } {
  let open!: () => void;
  const wait = new Promise<void>((resolve) => { open = resolve; });
  return { wait, open };
}

function materializeDeps(overrides: Partial<MaterializeQueryDependencies> = {}): MaterializeQueryDependencies {
  return {
    getPage: vi.fn(async () => null),
    savePage: vi.fn(async () => ({ revision: "rev-new" })),
    runGraphSearch: vi.fn(async () => ({ hits: [], diagnostics: [], explanation: { branches: [{ description: "valid", children: [] }] }, cancelled: false })),
    ...overrides,
  };
}

describe("materializeQueryWorkspace", () => {
  it.each(["diagnostic", "existing", "lookup-error"])("discards a stale %s response", async (stage) => {
    let current = true;
    const deps = materializeDeps({
      runGraphSearch: vi.fn(async () => {
        if (stage === "diagnostic") current = false;
        return { hits: [], diagnostics: stage === "diagnostic" ? [{ code: "invalid_regex", message: "old diagnostic" }] : [], explanation: { branches: [{ description: "valid", children: [] }] }, cancelled: false };
      }),
      getPage: vi.fn(async () => {
        current = false;
        if (stage === "lookup-error") throw new Error("old lookup error");
        return { name: "Saved", title: "Saved", kind: "page", pre_block: null, blocks: [] } satisfies PageDto;
      }),
    });
    const result = await materializeQueryWorkspace({ title: "Saved", sourceKind: "search", source: "alpha", presentation: "list", routeId: "q" }, deps, () => current);
    expect(result).toMatchObject({ ok: false, kind: "superseded" });
    expect(deps.savePage).not.toHaveBeenCalled();
  });

  it.each(["validation", "lookup"])("refuses stale input after %s", async (stage) => {
    let release!: () => void;
    const barrier = new Promise<void>((resolve) => { release = resolve; });
    let current = true;
    const deps = materializeDeps(stage === "validation" ? {
      runGraphSearch: vi.fn(async () => { await barrier; return { hits: [], diagnostics: [], explanation: { branches: [{ description: "valid", children: [] }] }, cancelled: false }; }),
    } : { getPage: vi.fn(async () => { await barrier; return null; }) });
    const pending = materializeQueryWorkspace({ title: "Saved", sourceKind: "search", source: "alpha", presentation: "list", routeId: "q" }, deps, () => current);
    await Promise.resolve();
    current = false;
    release();
    expect(await pending).toMatchObject({ ok: false, kind: "superseded" });
    expect(deps.savePage).not.toHaveBeenCalled();
  });

  it("rejects empty, exclusion-only, and Rust-diagnostic friendly searches before any graph write", async () => {
    for (const source of ["   ", "-draft", "/(a)\\1/"]) {
      const deps = materializeDeps({ runGraphSearch: vi.fn(async () => source === "-draft"
        ? { hits: [], diagnostics: [], explanation: { branches: [] }, cancelled: false }
        : { hits: [], diagnostics: [{ code: "invalid_regex", message: "invalid regex" }], explanation: { branches: [] }, cancelled: false }) });
      const result = await materializeQueryWorkspace({ title: "Unsafe", sourceKind: "search", source, presentation: "search", routeId: "query-unsafe" }, deps);
      expect(result.ok).toBe(false);
      expect(deps.getPage).not.toHaveBeenCalled();
      expect(deps.savePage).not.toHaveBeenCalled();
    }
  });
  it("uses an explicit stable route lane and zero-limit Rust validation for every nonblank friendly save", async () => {
    const validate = vi.fn(async () => ({ hits: [], diagnostics: [], explanation: { branches: [{ description: "valid", children: [] }] }, cancelled: false }));
    const deps = materializeDeps({ runGraphSearch: validate });
    const input = { title: "Saved", sourceKind: "search" as const, source: " alpha ", presentation: "search" as const, routeId: "query-stable" };
    await materializeQueryWorkspace(input, deps);
    await materializeQueryWorkspace(input, deps);
    expect(validate).toHaveBeenCalledTimes(2);
    expect(validate).toHaveBeenNthCalledWith(1, "alpha", 0, 0, "query-workspace:query-stable:materialize", true);
    expect(validate).toHaveBeenNthCalledWith(2, "alpha", 0, 0, "query-workspace:query-stable:materialize", true);

    const blank = materializeDeps();
    await materializeQueryWorkspace({ ...input, source: "   " }, blank);
    expect(blank.runGraphSearch).not.toHaveBeenCalled();
    expect(blank.getPage).not.toHaveBeenCalled();
    expect(blank.savePage).not.toHaveBeenCalled();
  });
  it("rejects JavaScript-invalid, cancelled, and failed Rust validation before page lookup", async () => {
    const input = { title: "Unsafe", sourceKind: "search" as const, source: "/(unclosed/", presentation: "search" as const, routeId: "query-rejected" };
    const diagnostic = materializeDeps({ runGraphSearch: vi.fn(async () => ({ hits: [], diagnostics: [{ code: "invalid_regex", message: "invalid regex" }], explanation: { branches: [] }, cancelled: false })) });
    await materializeQueryWorkspace(input, diagnostic);
    expect(diagnostic.runGraphSearch).toHaveBeenCalledWith("/(unclosed/", 0, 0, "query-workspace:query-rejected:materialize", true);
    expect(diagnostic.getPage).not.toHaveBeenCalled(); expect(diagnostic.savePage).not.toHaveBeenCalled();
    const cancelled = materializeDeps({ runGraphSearch: vi.fn(async () => ({ hits: [], diagnostics: [], explanation: { branches: [] }, cancelled: true })) });
    await materializeQueryWorkspace({ ...input, source: "alpha" }, cancelled);
    expect(cancelled.getPage).not.toHaveBeenCalled(); expect(cancelled.savePage).not.toHaveBeenCalled();
    const failed = materializeDeps({ runGraphSearch: vi.fn(async () => { throw new Error("IPC unavailable"); }) });
    await materializeQueryWorkspace({ ...input, source: "alpha" }, failed);
    expect(failed.getPage).not.toHaveBeenCalled(); expect(failed.savePage).not.toHaveBeenCalled();
  });
  it("creates one canonical friendly query block through the guarded no-baseline save", async () => {
    const deps = materializeDeps();
    const beforeInventory = pageInventoryRev();
    const result = await materializeQueryWorkspace({
      title: "  Project dashboard  ",
      sourceKind: "search",
      source: "alpha -draft",
      presentation: "search",
      routeId: "query-project-dashboard",
    }, deps);

    expect(result.ok).toBe(true);
    if (!result.ok) throw new Error(result.message);
    expect(result.page).toEqual({
      name: "Project dashboard",
      kind: "page",
      title: "Project dashboard",
      pre_block: null,
      format: "md",
      blocks: [{
        id: "",
        raw: '{{query (search "alpha -draft")}}\ntine.view:: search',
        collapsed: false,
        children: [],
      }],
    });
    expect(deps.getPage).toHaveBeenCalledWith("Project dashboard", "page");
    expect(deps.savePage).toHaveBeenCalledTimes(1);
    expect(deps.savePage).toHaveBeenCalledWith(result.page, null, false);
    expect(pageInventoryRev()).toBeGreaterThan(beforeInventory);
  });

  it("preserves canonical raw DSL and writes a presentation property only when needed", async () => {
    const listDeps = materializeDeps();
    const list = await materializeQueryWorkspace({
      title: "Tasks",
      sourceKind: "dsl",
      source: "  (and (todo TODO) (priority A))  ",
      presentation: "list",
      routeId: "query-tasks",
    }, listDeps);
    expect(list.ok && list.page.blocks).toHaveLength(1);
    expect(list.ok && list.page.blocks[0].raw).toBe("{{query (and (todo TODO) (priority A))}}");

    const tableDeps = materializeDeps();
    const table = await materializeQueryWorkspace({
      title: "Task table",
      sourceKind: "dsl",
      source: "(todo TODO)",
      presentation: "table",
      routeId: "query-task-table",
    }, tableDeps);
    expect(table.ok && table.page.blocks).toHaveLength(1);
    expect(table.ok && table.page.blocks[0].raw).toBe("{{query (todo TODO)}}\ntine.view:: table");
  });

  it("places the view property where the captured graph format reads it back", async () => {
    const orgDeps = materializeDeps();
    const org = await materializeQueryWorkspace({
      title: "Org board",
      sourceKind: "dsl",
      source: "(todo TODO)",
      presentation: "board",
      routeId: "query-org-board",
      format: "org",
    }, orgDeps);
    expect(org.ok).toBe(true);
    if (!org.ok) throw new Error(org.message);
    // An org block carries properties in a drawer; a markdown `key:: value` line
    // here would render as body text and never read back as a property.
    expect(org.page.blocks[0].raw).toBe("{{query (todo TODO)}}\n:PROPERTIES:\n:tine.view: board\n:END:");
    expect(org.page.format).toBe("org");
    expect(orgDeps.savePage).toHaveBeenCalledWith(org.page, null, false);

    // The default list view is spelled as an ABSENT property in both formats,
    // so an org list workspace materializes a bare query block with no drawer.
    const orgList = await materializeQueryWorkspace({
      title: "Org list",
      sourceKind: "dsl",
      source: "(todo TODO)",
      presentation: "list",
      routeId: "query-org-list",
      format: "org",
    }, materializeDeps());
    expect(orgList.ok && orgList.page.blocks[0].raw).toBe("{{query (todo TODO)}}");

    // An absent format keeps every dependency-injected caller on markdown.
    const md = await materializeQueryWorkspace({
      title: "Md board",
      sourceKind: "dsl",
      source: "(todo TODO)",
      presentation: "board",
      routeId: "query-md-board",
    }, materializeDeps());
    expect(md.ok && md.page.blocks[0].raw).toBe("{{query (todo TODO)}}\ntine.view:: board");
    expect(md.ok && md.page.format).toBe("md");
  });

  it("refuses an existing page without attempting a write", async () => {
    const existing: PageDto = {
      name: "Taken",
      kind: "page",
      title: "Taken",
      pre_block: null,
      blocks: [],
    };
    const deps = materializeDeps({ getPage: vi.fn(async () => existing) });
    const result = await materializeQueryWorkspace({
      title: "Taken",
      sourceKind: "search",
      source: "alpha",
      presentation: "list",
      routeId: "query-taken",
    }, deps);

    expect(result).toMatchObject({ ok: false, kind: "exists" });
    expect(deps.savePage).not.toHaveBeenCalled();
  });

  it("keeps the workspace virtual when a create race reaches the save guard", async () => {
    const deps = materializeDeps({
      savePage: vi.fn(async () => { throw new SaveConflictError(17); }),
    });
    const result = await materializeQueryWorkspace({
      title: "Raced",
      sourceKind: "search",
      source: "alpha",
      presentation: "board",
      routeId: "query-raced",
    }, deps);

    expect(result).toMatchObject({ ok: false, kind: "conflict" });
    expect(deps.savePage).toHaveBeenCalledTimes(1);
  });

  it("does not classify code-shaped non-conflicts from their prose", async () => {
    const deps = materializeDeps({
      savePage: vi.fn(async () => {
        throw new Error("precheck.portable_collision: a page with that spelling already exists");
      }),
    });
    const result = await materializeQueryWorkspace({
      title: "Portable collision",
      sourceKind: "search",
      source: "alpha",
      presentation: "list",
      routeId: "query-portable-collision",
    }, deps);

    expect(result).toMatchObject({ ok: false, kind: "error" });
  });
});

function routerMock(activeRoute: QueryRoute = { kind: "query", id: "query-mock", sourceKind: "search", source: "", presentation: "search" }) {
  return {
    route: vi.fn(() => activeRoute),
    updateActiveQuery: vi.fn(),
    replaceActiveRoute: vi.fn(),
    openPage: vi.fn(),
    openPageTarget: vi.fn(),
    openPageAtBlock: vi.fn(),
  } as unknown as PaneRouter;
}

function executionFixture(explained: boolean): QueryExecution {
  return {
    hits: [
      {
        entity: "page",
        page: { name: "Alpha notes", kind: "page", date_key: null, path: "pages/alpha.md" },
        display_text: "Alpha notes",
        evidence: [{
          clause_id: 1,
          field: "page_name",
          mode: "fuzzy",
          spans: [{ start: 0, end: 5 }],
          score: 100,
        }],
        score: 100,
      },
      {
        entity: "block",
        page: "Research",
        kind: "page",
        path: "pages/client-b/Research.md",
        block: {
          id: "block-1",
          raw: "An alpha result",
          collapsed: false,
          children: [],
          breadcrumb: ["Parent"],
          properties: [["ID", "authored-block-1"]],
        },
        display_text: "An alpha result",
        evidence: [{
          clause_id: 2,
          field: "visible_content",
          mode: "contains",
          spans: [{ start: 3, end: 8 }],
        }],
      },
    ],
    diagnostics: [{ code: "bounded", message: "Results are limited for this preview." }],
    explanation: {
      branches: explained ? [{
        description: "Search page names and visible block text",
        children: [{ clause_id: 2, description: "contains alpha", children: [] }],
      }] : [],
    },
    cancelled: false,
  };
}

function workspaceDeps(): QueryWorkspaceDependencies {
  return {
    getPage: vi.fn(async () => null),
    savePage: vi.fn(async () => ({ revision: "saved-rev" })),
    runGraphSearch: vi.fn(async (_source, pageLimit, blockLimit, _lane, explain) =>
      pageLimit === 0 && blockLimit === 0
        ? { hits: [], diagnostics: [], explanation: { branches: [{ description: "valid", children: [] }] }, cancelled: false }
        : executionFixture(explain)),
    runQuery: vi.fn(async () => []),
    runAdvancedQuery: vi.fn(async () => ({ groups: [], ran: [], ignored: [], supported: true })),
  };
}

async function waitFor(check: () => void): Promise<void> {
  await vi.waitFor(check, { timeout: 1_000, interval: 5 });
}

/** A workspace only ever exists with a graph open, and the graph is what says
 *  which on-disk format a new page is written in. */
function openGraph(root: string, format: "md" | "org" = "md"): void {
  setGraphMeta({ root, preferred_format: format } as never);
}

/** Hold ONE real await of the save lifecycle open, so the fixture races the
 *  component's own routing rather than a stand-in callback. */
function gatedDeps(stage: "validation" | "lookup" | "save"): {
  deps: QueryWorkspaceDependencies;
  open: () => void;
} {
  const { wait, open } = gate();
  const deps = workspaceDeps();
  if (stage === "validation") {
    const live = deps.runGraphSearch;
    // Only the zero-limit materialize lane waits: the pane's own live search
    // has to keep answering, or the workspace under test would never render.
    deps.runGraphSearch = vi.fn(async (source, pageLimit, blockLimit, lane, explain) => {
      if (pageLimit === 0 && blockLimit === 0) await wait;
      return live(source, pageLimit, blockLimit, lane, explain);
    });
  } else if (stage === "lookup") {
    deps.getPage = vi.fn(async () => { await wait; return null; });
  } else {
    const live = deps.savePage;
    deps.savePage = vi.fn(async (page, baseRev, force) => { await wait; return live(page, baseRev, force); });
  }
  return { deps, open };
}

function typeInto(input: HTMLInputElement, value: string): void {
  input.value = value;
  input.dispatchEvent(new InputEvent("input", { bubbles: true }));
}

function submitSave(root: HTMLElement, name: string): void {
  typeInto(root.querySelector<HTMLInputElement>(".query-workspace-save input")!, name);
  (root.querySelector(".query-workspace-save") as HTMLFormElement)
    .dispatchEvent(new SubmitEvent("submit", { bubbles: true, cancelable: true }));
}

function clickPresentation(root: HTMLElement, label: string): void {
  [...root.querySelectorAll<HTMLButtonElement>(".query-presentations button")]
    .find((candidate) => candidate.textContent === label)!.click();
}

function saveButton(root: HTMLElement): HTMLButtonElement {
  return root.querySelector<HTMLButtonElement>(".query-workspace-save button")!;
}

function text(root: HTMLElement, selector: string): string {
  return root.querySelector(selector)?.textContent ?? "";
}

describe("QueryWorkspace", () => {
  it("peels a QueryBuilder child before its Advanced parent and preserves the draft", async () => {
    const route: QueryRoute = {
      kind: "query",
      id: "query-transient-ladder",
      sourceKind: "dsl",
      source: "(and (task TODO))",
      presentation: "list",
    };
    // The pane shows what the PRINTER returned (I-12); the dev-preview backend
    // has no printer, so this test says what Rust would answer.
    vi.spyOn(backend(), "printQuery").mockResolvedValue(route.source);
    const lower = vi.fn(() => true);
    const unregisterLower = registerTransientLayer({ id: "query-workspace-lower", dismiss: lower });
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={routerMock(route)} deps={workspaceDeps()} />, root);
    try {
      const toggle = root.querySelector<HTMLButtonElement>(".query-advanced-toggle")!;
      toggle.click();
      await Promise.resolve();
      const dialog = root.querySelector<HTMLElement>(".query-advanced-modal")!;
      expect(dialog).not.toBeNull();

      // The sheet is over the IR now, so its rows appear once the ENGINE has
      // read the route's text — there is no frontend parser left to do it
      // synchronously. In the workspace the sheet is always open and NOT
      // portalled: it is inside the Advanced modal, so the modal's Tab trap
      // keeps containing it.
      // The dev-preview backend has no parser, so this route's text comes back
      // as ONE retained row — which still has its own ⋮ popover to peel.
      await waitFor(() => expect(root.querySelector(".qs-row .qs-row-menu")).not.toBeNull());
      root.querySelector<HTMLButtonElement>(".qs-row .qs-row-menu")!.click();
      expect(root.querySelector(".qs-menu")).not.toBeNull();

      // Rung one: Escape peels the child popover and leaves the modal standing.
      expect(dismissTopTransient("escape")).toBe(true);
      expect(root.querySelector(".qs-menu")).toBeNull();
      expect(root.querySelector(".query-advanced-modal")).not.toBeNull();

      // Same rung by pointer (GH #472): a press on the modal's own header is an
      // outside press for the row menu, so the menu closes and only the menu.
      root.querySelector<HTMLButtonElement>(".qs-row .qs-row-menu")!.click();
      expect(root.querySelector(".qs-menu")).not.toBeNull();
      dialog.querySelector(".query-advanced-header")!
        .dispatchEvent(new MouseEvent("pointerdown", { bubbles: true }));
      expect(root.querySelector(".qs-menu")).toBeNull();
      expect(root.querySelector(".query-advanced-modal")).not.toBeNull();
      await waitFor(() =>
        expect(root.querySelector<HTMLTextAreaElement>(".query-text-pane-input")?.value).toBe(route.source),
      );
      expect(lower).not.toHaveBeenCalled();

      expect(dismissTopTransient("back")).toBe(true);
      await Promise.resolve();
      expect(root.querySelector(".query-advanced-modal")).toBeNull();
      expect(document.activeElement).toBe(toggle);
      expect(lower).not.toHaveBeenCalled();
    } finally {
      unregisterLower();
      dispose();
    }
  });

  it("keeps empty workspaces local, neutral, and query-free", async () => {
    const route: QueryRoute = { kind: "query", id: "query-empty", sourceKind: "search", source: "", presentation: "search" };
    const deps = workspaceDeps();
    const root = document.createElement("div"); document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={routerMock()} deps={deps} focusSource />, root);
    await Promise.resolve(); await Promise.resolve();
    expect(root.querySelector(".query-workspace-status")?.textContent).toContain("Enter a search to begin.");
    expect(root.querySelector(".query-workspace-status")?.textContent).not.toContain("0 results");
    expect(root.querySelector(".query-workspace")?.getAttribute("data-query-route-id")).toBe("query-empty");
    expect(deps.runGraphSearch).not.toHaveBeenCalled(); expect(deps.runQuery).not.toHaveBeenCalled(); expect(deps.runAdvancedQuery).not.toHaveBeenCalled();
    dispose();
  });
  it("uses the correct empty DSL prompt and focuses on pane activation or a route transition without stealing same-route control focus", async () => {
    const emptyDsl: QueryRoute = { kind: "query", id: "query-dsl", sourceKind: "dsl", source: "", presentation: "list" };
    const root = document.createElement("div"); document.body.append(root);
    const disposeDsl = render(() => <QueryWorkspace route={emptyDsl} router={routerMock(emptyDsl)} deps={workspaceDeps()} focusSource />, root);
    await Promise.resolve(); await Promise.resolve();
    expect(root.querySelector(".query-workspace-status")?.textContent).toContain("Enter a query to begin.");
    expect(root.querySelector(".query-workspace-status")?.textContent).not.toContain("0 results");
    disposeDsl(); root.innerHTML = "";

    const first: QueryRoute = { kind: "query", id: "query-focus-a", sourceKind: "search", source: "", presentation: "search" };
    const second: QueryRoute = { ...first, id: "query-focus-b" };
    const [active, setActive] = createSignal<QueryRoute>(first);
    const [focusSource, setFocusSource] = createSignal(false);
    const router = routerMock(first);
    vi.mocked(router.route).mockImplementation(active);
    const dispose = render(() => <QueryWorkspace route={active()} router={router} deps={workspaceDeps()} focusSource={focusSource()} />, root);
    await Promise.resolve(); await Promise.resolve();
    const source = root.querySelector<HTMLInputElement>(".query-workspace-source")!;
    const filters = root.querySelector<HTMLButtonElement>(".query-advanced-toggle")!;
    filters.focus();
    expect(document.activeElement).toBe(filters);
    // A restored route may have mounted while its pane was inactive. Activating
    // that pane must focus this same route once, rather than waiting for a new
    // tab/route identity.
    setFocusSource(true);
    await Promise.resolve(); await Promise.resolve();
    expect(document.activeElement).toBe(source);
    filters.focus();
    setActive({ ...first, source: "alpha" });
    await Promise.resolve(); await Promise.resolve();
    expect(document.activeElement).toBe(filters);
    setActive({ ...first, source: "alpha", presentation: "table" });
    await Promise.resolve(); await Promise.resolve();
    expect(document.activeElement).toBe(filters);
    setActive(second);
    await Promise.resolve(); await Promise.resolve();
    expect(document.activeElement).toBe(source);
    dispose();
  });
  it("keeps invalid friendly input editable and reports the Rust diagnostic", async () => {
    const route: QueryRoute = { kind: "query", id: "query-invalid", sourceKind: "search", source: "/(a)\\1/", presentation: "search" };
    const deps = workspaceDeps();
    vi.mocked(deps.runGraphSearch).mockResolvedValue({ hits: [], diagnostics: [{ code: "invalid_regex", message: "invalid regex" }], explanation: { branches: [] }, cancelled: false });
    const root = document.createElement("div"); document.body.append(root);
    const router = routerMock(route);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-diagnostics")?.textContent).toContain("invalid regex"));
    const input = root.querySelector<HTMLInputElement>(".query-workspace-source")!;
    expect(input.value).toBe("/(a)\\1/");
    input.value = "  /(a)\\1/  "; input.dispatchEvent(new InputEvent("input", { bubbles: true }));
    expect(router.updateActiveQuery).toHaveBeenLastCalledWith({ source: "  /(a)\\1/  ", sourceKind: "search" });
    dispose();
  });
  it("shows evidence, diagnostics and explanations, and switches presentations without changing membership", async () => {
    const route: QueryRoute = {
      kind: "query",
      id: "query-test",
      sourceKind: "search",
      source: "alpha",
      presentation: "search",
    };
    const router = routerMock();
    const deps = workspaceDeps();
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);

    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));
    expect(root.querySelector(".query-workspace-diagnostics")?.textContent).toContain("Results are limited");
    expect([...root.querySelectorAll("mark")].map((mark) => mark.textContent)).toEqual(["Alpha", "alpha"]);
    expect(root.querySelector(".search-result-context")?.textContent).toContain("Page");
    expect(root.querySelectorAll(".query-result-row")).toHaveLength(2);
    const resultRows = [...root.querySelectorAll<HTMLButtonElement>(".query-result-row")];
    resultRows[0].click();
    expect(router.openPageTarget).toHaveBeenCalledWith({
      name: "Alpha notes", pageKind: "page", path: "pages/alpha.md",
    });
    resultRows[1].click();
    expect(router.openPageAtBlock).toHaveBeenCalledWith({
      name: "Research", pageKind: "page", path: "pages/client-b/Research.md", block: "authored-block-1",
    });

    for (const [label, selector] of [
      ["List", ".query-results-list"],
      ["Table", ".query-results-table"],
      ["Board", ".query-results-board"],
    ] as const) {
      const button = [...root.querySelectorAll<HTMLButtonElement>(".query-presentations button")]
        .find((candidate) => candidate.textContent === label)!;
      button.click();
      expect(root.querySelector(selector)).not.toBeNull();
      expect([...root.querySelectorAll("mark")].map((mark) => mark.textContent)).toEqual(["Alpha", "alpha"]);
      expect(router.updateActiveQuery).toHaveBeenCalledWith({ presentation: label.toLowerCase() });
    }

    const explain = root.querySelector(".query-explain-toggle") as HTMLButtonElement;
    explain.click();
    await waitFor(() => expect(root.querySelector(".query-workspace-explanation")?.textContent).toContain("contains alpha"));
    expect(deps.runGraphSearch).toHaveBeenLastCalledWith("alpha", 40, 100, "query-workspace:query-test", true);

    dispose();
  });

  it("saves by naming and replaces the virtual route only after the guarded write succeeds", async () => {
    openGraph("/graphs/A");
    const route: QueryRoute = {
      kind: "query",
      id: "query-save",
      sourceKind: "search",
      source: "alpha OR beta",
      presentation: "board",
    };
    // The ACTIVE route is this workspace's own route: the save guard compares
    // route identity, so a mock that claims some other tab is active would be
    // testing a refusal, not a save.
    const router = routerMock(route);
    const deps = workspaceDeps();
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

    submitSave(root, "Saved search");

    await waitFor(() => expect(router.replaceActiveRoute).toHaveBeenCalledWith({
      kind: "page",
      name: "Saved search",
      pageKind: "page",
    }));
    const saved = vi.mocked(deps.savePage).mock.calls[0][0];
    expect(saved.blocks).toHaveLength(1);
    expect(saved.blocks[0].raw).toBe('{{query (search "alpha OR beta")}}\ntine.view:: board');
    expect(saved.format).toBe("md");
    expect(deps.savePage).toHaveBeenCalledWith(saved, null, false);
    expect(text(root, ".query-workspace-save-error")).toBe("");
    expect(text(root, ".query-workspace-save-notice")).toBe("");

    dispose();
  });

  it("writes the org drawer form when the open graph prefers org", async () => {
    openGraph("/graphs/Org", "org");
    const route: QueryRoute = {
      kind: "query",
      id: "query-org-save",
      sourceKind: "search",
      source: "alpha",
      presentation: "table",
    };
    const router = routerMock(route);
    const deps = workspaceDeps();
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

    submitSave(root, "Org search");
    await waitFor(() => expect(deps.savePage).toHaveBeenCalledTimes(1));
    const saved = vi.mocked(deps.savePage).mock.calls[0][0];
    expect(saved.format).toBe("org");
    expect(saved.blocks[0].raw).toBe('{{query (search "alpha")}}\n:PROPERTIES:\n:tine.view: table\n:END:');

    dispose();
  });

  it("refuses a save whose collision the backend reports, without ever forcing it", async () => {
    openGraph("/graphs/A");
    const route: QueryRoute = {
      kind: "query",
      id: "query-collision",
      sourceKind: "search",
      source: "alpha",
      presentation: "list",
    };
    const router = routerMock(route);
    const deps = workspaceDeps();
    vi.mocked(deps.savePage).mockRejectedValue(new SaveConflictError(9));
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

    submitSave(root, "Raced");
    await waitFor(() => expect(text(root, ".query-workspace-save-error")).toContain("has not been overwritten"));
    expect(router.replaceActiveRoute).not.toHaveBeenCalled();
    expect(deps.savePage).toHaveBeenCalledTimes(1);
    expect(vi.mocked(deps.savePage).mock.calls[0].slice(1)).toEqual([null, false]);
    expect(saveButton(root).disabled).toBe(false);

    dispose();
  });

  const staleEdits = [
    { edit: "source", local: true },
    { edit: "display", local: true },
    { edit: "display-draft", local: true },
    { edit: "source-reverted", local: true },
    { edit: "title", local: true },
    { edit: "format", local: true },
    { edit: "graph", local: false },
    { edit: "route", local: false },
  ] as const;
  const staleCases = staleEdits.flatMap((entry) =>
    (["validation", "lookup"] as const).map((stage) => ({ ...entry, stage })));

  it.each(staleCases)(
    "writes no page when the $edit changes while the $stage is still in flight",
    async ({ edit, local, stage }) => {
      openGraph("/graphs/A");
      const route: QueryRoute = {
        kind: "query",
        id: "query-stale",
        sourceKind: "search",
        source: "alpha",
        presentation: "board",
      };
      const [activeRoute, setActiveRoute] = createSignal<QueryRoute>(route);
      const { deps, open } = gatedDeps(stage);
      const router = routerMock(route);
      vi.mocked(router.route).mockImplementation(activeRoute);
      const root = document.createElement("div");
      document.body.append(root);
      const dispose = render(
        () => <QueryWorkspace route={activeRoute()} router={router} deps={deps} />, root);
      await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

      submitSave(root, "Saved search");
      await waitFor(() => expect(saveButton(root).disabled).toBe(true));

      // The user moves on while the attempt is parked on its await.
      if (edit === "source") typeInto(root.querySelector<HTMLInputElement>(".query-workspace-source")!, "beta");
      else if (edit === "display") clickPresentation(root, "Table");
      else if (edit === "display-draft") setActiveRoute({ ...route, display: { sample: 0 } });
      else if (edit === "source-reverted") {
        typeInto(root.querySelector<HTMLInputElement>(".query-workspace-source")!, "beta");
        typeInto(root.querySelector<HTMLInputElement>(".query-workspace-source")!, "alpha");
      }
      else if (edit === "title") typeInto(root.querySelector<HTMLInputElement>(".query-workspace-save input")!, "Renamed");
      else if (edit === "format") openGraph("/graphs/A", "org");
      else if (edit === "graph") openGraph("/graphs/B");
      else setActiveRoute({ ...route, id: "query-other-tab" });
      open();

      await waitFor(() => expect(saveButton(root).disabled).toBe(false));
      expect(deps.savePage).not.toHaveBeenCalled();
      expect(router.replaceActiveRoute).not.toHaveBeenCalled();
      expect(text(root, ".query-workspace-save-notice")).toBe("");
      if (local) {
        // Still this workspace, so it says — truthfully — that nothing was
        // written and the remedy is to save again.
        expect(text(root, ".query-workspace-save-error")).toContain("Try saving again");
      } else {
        // A different tab or a different graph: a refusal of THIS attempt has
        // no business appearing there.
        expect(text(root, ".query-workspace-save-error")).toBe("");
      }

      dispose();
    });

  it.each(["source", "route"] as const)(
    "keeps a page that committed before the $0 changed, without hijacking the route",
    async (edit) => {
      openGraph("/graphs/A");
      const route: QueryRoute = {
        kind: "query",
        id: "query-committed",
        sourceKind: "search",
        source: "alpha",
        presentation: "board",
      };
      const [activeRoute, setActiveRoute] = createSignal<QueryRoute>(route);
      const { deps, open } = gatedDeps("save");
      const router = routerMock(route);
      vi.mocked(router.route).mockImplementation(activeRoute);
      const root = document.createElement("div");
      document.body.append(root);
      const dispose = render(
        () => <QueryWorkspace route={activeRoute()} router={router} deps={deps} />, root);
      await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

      const beforeInventory = pageInventoryRev();
      submitSave(root, "Saved search");
      // `savePage` has already begun: from here the page may legitimately land.
      await waitFor(() => expect(deps.savePage).toHaveBeenCalledTimes(1));
      if (edit === "source") typeInto(root.querySelector<HTMLInputElement>(".query-workspace-source")!, "beta");
      else setActiveRoute({ ...route, id: "query-other-tab" });
      open();

      await waitFor(() => expect(saveButton(root).disabled).toBe(false));
      // The write landed under the input it captured, was never retried under
      // the new one, and the page inventory knows about it.
      expect(deps.savePage).toHaveBeenCalledTimes(1);
      expect(vi.mocked(deps.savePage).mock.calls[0][0].blocks[0].raw)
        .toBe('{{query (search "alpha")}}\ntine.view:: board');
      expect(pageInventoryRev()).toBeGreaterThan(beforeInventory);
      expect(router.replaceActiveRoute).not.toHaveBeenCalled();
      expect(text(root, ".query-workspace-save-error")).toBe("");
      if (edit === "source") {
        // Same workspace: acknowledge the page rather than claim it was undone.
        expect(text(root, ".query-workspace-save-notice")).toContain("Saved search");
        expect(text(root, ".query-workspace-save-notice")).toContain("was saved");
      } else {
        expect(text(root, ".query-workspace-save-notice")).toBe("");
      }

      dispose();
    });

  it("lets a save land after the workspace is unmounted without routing anything", async () => {
    openGraph("/graphs/A");
    const route: QueryRoute = {
      kind: "query",
      id: "query-unmounted",
      sourceKind: "search",
      source: "alpha",
      presentation: "list",
    };
    const { deps, open } = gatedDeps("save");
    const router = routerMock(route);
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={deps} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

    submitSave(root, "Saved search");
    await waitFor(() => expect(deps.savePage).toHaveBeenCalledTimes(1));
    dispose();
    open();
    await new Promise((resolve) => setTimeout(resolve, 0));

    expect(deps.savePage).toHaveBeenCalledTimes(1);
    expect(router.replaceActiveRoute).not.toHaveBeenCalled();
  });

  it("opens a focus-managed modal and preserves an untouched OR search exactly", async () => {
    const route: QueryRoute = {
      kind: "query",
      id: "query-filters",
      sourceKind: "search",
      source: "Alpha -Draft OR Beta -Draft",
      presentation: "search",
    };
    const router = routerMock();
    const root = document.createElement("div");
    document.body.append(root);
    const dispose = render(() => <QueryWorkspace route={route} router={router} deps={workspaceDeps()} />, root);
    await waitFor(() => expect(root.querySelector(".query-workspace-status")?.textContent).toContain("2 results"));

    const toggle = root.querySelector(".query-advanced-toggle") as HTMLButtonElement;
    toggle.focus();
    toggle.click();
    await Promise.resolve();
    expect(root.querySelector('[role="dialog"]')).not.toBeNull();
    expect((document.activeElement as HTMLInputElement)?.value).toBe("");
    const fields = root.querySelectorAll<HTMLInputElement>(".query-friendly-fields input");
    expect(fields[1].value).toBe("alpha beta");
    expect(fields[3].value).toBe("draft");

    const apply = [...root.querySelectorAll<HTMLButtonElement>(".query-advanced-actions button")]
      .find((button) => button.textContent === "Apply")!;
    apply.click();
    expect(router.updateActiveQuery).toHaveBeenCalledWith({
      source: "Alpha -Draft OR Beta -Draft",
      sourceKind: "search",
    });
    await Promise.resolve();
    expect(document.activeElement).toBe(toggle);

    dispose();
  });
});
