import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { render } from "solid-js/web";
import { backend } from "../backend";
import { clearTransientLayersForTest } from "../transientLayers";
import { endEdit } from "../editorController";
import type { Filter, Query } from "../editor/queryIr";
import { initParser } from "../render/parse";
import { backendReadsQueries, blockRunResult } from "../queryReadingsTestkit";
import { resetSharedQueryResultsForTests } from "../queryResultCache";
import {
  doc,
  redo,
  resetStore,
  setDoc,
  undo,
  type FeedPage,
  type Node as StoreNode,
} from "../store";
import { mainPaneRouter, resetTabsToJournals } from "../router";
import type { PageDto } from "../types";
import { PageView } from "./Page";

beforeAll(async () => {
  await initParser();
});

const PAGE = "P6 undo continuity";
const INITIAL_ARGUMENT =
  "@block and content = 'alpha' and content = 'beta' and content = 'gamma' and content = 'delta'";
const DISABLED_ARGUMENT =
  "@block and Off(content = 'alpha') and content = 'beta' and content = 'gamma' and content = 'delta'";
const INITIAL_RAW = `{{tine-query ${INITIAL_ARGUMENT}}}`;
const DISABLED_RAW = `{{tine-query ${DISABLED_ARGUMENT}}}`;

const content = (text: string): Filter => ({
  kind: "leaf",
  leaf: { kind: "attr", attr: "content", op: "eq", value: { kind: "text", text } },
});
const enabledFilter: Filter = {
  kind: "and",
  items: [content("alpha"), content("beta"), content("gamma"), content("delta")],
};
const disabledFilter: Filter = {
  kind: "and",
  items: [{ kind: "off", inner: content("alpha") }, content("beta"), content("gamma"), content("delta")],
};

function node(id: string, raw: string): StoreNode {
  return { id, raw, collapsed: false, parent: null, page: PAGE, children: [] };
}

function feedPage(): FeedPage {
  return {
    name: PAGE,
    kind: "page",
    title: PAGE,
    preBlock: null,
    roots: ["query", "unrelated"],
    format: "md",
    readOnly: false,
    guide: false,
  };
}

function pageDto(): PageDto {
  return {
    name: PAGE,
    kind: "page",
    title: PAGE,
    pre_block: null,
    format: "md",
    blocks: [
      { id: "query", raw: INITIAL_RAW, collapsed: false, children: [] },
      { id: "unrelated", raw: "Unrelated bytes stay exact", collapsed: false, children: [] },
    ],
  };
}

function printedArgument(query: Query): string {
  const first = query.filter.kind === "and" ? query.filter.items[0] : undefined;
  return first?.kind === "off" ? DISABLED_ARGUMENT : INITIAL_ARGUMENT;
}

function firstEnabled(): HTMLButtonElement {
  const control = document.querySelector<HTMLButtonElement>(
    '.qs-sheet > .qs-rows > .qs-row[data-qs-parent=""][data-row-index="0"] .qs-enabled',
  );
  if (!control) throw new Error("the first P6 enabled switch is absent");
  return control;
}

async function expectRowState(enabled: boolean): Promise<void> {
  await vi.waitFor(() => {
    expect(firstEnabled().getAttribute("aria-checked")).toBe(enabled ? "true" : "false");
  });
}

afterEach(() => {
  clearTransientLayersForTest();
  resetSharedQueryResultsForTests();
  endEdit("blur");
  resetStore();
  resetTabsToJournals();
  vi.restoreAllMocks();
  localStorage.clear();
  document.body.replaceChildren();
});

describe("PageView query undo lifecycle", () => {
  it("keeps the open sheet mounted while Off save, undo, and redo replay its page snapshot", async () => {
    const dto = pageDto();
    setDoc({
      byId: {
        query: node("query", INITIAL_RAW),
        unrelated: node("unrelated", dto.blocks[1]!.raw),
      },
      pages: [feedPage()],
      feed: [PAGE],
      loaded: true,
    });

    vi.spyOn(backend(), "getPage").mockResolvedValue(dto);
    vi.spyOn(backend(), "queryRun").mockResolvedValue(blockRunResult([]));
    vi.spyOn(backend(), "queryOgExpressible").mockResolvedValue(false);
    vi.spyOn(backend(), "printQuery").mockImplementation(async (query) => printedArgument(query));
    backendReadsQueries({
      [INITIAL_ARGUMENT]: { form: INITIAL_ARGUMENT, kind: "tql", filter: enabledFilter },
      [DISABLED_ARGUMENT]: { form: DISABLED_ARGUMENT, kind: "tql", filter: disabledFilter },
    });

    mainPaneRouter.replaceActiveRoute({ kind: "page", name: PAGE, pageKind: "page" });
    const host = document.createElement("div");
    document.body.append(host);
    const dispose = render(() => <PageView />, host);

    try {
      const gear = await vi.waitFor(() => {
        const found = host.querySelector<HTMLButtonElement>(".qs-gear");
        if (!found) throw new Error("the routed query block did not render its builder");
        return found;
      });
      gear.click();
      const sheet = await vi.waitFor(() => {
        const found = document.querySelector<HTMLElement>('.qs-sheet[aria-label="Query filter"]');
        if (!found) throw new Error("the query sheet did not open");
        return found;
      });
      expect(gear.getAttribute("aria-expanded")).toBe("true");
      await expectRowState(true);

      firstEnabled().click();
      await vi.waitFor(() => expect(doc.byId.query.raw).toBe(DISABLED_RAW));
      await expectRowState(false);
      expect(host.querySelector(".qs-gear")).toBe(gear);
      expect(document.querySelector('.qs-sheet[aria-label="Query filter"]')).toBe(sheet);
      expect(doc.byId.unrelated.raw).toBe("Unrelated bytes stay exact");

      undo();
      await vi.waitFor(() => expect(doc.byId.query.raw).toBe(INITIAL_RAW));
      await expectRowState(true);
      expect(host.querySelector(".qs-gear")).toBe(gear);
      expect(document.querySelector('.qs-sheet[aria-label="Query filter"]')).toBe(sheet);
      expect(gear.getAttribute("aria-expanded")).toBe("true");
      expect(sheet.isConnected).toBe(true);
      expect(doc.byId.unrelated.raw).toBe("Unrelated bytes stay exact");

      redo();
      await vi.waitFor(() => expect(doc.byId.query.raw).toBe(DISABLED_RAW));
      await expectRowState(false);
      expect(host.querySelector(".qs-gear")).toBe(gear);
      expect(document.querySelector('.qs-sheet[aria-label="Query filter"]')).toBe(sheet);
      expect(gear.getAttribute("aria-expanded")).toBe("true");
      expect(sheet.isConnected).toBe(true);
      expect(doc.byId.unrelated.raw).toBe("Unrelated bytes stay exact");
    } finally {
      dispose();
    }
  });
});
