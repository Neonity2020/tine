import { For, Show, Switch, Match, createEffect, createMemo, createResource, createSignal, useContext, createUniqueId, onCleanup, onMount, type JSX } from "solid-js";
import { backend, QueryPrintRefusedError } from "../backend";
import { focusedRouter, openRouteInOtherPane } from "../panes";
import { openPageTarget, openPageAtBlock, openPageTargetInNewTab, openInNewTab } from "../router";
import { openPageInSidebar, openBlockInSidebar, openPageContextMenu, dataRev, graphEpoch, graphMeta, pageIdentityKey } from "../ui";
import { blockProperty, doc, formatForPage, formatForBlock, pageByName, resolveGuidePageDto, setBlockProperty, setRaw, withUndoUnit } from "../store";
import { resolveBlockBatched } from "../resolveBatch";
import { internalLinkAuxClick, internalLinkDest, internalLinkMouseDown } from "../linkGesture";
import { shouldOpenTextContextMenu } from "../contextMenuPolicy";
import { LiveRefGroup } from "./LiveRefGroup";
import { QueryBuilder } from "./QueryBuilder";
import { SearchResultRow } from "./SearchResultRow";
import {
  advancedToClause,
  clearSimpleForm,
  getSimpleForm,
  toDsl,
} from "../editor/queryBuilder";
import { foldAggregate, groupRows, type AggDirective } from "../editor/queryAggregate";
import { quoteEdnString, unquoteEdnString } from "../editor/edn";
import { queryMacroExtents, QUERY_MACRO_NAMES } from "../editor/queryMacro";
import {
  macroTextDialect,
  sourceOptions,
  sourceOriginal,
  sourcePrintDialect,
  type Query,
  type Source,
  type ViewSettings,
} from "../editor/queryIr";
import { visibleBody } from "../render/block";
import { facetsOf } from "../render/facets";
import { sheetConfig } from "../sheet/config";
import { InlineText } from "../render/inline";
import { SheetTable } from "./SheetTable";
import { SheetBoard } from "./SheetBoard";
import { SheetContainer } from "./SheetContainer";
import type { PageKind, RefGroup } from "../types";
import { sharedQueryResult } from "../queryResultCache";
import { savedDslToFriendlySearch } from "../editor/searchQuery";
import type { QueryExecution, QueryHit } from "../types";
import { LinkDepthContext, LinkDepthWarning, MAX_DEPTH_OF_LINKS } from "./linkDepth";
import { blockDtoExternalId } from "../blockIdentity";
import { ExternalLink } from "./ExternalLink";

// Recognize the typed Logseq input without treating an example in a string or
// `;;` comment as live. Only a direct token in the :inputs vector makes query
// execution depend on focused-pane navigation.
function declaresCurrentPageInput(source: string): boolean {
  const boundary = (ch: string | undefined) =>
    ch === undefined || /[\s,\[\](){}]/.test(ch);
  let i = 0;
  while (i < source.length) {
    if (source[i] === '"') {
      i += 1;
      while (i < source.length) {
        if (source[i] === "\\") i += 2;
        else if (source[i] === '"') {
          i += 1;
          break;
        } else i += 1;
      }
      continue;
    }
    if (source[i] === ";") {
      while (i < source.length && source[i] !== "\n") i += 1;
      continue;
    }
    if (
      source.startsWith(":inputs", i) &&
      boundary(source[i - 1]) &&
      boundary(source[i + ":inputs".length])
    ) {
      let cursor = i + ":inputs".length;
      while (cursor < source.length && /[\s,]/.test(source[cursor])) cursor += 1;
      if (source[cursor] !== "[") return false;
      let depth = 1;
      cursor += 1;
      while (cursor < source.length && depth > 0) {
        if (source[cursor] === '"') {
          cursor += 1;
          while (cursor < source.length) {
            if (source[cursor] === "\\") cursor += 2;
            else if (source[cursor] === '"') {
              cursor += 1;
              break;
            } else cursor += 1;
          }
          continue;
        }
        if (source[cursor] === ";") {
          while (cursor < source.length && source[cursor] !== "\n") cursor += 1;
          continue;
        }
        if ("[({".includes(source[cursor])) depth += 1;
        else if ("])}".includes(source[cursor])) depth -= 1;
        else if (
          depth === 1 &&
          source.slice(cursor, cursor + ":current-page".length).toLowerCase() ===
            ":current-page" &&
          boundary(source[cursor - 1]) &&
          boundary(source[cursor + ":current-page".length])
        ) {
          return true;
        }
        cursor += 1;
      }
      return false;
    }
    i += 1;
  }
  return false;
}

type QueryView = "search" | "list" | "table" | "board";
const QUERY_VIEWS: QueryView[] = ["search", "list", "table", "board"];
const QUERY_VIEW_LABEL: Record<QueryView, string> = {
  search: "Search",
  list: "List",
  table: "Table",
  board: "Board",
};

// Collapsed state for query results, keyed by graph + rendered query identity.
// A raw query-string key made unrelated dashboards across pages/graphs collide.
const QCOLLAPSE_KEY = "logseq-claude.queryCollapsed";
function loadCollapsed(key: string): boolean | null {
  try {
    const m = JSON.parse(localStorage.getItem(QCOLLAPSE_KEY) ?? "{}");
    return typeof m[key] === "boolean" ? m[key] : null;
  } catch {
    return null;
  }
}
function saveCollapsed(key: string, v: boolean) {
  try {
    const m = JSON.parse(localStorage.getItem(QCOLLAPSE_KEY) ?? "{}");
    // Keep explicit false: it overrides a source `:collapsed? true` default on
    // remount. Deleting false made an expanded query re-collapse immediately.
    m[key] = v;
    localStorage.setItem(QCOLLAPSE_KEY, JSON.stringify(m));
  } catch {
    // ignore
  }
}

interface Row {
  page: string;
  kind: PageKind;
  path?: string;
  text: string;
  props: Record<string, string>;
}

/** Remove the block a query is written in from that query's own results.
 *
 *  `{{query "xyz"}}` contains `xyz`, so the block matches its own query and the
 *  backend says so honestly. Rendering that match renders the page the query
 *  lives on, which renders the query, which lists the page again — the
 *  recursion in GH #469. OG removes exactly the host block for the same reason
 *  and states it where it does it (`frontend/components/query/result.cljs` at
 *  `6e7afa8e`: "exclude the current one, otherwise it'll loop forever"). Only
 *  the block itself goes; its children are ordinary results.
 *
 *  Returns the input unchanged when nothing matches, so a query whose results
 *  never contain its host keeps referential equality for downstream memos. */
export function withoutHostBlock(groups: RefGroup[], hostBlockId: string | undefined): RefGroup[] {
  if (!hostBlockId) return groups;
  const hosts = (group: RefGroup) => group.blocks.some((block) => block.id === hostBlockId);
  if (!groups.some(hosts)) return groups;
  return groups
    .map((group) =>
      hosts(group)
        ? { ...group, blocks: group.blocks.filter((block) => block.id !== hostBlockId) }
        : group,
    )
    .filter((group) => group.blocks.length > 0);
}

