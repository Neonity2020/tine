import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

// GH #276: Logseq's navigation hotstrings beside `g j` — `gh` (home page via
// the existing resolver, journals as the default home), `gn`/`gp` (next /
// previous calendar journal day, anchored at the current journal's parsed
// date or at today outside a journal).

let appStrings: Record<string, string> = {};
let pagesByName: Record<string, unknown> = {};

vi.mock("./backend", () => ({
  backend: () => ({
    getAppString: async (key: string, fallback: string) => appStrings[key] ?? fallback,
    setAppString: async (key: string, value: string) => { appStrings[key] = value; },
    getPage: async (name: string) => pagesByName[name] ?? null,
  }),
}));

const { goAdjacentJournal, goHome } = await import("./keybindings");
const { resetPaneLayoutToSingle, paneRouter } = await import("./panes");
const { setGraphMeta } = await import("./ui");
const { setJournalTitleFormat } = await import("./journal");
const { initParser } = await import("./render/parse");

const ROOT = "/journal-hotstring-test";
const graphMetaLike = {
  root: ROOT,
  journal_page_title_format: "MMM do, yyyy",
} as never;

const journalRouteSnapshot = (name: string) => ({
  tabs: [{ history: [{ kind: "page" as const, name, pageKind: "journal" as const }], pos: 0, pinned: false }],
  activeIndex: 0,
});

const plainPageSnapshot = (name: string) => ({
  tabs: [{ history: [{ kind: "page" as const, name, pageKind: "page" as const }], pos: 0, pinned: false }],
  activeIndex: 0,
});

const mainRoute = () => paneRouter("main").route();

async function flushMicrotasks(rounds = 6) {
  for (let i = 0; i < rounds; i++) await Promise.resolve();
}

beforeEach(async () => {
  await initParser();
  appStrings = {};
  pagesByName = {};
  setJournalTitleFormat(undefined); // default "MMM do, yyyy"
  setGraphMeta(graphMetaLike);
  vi.useFakeTimers({ now: new Date(2026, 7, 22, 12, 0, 0) }); // Aug 22nd, 2026
  resetPaneLayoutToSingle({
    tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }],
    activeIndex: 0,
  });
});

afterEach(() => {
  vi.useRealTimers();
  resetPaneLayoutToSingle({
    tabs: [{ history: [{ kind: "journals" }], pos: 0, pinned: false }],
    activeIndex: 0,
  });
});

describe("goAdjacentJournal (GH #276 gn/gp)", () => {
  it("opens next and previous day relative to the current journal", () => {
    resetPaneLayoutToSingle(journalRouteSnapshot("Aug 20th, 2026"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ kind: "page", name: "Aug 21st, 2026", pageKind: "journal" });
    goAdjacentJournal(-1);
    expect(mainRoute()).toMatchObject({ kind: "page", name: "Aug 20th, 2026", pageKind: "journal" });
  });

  it("crosses month and year boundaries by calendar day, not epoch arithmetic", () => {
    resetPaneLayoutToSingle(journalRouteSnapshot("Jul 31st, 2026"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ name: "Aug 1st, 2026" });

    resetPaneLayoutToSingle(journalRouteSnapshot("Dec 31st, 2025"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ name: "Jan 1st, 2026" });

    resetPaneLayoutToSingle(journalRouteSnapshot("Jan 1st, 2026"));
    goAdjacentJournal(-1);
    expect(mainRoute()).toMatchObject({ name: "Dec 31st, 2025" });
  });

  it("handles leap day in both directions", () => {
    resetPaneLayoutToSingle(journalRouteSnapshot("Feb 28th, 2024"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ name: "Feb 29th, 2024" });

    resetPaneLayoutToSingle(journalRouteSnapshot("Mar 1st, 2024"));
    goAdjacentJournal(-1);
    expect(mainRoute()).toMatchObject({ name: "Feb 29th, 2024" });
  });

  it("anchors at today when invoked outside a journal", () => {
    resetPaneLayoutToSingle(plainPageSnapshot("Some Ordinary Page"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ kind: "page", name: "Aug 23rd, 2026", pageKind: "journal" });
    goAdjacentJournal(-1);
    expect(mainRoute()).toMatchObject({ kind: "page", name: "Aug 22nd, 2026", pageKind: "journal" });
  });

  it("uses the graph's configured journal title format for both parsing and emitting", () => {
    setJournalTitleFormat("yyyy-MM-dd");
    resetPaneLayoutToSingle(journalRouteSnapshot("2026-01-31"));
    goAdjacentJournal(1);
    expect(mainRoute()).toMatchObject({ name: "2026-02-01", pageKind: "journal" });
  });
});

describe("goHome (GH #276 gh)", () => {
  it("opens the configured graph home page through the existing resolver", async () => {
    appStrings[`home.page.${ROOT}`] = "Home Page";
    pagesByName["Home Page"] = { name: "Home Page", kind: "page" };

    goHome();
    await flushMicrotasks();

    expect(mainRoute()).toMatchObject({ kind: "page", name: "Home Page", pageKind: "page" });
  });

  it("falls back to the journals landing when no home page is configured", async () => {
    goHome();
    await flushMicrotasks();

    expect(mainRoute()).toMatchObject({ kind: "journals" });
  });

  it("falls back to the journals landing when the configured page no longer resolves", async () => {
    appStrings[`home.page.${ROOT}`] = "Deleted Page";

    goHome();
    await flushMicrotasks();

    expect(mainRoute()).toMatchObject({ kind: "journals" });
  });

  it("does not override navigation made while the home page is resolving", async () => {
    appStrings[`home.page.${ROOT}`] = "Home Page";
    let resolveHome!: (page: unknown) => void;
    pagesByName["Home Page"] = new Promise((resolve) => { resolveHome = resolve; });

    goHome();
    await flushMicrotasks(2);
    resetPaneLayoutToSingle(plainPageSnapshot("User Chose This"));
    resolveHome({ name: "Home Page", kind: "page" });
    await flushMicrotasks();

    expect(mainRoute()).toMatchObject({ kind: "page", name: "User Chose This", pageKind: "page" });
  });
});