// A {{query ...}} block: runs the query and renders matching blocks as a list
// or a sortable table. When `blockId` is given (the block is a standalone query
// block, not an inline-in-text macro) an interactive builder bar is shown and
// edits rewrite the {{query ...}} macro in that block's raw text.
export function QueryMacro(props: {
  body: string;
  /** The macro name this query was AUTHORED under (§7.9). Supplied by the render
   *  dispatch, which recovered it from the raw source alongside the argument.
   *  Absent for callers that build a body string themselves; the name is then
   *  read back off `body`, and failing that defaults to the legacy spelling. */
  macroName?: string;
  blockId?: string;
  title?: string;
  /** Read-only query surfaces can supply page context without a blockId, which
   * would incorrectly enable editing controls. */
  currentPage?: string;
  /** BEGIN_QUERY must never execute a partially understood query or expose its
   * authored payload in an error. The ordinary {{query}} path keeps its existing
   * partial-query diagnostics unless these read-only options are requested. */
  strictAdvanced?: boolean;
  unsupportedLabel?: string;
  // When set, render nothing at all if the query has no results (used for the
  // app-inserted journal agenda, which should disappear once vacated — unlike a
  // user-authored {{query}} block, which keeps showing "No results" so it stays
  // editable).
  hideWhenEmpty?: boolean;
}): JSX.Element {
  const linkDepth = useContext(LinkDepthContext);
  if (linkDepth > MAX_DEPTH_OF_LINKS) return <LinkDepthWarning />;

  // §7.9: strip whichever query macro name this block was authored under, not a
  // hard-coded `query`. A `{{tine-query …}}` body whose name was not stripped
  // would be handed to the parser as `tine-query @block and …`, which is not a
  // query in any grammar.
  const macroName = (): string => {
    if (props.macroName) return props.macroName;
    const authored = QUERY_MACRO_NAMES.find((name) =>
      new RegExp(`^${name}(\\s|$)`, "i").test(props.body.trim()),
    );
    return authored ?? QUERY_MACRO_NAMES[0];
  };
  const arg = () =>
    props.body.trim().replace(new RegExp(`^${macroName()}\\s*`, "i"), "").trim();
  // The host block's `tine.*` properties, which §4.1 gives precedence over the
  // directives lifted from the query text. Merging the two is the engine's job,
  // so they are handed to it rather than reconciled here.
  const blockDirectives = createMemo<[string, string][]>(() => {
    const id = props.blockId;
    const node = id ? doc.byId[id] : undefined;
    if (!id || !node) return [];
    return facetsOf(node.raw, formatForBlock(id)).properties.filter(([key]) =>
      key.startsWith("tine."),
    );
  });
  // **The macro argument is read by the ONE engine (§7.1, X4).** Where the
  // trailing options map begins, and whether a `{{query …}}` holds the OG DSL or
  // advanced datalog, are query-language questions that Rust already answers in
  // `query_parse`. This component used to answer both a second time — with
  // `splitTrailingMap` and an `ADVANCED_RE` regex — and the two answers differed
  // on exactly the inputs that matter (a literal `}` inside a string, a `:find`
  // inside a string). Both twins are gone (I-12, D-14).
  const parseRequest = createMemo(() => ({
    argument: arg(),
    name: macroName(),
    properties: blockDirectives(),
  }));
  const [parsed] = createResource(parseRequest, (request) =>
    backend().parseQuery(request.argument, macroTextDialect(request.name), request.properties),
  );
  // `latest` rather than `parsed()`: a re-parse after an edit keeps the previous
  // reading visible instead of blanking the query for a frame.
  const source = (): Source | undefined => parsed.latest?.query.source;
  const view = (): ViewSettings => parsed.latest?.view ?? {};
  const form = (): string => {
    const s = source();
    return (s ? sourceOriginal(s) : null) ?? "";
  };
  const opts = (): string => {
    const s = source();
    return s ? sourceOptions(s) : "";
  };
  // `:title` / `:collapsed?` / `:table-view?` are read out of the OPAQUE options
  // map, which the engine carries verbatim and deliberately does not interpret
  // (§4.3, Y2). There is no Rust answer being duplicated here — reading three
  // display keys out of the author's own map is this component's own question.
  const titleOption = (): string | undefined => {
    const m = /:title\s+"((?:[^"\\]|\\.)*)"/.exec(opts());
    return m ? unquoteEdnString(m[1]) : undefined;
  };
  const collapsedOption = () => /:collapsed\?\s+true/.test(opts());
  const tableViewOption = () => /:table-view\?\s+true/.test(opts());
  // GH #301: `<% current page %>` inside a query binds to the FOCUSED pane's
  // route page and re-runs on navigation. Substitution is execution-only —
  // authoring text and every editing/display derivation keep the literal dyvar
  // (the same house rule as template insertion in editor/templateVars.ts).
  const currentPageMarker = createMemo(() => /<%\s*current page\s*%>/i.test(arg()));
  const focusedQueryPage = () => {
    const r = focusedRouter().route();
    return r.kind === "page" ? r.name : undefined;
  };
  const executableForm = createMemo(() => {
    const f = form();
    if (!currentPageMarker()) return f;
    const pageName = focusedQueryPage();
    if (!pageName) return f; // no focused page: leave verbatim, like templates
    return f.replace(/<%\s*current page\s*%>/gi, `[[${pageName}]]`);
  });
  // The query LANGUAGE decision rides the same substituted form the execution
  // uses (never authoring rewrites): presentation and execution can't disagree
  // about what ran (GH #301).
  const friendlySearch = createMemo(() => savedDslToFriendlySearch(executableForm()));
  const sheet = createMemo(() => {
    if (!props.blockId || !doc.byId[props.blockId]) return null;
    return sheetConfig(facetsOf(doc.byId[props.blockId].raw, formatForBlock(props.blockId)).properties);
  });
  const currentView = (): QueryView => {
    if (!props.blockId) return "list";
    const view = blockProperty(props.blockId, "tine.view");
    return view === "search" || view === "table" || view === "board" ? view : "list";
  };
  const sheetFace = () => currentView() === "table" || currentView() === "board";
  const legacyTable = () => currentView() === "list" && tableViewOption();
  const setQueryView = (next: QueryView) => {
    const blockId = props.blockId;
    if (!blockId) return;
    const node = doc.byId[blockId];
    if (!node) return;
    const storedView = blockProperty(blockId, "tine.view");
    if ((next === "list" && storedView === null) || (next !== "list" && storedView === next)) return;
    withUndoUnit(`query:view:${next}`, [node.page], () => {
      if (next === "list") {
        setBlockProperty(blockId, "tine.view", null);
        return;
      }
      setBlockProperty(blockId, "tine.view", next);
      if (next === "board" && blockProperty(blockId, "tine.group-by") === null) {
        setBlockProperty(blockId, "tine.group-by", "state");
      }
    });
  };

  // Rewrite just THIS {{query ...}} macro inside the owning block, preserving the
  // front-matter options and surrounding property lines (id::/collapsed::). The
  // extents are found brace/string/page-ref-aware (queryMacroExtents), NOT a lazy
  // regex. A block can hold more than one query, so target the extent whose
  // current body matches OURS (props.body) — editing the 2nd query must not
  // rewrite the 1st. Falls back to the only/first query for the common case.
  const rewriteMacro = (newMacro: string) => {
    if (!props.blockId) return;
    const raw = doc.byId[props.blockId]?.raw ?? "";
    const extents = queryMacroExtents(raw);
    if (!extents.length) return;
    // Target by the extent's own recovered name+argument rather than by a
    // whitespace-normalized slice of the source: that is the same pair the
    // renderer handed this component as `body`, so the match is exact even when
    // two macros differ only inside a string literal.
    const norm = (s: string) => s.replace(/\s+/g, " ").trim();
    const mine = norm(props.body);
    const target =
      extents.find((e) => norm(`${e.name} ${e.argument}`) === mine)
      ?? extents.find((e) => norm(raw.slice(e.start + 2, e.end - 2)) === mine)
      ?? extents[0];
    setRaw(props.blockId, raw.slice(0, target.start) + newMacro + raw.slice(target.end));
  };
  const applyDsl = (dsl: string) => {
    const options = opts() ? ` ${opts()}` : "";
    // Re-emit under the name the block already carries (§7.9). Promoting a
    // `{{query}}` to `{{tine-query}}` is the SAVE path's decision, made from
    // `query_og_expressible` — never a side effect of editing a chip.
    rewriteMacro(`{{${macroName()} ${dsl}${options}}}`);
  };
  // Edit the query's display title (:title "…" in the options map). Only offered
  // for a user-authored standalone query (blockId set, no app-supplied title).
  const [editingTitle, setEditingTitle] = createSignal(false);
  const titleText = () => props.title ?? titleOption() ?? "Query";
  const titleEditable = () => !!props.blockId && props.title === undefined;
  // **A title edit is not a filter conversion (§4.3.1).** The new options map is
  // handed back to the printer with `preserveForm`, which re-emits
  // `source.original` verbatim and never re-lowers the IR — so renaming a query
  // the engine only partly understands cannot rewrite the author's filter, and a
  // query that OG could not express is still renameable. The dialect comes off
  // the SOURCE, not the macro name: a `{{query …}}` holding datalog prints as
  // `advanced_macro`.
  const setTitle = async (t: string) => {
    if (!props.blockId) return;
    const reading = parsed.latest;
    if (!reading) return;
    const inner = opts().replace(/^\{|\}$/g, "").trim();
    // Drop any existing :title (escape-aware), keep the other options.
    const rest = inner.replace(/:title\s+"(?:[^"\\]|\\.)*"\s*/, "").trim();
    // Strip chars that would break the {{…}} macro / {…} options map; escape the
    // rest so quotes/backslashes round-trip faithfully through a re-parse.
    const title = t.trim().replace(/[\r\n{}]/g, "");
    const parts = [title ? `:title "${quoteEdnString(title)}"` : "", rest].filter(Boolean);
    const nextOptions = parts.length ? `{${parts.join(" ")}}` : "";
    const nextQuery: Query = {
      ...reading.query,
      source: { ...reading.query.source, og_options: nextOptions } as Source,
    };
    try {
      const argument = await backend().printQuery(
        nextQuery,
        reading.view,
        sourcePrintDialect(reading.query.source),
        true,
      );
      setPrintError(null);
      rewriteMacro(`{{${macroName()} ${argument}}}`);
    } catch (error) {
      // I-4 / T7: a refused print is NEVER swallowed. Nothing is written, and the
      // reason is shown next to the edit that provoked it. A catch-all that
      // turned this into a silent no-op is how an unsaved rename looks saved.
      setPrintError(
        error instanceof QueryPrintRefusedError
          ? error.message
          : error instanceof Error
            ? error.message
            : String(error),
      );
    }
  };
  const [printError, setPrintError] = createSignal<string | null>(null);

  // Whether this is an advanced (datalog) query is the ENGINE's reading of the
  // text, not a regex over it (§7.1): a `:find` inside a string literal is text.
  const isAdvanced = () => source()?.kind === "advanced";
  const currentPageInput = createMemo(() =>
    isAdvanced() && declaresCurrentPageInput(form())
  );
  const simpleBackDsl = createMemo<string | null>(() => {
    const blockId = props.blockId;
    if (!blockId || !isAdvanced()) return null;
    const stashed = getSimpleForm(blockId);
    if (stashed !== undefined) return stashed;
    const c = advancedToClause(form());
    return c ? toDsl(c) : null;
  });
  const simpleBackTitle = () =>
    simpleBackDsl() !== null
      ? "Back to the visual query builder"
      : "This advanced query can't be converted back to the visual builder automatically — edit it as raw text, or rebuild it visually.";
  const backToSimple = (e: MouseEvent) => {
    e.stopPropagation();
    const blockId = props.blockId;
    if (!blockId) return;
    const stashed = getSimpleForm(blockId);
    if (stashed !== undefined) {
      applyDsl(stashed);
      clearSimpleForm(blockId);
      return;
    }
    const c = advancedToClause(form());
    if (c) applyDsl(toDsl(c));
  };
  const simpleBackButton = () => (
    <span
      class="query-simple-toggle-wrap"
      title={simpleBackTitle()}
      onClick={(e) => e.stopPropagation()}
    >
      <button
        type="button"
        class="qb-sort query-simple-toggle"
        title={simpleBackTitle()}
        disabled={simpleBackDsl() === null}
        onClick={backToSimple}
      >
        ← Simple
      </button>
    </span>
  );
  const currentPage = () => props.currentPage ?? (props.blockId ? doc.byId[props.blockId]?.page : undefined);
  const [advInfo, setAdvInfo] = createSignal<{ ran: string[]; ignored: string[]; supported: boolean } | null>(
    null
  );
  const [searchExecution, setSearchExecution] = createSignal<QueryExecution | null>(null);
  const collapseKey = () => JSON.stringify([
    graphMeta()?.root ?? "",
    props.blockId ?? currentPage() ?? "global",
    arg(),
  ]);
  const storedCollapse = loadCollapsed(collapseKey());
  const [collapsed, setCollapsed] = createSignal(storedCollapse ?? false);
  // `{:collapsed? true}` is an authored DEFAULT, not a reader's choice, so it
  // only applies when this reader has no stored preference for this query. It
  // can only be honoured once the engine has separated the options map from the
  // form, which is why it is seeded when the parse lands rather than at setup.
  if (storedCollapse === undefined || storedCollapse === null) {
    let seeded = false;
    createEffect(() => {
      if (seeded || !parsed.latest) return;
      seeded = true;
      if (collapsedOption()) setCollapsed(true);
    });
  }
  const toggleCollapsed = () => {
    const v = !collapsed();
    setCollapsed(v);
    saveCollapsed(collapseKey(), v);
  };
  // Re-run when the query text changes OR after any save lands (dataRev), so
  // results track edits live — e.g. a task flipped to DONE leaves a (task TODO)
  // query. createResource keeps the previous value during refetch (no flicker).
  // A COLLAPSED query keys off the form only (no dataRev), so it fetches once for
  // its count and doesn't re-run a whole-graph scan on every save while hidden;
  // expanding it (key flips to include dataRev) refreshes it.
  // Nothing runs before the engine has read the text: the form the executor is
  // given is `source.original`, and until the parse lands there is no form —
  // only the raw argument, which still carries the options map. Returning
  // `undefined` keeps `createResource` from fetching at all, rather than running
  // a query nobody authored.
  const queryRequestKey = (): string | undefined => {
    if (!parsed.latest) return undefined;
    return `${graphEpoch()}\0${collapsed() ? `collapsed ${form()}` : `${form()} ${dataRev()}`}${currentPageMarker() || currentPageInput() ? `\0cp:${focusedQueryPage() ?? ""}` : ""}`;
  };
  const fetchGroups = async (requestKey: string): Promise<RefGroup[]> => {
    {
      const scope = `${graphMeta()?.root ?? ""}\0${graphEpoch()}`;
      const searchSource = friendlySearch();
      if (searchSource !== null) {
        setAdvInfo(null);
        const execution = await sharedQueryResult(
          scope,
          `friendly-search\0${requestKey}`,
          () => backend().runGraphSearch(
            searchSource,
            500,
            5_000,
            `inline-query:${props.blockId ?? currentPage() ?? "global"}`,
            false
          ),
        );
        if (queryRequestKey() !== requestKey) return [];
        // The Search presentation renders these hits directly rather than the
        // RefGroups below, so the host block has to come out here too — the same
        // exclusion `withoutHostBlock` makes, at the other place membership is
        // decided (GH #469). Diagnostics and the explanation are untouched: the
        // hit was really found, it is just not shown to itself.
        const hits = props.blockId
          ? execution.hits.filter((hit) => !(hit.entity === "block" && hit.block.id === props.blockId))
          : execution.hits;
        setSearchExecution(hits.length === execution.hits.length ? execution : { ...execution, hits });
        const grouped = new Map<string, RefGroup>();
        for (const hit of hits) {
          if (hit.entity !== "block") continue;
          const key = `${hit.kind}\0${hit.page}\0${hit.path ?? ""}`;
          const group = grouped.get(key) ?? { page: hit.page, kind: hit.kind, path: hit.path, blocks: [] };
          group.blocks.push(hit.block);
          grouped.set(key, group);
        }
        return [...grouped.values()];
      }
      setSearchExecution(null);
      // Advanced (datalog) queries take a separate path that maps the supported
      // clause subset onto the engine and reports what ran vs was ignored.
      if (isAdvanced()) {
        // `:inputs [:current-page]` is a focused-pane binding. Advanced forms
        // without it retain the owner page for :query-page compatibility.
        const page = currentPageInput() ? focusedQueryPage() : currentPage();
        const r = await sharedQueryResult(
          scope,
          `advanced\0${page ?? ""}\0${requestKey}`,
          () => backend().runAdvancedQuery(executableForm(), page),
        );
        if (queryRequestKey() !== requestKey) return [];
        setAdvInfo({ ran: r.ran, ignored: r.ignored, supported: r.supported });
        return r.groups;
      }
      setAdvInfo(null);
      return sharedQueryResult(scope, `simple\0${requestKey}`, () => backend().runQuery(executableForm()));
    }
  };
  // A query must not return the block it is written in. `{{query "xyz"}}`
  // contains `xyz`, so the backend answers honestly and the block matches its
  // own query — then the result renders the page it lives on, which renders the
  // query, which lists the page again. OG removes exactly the host block for
  // this reason and says so where it does it (frontend/components/query/result.cljs
  // at 6e7afa8e: "exclude the current one, otherwise it'll loop forever"). Its
  // children are NOT removed; only the block itself. Applied once here, after
  // every fetch path, rather than in each of the three (GH #469).
  const [groups] = createResource(
    queryRequestKey,
    async (requestKey) => withoutHostBlock(await fetchGroups(requestKey), props.blockId),
  );
  const groupsError = () => {
    const error = groups.error;
    if (!error) return null;
    const message = error instanceof Error ? error.message : String(error);
    const oversized = message.startsWith("result-too-large:");
    return {
      lead: oversized ? "Query result is too large to display safely:" : "Query couldn't be loaded:",
      message: message.replace(/^result-too-large:\s*/, ""),
    };
  };
  // Presentation never changes membership. Canonical `(search "…")` queries
  // already carry page/block hits and match evidence from QueryPlan. Ordinary
  // DSL queries return RefGroups, so adapt those same blocks into evidence-free
  // search rows instead of making the Search presentation appear empty.
  const searchPresentationHits = createMemo<QueryHit[]>(() => {
    if (friendlySearch() !== null) return searchExecution()?.hits ?? [];
    return (groups() ?? []).flatMap((group) => group.blocks.map((block) => ({
      entity: "block" as const,
      page: group.page,
      kind: group.kind,
      block,
      display_text: visibleBody(block.raw).join(" "),
      evidence: [],
    })));
  });
  const total = () => currentView() === "search"
    ? searchPresentationHits().length
    : groups()?.reduce((a, g) => a + g.blocks.length, 0) ?? 0;
  // A `(sort-by …)` query is sorted GLOBALLY by the engine and returned as one
  // block per group in that order — so the list view must render flat (a single
  // ordered sequence with a per-row page breadcrumb), not grouped by page, or the
  // global order would be lost to page headers.
  // The engine sorts GLOBALLY when the view carries a sort, returning one block
  // per group in that order — so the list view must render flat (one ordered
  // sequence with a per-row breadcrumb) or the global order is lost to page
  // headers. Which is read off the parsed view, not re-detected in the text.
  const globalSort = createMemo(() => (view().sort ?? []).length > 0);
  const queryGroupKey = (group: RefGroup, flat: boolean) =>
    flat
      ? `${group.kind}\0${group.page}\0${group.path ?? ""}\0${group.blocks.map((block) => block.id).join("\0")}`
      : `${group.kind}\0${group.page}\0${group.path ?? ""}`;
  const groupedQueryByKey = createMemo(() =>
    new Map((groups() ?? []).map((group) => [queryGroupKey(group, false), group] as const))
  );
  const flatQueryByKey = createMemo(() =>
    new Map((groups() ?? []).map((group) => [queryGroupKey(group, true), group] as const))
  );
  const [sortCol, setSortCol] = createSignal<string>("");
  const [sortDir, setSortDir] = createSignal(1);

  const rows = createMemo<Row[]>(() =>
    (groups() ?? []).flatMap((g) =>
      g.blocks.map((b) => {
        // Properties come off the DTO (computed once in Rust off the lsdoc parse);
        // the row's text is the visible body. No re-derivation here.
        const props: Record<string, string> = {};
        for (const [k, val] of b.properties ?? []) props[k] = val;
        return { page: g.page, kind: g.kind, path: g.path, text: visibleBody(b.raw).join(" "), props };
      })
    )
  );

  const cols = createMemo(() => {
    const keys = new Set<string>();
    for (const r of rows()) for (const k of Object.keys(r.props)) keys.add(k);
    return Array.from(keys);
  });

  // Result summarization (1a): `aggregate` / `group-by` are VIEW settings, lifted
  // out of the query text (or the block's `tine.*` properties) by the engine and
  // returned alongside the IR. The engine returns the full block set and ignores
  // them, so the math is computed HERE from the returned rows. Only the simple
  // DSL carries them (datalog aggregation is OG's :result-transform, which we
  // list as ignored).
  const directives = createMemo<{ agg: AggDirective | null; group: string | null }>(() => {
    if (isAdvanced()) return { agg: null, group: null };
    const settings = view();
    const [field, fn] = settings.aggregates?.[0] ?? [];
    return {
      agg: fn ? { agg: fn, field: field ?? null } : null,
      group: settings.group_by ?? null,
    };
  });
  const aggLabel = () => {
    const a = directives().agg;
    if (!a || a.agg === "count") return "Count";
    return `${a.agg === "sum" ? "Sum" : "Avg"} of ${a.field}`;
  };
  type Summary =
    | { kind: "single"; text: string; skipped: number }
    | { kind: "grouped"; field: string; groups: { key: string; text: string; skipped: number }[] };
  const summary = createMemo<Summary | null>(() => {
    const d = directives();
    if (!d.agg && !d.group) return null;
    if (!d.group) return { kind: "single", ...foldAggregate(rows(), d.agg) };
    return {
      kind: "grouped",
      field: d.group,
      groups: Array.from(groupRows(rows(), d.group).entries()).map(([key, set]) => ({
        key,
        ...foldAggregate(set, d.agg),
      })),
    };
  });
  const summarySingle = () => {
    const s = summary();
    return s && s.kind === "single" ? s : null;
  };
  const summaryGrouped = () => {
    const s = summary();
    return s && s.kind === "grouped" ? s : null;
  };

  const sorted = createMemo(() => {
    const c = sortCol();
    if (!c) return rows();
    const val = (r: Row) => (c === "page" ? r.page : c === "content" ? r.text : r.props[c] ?? "");
    return [...rows()].sort((a, b) => val(a).localeCompare(val(b)) * sortDir());
  });

  const sortBy = (c: string) => {
    if (sortCol() === c) setSortDir(-sortDir());
    else {
      setSortCol(c);
      setSortDir(1);
    }
  };
  // Clicks on query controls must not bubble to the block's onClick (which would
  // start editing the {{query}} block and replace results with raw markdown).
  const stop = (e: MouseEvent) => e.stopPropagation();
  const arrow = (c: string) => (sortCol() === c ? (sortDir() > 0 ? " ▲" : " ▼") : "");

  // Hide the whole block when asked and there's nothing to show (advanced
  // queries still render their "unsupported" notice).
  const hidden = () => props.hideWhenEmpty && !isAdvanced() && total() === 0;
  const unsupportedAdvanced = () => isAdvanced() && advInfo() && (
    !advInfo()!.supported || (props.strictAdvanced === true && advInfo()!.ignored.length > 0)
  );

  return (
    <Show when={!hidden()}>
      <div class="query-block" classList={{ "query-sheet-block": sheetFace() }}>
        <Switch>
          <Match when={unsupportedAdvanced()}>
            <div class="query-unsupported" role={props.unsupportedLabel ? "alert" : undefined}>
              <Show when={props.blockId}>{simpleBackButton()}</Show>
              <Show
                when={props.unsupportedLabel}
                fallback={<>Advanced (datalog) query: no supported clauses. <code>{`{{${props.body}}}`}</code></>}
              >
                {(label) => <>{label()}: query contains unsupported clauses.</>}
              </Show>
            </div>
          </Match>
          <Match when={true}>
            <Show when={isAdvanced() && advInfo()?.supported}>
              <div class="query-adv-note">
                <Show when={props.blockId}>{simpleBackButton()}</Show>
                Partial datalog — ran: {advInfo()!.ran.join(", ") || "—"}
                <Show when={advInfo()!.ignored.length > 0}>
                  {` · ignored: ${advInfo()!.ignored.join(", ")}`}
                </Show>
              </div>
            </Show>
            <div class="query-header">
              <span
                class="query-collapse"
                classList={{ collapsed: collapsed() }}
                title={collapsed() ? "Expand results" : "Collapse results"}
                onClick={(e) => {
                  e.stopPropagation();
                  toggleCollapsed();
                }}
              >
                <svg viewBox="0 0 24 24" class="triangle">
                  <path d="M8 5l8 7-8 7z" />
                </svg>
              </span>
              <Show
                when={editingTitle()}
                fallback={
                  <span
                    class="query-title"
                    classList={{ "query-title-editable": titleEditable() }}
                    title={titleEditable() ? "Click to rename this query" : undefined}
                    onClick={(e) => {
                      if (titleEditable()) {
                        e.stopPropagation();
                        setEditingTitle(true);
                      }
                    }}
                  >
                    {titleText()}
                  </span>
                }
              >
                {(() => {
                  let canceled = false;
                  return (
                    <input
                      class="query-title-input"
                      autofocus
                      value={titleOption() ?? ""}
                      placeholder="Query title"
                      onClick={(e) => e.stopPropagation()}
                      onKeyDown={(e) => {
                        e.stopPropagation();
                        if (e.key === "Enter") {
                          void setTitle(e.currentTarget.value);
                          setEditingTitle(false);
                        } else if (e.key === "Escape") {
                          canceled = true;
                          setEditingTitle(false);
                        }
                      }}
                      onBlur={(e) => {
                        if (!canceled) void setTitle(e.currentTarget.value);
                        setEditingTitle(false);
                      }}
                    />
                  );
                })()}
              </Show>{" "}
              <span class="query-count">{total()}</span>
              <Show when={props.blockId}>
                <div class="query-view-switcher" role="group" aria-label="Query view" onClick={stop}>
                  <For each={QUERY_VIEWS}>
                    {(view) => (
                      <button
                        type="button"
                        classList={{ active: currentView() === view }}
                        onClick={(e) => {
                          e.stopPropagation();
                          setQueryView(view);
                        }}
                      >
                        {QUERY_VIEW_LABEL[view]}
                      </button>
                    )}
                  </For>
                </div>
              </Show>
            </div>
            {/* The visual builder only models the simple DSL. For an advanced
                (datalog) query, hide the chip bar (its clauses aren't builder-
                representable) — the block is editable as raw text by clicking it, and
                the ran/ignored note above shows which clauses took. */}
            <Show when={props.blockId && !isAdvanced()}>
              <QueryBuilder dsl={form} onChange={applyDsl} blockId={props.blockId} />
            </Show>
            <Show when={printError()}>
              {(message) => (
                <div class="query-unsupported query-print-refused" role="alert">
                  The query wasn't changed: {message()}
                </div>
              )}
            </Show>
            <Show when={groupsError()}>
              {(message) => (
                <div class="query-unsupported" role="alert">
                  {message().lead} {message().message}
                </div>
              )}
            </Show>
            <Show when={!collapsed()}>
              <Show
                when={sheetFace()}
                fallback={
                  <>
                    <Show when={currentView() === "search"}>
                      <div class="query-search-results" role="list" aria-label="Search results" onClick={stop}>
                        <Show
                          when={searchPresentationHits().length > 0}
                          fallback={<div class="query-empty">No results</div>}
                        >
                          <For each={searchPresentationHits()}>
                            {(hit) => (
                              <Show
                                when={hit.entity === "block" ? hit : null}
                                fallback={hit.entity === "page" ? (
                                  <button
                                    type="button"
                                    class="query-search-page"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      const target = {
                                        name: hit.page.name,
                                        pageKind: hit.page.kind,
                                        ...(hit.page.path ? { path: hit.page.path } : {}),
                                      };
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") openPageInSidebar(target);
                                      else if (dest === "background") openPageTargetInNewTab(target);
                                      else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target });
                                      else openPageTarget(target);
                                    }}
                                    onAuxClick={(e) => internalLinkAuxClick(e, () => openPageTargetInNewTab({
                                      name: hit.page.name,
                                      pageKind: hit.page.kind,
                                      ...(hit.page.path ? { path: hit.page.path } : {}),
                                    }))}
                                  >
                                    <span class="switcher-kind">{hit.page.kind}</span>
                                    <span>{hit.display_text}</span>
                                  </button>
                                ) : null}
                              >
                                {(blockHit) => (
                                  <button
                                    type="button"
                                    class="query-search-hit switcher-row block-result"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      const bh = blockHit();
                                      const uuid = blockDtoExternalId(bh.block);
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") {
                                        openBlockInSidebar({ uuid, page: bh.page, pageKind: bh.kind, ...(bh.path ? { path: bh.path } : {}) });
                                      } else if (dest === "background") {
                                        openInNewTab({ kind: "page", name: bh.page, pageKind: bh.kind, block: uuid, ...(bh.path ? { path: bh.path } : {}) });
                                      } else if (dest === "pane") {
                                        openRouteInOtherPane({ kind: "page", name: bh.page, pageKind: bh.kind, block: uuid, ...(bh.path ? { path: bh.path } : {}) });
                                      } else {
                                        openPageAtBlock({
                                          name: bh.page,
                                          pageKind: bh.kind,
                                          block: uuid,
                                          ...(bh.path ? { path: bh.path } : {}),
                                        });
                                      }
                                    }}
                                    onAuxClick={(e) => internalLinkAuxClick(e, () => {
                                      const bh = blockHit();
                                      openInNewTab({ kind: "page", name: bh.page, pageKind: bh.kind, block: blockDtoExternalId(bh.block), ...(bh.path ? { path: bh.path } : {}) });
                                    })}
                                  >
                                    <SearchResultRow
                                      page={blockHit().page}
                                      breadcrumb={blockHit().block.breadcrumb ?? []}
                                      text={blockHit().display_text}
                                      spans={blockHit().evidence.flatMap((evidence) => evidence.spans)}
                                    />
                                  </button>
                                )}
                              </Show>
                            )}
                          </For>
                        </Show>
                      </div>
                    </Show>
                    <Show when={currentView() !== "search"}>
                    {/* Summary panel (1a): count/sum/avg overall, or a per-group breakdown.
                        Rendered above the full result list, which stays grouped by page. */}
                    <Show when={summarySingle()}>
                      {(s) => (
                        <div class="query-summary" onClick={stop}>
                          <span class="qs-label">{aggLabel()}:</span>{" "}
                          <span class="qs-value">{s().text}</span>
                          <Show when={s().skipped > 0}>
                            <span class="qs-skip"> ({s().skipped} non-numeric skipped)</span>
                          </Show>
                        </div>
                      )}
                    </Show>
                    <Show when={summaryGrouped()}>
                      {(s) => (
                        <table class="md-table query-summary-table" onClick={stop}>
                          <thead>
                            <tr>
                              <th>{s().field}</th>
                              <th>{aggLabel()}</th>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={s().groups}>
                              {(row) => (
                                <tr>
                                  <td>{row.key}</td>
                                  <td>
                                    {row.text}
                                    <Show when={row.skipped > 0}>
                                      <span class="qs-skip"> ({row.skipped} skipped)</span>
                                    </Show>
                                  </td>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      )}
                    </Show>
                    <Show
                      when={groups() && groups()!.length > 0}
                      fallback={<div class="query-empty">No results</div>}
                    >
                      <Show
                        when={legacyTable()}
                        fallback={
                          <Show
                            when={globalSort()}
                            fallback={
                              <For each={[...groupedQueryByKey().keys()]}>
                                {(key) => <QueryGroup group={() => groupedQueryByKey().get(key)} />}
                              </For>
                            }
                          >
                            {/* Sorted: flat global order (each group holds one block). Iterate the
                                groups DIRECTLY and pass the group object — re-`find()`ing the group
                                by page/id for every row was O(groups²) on broad queries (audit #3). */}
                            <For each={[...flatQueryByKey().keys()]}>
                              {(key) => <QueryGroup group={() => flatQueryByKey().get(key)} flat />}
                            </For>
                          </Show>
                        }
                      >
                        <table class="md-table query-table">
                          <thead>
                            <tr onClick={stop}>
                              <th onClick={() => sortBy("content")}>Content{arrow("content")}</th>
                              <th onClick={() => sortBy("page")}>Page{arrow("page")}</th>
                              <For each={cols()}>
                                {(c) => <th onClick={() => sortBy(c)}>{c}{arrow(c)}</th>}
                              </For>
                            </tr>
                          </thead>
                          <tbody>
                            <For each={sorted()}>
                              {(r) => (
                                <tr>
                                  <td>
                                    <InlineText text={r.text} format={formatForPage(r.page)} />
                                  </td>
                                  <td
                                    class="qt-page"
                                    onMouseDown={internalLinkMouseDown}
                                    onClick={(e) => {
                                      e.stopPropagation();
                                      const target = { name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) };
                                      const dest = internalLinkDest(e);
                                      if (dest === "sidebar") openPageInSidebar(target);
                                      else if (dest === "background") openPageTargetInNewTab(target);
                                      else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target });
                                      else openPageTarget(target);
                                    }}
                                    onAuxClick={(e) => {
                                      if (internalLinkAuxClick(e, () =>
                                        openPageTargetInNewTab({ name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) })
                                      )) e.stopPropagation();
                                    }}
                                    onContextMenu={(e) => {
                                      if (!shouldOpenTextContextMenu(e.target)) return;
                                      e.preventDefault();
                                      e.stopPropagation();
                                      openPageContextMenu(e.clientX, e.clientY, { name: r.page, pageKind: r.kind, ...(r.path ? { path: r.path } : {}) });
                                    }}
                                  >
                                    {r.page}
                                  </td>
                                  <For each={cols()}>{(c) => <td>{r.props[c] ?? ""}</td>}</For>
                                </tr>
                              )}
                            </For>
                          </tbody>
                        </table>
                      </Show>
                    </Show>
                    </Show>
                  </>
                }
              >
                <Show when={groups() && groups()!.length > 0} fallback={<div class="query-empty">No results</div>}>
                  <Show when={(sheet()?.view === "table" || sheet()?.view === "board") && props.blockId}>
                    <SheetContainer>
                      <Switch>
                        <Match when={sheet()?.view === "table"}>
                          <SheetTable ownerId={props.blockId!} rowSource="query" groups={groups() ?? []} />
                        </Match>
                        <Match when={sheet()?.view === "board"}>
                          <SheetBoard ownerId={props.blockId!} rowSource="query" groupBy={sheet()?.groupBy} groups={groups() ?? []} />
                        </Match>
                      </Switch>
                    </SheetContainer>
                  </Show>
                </Show>
              </Show>
            </Show>
          </Match>
        </Switch>
      </div>
    </Show>
  );
}

// One page's query results, rendered as LIVE editable blocks. The result page
// is loaded into the shared working set on demand; each result is the same
// <Block> the main view uses (so editing a result edits the real block and
// saves to its page). Until the page is loaded, a read-only block stands in.
//
// Keyed by page name (outer <For>) and block uuid (inner <For>) so a reactive
// re-query that returns the same membership reuses the existing rows — it never
// re-mounts a block you're editing in a result and yanks the caret out.
function QueryGroup(props: { group: () => RefGroup | undefined; flat?: boolean }): JSX.Element {
  const kind = (): PageKind => props.group()?.kind ?? "page";
  const page = () => props.group()?.page ?? "";
  const target = () => ({ name: page(), pageKind: kind(), ...(props.group()?.path ? { path: props.group()!.path } : {}) });
  return (
    <Show when={props.group()}>
      {(g) => (
        <div class="query-group" classList={{ "query-group-flat": props.flat }}>
          <div
            class={props.flat ? "query-crumb" : "query-page"}
            onMouseDown={internalLinkMouseDown}
            onClick={(e) => {
              e.stopPropagation();
              const dest = internalLinkDest(e);
              if (dest === "sidebar") openPageInSidebar(target());
              else if (dest === "background") openPageTargetInNewTab(target());
              else if (dest === "pane") openRouteInOtherPane({ kind: "page", ...target() });
              else openPageTarget(target());
            }}
            onAuxClick={(e) => {
              if (internalLinkAuxClick(e, () => openPageTargetInNewTab(target()))) e.stopPropagation();
            }}
            onContextMenu={(e) => {
              if (!shouldOpenTextContextMenu(e.target)) return;
              e.preventDefault();
              e.stopPropagation();
              openPageContextMenu(e.clientX, e.clientY, target());
            }}
          >
            {page()}
          </div>
          <LiveRefGroup page={page()} kind={kind()} path={g().path} blocks={g().blocks} surface="query" showBreadcrumb />
        </div>
      )}
    </Show>
  );
}

interface YoutubePlayer {
  seekTo(seconds: number, allowSeekAhead: boolean): void;
  getCurrentTime(): number;
  destroy?(): void;
}

interface YoutubeApi {
  Player: new (iframeId: string, options: { events?: { onReady?: () => void } }) => YoutubePlayer;
}

type YoutubeWindow = Window & {
  YT?: YoutubeApi;
  onYouTubeIframeAPIReady?: () => void;
};

const youtubePlayers = new Map<string, YoutubePlayer>();
let youtubeApiLoading: Promise<YoutubeApi | null> | null = null;
const YOUTUBE_API_SCRIPT_ID = "tine-youtube-iframe-api";

// The API is intentionally fetched only from a mounted YouTube embed. OG does
// the same mount-time load/register sequence (og-1.0.0 6e7afa8eb,
// extensions/video/youtube.cljs:20-27, :45-53).
function loadYoutubeApi(): Promise<YoutubeApi | null> {
  if (typeof window === "undefined" || typeof document === "undefined") return Promise.resolve(null);
  const ytWindow = window as YoutubeWindow;
  if (ytWindow.YT?.Player) return Promise.resolve(ytWindow.YT);
  if (youtubeApiLoading) return youtubeApiLoading;

  youtubeApiLoading = new Promise((resolve) => {
    let settled = false;
    const settle = (api: YoutubeApi | undefined) => {
      if (settled) return;
      settled = true;
      resolve(api?.Player ? api : null);
    };
    const priorReady = ytWindow.onYouTubeIframeAPIReady;
    ytWindow.onYouTubeIframeAPIReady = () => {
      try {
        priorReady?.();
      } finally {
        settle(ytWindow.YT);
      }
    };
    let script = document.getElementById(YOUTUBE_API_SCRIPT_ID) as HTMLScriptElement | null;
    if (!script) {
      script = document.createElement("script");
      script.id = YOUTUBE_API_SCRIPT_ID;
      script.async = true;
      script.src = "https://www.youtube.com/iframe_api";
      document.head.appendChild(script);
    }
    script.addEventListener("error", () => settle(undefined), { once: true });
  });
  return youtubeApiLoading;
}

// OG's get-player selects the last YouTube iframe whose DOM position precedes
// the target (compareDocumentPosition(..., target) has FOLLOWING set), then
// looks up that iframe's registered handle (og-1.0.0 6e7afa8eb,
// extensions/video/youtube.cljs:85-101).
export function youtubePlayerForTarget(target: Node): YoutubePlayer | undefined {
  if (typeof document === "undefined" || typeof Node === "undefined") return undefined;
  const iframe = Array.from(document.getElementsByTagName("iframe"))
    .filter((node) => node.src.includes("youtube.com"))
    .filter((node) => (node.compareDocumentPosition(target) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0)
    .at(-1);
  return iframe ? youtubePlayers.get(iframe.id) : undefined;
}

// The OG generator floors getCurrentTime before it formats the macro; with no
// registered/ready player it produces NOTHING — the command is a no-op, no
// macro is inserted (og-1.0.0 6e7afa8eb, extensions/video/youtube.cljs:113-122).
export function youtubeTimestampMacroFor(target: Node): string | null {
  const seconds = youtubePlayerForTarget(target)?.getCurrentTime();
  if (typeof seconds !== "number" || !Number.isFinite(seconds)) return null;
  return `{{youtube-timestamp ${Math.max(0, Math.floor(seconds))}}}`;
}

// A {{video}} / {{youtube}} / {{vimeo}} / {{bilibili}} macro: embeds YouTube,
// Vimeo or Bilibili as an iframe and direct media files as a <video>. Each of the
// provider-named macros also accepts a bare id (e.g. `{{vimeo 12345}}`), matching
// OG; the generic `{{video URL}}` sniffs the provider from the URL. Falls back to a
// link. (`youtube-timestamp` is a SEPARATE macro — handled before this one.)
export function VideoMacro(props: { body: string }): JSX.Element {
  const iframeId = `youtube-player-${createUniqueId()}`;
  const parsed = () => {
    const m = /^(\w+)\s*([\s\S]*)$/.exec(props.body.trim());
    const name = (m?.[1] ?? "video").toLowerCase();
    const arg = (m?.[2] ?? "").trim().replace(/^\[\[|\]\]$/g, "");
    return { name, arg };
  };
  const url = () => parsed().arg;
  const embed = () => {
    const { name, arg } = parsed();
    // `?enablejsapi=1` matches OG (youtube.cljs:58) and, together with the
    // referrerpolicy below, is what makes the embed play under WebKitGTK — a bare
    // src with no referrer is rejected by YouTube's player as error 153.
    const yt = /(?:youtube\.com\/(?:watch\?v=|embed\/)|youtu\.be\/)([\w-]{11})/.exec(arg);
    if (yt) return `https://www.youtube.com/embed/${yt[1]}?enablejsapi=1`;
    if (name === "youtube" && /^[\w-]{11}$/.test(arg)) return `https://www.youtube.com/embed/${arg}?enablejsapi=1`;
    const vimeo = /vimeo\.com\/(\d+)/.exec(arg);
    if (vimeo) return `https://player.vimeo.com/video/${vimeo[1]}`;
    if (name === "vimeo" && /^\d+$/.test(arg)) return `https://player.vimeo.com/video/${arg}`;
    const bili = /bilibili\.com\/video\/(BV[0-9A-Za-z]+)/i.exec(arg);
    const bvid = bili ? bili[1] : name === "bilibili" && /^BV[0-9A-Za-z]+$/.test(arg) ? arg : null;
    if (bvid) return `https://player.bilibili.com/player.html?bvid=${bvid}&high_quality=1`;
    return null;
  };
  // OG parity (og-1.0.0 6e7afa8eb): the embed iframe's `allow`/`referrerpolicy`.
  // YouTube (youtube.cljs:54-70) sends a `strict-origin-when-cross-origin`
  // referrer so the app origin reaches YouTube — without a referrer the player
  // fails with error 153. Vimeo (block.cljs:1290-1305) gets the same `allow` list
  // minus picture-in-picture/web-share and NO referrerpolicy; bilibili sets
  // neither (the `.embed-iframe` class already removes the border).
  const embedAttrs = (): Record<string, string> => {
    const src = embed() ?? "";
    if (/(?:^|\/\/)(?:www\.)?(?:youtube\.com|youtube-nocookie\.com)\/embed\//.test(src))
      return {
        allow: "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope; picture-in-picture; web-share",
        referrerpolicy: "strict-origin-when-cross-origin",
      };
    if (src.includes("player.vimeo.com"))
      return { allow: "accelerometer; autoplay; clipboard-write; encrypted-media; gyroscope" };
    return {};
  };
  const isYoutubeEmbed = () => /(?:^|\/\/)(?:www\.)?(?:youtube\.com|youtube-nocookie\.com)\/embed\//.test(embed() ?? "");
  let player: YoutubePlayer | undefined;
  onMount(() => {
    if (!isYoutubeEmbed()) return;
    let mounted = true;
    void loadYoutubeApi().then((api) => {
      if (!mounted || !api) return;
      try {
        const registered = new api.Player(iframeId, { events: { onReady: () => undefined } });
        if (!mounted) {
          registered.destroy?.();
          return;
        }
        player = registered;
        youtubePlayers.set(iframeId, registered);
      } catch {
        // Offline, blocked, or malformed API responses leave the timestamp label usable.
      }
    });
    onCleanup(() => {
      mounted = false;
      if (youtubePlayers.get(iframeId) === player) youtubePlayers.delete(iframeId);
      player?.destroy?.();
    });
  });
  return (
    <Show
      when={embed()}
      fallback={
        <Show
          when={/\.(mp4|webm|ogg)(\?|$)/i.test(url())}
          fallback={<ExternalLink class="external-link" dest={url()} target="_blank" rel="noreferrer">{url()}</ExternalLink>}
        >
          <video class="embed-video" src={url()} controls />
        </Show>
      }
    >
      <div class="embed-iframe-wrap">
        <iframe id={isYoutubeEmbed() ? iframeId : undefined} class="embed-iframe" src={embed()!} allowfullscreen title="video" {...embedAttrs()} />
      </div>
    </Show>
  );
}

// A {{tweet URL}} / {{twitter URL}} macro (`twitter` is OG's alias for `tweet`) —
// rendered as a link (no third-party script embedding).
export function TweetMacro(props: { body: string }): JSX.Element {
  const url = () => props.body.replace(/^(tweet|twitter)\s*/i, "").trim();
  return (
    <ExternalLink class="external-link tweet-link" dest={url()} target="_blank" rel="noreferrer">
      🐦 {url()}
    </ExternalLink>
  );
}

// `{{youtube-timestamp <seconds>}}` seeks the OG-selected on-page YouTube player.
export function YoutubeTimestamp(props: { body: string }): JSX.Element {
  const secs = () => {
    const raw = props.body.replace(/^youtube-timestamp\s*/i, "").trim();
    const n = parseInt(raw, 10);
    return Number.isFinite(n) ? n : 0;
  };
  const label = () => {
    const s = Math.max(0, secs());
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = s % 60;
    const pad = (x: number) => String(x).padStart(2, "0");
    return h > 0 ? `${h}:${pad(m)}:${pad(sec)}` : `${m}:${pad(sec)}`;
  };
  return (
    <a
      class="youtube-ts"
      title="Seek the preceding YouTube video"
      onClick={(event) => {
        event.preventDefault();
        event.stopPropagation();
        // OG timestamp click stops the event and calls seekTo(seconds, true)
        // on get-player's result (og-1.0.0 6e7afa8eb,
        // extensions/video/youtube.cljs:103-111).
        youtubePlayerForTarget(event.currentTarget)?.seekTo(secs(), true);
      }}
    >
      ⏱ {label()}
    </a>
  );
}

// `{{cloze answer}}` (optionally `{{cloze answer\\cue}}`) — in OG this is hidden
// only inside the SRS flashcard-review loop. Tine has no SRS engine, so we degrade
// to a click-to-reveal: shows the cue (or `[...]`) until clicked, then the answer.
export function ClozeMacro(props: { body: string }): JSX.Element {
  const [revealed, setRevealed] = createSignal(false);
  const parts = () => props.body.replace(/^cloze\s*/i, "").trim().split(/\\\\/);
  const answer = () => (parts()[0] ?? "").trim();
  const cue = () => parts()[1]?.trim();
  return (
    <span
      class="cloze"
      classList={{ revealed: revealed() }}
      title={revealed() ? "Click to hide" : "Click to reveal"}
      onClick={(e) => {
        e.stopPropagation();
        setRevealed((v) => !v);
      }}
    >
      {revealed() ? answer() : (cue() ?? "[...]")}
    </span>
  );
}

// `{{zotero-imported-file ...}}` / `{{zotero-linked-file ...}}` — OG resolves the
// Zotero item-key to a real attachment via its Zotero connector (data dir + item
// metadata + storage config). Tine has no Zotero integration, so resolving would
// yield a dead link; we degrade to a muted, non-navigating label rather than a
// broken link. Flagged as a known parity gap (niche).
export function ZoteroMacro(props: { body: string }): JSX.Element {
  const arg = () => props.body.replace(/^zotero-(imported|linked)-file\s*/i, "").trim();
  return (
    <span class="zotero-ref" title="Zotero integration isn't supported in Tine">
      📎 {arg() || "Zotero attachment"}
    </span>
  );
}

// A {{embed ((uuid))}} or {{embed [[Page]]}} block.
export function EmbedMacro(props: { body: string; blockId?: string }): JSX.Element {
  const linkDepth = useContext(LinkDepthContext);
  if (linkDepth > MAX_DEPTH_OF_LINKS) return <LinkDepthWarning />;

  const target = () => props.body.replace(/^embed\s*/i, "").trim();
  const pageTarget = () => /^\[\[([^\]]+)\]\]$/.exec(target())?.[1];
  const selfPageEmbed = () => {
    const sourcePage = props.blockId ? doc.byId[props.blockId]?.page : undefined;
    const targetPage = pageTarget();
    return !!sourcePage
      && pageByName(sourcePage)?.kind === "page"
      && !!targetPage
      && pageIdentityKey(sourcePage) === pageIdentityKey(targetPage);
  };

  const [data] = createResource(
    () => selfPageEmbed() ? null : `${target()} ${graphEpoch()} ${dataRev()}`,
    async () => {
    const t = target();
    const blockRef = /^\(\(([^)]+)\)\)$/.exec(t);
    if (blockRef) {
      const g = await resolveBlockBatched(blockRef[1]);
      // embedId = the embedded block's own id, so its ref-count badge is hidden
      // inside the embed (OG hide-block-refs-count?); its children keep theirs.
      return g ? { page: g.page, kind: g.kind, blocks: g.blocks, embedId: g.blocks[0]?.id } : null;
    }
    const pageRef = /^\[\[([^\]]+)\]\]$/.exec(t);
    if (pageRef) {
      // Backend miss → the virtual in-app Guide, matched by bare title (the embed
      // carries no source context to remap the name). No-op for real graphs.
      const p = (await backend().getPage(pageRef[1], "page")) ?? resolveGuidePageDto(pageRef[1]);
      return p ? { page: p.name, kind: "page" as PageKind, blocks: p.blocks, embedId: undefined } : null;
    }
    return null;
  });

  return (
    <div class="embed-block">
      <Show when={!selfPageEmbed()}>
        <Show when={data()} fallback={<div class="embed-missing">{`{{${props.body}}}`}</div>}>
          <LiveRefGroup
            page={data()!.page}
            kind={data()!.kind}
            blocks={data()!.blocks}
            embedId={data()!.embedId}
            hostBlockId={props.blockId}
            surface="embed"
          />
        </Show>
      </Show>
    </div>
  );
}
